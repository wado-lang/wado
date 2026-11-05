# wado-host

The native host pieces that `wado-cli`, `wado-dev-tools`, the runners, and
`wado-compiler-tests` share.

## Rules

- This crate exists so that the compiler's tests never depend on `wado-cli`.
  It depends on the compiler, so nothing here may be needed by the compiler
  library itself.
- The `toolchain` feature, on by default, gates what needs the compiler. A
  runner turns it off, so everything a guest reaches and the command line of a
  run stay free of it.
- It is native-only. Code the browser also runs belongs in `wado-lsp`, which
  must build for `wasm32-unknown-unknown`.
- Put a piece here once a second crate needs it, not before. What only `wado`
  needs stays in `wado-cli`.

## Module Map

- `timezone.rs` — the host for `wasi:clocks/timezone`, which wasmtime binds but
  does not implement.
- `stub_host.rs` — `StubHost`, `wado-lsp`'s `FilesystemCompilerHost` with the
  dependency index and environment answered from `HostStubs`. It compiles the
  e2e fixtures for both the tests and the golden dumps.
- `fixture.rs` — `CompileInputs`, the `__DATA__` keys that say how to compile a
  fixture. The e2e harness and the golden dumps both read them, so a golden
  records the program its test runs.
- `kiln/` — the Kiln pipeline (`driver.rs`), the generator runtime
  (`runtime.rs`), the compiled-component cache (`component_cache.rs`), and the
  per-invocation cache state (`metadata.rs`). `wado` feeds the pipeline its own
  generator provider; `local.rs` feeds it the generators a fixture names,
  each compiled after its own invocations have run.
- `random.rs` — the fixed-seed `wasi:random/insecure` and `insecure-seed` that
  a Kiln generator and a `core:eval` program get, so each stays a function of
  its input.
- `sync.rs` — how production code locks a mutex and joins a task or thread. A
  poisoned guard is recovered, and test mocks may still `unwrap`. A panic in a
  joined task continues in the caller, and one in a thread nobody joins ends
  the process.
- `sandbox.rs` — spawning a child confined to the guest's grants plus a fixed
  baseline, for `core:process`: Landlock and seccomp in `sandbox/linux.rs`,
  `sandbox-exec` in `sandbox/macos.rs`, and the terminal a child holds as a
  shell's job does in `sandbox/terminal.rs`. See
  [WEP: `core:process`](../docs/wep-2026-10-21-core-process.md).
- `process_host.rs` — the `core:process` host: whether a guest may spawn, and
  the streams and kill of each child. `sandbox.rs` confines it.
- `http_hooks.rs` — the `wasi:http` client's connections, on the trust store of
  `tls_trust.rs`.
- `user_config.rs` — the user's `~/.config/wado/config.toml`
  (`$XDG_CONFIG_HOME`): the Wado root and `[process] read`. Read once, and
  ignored whole when another user could have written it.
- `path_search.rs` — finding a program on `PATH`, for `wado <name>`,
  `core:process`, and a runner started without `WADO`.
- `ticker.rs` — `Ticker`, a thread on an interval that dropping stops and joins.
- `engine.rs` — the Wasm features and GC defaults every wasmtime engine takes,
  so the e2e harness runs a program as `wado` does, and the fuel-metered engine
  generators and `core:eval` run on.
- `options.rs` — every option `wado` and the runners share, with the `CliExit`
  a command ends in. `wado-cli` applies a compile knob to the compiler's types;
  a runner passes it on as spelled.
- `guest.rs` — what a guest reaches, on every host: the store data, the linker
  and the engine under the runtime knobs. A host embeds a `GuestState` and names
  the field to `guest_views!`.
- `runner.rs` — what `wado run` and each `wado run-*` subcommand share, so a
  program behaves the same under all of them: the command line, the compile
  (`wado build -o -`), the run and the guest profile.
- `fuel.rs` — the price of each Wasm operator in fuel, which every metered
  engine takes. See
  [WEP: Fuel Benchmarks](../docs/wep-2026-10-25-fuel-benchmarks.md).
