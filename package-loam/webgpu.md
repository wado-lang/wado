# Loam on WebGPU

This is the design of step 14 of the roadmap in
[WEP: Loam](../docs/wep-2026-09-20-loam.md): a `forward` that runs on a WebGPU
device from its first kernel to its last, with fusion chosen for the number of
dispatches.

## Context

At batch size 1 the cost of a WebGPU forward pass is set by how many dispatches
it makes, not by how good each kernel is
([research §5](../docs/research-ml-inference-webgpu.md)). The first cut of the
backend did not reach that regime. It ran the products and the elementwise
kernels on the device, and every other kernel on the CPU after reading its
operands back. A read back waits for the device to finish, so each such kernel
was a full synchronization.

`loam dump` counts what GPT-2 (124M) asks of a backend in one `forward`, with
the fusion of step 13:

| Lowering | Nodes |
| -------- | ----- |
| kernel   | 548   |
| fused    | 172   |
| folded   | 2351  |
| alias    | 24    |

Among the kernels are 148 `Reshape`, 60 `Transpose`, 50 `ReduceMean`, 25 each
of the `Sub`, `Pow` and `Sqrt` a layer normalization spells, and 12 `Softmax`.
The reasons the plan gives for a kernel writing its output are mostly a
regrouping (247), a value read more than once (87), and a reader that is a
reduction or a regrouping (98).

The first cut also kept every weight on the host and uploaded it on each call,
and submitted each dispatch on its own.

## Decision

The work is in three parts, in this order. The first two are measured apart,
since fusing kernels cannot be measured while synchronizations dominate.

### A tensor stays on the device

`forward` reads nothing back from the device until it returns. Only its
outputs cross back, and the condition of an `If`, which decides which branch
runs.

- `prepare` uploads a weight once, and `Prepared` holds it on the device.
- A device tensor is a view: a buffer, an offset into it, and a stride per
  axis. A broadcast axis has stride 0.
- A regrouping makes a view and dispatches nothing: `transpose2`, `permute`,
  `lift`, `slice_axis` and with it `Split`, and a `reshape` wherever the
  view's strides can say it, as numpy's reshape asks before it copies. The
  kernel that reads a view reads through its strides, as DNNFusion folds a
  reorganizing or shuffling operator into the index of the operator reading it
  (Niu et al., PLDI 2021). Merging heads after a transpose is a `reshape` no
  strides can say, so it copies.
- Every kernel GPT-2 needs runs on the device: the products (`matmul_broadcast`
  included, its stretched batch a stride of 0), `gather_rows` over a device
  table with indices from the host, `select`, `concat`, `softmax`,
  `reduce_sum` and `reduce_mean`. An elementwise operand that stretches any
  axis reads through a stride of 0, so every `fused` call runs on the device.
- `i64` and `bool` tensors stay on the host, since WGSL stores neither. Indices
  and conditions are small, and a kernel that reads one uploads it as `u32`
  when it runs.
- The convolutions and `max_pool` stay on the CPU. No model step 14 targets
  has them.
- The device's limits are requested at the adapter's own, and a dispatch binds
  only the span of a buffer its views reach. GPT-2's token embedding is 154 MB,
  past WebGPU's default binding of 128 MiB, which is all llvmpipe offers. A
  gather whose rows still outgrow a binding runs over halves of its indices,
  and a copy over halves of an axis. A product runs over chunks of its inner
  axis, whole tiles each, every chunk continuing the sums the one before left,
  so it adds what one dispatch would, in the same order.

Every dispatch of one `forward` goes into one command encoder, which is
submitted when a tensor is read back. A dispatch's parameters are written to a
buffer of its own, so none is overwritten before the encoder runs.

A resource is move-only in Wado, so views cannot share a `GpuBuffer` by value.
The backend owns its buffers and a view names one by its place. `keep`, which
`Prepared` calls for every weight, writes a buffer that lives as long as the
backend. Every other buffer belongs to the pass, and `begin`, which `forward`
calls first, releases those of the pass before: `forward` returns only what it
downloaded, so nothing still names them.

### Fusion is chosen for dispatches

The plan is one plan for every backend. Each rule below removes dispatches on a
device and passes over memory on the CPU, so the two objectives agree on all of
them. A rule they disagree on would be the reason to split the plan, and none
has come up.

#### A value fuses where all its readers are

A value fuses into a group when every node that reads it is in that group,
rather than only when one node reads it. An `Expr` is already a graph of steps,
each naming the steps it reads, so a value read three times in a kernel is
computed once there. GPT-2's GELU reads its input four times, and becomes one
kernel.

