# Loam on WebGPU

This is the design of step 14 of the roadmap in
[WEP: Loam](../docs/wep-2026-09-20-loam.md): a model that runs on a WebGPU
device from its first kernel to its last, with fusion chosen for the number of
dispatches.

Step 14 built it as a generated `forward`, a sequence of kernel calls whose
fusion the generator planned at build time.
[Loam on TPU and WebGPU](./devices.md) replaced that kernel pass with
`compute` over a recorded graph. `WebGpuBackend` compiles the recording once
per shape class, grouping it into kernels as the rules below say
(`src/runtime/fusion.wado`), and each call replays the program. The device
kernels and the rules are this design's.

## Context

At batch size 1 the cost of a WebGPU forward pass is set by how many dispatches
it makes, not by how good each kernel is
([research §5](../docs/research-ml-inference-webgpu.md)). The first cut of the
backend did not reach that regime. It ran the products and the elementwise
kernels on the device, and every other kernel on the CPU after reading its
operands back. A read back waits for the device to finish, so each such kernel
was a full synchronization.

`loam dump` counted what GPT-2 (124M) asked of a backend in one `forward`,
with the fusion of step 13:

| Lowering | Nodes |
| -------- | ----- |
| kernel   | 548   |
| fused    | 172   |
| folded   | 2351  |
| alias    | 24    |

Among the kernels were 148 `Reshape`, 60 `Transpose`, 50 `ReduceMean`, 25 each
of the `Sub`, `Pow` and `Sqrt` a layer normalization spells, and 12 `Softmax`.
The reasons the plan gave for a kernel writing its output were mostly a
regrouping (247), a value read more than once (87), and a reader that is a
reduction or a regrouping (98).

The first cut also kept every weight on the host and uploaded it on each call,
and submitted each dispatch on its own.

## Decision

The work is in three parts, in this order. The first two are measured apart,
since fusing kernels cannot be measured while synchronizations dominate.

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
  them as `u32` when it runs.
- `convolution` and `reduce_window` run on the CPU. No model step 14 targets
  has them.
- `group_query_attention`, which Gemma 3 has and GPT-2 does not, is three
  dispatches. One appends the new keys, turned by the rotary tables, and
  values to the attention cache. The cache holds a power of two of positions,
  so a step writes only its own positions, into the cache its past is in, and
  binds what the step before bound. A step whose past is not the cache's last,
  as a branch from an earlier past is, or that outgrows the cache, copies the
  past to a cache of its own first. The next attends each query row over each
  part of 4 positions in a workgroup of its own: the turned query, its scores,
  their weights against the part's greatest, and the values they weigh. 64
  lanes split each position's dot, as a lane a position summed Gemma's 256
  elements alone. The cache's parts are held for every position it holds, so
  they too bind as the step before's did. A prompt whose rows would outgrow a
  binding in parts of 4 takes larger parts, as many rows spread over the device
  in any. The last joins a row's parts, rescaling each to the row's greatest
  score. Gemma's step over one token has 4
  query rows, so a workgroup a row left the device idle and grew slower with
  the sequence: at 600 positions a token took 1.9 times as long as with parts
  of 64 positions. Parts of 4 with the dot split took Gemma's 8 tokens from a
  short prompt in 0.94 times the time of parts of 64.
  `conformance/webgpu_attention.wado` holds it to `Cpu`.
- A product of at most six rows, as a step over one token or a short prompt
  makes, does not run in 16 by 16 tiles, which would leave most lanes idle.
  Each workgroup computes a run of columns, its lanes splitting the inner
  axis, four rows at a time. Its time grows with the rows, and the tiled
  product's does not: on GPT-2 (124M) the two meet at about seven. The lanes
  that read the weight together read where it is contiguous: one row's run of
  columns, or one column of a transposed weight, such as Gemma's head. Across
  a transposed weight a workgroup takes the most columns that still leave 128
  workgroups, from 16 down to 4, and adds each column's parts in halves. A
  layer's products of 256 to 2048 columns ran in 32 to 256 workgroups of 8
  columns, too few reads at once for the device, and took 1.2 times as long.
- `arg_max` ranks each part of 4096 elements of a lane in a workgroup of its
  own, then a lane's parts in another, and reads back one index per lane, as
  an f32, which holds a vocabulary's indices exactly. A language
  model picking its next token then reads back one index, not the vocabulary's
  logits.
