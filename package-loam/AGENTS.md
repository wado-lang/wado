# Loam Development Guide

Loam compiles an ONNX graph into Wado source through Kiln. Its design is in
[design.md](./design.md), which names the documents it builds on. The roadmap
and the known gaps are in [TODO.md](./TODO.md). Further work on Loam changes
this directory alone.

## Sources

- Do not read the code of open-source projects: no ML framework, runtime,
  exporter or kernel library. Their licenses do not reach this repository, and
  code written after reading them is a derivative.
- Published papers may be read, and so may the ONNX specification: the
  operator definitions and `onnx.proto`.
- onnxruntime's operator documentation may be read, for the operators it
  defines beyond ONNX, such as `com.microsoft::GroupQueryAttention`
  (`docs/ContribOperators.md`). Its code may not. Where the documentation
  leaves a behavior open, onnxruntime is run as an oracle for it.
- An existing implementation may be run as an oracle, so long as its code is
  not read. onnxruntime is run this way for expected outputs.
- Test data and models may be copied in and read, each directory carrying its
  source and license, as `tests/onnx/` and `tests/models/` do.

## Debugging tools

`loam gen` runs what the Kiln generator runs, without Kiln, and prints the
module. It is the entry point for profiling the generator. `loam dump` prints
the checked graph: how `forward` lowers each node (a kernel call, a step fused
into a later kernel, folded by stage 0, an alias of its operand, or a branch),
why each kernel writes its output, each tensor's element type and axes, the
values stage 0 folded, and what each scope leaves `forward` to check.
`--format json` prints the same for a test to read. `"fuse": false` in the
options turns fusion off, for a comparison.

```sh
wado run package-loam gen --type onnx --options options.json --checkpoint model.safetensors model.onnx
wado run package-loam dump --type onnx --options options.json model.onnx
```

`--type` takes the words a use site's `type` does, and `--options` names a
JSON file holding what its `generator.options` holds.

## Oracles

Two suites take their expectations from onnxruntime.
`conformance/operators_test.wado` covers every operator Loam lowers, over
static shapes, with the attributes and edge values where a reading can go
wrong. `semantics_test.wado` covers shape arithmetic over a symbolic length, at
two lengths. A script beside each, `operators.mjs` or `semantics.mjs`, writes
one small model per case and what onnxruntime computes from it. The test builds
each model as the generator would and runs the result under `core:eval`, so a
refused build, a trap and a wrong answer each show as a difference from
onnxruntime. A case Loam does not match yet is a `#[TODO]` test, so the run
lists where the two part.

`package.json` pins the Node runtimes the oracle scripts (`*.mjs`) run, and
`mise run loam-oracle-deps` installs them. `mise run loam-ort-inspect <model.onnx> [--dim name=extent]...` reports what onnxruntime makes of a model:
the graph transformers that changed it, where each node runs, the optimized
graph with its inferred shapes, and one profiled run. It is the reference a
Loam fusion decision is measured against, as `loam dump` reports one.
