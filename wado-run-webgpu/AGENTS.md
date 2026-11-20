# wado-run-webgpu

The `wado run-webgpu` subcommand: it compiles a Wado program with `wado` and
runs the component on a wasmtime host that serves `wasi:webgpu`.

## Rules

- This crate is a workspace of its own and is excluded from the repository's,
  so that nothing at the repository root builds wgpu. Nothing here is built by
  `cargo build` at the repository root, `mise run test`, or the repository's
  clippy and fmt jobs. The `test-webgpu` CI job is what builds, lints and tests
  it, on every change that is not documentation.
- `wasi-webgpu-wasmtime` must build on the wasmtime the repository pins, since
  this crate shares `wado-host`'s `runner`.
- A program behaves the same here as under `wado run`, which is the only reason
  to have both. `wado_host::runner`, `guest` and `options` hold what makes that so, and a divergence
  is a defect, not a variant.
- The tests set `WADO` as `wado` does
  ([External Subcommands](../docs/wep-2026-09-19-external-subcommands.md)), so
  the tests and a real invocation reach the compiler through one path.
- Running needs a GPU adapter. On x86-64 Linux, where wgpu finds none and the
  user named no driver (`VK_DRIVER_FILES`, `VK_ICD_FILENAMES`) or backend
  (`WGPU_BACKEND`), a run fetches SwiftShader, Chrome's software Vulkan driver,
  before any other thread starts (`src/software.rs`). CI installs
  `mesa-vulkan-drivers` and runs on lavapipe instead, since SwiftShader runs
  out of memory on Loam's graph conformance program. Elsewhere
  with no adapter, the host says so and exits rather than letting the guest see
  `request-adapter` answer `none`. macOS answers with Metal and Windows with
  D3D12.
- Dependencies live in this crate's own `Cargo.toml`, there being no workspace
  above it to hold them. The other Rust rules apply as written.
