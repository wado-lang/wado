# Loam on WebGPU

This is the design of step 14 of the roadmap in
[WEP: Loam](../docs/wep-2026-09-20-loam.md): a model that runs on a WebGPU
device from its first kernel to its last, with fusion chosen for the number of
dispatches. [Loam on TPU and WebGPU](./devices.md) says how a pass is recorded
and compiled. This document holds the device kernels and the fusion rules that
compiler applies.

## Context

At batch size 1 the cost of a WebGPU forward pass is set by how many dispatches
it makes, not by how good each kernel is
([research §5](../docs/research-ml-inference-webgpu.md)). A read back waits for
the device to finish, so a kernel that runs on the CPU after reading its
operands back is a full synchronization.

## Decision

### A tensor stays on the device

`compute` reads nothing back from the device until it returns. Only its
outputs cross back, and the condition of an `If`, which decides which branch
runs.

- `Params::load` uploads a weight once, and `Params` holds it on the device.
- A device tensor is a view: a buffer, an offset into it, and a stride per
  axis. A broadcast axis has stride 0.
- A regrouping makes a view and dispatches nothing: `transpose`, `slice`,
  `reverse`, `broadcast_in_dim`, and a `reshape` wherever the view's strides
  can say it, as numpy's reshape asks before it copies. The kernel that reads
  a view reads through its strides, as DNNFusion folds a reorganizing or
  shuffling operator into the index of the operator reading it (Niu et al.,
  PLDI 2021). Merging heads after a transpose is a `reshape` no strides can
  say, so it copies.
- Every operation GPT-2 needs runs on the device: the products of
  `dot_general` (a stretched batch a stride of 0), a `gather` of whole slices
  over a device table with indices from the host, `select`, `concatenate`,
  `scatter`, a `reduce` summing or taking the maximum, and every elementwise
  operation. An elementwise operand that stretches any axis reads through a
  stride of 0.
- A `bool` tensor lives on the device as 1.0 and 0.0. An `i64` tensor stays
  on the host, since WGSL stores none. Indices are small, and a gather uploads
  them as `u32` when it runs. The index `arg_max` ranks stays on the device as
  an f32, so a token a loop feeds back needs no read back.
- `group_query_attention`, which Gemma 3 has and GPT-2 does not, is three
  dispatches. One appends the new keys, turned by the rotary tables, and values
  to the attention cache. The cache holds a power of two of positions, so a step
  writes only its own positions and binds what the step before bound. The next
  attends each query row over each part of 4 positions in a workgroup of its
  own, 64 lanes splitting each position's dot. The last joins a row's parts,
  rescaling each to the row's greatest score. Gemma's step over one token has 4
  query rows, so a workgroup a row left the device idle and grew slower with the
  sequence: at 600 positions a token took 1.9 times as long as with parts of 64
  positions. Parts of 4 with the dot split took Gemma's 8 tokens from a short
  prompt in 0.94 times the time of parts of 64.
- A product of at most eight rows, as a step over one token or a short prompt
  makes, does not run in 16 by 16 tiles, which would leave most lanes idle.
  Each workgroup computes a run of columns, its lanes splitting the inner axis,
  four rows at a time. Its time grows with the rows, and the tiled product's
  does not: on GPT-2 (124M) the two meet at about seven. Across a transposed
  weight a workgroup takes the most columns that still leave 128 workgroups. A
  layer's products of 256 to 2048 columns ran in 32 to 256 workgroups of 8
  columns, too few reads at once for the device, and took 1.2 times as long.
- `arg_max` leaves one index per lane on the device, as an f32, which holds a
  vocabulary's indices exactly. A language model picking its next token then
  reads back at most one index, not the vocabulary's logits.
- The device's limits are requested at the adapter's own, and a dispatch binds
  only the span of a buffer its views reach. GPT-2's token embedding is 154 MB,
  past WebGPU's default binding of 128 MiB, which is all llvmpipe offers. A
  kernel whose reach still outgrows a binding runs in parts, and a tiled
  product's parts add what one dispatch would, in the same order.

Dispatches go into one compute pass of one command encoder, which is submitted
when a tensor is read back or after every `SUBMIT_EVERY` dispatches, so the
device runs the first while the host records the rest.

A weight's buffer lives as long as the backend, and a launch of a compiled
program owns the buffers it makes. Every other buffer belongs to the pass, and
the next pass releases it, unless this pass retained it: Gemma's attention cache
stays on the device from one token to the next.

A released buffer is a spare, and the next pass takes one of the size it asks
for before it makes one. A pass of a model asks for the sizes the one before it
did, so each dispatch binds the buffers the same dispatch bound a pass before,
and finds the bind group that one made. Making a bind group cost the host 2.9
times what setting the pipeline and bind group and dispatching did, and making a
buffer 0.9 times.

