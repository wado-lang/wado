# wado-run-webgpu

The `wado run-webgpu` subcommand: it compiles a Wado program with `wado` and
runs the component on a wasmtime host that serves `wasi:webgpu`.

## Rules

- This crate is a workspace of its own and is excluded from the repository's.
  It needs wasmtime 48 where the workspace pins 47.0.3, and one workspace
  resolves one version of a crate. Nothing here is built by `cargo build` at the
  repository root, `mise run test`, or the repository's clippy and fmt jobs. The
  `test-webgpu` CI job is what builds, lints and tests it, on every change that
  is not documentation.
- It compiles nothing itself. `WADO` names the binary that dispatched the
  subcommand ([External Subcommands](../docs/wep-2026-09-19-external-subcommands.md)),
  and the tests set it the same way, so the tests and a real invocation reach
  the compiler through one path.
- Running needs a GPU adapter. wgpu finds none without a driver, and the host
  says so and exits rather than letting the guest see `request-adapter` answer
  `none`. On Linux `mesa-vulkan-drivers` supplies the lavapipe software adapter,
  which is what CI installs; macOS answers with Metal and Windows with D3D12.
- The Rust rules in `.claude/skills/rust/SKILL.md` apply here too, except that
  dependencies live in this crate's own `Cargo.toml`, there being no workspace
  above it to hold them.

## Module Map

- `args.rs` — the command line: `-O<level>`, `--dir`, `--no-dir`, and the
  program's own arguments after the input file.
- `compile.rs` — the input as a component: a `.wasm` is taken as given, a
  `.wado` goes through `WADO compile` into a temporary directory.
- `host.rs` — the wasmtime engine, the WASI P3 and `wasi:webgpu` linkers, and
  the wgpu instance the latter draws on.
