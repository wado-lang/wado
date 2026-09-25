# `.proto` corpus

Schemas Grog's tests parse and compile, keeping each upstream's directory layout
under a directory named for its submodule, so `protobuf/src/…` here names
`vendor/protobuf/src/…` there.

- `protobuf/` — [protocolbuffers/protobuf](https://github.com/protocolbuffers/protobuf)
  at release v36.2 (commit `2c74169b34066ceb8ddb6b882fcb3fb32d737a55`), BSD-3-Clause.
- `onnx/` — [onnx/onnx](https://github.com/onnx/onnx) at commit
  `718bf2eaa65ad7df49f1fb2dfc91e15f51a1ef92`, Apache-2.0.

They are committed rather than read out of the submodules, which CI does not
check out. `mise run sync-proto-corpus` fetches them: it reads the tests for
which files to take, reports a file no test names any more, and says when a
submodule has moved off the commit above.
