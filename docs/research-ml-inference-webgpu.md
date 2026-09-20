# ML Inference and WebGPU — Research Reference

Date: 2026-09-20

A survey of how ML inference frameworks are built, and of what the 2026 literature
says about running inference on WebGPU. This document records findings only. It
draws no conclusions about Wado; a WEP will do that separately.

> **Source**: the papers and specifications linked inline. Searched 2026-09-20.
> Venues cited are ICLR 2026, ECOOP 2026, and arXiv preprints from 2026.

---

## Table of Contents

1. [Summary](#1-summary)
2. [A Taxonomy of Inference Frameworks](#2-a-taxonomy-of-inference-frameworks)
3. [llama.cpp and ggml](#3-llamacpp-and-ggml)
4. [WebGPU Inference Is Now Competitive](#4-webgpu-inference-is-now-competitive)
5. [At Batch Size 1, Dispatch Count Is the Bottleneck](#5-at-batch-size-1-dispatch-count-is-the-bottleneck)
6. [Megakernels](#6-megakernels)
7. [Shape Checking: Staging Instead of Dependent Types](#7-shape-checking-staging-instead-of-dependent-types)
8. [Axis Notation Converges on einx](#8-axis-notation-converges-on-einx)
9. [Quantization Formats](#9-quantization-formats)
10. [WebGPU Feature Status](#10-webgpu-feature-status)
11. [Backend Abstraction](#11-backend-abstraction)
12. [wasi-nn](#12-wasi-nn)
13. [Open Problems](#13-open-problems)
14. [Sources](#14-sources)

---

## 1. Summary

Six findings, in order of how much they move the design space.

- **The bottleneck on WebGPU is dispatch count, not kernel quality.** At batch
  size 1 the per-dispatch cost dominates, and it does not depend on the data
  type. Fusion pays for itself by removing dispatches, not by saving bandwidth.
- **On CUDA the endpoint of that logic is already shipping.** Compiling a whole
  model into one persistent kernel buys 1.2x to 6.7x on latency, and is deployed
  commercially under a 1-5 ms budget.
- **WebGPU cannot host a true megakernel.** It has no device-wide barrier, so
  cross-workgroup synchronization inside one dispatch is not expressible. The
  reachable target is fewer, larger dispatches, not one.
- **WebGPU inference is no longer slow enough to dismiss.** A 2026 WebGPU backend
  for llama.cpp matches or beats vendor-specific native backends on some devices.
- **Compile-time shape checking has a practical formulation.** Staging replaces
  the proof obligations that made dependent and refinement types unwieldy.
- **Quantization has converged on microscaling block formats** (MXFP4, NVFP4),
  with an unresolved accuracy gap and a steady stream of correction techniques.

---

## 2. A Taxonomy of Inference Frameworks

The field is often discussed as one category. It is at least six, and they differ
in _when_ the computation graph is fixed and _who_ writes the kernels.

| Kind                                 | Graph fixed at | Kernels         | Examples                              |
| ------------------------------------ | -------------- | --------------- | ------------------------------------- |
| Interchange format                   | export time    | none (it is IR) | ONNX, GGUF                            |
| Runtime graph + hand-written kernels | run time       | humans          | ggml / llama.cpp                      |
| Load-time graph optimizer            | load time      | humans, per EP  | ONNX Runtime                          |
| Tracing JIT with fused codegen       | first trace    | compiler        | JAX/XLA, torch.compile, tinygrad, MLX |
| AOT compiler to a portable artifact  | build time     | compiler        | IREE, TVM/Relax, WebLLM               |
| Kernel DSL in a typed host language  | build/JIT time | you, abstractly | Triton, CubeCL, Mojo, Halide, Exo     |
| Typed array language                 | build time     | compiler        | Dex, Futhark                          |

Two observations about the table.

**ONNX is not a framework.** It is a serialized graph IR plus an operator set.
Its shapes are dynamic (symbolic `dim_param`), its axes positional, and its type
information is dtype only. Optimization belongs to the runtime that loads it, not
to the format.

**Popularity is anticorrelated with compiler sophistication.** The two kinds with
the weakest compiler stories (runtime graph, load-time optimizer) have the largest
deployments. Section 3 covers why.

---

## 3. llama.cpp and ggml

llama.cpp is a **runtime graph interpreter driving hand-written kernels** — the
opposite end of the space from the compiler approach.

The tensor type in ggml carries `int64_t ne[4]` for dimensions, `size_t nb[4]`
for strides, and a `ggml_type` enum for the element format. Shape and dtype are
both runtime values, and the rank ceiling is 4. As an object model this is close
to NumPy's `ndarray`.

Execution matches. A `ggml_cgraph` is built for the forward pass, a scheduler
assigns each node to a backend, and nodes execute one at a time. Kernels are
written by hand for each backend — CUDA, Metal, Vulkan, SYCL, HIP, and CPU SIMD.
There is no code generation and no autotuning; a "fused op" means a person wrote
another kernel. Model architectures are hardcoded as a switch over an
architecture enum in C++, so supporting a new one is a patch to the core.

Its actual technical center is elsewhere: **quantization formats** (k-quants,
i-quants, importance-matrix-driven quantization) and **deployment**, where GGUF
gives a single mmap-able file, no dependencies, and one binary.

### Why the weaker compiler story won

Batch-1 decode is memory-bandwidth bound and has low arithmetic intensity. In
that regime hand-written kernels land near the achievable ceiling, and the thing
compilers optimize hardest — fusion for FLOPs and bandwidth — has little left to
take. TVM and IREE are better engineered as compilers and were adopted far less.

Section 5 is the 2026 result that complicates this story: on WebGPU the batch-1
ceiling is set by dispatch overhead instead, and dispatch count _is_ something
only a compiler can collapse.

> llama.cpp moves quickly; the specifics above describe its long-standing
> architecture rather than any one revision.

---

## 4. WebGPU Inference Is Now Competitive

[Llamas on the Web](https://arxiv.org/abs/2605.20706) (LlamaWeb, Microsoft
Research, 2026-05) builds a WebGPU backend for llama.cpp and evaluates it across
16 devices from 8 vendors, 10 models, and 4 weight formats.

| Metric                   | Result                                                      |
| ------------------------ | ----------------------------------------------------------- |
| Memory                   | 29-33% less than existing browser frameworks                |
| Decode throughput        | 45-69% better, across four vendor GPUs                      |
| Against native llama.cpp | Matches or exceeds vendor-specific backends on some devices |

The techniques are static memory planning, efficient model loading, a tunable
kernel library for cross-device variability, and **templated GPU kernels that
support many quantization formats from one source**.

The last one is notable: it is the "quantization format as a kernel parameter"
idea, implemented as hand-written templates.

The takeaway is that portability no longer costs an order of magnitude. The
earlier assumption that WebGPU is for reach and native is for speed is too
pessimistic as of 2026.

---

## 5. At Batch Size 1, Dispatch Count Is the Bottleneck

[Measuring and Reducing WebGPU Dispatch Overhead for LLM Inference](https://arxiv.org/abs/2608.08730)
(2026-08) introduces a sequential-dispatch measurement method, having first shown
that naive measurements conflate dispatch cost with synchronization cost.

Its finding, stated directly:

> At batch size 1, dispatch overhead, not kernel quality, is the bottleneck.

Per-dispatch cost is independent of the data type. The recommended remedy is
**dispatch amortization, both in inference engines and in the WebGPU
specification itself** — meaning part of the ceiling is not under an engine
author's control.

This reframes what fusion is for. The objective function for a WebGPU inference
compiler is minimizing the number of dispatches, not minimizing memory traffic.
The two agree often but not always, and where they disagree the dispatch count
wins at batch 1.

---

## 6. Megakernels

On CUDA the same reasoning has already been taken to its conclusion. **Mirage
Persistent Kernel (MPK)** compiles an entire LLM forward pass into a single fused
kernel that performs all computation and communication in one launch, reporting
**1.2x to 6.7x lower inference latency**. It eliminates launch overhead and
overlaps computation, data loading, and inter-GPU communication across layers;
operator chaining happens through shared memory and registers rather than
round-tripping through HBM.

[Ada-MK](https://arxiv.org/html/2605.11581v1) (2026) searches the operator DAG to
build megakernels adaptively, embeds the result as a TensorRT-LLM plugin to serve
both prefill (throughput-shaped) and decode (latency-shaped), and reports the
first large-scale industrial deployment, under a 1-5 ms deterministic end-to-end
budget.

The structural point: a framework whose unit of fusion is "a kernel a human
wrote" cannot reach this, no matter how good the kernels are. Whole-graph fusion
is a compile-time property of the architecture.

---

### 6.1 WebGPU Cannot Host a True Megakernel

WebGPU provides synchronization within a workgroup and none across the device.
Without a grid-wide barrier, a dependency between two workgroups cannot be
expressed inside one dispatch, and a persistent kernel spanning the whole model
is therefore not implementable.

The reachable target on WebGPU is **fewer and larger dispatches** — collapsing a
layer, or a fused block, into one — rather than one dispatch per token. The CUDA
speedups above should not be assumed to transfer.

---

## 7. Shape Checking: Staging Instead of Dependent Types

[Compile-Time Tensor Shape Checking via Staged Shape-Dependent Types](https://arxiv.org/abs/2604.23807)
(Suwa and Igarashi, **ECOOP 2026**) is the most transferable result found.

It starts from why existing approaches have not been adopted. Dependent types
(`Mat m n`) and refinement types both require _proofs_ to decide type equality or
subtyping, which is unwieldy under continuous development; automated proving does
not fix it, because its running time is unpredictable.

The alternative rests on an observation: **the conditions for shape consistency
can usually be extracted before the tensor computation runs**. So the formalism
uses staging, and discharges shape consistency through **assertions evaluated as
compile-time computations rather than proofs**.

The authors built a prototype type-checker and interpreter, ported a set of
DNN-related programs from ocaml-torch, and report the method as effective enough
for realistic tensor-handling code.

This is the first formulation found that makes compile-time shape checking look
implementable in a general-purpose language without a proof engine.

---

## 8. Axis Notation Converges on einx

[It's All Just Vectorization: einx](https://openreview.net/pdf?id=QqvQ3iAdpC)
(**ICLR 2026**) compresses the tensor API surface into a small set of base
operations under one declarative notation. Its innovation is bracket notation,
where `[]` marks the axes the core operation actually sees:

```python
einx.dot("a [b], [b] c -> a c", x, y)
```

`[b]` says the dot product runs over `b`; `a` and `c` are merely vectorized over.
Unlike `einsum`, axis names are self-documenting and the style is declarative
rather than imperative.

The practical state of the art is therefore a **string DSL**, with static
checking bolted on from outside: `pyrefly-einops-stubs` provides shape-aware
annotations for `rearrange`, `reduce`, `repeat`, and `einsum` through a type-level
shape DSL. Named axes as a first-class language feature have not stuck — PyTorch
named tensors shipped and went quiet; `xarray` thrives in a different niche;
[Named Tensor Notation](https://arxiv.org/pdf/2102.13196) remains a notation for
papers rather than a type system.

---

## 9. Quantization Formats

The hardware-supported microscaling 4-bit floating point formats (MXFP4, NVFP4)
are the substrate everyone is now targeting. MXFP4 partitions a tensor into
blocks of 32 elements sharing an E8M0 scale factor.

[Bridging the Gap Between Promise and Performance for Microscaling FP4
Quantization](https://arxiv.org/abs/2509.23202) (**ICLR 2026**) is the first
comprehensive post-training-quantization study of both, and finds a gap between
the formats' promise and their measured behavior. It proposes **MR-GPTQ**
(Micro-Rotated GPTQ), which adapts GPTQ to FP4 with block-wise Hadamard
transforms and format-specific optimizations.

Follow-on work addresses MXFP4 specifically, which trails NVFP4 in accuracy:

- [Overflow-Aware Scaling and Macro Block Scaling](https://arxiv.org/abs/2603.08713) —
  software-only fidelity improvements, no hardware change required.
- [DuQuant++](https://arxiv.org/html/2604.17789v1) — fine-grained rotation.

The pattern worth recording is that the formats are a moving target parameterized
by _block size_, _scale representation_, and _whether a rotation is applied_ —
not a fixed list of names.

---

## 10. WebGPU Feature Status

| Feature                              | Status (2026-09)                                                                                                                      |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------- |
| Subgroups                            | Shipped (Chrome 134, 2025-02) after a year of trials                                                                                  |
| Subgroup matrix (cooperative matrix) | In standardization, [gpuweb#4195](https://github.com/gpuweb/gpuweb/issues/4195); Khronos 2026 material lists it as on the back burner |
| Device-wide barrier                  | Absent, and not proposed                                                                                                              |
| Dispatch amortization                | Identified as needed at the specification level (§5)                                                                                  |

Open questions in the subgroup matrix issue include which address spaces to
support (a uniform subset of `storage` and `workgroup`, or all) and whether
`row_major`/`column_major` must be selectable at runtime. Equivalent
functionality already exists in Metal, HLSL, and Vulkan/SPIR-V, so this is the
main structural gap against native on the prefill matmuls.

---

## 11. Backend Abstraction

The mainstream answer is **traits plus JIT**.

[CubeCL](https://github.com/tracel-ai/cubecl) leans on Rust's type system rather
than shipping a separate frontend, and compiles kernels **just in time**: only
the variants actually launched are generated. Its stated reason is that ahead-of-
time compilation must cover every combination of shape, hardware target, and
instruction set, which for hand-written CUDA projects reaches gigabytes. CubeCL
is also a runtime — per-platform layers handle compilation, dispatch, autotune
caching, and memory, independently of the `#[cube]` frontend. In September 2026
NVIDIA announced it is investing in native GPU programming in Rust, naming CubeCL
alongside rust-gpu and rust-cuda.

[HetGPU](https://arxiv.org/html/2506.15993v1) takes the other route — a compiler,
runtime, and abstraction layer letting a single GPU binary run on NVIDIA, AMD,
Intel, and Tenstorrent.

**No example was found of using algebraic effects or effect handlers to abstract
the compute device.** Effect-handler research in 2025-2026 is active (tracing JIT
compilation for effects and handlers, control-flow analysis for handlers, staging
effect handlers for modular search) and
[Continuing WebAssembly with Effect Handlers](https://arxiv.org/pdf/2308.08347)
covers the Wasm substrate, but the intersection with device abstraction appears
unexplored.

The CubeCL result is a useful caution regardless of the abstraction chosen: fully
ahead-of-time specialization across shapes and targets does not scale, so
generating only the variants actually used is a requirement rather than an
optimization. This agrees with the staging approach in §7.

---

## 12. wasi-nn

[wasi-nn](https://github.com/WebAssembly/wasi-nn) is the WASI proposal for
performing ML inference from inside a Wasm sandbox through a vendor-neutral
interface. The guest selects a backend, loads a model as a graph, passes tensor
inputs, computes, and retrieves outputs. It is listed among the key WASI
developments for 2026-2027.

It sits at a different layer from anything else in this document: it delegates the
entire model to the host rather than describing how kernels are produced. It is an
alternative delivery path, not a competitor to a compiler stack.

---

## 13. Open Problems

Recorded as constraints on anything built in this space.

1. **WebGPU's dispatch ceiling is partly a specification problem.** §5's own
   recommendation is amortization in the spec. Engine-side work has a limit that
   no implementation can pass.
2. **No device-wide barrier** means the megakernel result (§6) does not transfer
   to WebGPU as stated.
3. **Subgroup matrix has not landed** (§10), leaving prefill matmuls the clearest
   remaining gap against native.
4. **MXFP4 accuracy trails NVFP4** (§9), and the correction techniques are still
   appearing at a steady rate. Committing to a fixed format list will age badly.
5. **Full AOT specialization does not scale** (§11), so any compile-time approach
   needs a variant-selection story.
6. **Compile-time shape checking is prototype-stage** (§7). The ECOOP 2026 work
   validated it on ported ocaml-torch examples, not on a production toolchain.

---

## 14. Sources

### Papers

| Topic                            | Reference                                                                                                                                                                            |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| WebGPU LLM inference backend     | [Llamas on the Web (arXiv 2605.20706)](https://arxiv.org/abs/2605.20706)                                                                                                             |
| WebGPU dispatch overhead         | [Measuring and Reducing WebGPU Dispatch Overhead (arXiv 2608.08730)](https://arxiv.org/abs/2608.08730)                                                                               |
| In-browser inference engine      | [WebLLM (arXiv 2412.15803)](https://arxiv.org/html/2412.15803v2)                                                                                                                     |
| Compile-time shape checking      | [Staged Shape-Dependent Types, ECOOP 2026 (arXiv 2604.23807)](https://arxiv.org/abs/2604.23807) · [LIPIcs](https://drops.dagstuhl.de/entities/document/10.4230/LIPIcs.ECOOP.2026.28) |
| Tensor notation                  | [einx, ICLR 2026](https://openreview.net/pdf?id=QqvQ3iAdpC) · [Named Tensor Notation (arXiv 2102.13196)](https://arxiv.org/pdf/2102.13196)                                           |
| FP4 microscaling quantization    | [Bridging the Gap, ICLR 2026 (arXiv 2509.23202)](https://arxiv.org/abs/2509.23202)                                                                                                   |
| MXFP4 error reduction            | [OAS and MBS (arXiv 2603.08713)](https://arxiv.org/abs/2603.08713)                                                                                                                   |
| FP4 rotation                     | [DuQuant++ (arXiv 2604.17789)](https://arxiv.org/html/2604.17789v1)                                                                                                                  |
| Megakernel search and deployment | [Ada-MK (arXiv 2605.11581)](https://arxiv.org/html/2605.11581v1)                                                                                                                     |
| GPU binary portability           | [HetGPU (arXiv 2506.15993)](https://arxiv.org/html/2506.15993v1)                                                                                                                     |
| Effect handlers on Wasm          | [Continuing WebAssembly with Effect Handlers (arXiv 2308.08347)](https://arxiv.org/pdf/2308.08347)                                                                                   |

### Implementations and specifications

| Topic                  | Reference                                                                                                                                  |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| Megakernel compiler    | [Mirage Persistent Kernel](https://github.com/mirage-project/mirage)                                                                       |
| Rust GPU kernel DSL    | [CubeCL](https://github.com/tracel-ai/cubecl)                                                                                              |
| WebGPU subgroup matrix | [gpuweb#4195](https://github.com/gpuweb/gpuweb/issues/4195)                                                                                |
| WebGPU roadmap         | [Khronos WebGL+WebGPU, SIGGRAPH 2026](https://www.khronos.org/assets/uploads/developers/presentations/WebGL%2BWebGPU_-_SIGGRAPH_Jul26.pdf) |
| Wasm ML interface      | [WebAssembly/wasi-nn](https://github.com/WebAssembly/wasi-nn)                                                                              |
