# wado-run-pjrt

The `wado run-pjrt` subcommand: it compiles a Wado program with `wado` and runs
the component on a wasmtime host that serves `core:pjrt` from a PJRT plugin.

## Rules

- A plugin is loaded at run time, from `--plugin` or `WADO_PJRT_PLUGIN`, so no
  build depends on one. `mise run test-pjrt` fetches the pinned CPU plugin
  (`scripts/fetch-pjrt-plugin.sh`) and runs the `#[ignore]`d tests on it.
- `xla/pjrt/c/pjrt_c_api.h` is the one part of XLA that may be read. `src/ffi.rs`
  follows it. An argument struct must be at least the plugin's size, so each
  struct is declared in full, whatever a call uses of it.
- A program behaves the same here as under `wado run`, which is the only reason
  to have both. `wado_host::runner` holds what makes that so, and a divergence
  is a defect, not a variant.