- The device's limits are requested at the adapter's own, and a dispatch binds
  only the span of a buffer its views reach. GPT-2's token embedding is 154 MB,
  past WebGPU's default binding of 128 MiB, which is all llvmpipe offers. A
  gather whose rows still outgrow a binding runs over halves of its indices,
  and a copy over halves of an axis. A product runs over chunks of its inner
  axis, whole tiles each, every chunk continuing the sums the one before left.
  A tiled product so adds what one dispatch would, in the same order.

Dispatches go into one compute pass of one command encoder, which is submitted
when a tensor is read back or after every `SUBMIT_EVERY` dispatches, so the
device runs the first while the host records the rest. Each dispatch's
parameters take a span of their own in a buffer of parameters, so none is
overwritten before the encoder runs.

A resource is move-only in Wado, so views cannot share a `GpuBuffer` by value.
The backend owns its buffers and a view names one by its place. `keep` writes a
buffer that lives as long as the backend, and `Params::load` keeps each weight
so. A launch of a compiled program owns the buffers it makes, and they live
while the launch runs. Every other buffer belongs to the pass, and `begin`,
which `compute` calls first, releases those of the pass before. `compute` has
downloaded its outputs before it returns, so nothing still names those buffers.
The exception is a buffer the pass retained, which lives through the next pass:
Gemma's attention cache stays on the device from one token to the next.

A released buffer is a spare, and the next pass takes one of the size it asks
for before it makes one, the spares in the order the last pass made them. A
pass of a model asks for the sizes the one before it did, so each dispatch
binds the buffers the same dispatch bound a pass before, and finds the bind
group that one made. Making a bind group cost the host 2.9 times what setting
the pipeline and bind group and dispatching did, and making a buffer 0.9
times. A spare the pass does not take, and a bind group it does not
bind, is dropped at the next `begin`, so what grows with the sequence, as an
attention cache does, holds no more than two passes' worth.

### A product weight is kept column by column, in bf16 where that loses nothing

`kept_matrix` writes a weight column by column, so the inner axis of a product
is the contiguous one. A weight one binding does not hold is kept row by row,
so a product can run over chunks of its inner axis. A product of a few rows
then gives each column a run of lanes splitting the inner axis. Kept row by
row, a lane takes a column, and a layer's products of 256 to 2048 columns ran
in 4 to 32 workgroups, leaving most of the device idle.

It writes the weight in bf16 when every value's low 16 bits are zero, two
elements to a word. A model released in bf16 and exported in f32 has such
weights: every product weight of Gemma 3 (270M) and its embedding do. The
product, gather and copy shaders widen each element as they read it, so the
results are the ones f32 storage gives, and every other kernel reads f32 only.
The weights take half the device memory, and a lane of a product of a few rows
reads two elements a word.

### Fusion is chosen for dispatches

`planned` groups a recorded pass into launches when the backend compiles it.
`Cpu` computes the graph unfused, so the rules serve the device alone.

#### A value fuses where all its readers are

A value fuses into a group when every node that reads it is in that group,
rather than only when one node reads it. An `Expr` is already a graph of steps,
each naming the steps it reads, so a value read three times in a kernel is
computed once there. GPT-2's GELU reads its input four times, and becomes one
kernel.

Composing two `Expr`s shares a step both already compute, so a value that
reaches a kernel by two paths is one step in it.

The plan reads the nodes last to first, so a node's readers have their kernels
when it is decided. A node that reads no argument is computed once, when the
pass is compiled, and a kernel folds in its value where every element holds
one. A view, or a conversion from a `bool` or to its own element, computes
nothing on the device, so it starts no kernel of its own.

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
second product would write its output and the kernel would read it back. The
plan pairs two matrix products of one left operand over one iteration space
instead (a `Paired` launch), with the operators after them as its epilogue,
which reads the two products as `operand(0)` and `operand(1)`.

Each right matrix must be a weight its product alone reads, and the two must
share one element type and one list of axes. `kept_weights` then keeps the two
as one matrix of twice the columns, a column of the first, then the same
column of the second (`Paired<N>`). A product of a few rows reading that matrix
along its inner axis runs as one dispatch: a workgroup's lanes sum an even
column and the one after it, and the lane holding the even one applies the
epilogue to both sums. Any other runs the first product, then the second with
the epilogue reading the first.

#### Products of one input read apart are one product

Attention's query, key and value are three products of one input, each read by
a kernel of its own, so they do not pair. The plan joins the products of one
left operand that each write their product alone, by one `DotDims`, each by a
weight its product alone reads (a `Joined` launch). `kept_weights` keeps their
weights as one matrix whose columns are each weight's in turn, each weight a
view of its columns, and the device computes the one product and reads each
part as a view. The parts are one kernel, so the normalization before them is
its prologue.

