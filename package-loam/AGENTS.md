# Loam Development Guide

Loam compiles an ONNX graph into Wado source through Kiln. The design lives in
[WEP: Loam](../docs/wep-2026-09-20-loam.md), and the tokenizer's in
[tokenizer.md](./tokenizer.md).

## Sources

- Do not read the code of open-source projects: no ML framework, runtime,
  exporter or kernel library. Their licenses do not reach this repository, and
  code written after reading them is a derivative.
- Published papers may be read, and so may the ONNX specification: the
  operator definitions and `onnx.proto`.
- An existing implementation may be run as an oracle, so long as its code is
  not read. onnxruntime is run this way for expected outputs.
- Test data and models may be copied in and read, each directory carrying its
  source and license, as `tests/onnx/` and `tests/models/` do.

## Oracles

`package.json` pins the Node runtimes the oracle scripts (`*.mjs`) run, and
`mise run loam-oracle-deps` installs them. `mise run loam-ort-inspect <model.onnx> [--dim name=extent]...` reports what onnxruntime makes of a model:
the graph transformers that changed it, where each node runs, the optimized
graph with its inferred shapes, and one profiled run. It is the reference a
Loam fusion decision is measured against.