Composing two `Expr`s shares a step both already compute, so a value that
reaches a kernel by two paths is one step in it.

The plan reads the nodes last to first, so a node's readers have their kernels
when it is decided. Two kinds of reader do not count. A folded node such as
`Shape` reads extents, which stage 0 already has. An alias, an `Identity` or a
`Cast` to the type it has, is its operand under another name, so its readers
are the operand's.

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

A node joins a row group when it is elementwise, a `ReduceMean` or `ReduceSum`
over the last axis keeping it, or a `Softmax` along the last axis, and every
tensor it reads or writes spans the group's rows: the whole row, or one value
per row. A mean's count is a constant of the `Expr`, so its row is of an extent
stage 0 knows. A row group takes no contraction, so a product's epilogue stays
elementwise.

The CPU evaluates a row group one row at a time, each step a loop over the row.
A row sum adds from 0.0 in the row's order, and a row maximum folds from
negative infinity, as `reduce_sum` and `softmax` do. So a fused module still
computes what the unfused one does, bit for bit.

#### `Where` over f32 is elementwise

`Where` picks between two f32 values by a condition. As an elementwise step
over a condition operand, read as 1.0 or 0.0, it fuses into a product's
epilogue and into a row group. GPT-2's attention mask is one.

#### A linear layer is one product over the leading axes

An exporter writes a linear layer as a `Reshape` flattening the leading axes,
a `Gemm`, and a `Reshape` restoring them. Between the product and the
operators after it stands a regrouping, which no kernel takes in. The plan
lowers the three as one `matmul_shared` over the leading axes instead (a
`Spread`), the two reshapes computing nothing. Each element of the product
sums the same terms in the same order, so the CPU's result does not change.
GPT-2's GELU and its residual adds then join the epilogue of the layer before
them.

Whether a node computes anything now depends on its neighbours, so lowering is
the plan's answer, `Plan::lowering`, which emission and `loam dump` both read.

#### What a layer costs

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

Ten dispatches a layer. The scale and the mask go with the softmax rather than
the scores' product, since a product and a fold share no kernel. The split and
the transposes are views. Around the layers are six more: the two embeddings,
their sum, the last normalization, the head, and the mask. The tiny GPT-2's
five layers take 56 a pass, which `wado-run-webgpu`'s tests hold it to, and
GPT-2 (124M)'s twelve take 126.

### Finished when GPT-2 picks onnxruntime's tokens on a device

`package-loam/example/gpt2-124m/` continues a prompt under
`wado run-webgpu` with the tokens onnxruntime picks, every kernel on the
device. Its time per token is recorded against `Cpu` and against onnxruntime,
which is run as a program and never read.

Memory planning, a buffer reused once the tensor in it is dead, waits for a
measurement that shows allocation matters.

## Plan

- [x] Device residency: views, the device kernels, prepared weights on the
  device, one encoder per `forward`, limits at the adapter's.
  `conformance/webgpu.wado` runs every kernel on the device against `Cpu`, and
  again under a binding smaller than its weights. `conformance/webgpu_gpt2.wado`
  counts one read back a pass.
- [x] Fusion for dispatches: sharing in `Expr`, groups where all readers are,
  row groups, and `Where` as a step. `loam dump` reports GPT-2's kernels
  per layer, and `conformance/fusion_test.wado` still holds the fused module
  to the unfused one bit for bit.
- [x] GPT-2 (124M) under `wado run-webgpu`, with the tokens onnxruntime picks
  and the times recorded.

## Measurements

`example/gpt2-124m/webgpu.wado` continues "Hello, my name is" with eight
greedy tokens on each backend, and picks the tokens onnxruntime picks on the
device. The device is llvmpipe, a software rasterizer, which is all this
machine offers. Its times say how many synchronizations and dispatches a pass
makes, not how a GPU runs it. `Cpu` runs under `wado run-webgpu`'s wasmtime,
and onnxruntime is `generate.mjs` on `onnxruntime-node`.

| Backend                    | Time a token | Dispatches a token | Read backs a token |
| -------------------------- | ------------ | ------------------ | ------------------ |
| `WebGpuBackend` (llvmpipe) | 1593 ms      | 127                | 1                  |
| `Cpu`                      | 3247 ms      |                    |                    |
| onnxruntime, CPU           | 15 ms        |                    |                    |

The 127 dispatches are the 126 the layer table predicts and one more: llvmpipe
binds 128 MiB at most, so the head's product, whose weight is the 154 MB
embedding, runs in two chunks.
