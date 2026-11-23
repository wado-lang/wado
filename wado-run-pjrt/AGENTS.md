# wado-run-pjrt

The `wado run-pjrt` subcommand: it compiles a Wado program with `wado` and runs
the component on a wasmtime host that serves `wado-lang:pjrt` from a PJRT
plugin. The directory is that package too, since nothing else runs it: its
interface is `assets/pjrt.wit`, and its Wado modules are in `src/` beside the
host's Rust.

## Rules

- A plugin is loaded at run time, so no build depends on one. It comes from
  `--plugin` or `WADO_PJRT_PLUGIN`; with neither, the host fetches the pinned
  CPU plugin into the user cache on first use (`wado_host::pinned_download`).
  `mise run test-pjrt` also fetches `libtpu.so` (`scripts/fetch-pjrt-plugin.sh`)
  and runs the `#[ignore]`d tests on both.
- `xla/pjrt/c/pjrt_c_api.h` is the one part of XLA that may be read. `src/ffi.rs`
  follows it. An argument struct must be at least the plugin's size, so each
  struct is declared in full, whatever a call uses of it.
- A program behaves the same here as under `wado run`, which is the only reason
  to have both. `wado_host::runner`, `guest` and `options` hold what makes that so, and a divergence
  is a defect, not a variant.
- A plugin's fault ends the process, so [crashes.md](./crashes.md) records each
  `SIGSEGV` and `SIGABRT`, fixed or not, with the top of its backtrace.
- A plugin reads as far as a guest's values say, so the host checks what reaches
  it through a pointer before the call. `crashes.md` lists what a plugin refuses
  on its own.
- Every plugin call goes through `Plugin::enter`, on the plugin's thread, never
  on the fiber a guest runs on.
