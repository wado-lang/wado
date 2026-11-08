# Loam Development Tools

## `loam gen` and `loam dump`

`loam gen` runs what the Kiln generator runs, without Kiln, and prints the
module. It is the entry point for profiling the generator. `loam dump` prints
the checked graph: how `build` lowers each node (operations of the graph,
folded by stage 0, an alias of its operand, a branch, a weight's transpose
viewed by the product reading it, or a part of an attention matched as the
composite), where each axis took its name from, each
tensor's element type and axes, the values stage 0 folded, what each scope
leaves `compute` to check, which lengths the run may pad to a capacity or the
node that refuses one, and whether each row of a batch holds those lengths of
its own or the node that makes the rows share them. The device compiler's
fusion is decided when it runs, so no dump reports it. `--format json` prints the same for a test to
read.

```sh
wado run package-loam gen --type onnx --options options.json --checkpoint model.safetensors model.onnx
wado run package-loam dump --type onnx --options options.json model.onnx
```

`--type` takes the words a use site's `type` does, and `--options` names a
JSON file holding what its `generator.options` holds.

## Testing

Test outward from the change:

1. The test beside the file changed, such as `src/runtime/recorded_test.wado`.
2. `cargo test --test compute loam` in `wado-run-webgpu/`, for a change
   `WebGpuBackend` runs. It checks each result against `Cpu`'s.
3. `mise run test-pjrt`, for a change `PjrtBackend` or the StableHLO writer
   runs. It does the same on XLA's CPU plugin, and compiles for a TPU.
4. `wado test package-loam`.

## Oracles

Two suites take their expectations from onnxruntime.
`conformance/operators_test.wado` covers every operator Loam lowers, over
static shapes, with the attributes and edge values where a reading can go
wrong. `semantics_test.wado` covers shape arithmetic over a symbolic length, at
two lengths. A script beside each, `operators.mjs` or `semantics.mjs`, writes
one small model per case and what onnxruntime computes from it. The test builds
each model as the generator would and runs the result under `core:eval`, so a
refused build, a trap and a wrong answer each show as a difference from
onnxruntime.

Each example's `generate.mjs` writes its model's `generate.json` through
`tools/greedy.mjs`: the tokens onnxruntime picks greedily, and its time a token
on the CPU and on its WebGPU provider, which must pick the same tokens.

`package-lock.json` pins the Node runtimes the oracle scripts (`*.mjs`) run, and
`mise run loam-oracle-deps` installs them.

`mise run loam-ort-inspect <model.onnx> [--dim name=extent]...` reports what
onnxruntime makes of a model: the graph transformers that changed it, where each
node runs, the optimized graph with its inferred shapes, and one profiled run.

## Benchmarks

[`benchmark/`](./benchmark/README.md) times Loam's backends against
onnxruntime, in the format of the repository's `benchmark/`. Its README says
how to fetch the models and run it.
