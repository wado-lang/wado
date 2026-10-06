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
- `stub_host.rs` — `StubHost`, a filesystem `CompilerHost` that answers the
  dependency index and environment from `HostStubs`. It compiles the e2e
  fixtures for both the tests and the golden dumps.
- `fixture.rs` — `CompileInputs`, the `__DATA__` keys that say how to compile a
  fixture. The e2e harness and the golden dumps both read them, so a golden
  records the program its test runs.
