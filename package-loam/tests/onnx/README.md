# ONNX backend test data

Models and tensors from [onnx/onnx](https://github.com/onnx/onnx), licensed
under the Apache License 2.0. Copied from
`onnx/backend/test/data/` at commit `718bf2eaa65ad7df49f1fb2dfc91e15f51a1ef92`,
keeping the upstream directory layout, so a path here names the path there.

Each `model.onnx` is a graph Loam compiles, and the `test_data_set_0/` beside it
holds the inputs and the output ONNX's own runtime produces. That output is the
oracle: `package-loam/conformance/` runs the generated kernels against the same
inputs and compares. `light/` holds one larger model, read by the protobuf
reader's own tests rather than compiled.

These are committed rather than read out of `vendor/onnx`, which is a submodule
and absent wherever one is not initialized. A file an import names has to be
there for the module to compile, so a test that reads the submodule is a build
failure rather than a skipped test.

To take a new model, copy it and its `test_data_set_0/` under the same upstream
path and update this file's commit if it moved. `scripts/sync-vendor.sh` updates
the submodule; it does not touch what is copied here.
