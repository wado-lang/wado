# Loam Development Guide

Loam compiles an ONNX graph into Wado source through Kiln. The design lives in
[WEP: Loam](../docs/wep-2026-09-20-loam.md).

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