#### A normalization is its product's prologue

A normalization before a layer is a row group that only the layer's products
read, by weights, as their left operand. The plan makes such a group, one that
takes no contraction in, the prologue of the products' kernel (`staged`). A row
group still takes no contraction: the prologue is a kernel of its own that the
product computes rather than reads. The product computes it over the rows it
reads, so a group that folds rows is one only where it writes them whole, not
one value per row. The plan finds prologues after every kernel is formed,
since a kernel the plan has not finished may still take a product in.

On a device, a product of a few rows runs it in the product's dispatch. Each
workgroup folds a block of rows and computes the block's elements into
workgroup memory, which its lanes then read as the left matrix. Every
workgroup repeats the fold, which costs the rows' reads again per workgroup
and saves a dispatch and the normalized tensor's write and read. A product of
more rows, of rows too long for workgroup memory, or a pair of products by
weights that are not transposed, writes the prologue with the row kernel it
would otherwise have had.

#### What a layer costs

The kernel pass, which applied these rules at build time, ran GPT-2's layer in
ten dispatches:

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

### Finished when GPT-2 picks onnxruntime's tokens on a device

`package-loam/example/gpt2-124m/` continues a prompt under
`wado run-webgpu` with the tokens onnxruntime picks, every kernel on the
device. Its time per token is recorded against `Cpu` and against onnxruntime,
which is run as a program and never read.

Memory planning within a pass, a buffer reused once the tensor in it is dead,
is a [known gap](./devices.md#known-gaps) of Loam on TPU and WebGPU.

## Plan

- [x] Device residency: views, the device kernels, weights on the device, one
  encoder a pass, limits at the adapter's. `conformance/webgpu_graph.wado`
  runs the graph's operations on the device against `Cpu`.
  `conformance/webgpu_gpt2.wado` counts one read back a pass.
- [x] Fusion for dispatches: sharing in `Expr`, groups where all readers are,
  row groups, and `select` as a step. `src/runtime/fusion_test.wado` holds the
  groups, and `conformance/webgpu_graph.wado` the fused program to `Cpu`.
- [x] GPT-2 (124M) under `wado run-webgpu`, with the tokens onnxruntime picks
  and the times recorded.

## Measurements

`example/gpt2-124m/webgpu.wado` continues "Hello, my name is" with eight
greedy tokens on each backend, and `example/gemma3-270m/webgpu.wado` continues
two prompts with eight each. Both run one token a call against the attention
cache, pick onnxruntime's tokens, and time each token after the prompt's first.
The device is an RTX 4060 Ti under Vulkan, and `wado run-webgpu` is the `dev`
build of `wado-run-webgpu`. GPT-2's `Cpu` runs under `wado run-webgpu`'s
wasmtime, and Gemma's is `example/gemma3-270m/gemma3.wado` under `wado run`.
Every Loam program runs at `-O2`. onnxruntime is each example's `generate.mjs`
on `onnxruntime-node`, on the CPU and on its WebGPU provider, which keeps the
attention cache on the device and reads back the logits. The kernel pass rows
were measured before its deletion, by the same programs running it.

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

`Cpu`'s `compute` reads each weight from a `List` with one bounds check an
element, where the kernel pass read packed vectors from linear memory.

A range spans Gemma's two prompts. onnxruntime on WebGPU reads near 0.35 on
the first, and near 0.35 or near 0.75 on the second from one run to the next.

On llvmpipe, which binds 128 MiB at most, GPT-2's head runs in two chunks, its
weight being the 154 MB embedding.

The ratios in the next paragraph are against onnxruntime on the CPU timed with
the prompt's token included, and the build of `wado-run-webgpu` behind them is
not recorded.

Gemma's token cost the host more than the device until the dispatches shared
one compute pass and one parameter buffer, `wado run-webgpu` built wasmtime and
wgpu-core optimized, and `arg_max` replaced reading back the logits. Each
roughly halved it. The products of a few rows then took it from 0.65 to 0.45,
submitting every 32 dispatches to 0.34, keeping the product weights column by
column in bf16 to 0.28, splitting `arg_max`'s lane over the device to 0.25,
and reusing buffers and bind groups to 0.19. Then the device set the time:
pairing the gate and the value, workgroups of 256 lanes for the rows and
attention, and one product of the query, key and value, its normalization in
it, took it to 0.13.
