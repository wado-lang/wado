# Loam Development Guide

Loam compiles an ONNX graph into Wado source through Kiln. The
`wado-package-standard` skill says where each kind of file goes. Its design is
in [docs/design.md](./docs/design.md), which names the documents it builds on.
The roadmap and the known gaps are in [TODO.md](./TODO.md). `loam gen`,
`loam dump`, the onnxruntime oracles and the benchmarks are in
[docs/tools.md](./docs/tools.md).

The documents here sketch a design and describe what exists; they decide
nothing. Correctness and speed come first.

## Architecture

The Kiln generator reads an ONNX graph, checks its shapes and names its axes,
folds what it can at build time, and writes a Wado module whose `compute` calls
a backend. The backends are in `src/runtime/`. `tests/conformance/` is a package of
its own that consumes Loam the way a user does, to run the onnxruntime
oracles.

## Sources

- Do not read the code of an ML framework, runtime, exporter or kernel library.
- Published papers and specifications may be read: ONNX's operator
  definitions, the StableHLO specification.
- ONNX's own source may not be read: the code under `vendor/onnx/onnx/`, its
  reference implementation among it, `vendor/onnx/tools/` and
  `vendor/onnx/docs/docsgen/`. Its headers, `.proto` files, docs and tests may
  be read, and the tests are `vendor/onnx/tests/`,
  `vendor/onnx/onnx/backend/test/` and the fuzzers in `vendor/onnx/onnx/fuzz/`.
  `permissions.deny` enforces this for the Read tool, and
  `.claude/hooks/vendor-license-guard.mts` for Bash, which also refuses a
  search of a directory holding that source: name the doc to search.
- An interface may be read, as a specification: any `.proto` file, and
  `xla/pjrt/c/pjrt_c_api.h`, the PJRT plugin's. The rest of XLA may not.
- onnxruntime's operator documentation may be read, for the operators it
  defines beyond ONNX, such as `com.microsoft::GroupQueryAttention`
  (`docs/ContribOperators.md`). Its code may not. Where the documentation
  leaves a behavior open, onnxruntime is run as an oracle for it.
- Test data and models may be copied in and read, each directory carrying its
  source and license, as `tests/onnx/` and `tests/models/` do.

## Backends

Each semantics is written once per backend: `Cpu`, `WebGpuBackend`, and
`PjrtBackend`. `Cpu` is the reference the others are held to, not a backend
anyone runs a model on, so a simpler design across the three outranks its
speed.

## Testing a change

Generated code reaches every backend, so a change can break code it never
touched. Test outward from the change, as
[docs/tools.md](./docs/tools.md#testing) lists, and run the whole package
once, last.

CI tests the logic on every device with very small programs, which should
reach as many operations and paths as they can. A real model such as Gemma 3
runs by hand, and its figures go in a measurement table.

## Measuring speed

A change that can move a model's speed is measured with
[`benchmark/`](./benchmark/README.md): `mise run <model>` there times Loam's
backends against onnxruntime and llama.cpp, and the result replaces the row in
its README's table.

## Oracles

The procedures are in [docs/tools.md](./docs/tools.md#oracles).

A case in `tests/conformance/operators_test.wado` or `semantics_test.wado` where Loam
does not match onnxruntime yet is a `#[TODO]` test, so the run lists where the
two part.

Loam does not reproduce a bug of onnxruntime's. It may part from onnxruntime
for a plain and sound reason, such as computing closer to the exact value. The
case then states that reason beside the bound it holds Loam to.