### A product weight is kept column by column, in bf16 where that loses nothing

A weight is written column by column, so the inner axis of a product is the
contiguous one. A weight one binding does not hold is kept row by row, so a
product can run over chunks of its inner axis. A product of a few rows then
gives each column a run of lanes splitting the inner axis. Kept row by row, a
lane takes a column, and a layer's products of 256 to 2048 columns ran in 4 to
32 workgroups, leaving most of the device idle.

A weight is written in bf16 when every value's low 16 bits are zero. A model
released in bf16 and exported in f32 has such weights: every product weight of
Gemma 3 (270M) and its embedding do. The product, gather and copy shaders widen
each element as they read it, so the results are the ones f32 storage gives. The
weights take half the device memory, and a lane of a product of a few rows reads
two elements a word.

### Fusion is chosen for dispatches

The device compiler groups a recorded pass into launches. `Cpu` computes the
graph unfused, so the rules serve the device alone.

#### A value fuses where all its readers are

A value fuses into a group when every node that reads it is in that group,
rather than only when one node reads it. A value read three times in a kernel is
computed once there, and a value that reaches a kernel by two paths is one step
in it. GPT-2's GELU reads its input four times, and becomes one kernel.

A node that reads no argument is computed once, when the pass is compiled, and
a kernel folds in its value where every element holds one. A view, or a
conversion from a `bool` or to its own element, computes nothing on the device,
so it starts no kernel of its own.

#### A reduction along the last axis joins its row

A row group is a kernel over the rows of its output, a row being the last axis.
Its `Expr` may sum or take the maximum of a step along the row, and read the
result at every element of that row. That is enough for a layer normalization,
which reduces twice, and for a softmax:

```text
mean     = row_sum(x) / n
centered = x - mean
y        = centered / sqrt(row_sum(centered * centered) / n + eps) * g + b

softmax(x) = exp(x - row_max(x)) / row_sum(exp(x - row_max(x)))
```

This is the stitching of reductions into the memory-bound operators around them
that AStitch does on a GPU (Zheng et al., ASPLOS 2022). On WebGPU one workgroup
owns a row, so a reduction needs a barrier within the workgroup and none across
the device, which WebGPU does not have.

A node joins a row group when it is elementwise, or a `reduce` summing or
taking the maximum along the last axis, and every tensor it reads or writes
spans the group's rows: the whole row, or one value per row. A row group takes
no contraction, so a product's epilogue stays elementwise.

#### `select` over f32 is elementwise

`select` picks between two f32 values by a condition. As an elementwise step
over a condition operand, read as 1.0 or 0.0, it fuses into a product's
epilogue and into a row group. GPT-2's attention mask is one.

#### A linear layer is one product over the leading axes

An exporter writes a linear layer as a `Reshape` flattening the leading axes,
a `Gemm`, and a `Reshape` restoring them. Each reshape is a view, so the
product's epilogue reads through the one after it, and GPT-2's GELU and its
residual adds join the epilogue of the layer before them.

#### Two products of one input pair

A gated layer multiplies one input by a gate matrix and by a value matrix, and
meets the two elementwise. A kernel otherwise follows one contraction, so the
second product would write its output and the kernel would read it back. Two
matrix products of one left operand over one iteration space pair instead, with
the operators after them as their epilogue.

Each right matrix must be a weight its product alone reads, and the two must
share one element type and one list of axes. The two are then kept as one matrix
of twice the columns, a column of the first, then the same column of the second.
A product of a few rows reading that matrix along its inner axis runs as one
dispatch. Any other runs the first product, then the second with the epilogue
reading the first.

#### Products of one input read apart are one product

Attention's query, key and value are three products of one input, each read by
a kernel of its own, so they do not pair. The products of one left operand that
each write their product alone, each by a weight its product alone reads, join.
Their weights are kept as one matrix whose columns are each weight's in turn,
and the device computes the one product and reads each part as a view. The parts
are one kernel, so the normalization before them is its prologue.

#### A normalization is its product's prologue

A normalization before a layer is a row group that only the layer's products
read, by weights, as their left operand. Such a group, one that takes no
contraction in, is the prologue of the products' kernel. The product computes it
over the rows it reads, so a group that folds rows is one only where it writes
them whole, not one value per row.

On a device, a product of a few rows runs it in the product's dispatch. Each
workgroup folds a block of rows and computes the block's elements into
workgroup memory, which its lanes then read as the left matrix. Every
workgroup repeats the fold, which costs the rows' reads again per workgroup
and saves a dispatch and the normalized tensor's write and read. A product of
more rows, of rows too long for workgroup memory, or a pair of products by
weights that are not transposed, writes the prologue with the row kernel it
would otherwise have had.

