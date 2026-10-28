# Loam on TPU

This is the design of a PJRT backend, which runs `forward` on TPUs and on native
GPUs. The roadmap item "The PJRT backend" in [TODO.md](./TODO.md) builds it
on XLA's CPU plugin first. Only a backend that really runs through PJRT shows the architecture
right, and a TPU run waits until it does.

## Context

[WEP: Loam](../docs/wep-2026-09-20-loam.md) runs a model's `forward` on any
implementation of one `Backend` trait. Two exist: `Cpu`, and `WebGpuBackend`
over `wasi:webgpu` ([Loam on WebGPU](./webgpu.md)). Neither reaches a TPU, and
neither reaches a GPU's matrix units: WebGPU's subgroup matrix is still in
standardization
([research §10](../docs/research-ml-inference-webgpu.md#10-webgpu-feature-status)).

[Research: Google TPU Through PJRT](../docs/research-tpu.md) records how a program
reaches a TPU. The only public entry is a PJRT plugin, `libtpu.so`, given a
StableHLO program for XLA to compile. The same C API drives XLA's CPU and CUDA
plugins. On a machine with no TPU, `libtpu.so` still compiles for a named TPU
generation and reports the program's device memory and estimated run time,
while the CPU plugin runs the program.

### What a backend is for

The goal is the best speed on a TPU and on a GPU, both for one request at a
time and for many requests served at once. `Cpu` only has to run correctly; it
is the reference the other backends are held to.

### Who fuses

The two device backends differ in who decides fusion.

| Backend         | Who fuses                                        | When a shape is fixed | What sets its speed                                   |
| --------------- | ------------------------------------------------ | --------------------- | ----------------------------------------------------- |
| `WebGpuBackend` | Loam's plan, chosen for the number of dispatches | at each call          | dispatches, residency on the device, weight width     |
| PJRT            | XLA, over the whole program it is given          | at compile            | XLA's fusion, the matrix units, bf16, buffer donation |

A PJRT backend that ran each kernel as its own program would pay a launch per
kernel and leave XLA nothing to fuse. A backend over XLA needs a whole
`forward` as one program.

## Decision

### `wado:pjrt`, served by `wado-run-pjrt`

The Wado binding to PJRT is `wado:pjrt`. `wasi:` is reserved for WASI, and PJRT
is no WASI proposal. The host that serves it is the crate `wado-run-pjrt`, a
workspace of its own as `wado-run-webgpu` is, since it loads a vendor's plugin
the rest of the toolchain never needs.

### The `Backend` trait stays

`forward<Bk: Backend>` stays the one body every backend runs. A PJRT backend is
a third implementation, not a second code generator.

### Two device backends, each its own role

`WebGpuBackend` is the portable one, on a standard. The PJRT backend is the fast
one, tied to a vendor's plugin: TPUs through `libtpu.so`, NVIDIA GPUs through the
CUDA plugin. Both are production backends. `Cpu` stays the reference.

A device may have one backend for a single request and another for many, where
the best kernels for the two differ. The trait admits both, and neither shapes
the other.

### The PJRT backend traces once and replays

A `pass` is one program. The first pass over a set of extents runs against
tensors that hold no values, and each trait method appends StableHLO
operations. Every later pass over the same extents replays: each method returns
a handle and appends nothing.

What a method takes from the host is either the program's shape or a value the
program reads:

- Extents, axes, a rank, a permutation, a slice's length and step, and an
  `Expr` shape the program. Together with the extents of the pass's inputs
  they are its key.
- `range_i64`'s `start` and `delta`, `slice_axis`'s `start`, and `filled`'s
  `value` are values. A decoder's position reaches `range_i64` as its `start`
  and changes every token. Each becomes a scalar parameter, so a replay passes
  the new value rather than compiling again. A slice from a parameter is a
  `stablehlo.dynamic_slice`, whose start is an operand.
- An `upload` is a parameter, and a prepared weight a parameter that stays on
  the device.

The pass ends at a trait method that names its outputs, which the generated
`forward` calls once `pass` returns and before any `download`. The backend
compiles there when tracing, and executes there when replaying. Each
`download` then copies out one result of that execution. `begin` and `retain`
already mark where a pass starts and what outlives it.

The calls of a pass depend only on its key, once every value is a parameter and
an `If` is a trait method (below): `forward` reads nothing from the device
before the pass ends. So replay checks each call against the trace as it
arrives, and traps on a difference, which would be a defect in Loam.

`fused`, `rows` and the products with prologues and epilogues lower to the
StableHLO operations they mean. XLA fuses them again for its target, so the
plan's fusion, chosen for WebGPU, costs XLA nothing.

### The seam carries what a compiled program cannot learn at run time

XLA refuses a dimension of unknown extent, and `libtpu.so` aborts the process
on one ([research §6](../docs/research-tpu.md#6-shapes-must-be-static)). So
every extent that varies between calls reaches the trait as a capacity fixed
per program, with what is valid within it as a value:

- An attention cache has a capacity and a valid length. The length is a scalar
  input, not an extent, so a decoder does not compile once per token.
  `WebGpuBackend` already keeps its cache this way inside the backend.
- A sequence is padded to the capacity of its bucket, and the valid length rides
  beside it. Only the graph knows which outputs padding would change, so the
  plan, not the backend, decides what the valid length masks.
- An `If` is a trait method that takes both branches. `WebGpuBackend` and `Cpu`
  read the condition back and run one. The PJRT backend traces both as the
  regions of one `stablehlo.case`.

### What every device backend gets

Each of these serves both device backends. The trait changes once for them,
in the step or roadmap item named:

- A batch of rows of different lengths, each with its own position and valid
  length: the roadmap item "Many requests at once".
- The next token picked on the device, so a pass reads back token ids alone.
  `arg_max` is already a trait method, and `WebGpuBackend` reads back one
  index a pass; the PJRT backend lowers it with the rest.
- Weights held at half width on the device: f16 on WebGPU in the roadmap item
  "A half graph keeps its type", bf16 on PJRT in step 5 below. `Self::Rhs`
  already lets a backend choose a weight's storage.
- The attention cache donated from a pass's input to its output, so it is
  updated in place: step 5 below.

## Plan

The steps of "The PJRT backend", in order:

1. [ ] `wado-run-pjrt`, serving `wado:pjrt`. Finished when a Wado program
   compiles a StableHLO module and runs it on the CPU plugin under
   `wado run-pjrt`, in CI, and CI compiles the same module for a TPU topology
   through `libtpu.so`.
2. [ ] The PJRT backend, tracing and replaying. Finished when the tiny GPT-2
   runs on the CPU plugin within the tolerance of `Cpu`, one compile per
   extent set, and the cost of a replayed pass beside the executed program is
   recorded.
3. [ ] Capacities and valid lengths in the seam. Finished when Gemma 3 (270M)
   decodes on the CPU plugin with one compile per bucket and picks
   onnxruntime's tokens, and `WebGpuBackend` passes its conformance through the
   changed seam.
4. [ ] `If` as a trait method over both branches. Finished when the conformance
   cases with an `If` pass on all three backends.
5. [ ] Weights in bf16 and the cache donated. Finished when the compiled
   program's device memory, as `PJRT_Executable_GetCompiledMemoryStats`
   reports it, holds Gemma 3's weights at half their f32 size and no second
   copy of the cache.

A TPU run and many requests at once are later items of the roadmap.

## Known gaps

- Text StableHLO carries no compatibility promise; only a portable artifact
  does, and writing one takes libStablehlo. Both plugins the research ran
  accepted the same text.
- `libtpu.so` aborts the process on an unknown extent rather than returning an
  error, so a host cannot recover from a program that carries one.
- The CPU plugin CI would run is ZML's build of XLA's
  `pjrt_c_api_cpu_plugin`. No CPU plugin is published by the XLA project.
- No PJRT run on a GPU or a TPU has been measured.
