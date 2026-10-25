# wado-host

The native host pieces that `wado-cli`, `wado-dev-tools`, and
`wado-compiler-tests` share.

## Rules

- This crate exists so that the compiler's tests never depend on `wado-cli`.
  It depends on the compiler, so nothing here may be needed by the compiler
  library itself.
- It is native-only. Code the browser also runs belongs in `wado-lsp`, which
  must build for `wasm32-unknown-unknown`.
- Put a piece here once a second crate needs it, not before. What only `wado`
  needs stays in `wado-cli`.

## Module Map

- `timezone.rs` — the host for `wasi:clocks/timezone`, which wasmtime binds but
  does not implement.
- `tls_trust.rs` — the rustls crypto provider and the trust store for outbound
  TLS.
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
  joined task continues in the caller.
- `sandbox.rs` — spawning a child confined to the guest's grants plus a fixed
  baseline, for `core:process`: Landlock and seccomp in `sandbox/linux.rs`,
  `sandbox-exec` in `sandbox/macos.rs`. `wado-cli`'s `process_host` is its
  caller. See [WEP: `core:process`](../docs/wep-2026-10-21-core-process.md).
- `atomic_write.rs` — write a file through a staged sibling and a rename.
- `engine.rs` — the Wasm features and GC defaults every wasmtime engine takes,
  so the e2e harness runs a program as `wado` does, and the fuel-metered engine
  generators and `core:eval` run on.