#### What a layer costs

These rules ran GPT-2's layer in ten dispatches when the generator applied them:

| Dispatch            | Nodes it covers                           |
| ------------------- | ----------------------------------------- |
| Layer normalization | 9 nodes                                   |
| QKV product         | `Gemm` and its bias                       |
| Attention scores    | `MatMul`                                  |
| Softmax             | `Softmax`, its scale, and the mask        |
| Attention values    | `MatMul`                                  |
| Heads merged        | a copy, the `reshape` after the transpose |
| Projection          | `Gemm`, its bias, and the residual `Add`  |
| Layer normalization | 9 nodes                                   |
| Feed-forward up     | `Gemm`, its bias, and the GELU            |
| Feed-forward down   | `Gemm`, its bias, and the residual `Add`  |

The scale and the mask go with the softmax rather than the scores' product,
since a product and a fold share no kernel. The split and the transposes are
views. Around the layers were six more: the two embeddings, their sum, the
last normalization, the head, and the mask. The tiny GPT-2's five layers took
56 a pass, and GPT-2 (124M)'s twelve took 126.

## Measurements

`example/gpt2-124m/webgpu.wado` continued "Hello, my name is" with eight
greedy tokens on each backend, and `example/gemma3-270m/webgpu.wado` continued
two prompts with eight each. [The benchmarks](./benchmark/README.md) time 32.
Both run one token a call against the attention cache, pick onnxruntime's
tokens, and time each token after the prompt's first. The device is an RTX 4060
Ti under Vulkan, and `wado run-webgpu` is the `dev` build of `wado-run-webgpu`.
GPT-2's `Cpu` runs under `wado run-webgpu`'s wasmtime, and Gemma's is
`example/gemma3-270m/gemma3.wado` under `wado run`. Every Loam program runs at
`-O2`. onnxruntime is each example's `generate.mjs` on `onnxruntime-node`, on the
CPU and on its WebGPU provider, which keeps the attention cache on the device
and reads back the logits. The kernel pass, the generated sequence of kernel
calls `compute` replaced, was measured before its deletion, by the same
programs.

| Model        | Backend                          | Time a token, onnxruntime on the CPU as 1 | Dispatches a token | Read backs a token |
| ------------ | -------------------------------- | ----------------------------------------- | ------------------ | ------------------ |
| GPT-2 (124M) | `WebGpuBackend`, `compute`       | 1.6                                       | 180                | 1                  |
| GPT-2 (124M) | `WebGpuBackend`, the kernel pass | 0.63                                      | 164                | 2                  |
| GPT-2 (124M) | `Cpu`, `compute`                 | 7.8                                       |                    |                    |
| GPT-2 (124M) | `Cpu`, the kernel pass           | 4.1                                       |                    |                    |
| GPT-2 (124M) | onnxruntime on WebGPU            | 1.3                                       |                    |                    |
| Gemma 3 270M | `WebGpuBackend`, `compute`       | 0.15–0.23                                 | 203                | 1                  |
| Gemma 3 270M | `WebGpuBackend`, the kernel pass | 0.18–0.19                                 | 204                | 1                  |
| Gemma 3 270M | `Cpu`, `compute`                 | 10                                        |                    |                    |
| Gemma 3 270M | `Cpu`, the kernel pass           | 2.5                                       |                    |                    |
| Gemma 3 270M | onnxruntime on WebGPU            | 0.34–0.75                                 |                    |                    |
| either       | onnxruntime on the CPU           | 1                                         |                    |                    |

A range spans Gemma's two prompts. onnxruntime on WebGPU reads near 0.35 on
the first, and near 0.35 or near 0.75 on the second from one run to the next.

On llvmpipe, which binds 128 MiB at most, GPT-2's head runs in two chunks, its
weight being the 154 MB embedding.

### What each change won on Gemma

Gemma's token cost the host more than the device until the dispatches shared
one compute pass and one parameter buffer, `wado run-webgpu` built wasmtime and
wgpu-core optimized, and `arg_max` replaced reading back the logits. Each
roughly halved it. By the table's last row the device set the time. The figures are
against onnxruntime on the CPU timed with the prompt's token included, and the
build of `wado-run-webgpu` behind them is not recorded.

| Change                                                         | Time a token |
| -------------------------------------------------------------- | ------------ |
| The three above                                                | 0.65         |
| Products of a few rows                                         | 0.45         |
| Submitting every 32 dispatches                                 | 0.34         |
| Product weights column by column, in bf16                      | 0.28         |
| `arg_max`'s lane split over the device                         | 0.25         |
| Buffers and bind groups reused                                 | 0.19         |
| Pairs of products, workgroups of 256 lanes, one product of QKV | 0.13         |
