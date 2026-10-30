# Loam Development Guide

Loam compiles an ONNX graph into Wado source through Kiln. Its design is in
[design.md](./design.md), which names the documents it builds on. The roadmap
and the known gaps are in [TODO.md](./TODO.md). `loam gen`, `loam dump`, the
onnxruntime oracles and the benchmarks are in [tools.md](./tools.md).

[WEP: Loam](../docs/wep-2026-09-20-loam.md) is complete, and nothing edits it
any more. A design that changes one of its decisions is written in this
directory and names the WEP's section.

## Sources

- Do not read the code of an ML framework, runtime, exporter or kernel library.
- Published papers and specifications may be read: ONNX's operator
  definitions, the StableHLO specification.
- ONNX's own implementation may not be read: the source under
  `vendor/onnx/onnx/`, its reference implementation among it. Its headers,
  `.proto` files, docs and tests may be read, and the tests are
  `vendor/onnx/tests/`, `vendor/onnx/onnx/backend/test/` and the fuzzers in
  `vendor/onnx/onnx/fuzz/`. `permissions.deny`
  enforces this for the Read tool, and `.claude/hooks/vendor-license-guard.sh`
  for Bash.
- An interface may be read, as a specification: any `.proto` file, and
  `xla/pjrt/c/pjrt_c_api.h`, the PJRT plugin's. The rest of XLA may not.
- onnxruntime's operator documentation may be read, for the operators it
  defines beyond ONNX, such as `com.microsoft::GroupQueryAttention`
  (`docs/ContribOperators.md`). Its code may not. Where the documentation
  leaves a behavior open, onnxruntime is run as an oracle for it.
- Test data and models may be copied in and read, each directory carrying its
  source and license, as `tests/onnx/` and `tests/models/` do.

## Oracles

`conformance/operators_test.wado` and `semantics_test.wado` take their
expectations from onnxruntime. A case Loam does not match yet is a `#[TODO]`
test, so the run lists where the two part.
