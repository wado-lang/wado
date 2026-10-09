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

## Debugging tools

`loam gen` runs what the Kiln generator runs, without Kiln, and prints the
module. It is the entry point for profiling the generator. `loam dump` prints
the checked graph: how `forward` lowers each node (a kernel call, folded by
stage 0, an alias of its operand, or a branch), each tensor's element type and
axes, the values stage 0 folded, and what each scope leaves `forward` to check.

```sh
wado run package-loam gen --type onnx --options options.json --checkpoint model.safetensors model.onnx
wado run package-loam dump --type onnx --options options.json model.onnx
```

`--type` takes the words a use site's `type` does, and `--options` names a
JSON file holding what its `generator.options` holds.
