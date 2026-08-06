# wado-cli

The `wado` binary. It drives `wado-compiler` and `wado-lsp`, hosts the wasmtime
runtime for `run` / `serve` / `test`, and owns the Kiln and dependency backends.

How to _use_ the CLI is the `wado-cli` skill, not this file.

## Rules

- Argument parsing is hand-rolled on `lexopt`. Do not introduce clap or another
  parser framework. Each subcommand parses its own options and owns its usage
  and help text in its own module.
- `process::exit` belongs to `main()` alone. A subcommand returns `CliExit`
  (`args.rs`) from both its parse and its run, so there is exactly one exit
  path. Use `CliExit::silent_failure` when the subcommand has already printed
  its own diagnostics.
- Adding a subcommand means adding a `Cmd` variant in `main.rs` — it must appear
  in `ALL` and gain a `name`, `args`, and `desc` arm — plus its module.
- The binary sets mimalloc as the global allocator: `wado serve` is
  allocation-heavy per request and the system allocator contends across threads.

## Module Map

- `compile.rs`, `check.rs`, `run.rs`, `serve.rs`, `test.rs`, `format.rs`, `doc.rs`, `dump.rs`, `wit.rs`, `query.rs` — one subcommand each.
- `runtime.rs`, `http_hooks.rs`, `timezone_host.rs`, `tls_trust.rs` — the wasmtime host: instantiation, WASI wiring, and the hooks `serve` needs.
- `kiln_driver.rs`, `kiln_provider.rs`, `kiln_runtime.rs`, `kiln_wit.rs`, `kiln_metadata.rs` — Kiln generators. `check` re-runs them and byte-compares against the committed source.
- `manifest.rs`, `build.rs`, `build_dep.rs`, `dep_component.rs`, `fetch.rs`, `git.rs`, `oci.rs`, `registry.rs`, `publish.rs` — `wado.toml` handling and the dependency backends behind `wado-manifest`'s `DependencyProvider` seam.
- `query_adapter.rs`, `lsp.rs` — bridge to `wado-lsp`, for the `query` subcommand and the stdio server.
- `discover.rs`, `test_report.rs` — test file discovery and the progress digest.

## Tests

`tests/cli_parse.rs` covers argument parsing without running a compile; the
rest (`cli.rs`, `serve.rs`, `lsp.rs`, `kiln_*.rs`, `dependency_resolution.rs`,
`git_dependency.rs`, `run_inprocess.rs`) drive the subcommands end to end.
