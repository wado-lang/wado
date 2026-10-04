# wado-run-webgpu

The `wado run-webgpu` subcommand: it compiles a Wado program with `wado` and
runs the component on a wasmtime host that serves `wasi:webgpu`.

## Rules

- This crate is a workspace of its own, since it needs a wasmtime other than the
  pin. No root build, test, clippy or fmt reaches it; only the `test-webgpu` CI
  job does.
- A program behaves the same here as under `wado run`, which is the only reason
  to have both: the arguments after the input file go to the guest unparsed,
  `--dir` and `--no-dir` grant what they grant there, and the engine takes the
  same collector and Cranelift level. A divergence is a defect, not a variant.
- It compiles nothing itself. `WADO` names the binary that dispatched the
  subcommand ([External Subcommands](../docs/wep-2026-09-19-external-subcommands.md)),
  and the tests set it the same way, so the tests and a real invocation reach
  the compiler through one path.
- Running needs a GPU adapter. wgpu finds none without a driver, and the host
  says so and exits rather than letting the guest see `request-adapter` answer
  `none`. On Linux `mesa-vulkan-drivers` supplies the lavapipe software adapter,
  which is what CI installs; macOS answers with Metal and Windows with D3D12.
- Dependencies live in this crate's own `Cargo.toml`.

## Module Map

- `args.rs` — the command line: `-O<level>`, `--dir`, `--no-dir`, and the
  program's own arguments after the input file.
- `compile.rs` — the input as a component: a `.wasm` is taken as given, a
  `.wado` goes through `WADO compile` into a temporary directory.
- `host.rs` — the wasmtime engine, the WASI P3 and `wasi:webgpu` linkers, and
  the wgpu instance the latter draws on.
