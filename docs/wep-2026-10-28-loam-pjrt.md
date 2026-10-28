# WEP: Loam on PJRT — TPUs and Native GPUs Behind the Backend Trait

Status: proposed.

## Context

[WEP: Loam](./wep-2026-09-20-loam.md) runs a model's `forward` on any
implementation of one `Backend` trait. Two exist: `Cpu`, and `WebGpuBackend`
over `wasi:webgpu` ([Loam on WebGPU](../package-loam/webgpu.md)). Neither reaches
a TPU, and neither reaches a GPU's matrix units: WebGPU's subgroup matrix is
still in standardization
([research §10](./research-ml-inference-webgpu.md#10-webgpu-feature-status)).

[Research: Google TPU Through PJRT](./research-tpu.md) records how a program
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

### Not decided here

The name and home of the Wado binding to PJRT, and of the host that serves it,
are open. `wasi:` is reserved for WASI, and PJRT is no WASI proposal.

## Decision

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

The first `forward` over a set of extents runs against tensors that hold no
values. Each trait method appends StableHLO operations. An `upload` becomes a
parameter, a prepared weight a parameter that stays on the device, and a
`download` an output. The backend then compiles the program.

Every later `forward` over the same extents replays. Each method returns a
handle and appends nothing, and the first `download` executes the compiled
program once. Replay hashes the sequence of calls it sees and traces again if
the sequence differs from the trace.

`fused`, `rows` and the products with prologues and epilogues lower to the
StableHLO operations they mean. XLA fuses them again for its target, so the
plan's fusion, chosen for WebGPU, costs XLA nothing.

A `pass` is one program. `begin` and `retain` already mark where one pass ends
and what outlives it.

### The seam carries what a compiled program cannot learn at run time

XLA refuses a dimension of unknown extent, and `libtpu.so` aborts the process
on one (research §6). So every extent that varies between calls reaches the
trait as a capacity fixed per program, with what is valid within it as a value:

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

Each of these serves both device backends, and the trait changes once for
them:

- A batch of rows of different lengths, each with its own position and valid
  length.
- The next token picked on the device, so a pass reads back token ids alone.
- Weights held at half width on the device: f16 on WebGPU, bf16 on PJRT.
  `Self::Rhs` already lets a backend choose a weight's storage.
- The attention cache donated from a pass's input to its output, so it is
  updated in place.

### `pjrt_c_api.h` may be read

`package-loam/AGENTS.md` bars reading the code of ML frameworks and runtimes.
`xla/pjrt/c/pjrt_c_api.h` is an exception: it is the plugin's interface, read as
an API specification. The rest of XLA stays barred.

## Roadmap

1. [ ] A host that serves PJRT to a Wado program. Finished when a Wado program
   compiles a StableHLO module and runs it on the CPU plugin under that host,
   in CI, and CI compiles the same module for a TPU topology through
   `libtpu.so`.
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
6. [ ] Gemma 3 on a TPU. Finished when it picks onnxruntime's tokens on a TPU
   VM, and its time a token is recorded beside the compiler's
   `optimal_seconds` for the same program.
7. [ ] Many requests at once. Finished when the tokens a second at several
   numbers of concurrent requests are recorded on a TPU and on `WebGpuBackend`,
   each request's tokens unchanged from its run alone.

## Known gaps

- Text StableHLO carries no compatibility promise; only a portable artifact
  does, and writing one takes libStablehlo. Both plugins the research ran
  accepted the same text.
- `libtpu.so` aborts the process on an unknown extent rather than returning an
  error, so a host cannot recover from a program that carries one.
- The CPU plugin CI would run is ZML's build of XLA's
  `pjrt_c_api_cpu_plugin`. No CPU plugin is published by the XLA project.
- No PJRT run on a GPU or a TPU has been measured.
