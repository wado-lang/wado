# wado-lsp

Language service engine for the Wado compiler toolchain.

## Rules

- This crate must be IO-free. No filesystem, network, or stdio operations. All IO goes through `CompilerHost`.
- This crate must compile for `wasm32-unknown-unknown`. Do not use OS-dependent `std` modules.
- Types follow LSP semantics
- Protocol handling (LSP JSON-RPC, MCP, CLI one-shot query) belongs in `wado-cli`

## Architecture

| File                 | Role                                                            |
| -------------------- | --------------------------------------------------------------- |
| `src/lib.rs`         | `Engine` struct: document management + query dispatch           |
| `src/diagnostics.rs` | Compiler `Diagnostic` to LSP-compatible `Diagnostic` conversion |

### Engine

`Engine` manages open documents (`IndexMap<String, String>`) and provides query methods. Each query takes a `&impl CompilerHost` to load imported modules.

### DiagnosticCollector

`DiagnosticCollector` in `lib.rs` wraps any `CompilerHost`, delegating `load_source` while silently collecting all emitted diagnostics. This avoids modifying or depending on a specific host implementation.

## Related Files in wado-cli

| File                   | Role                                                                            |
| ---------------------- | ------------------------------------------------------------------------------- |
| `src/lsp.rs`           | `wado lsp` subcommand: main loop, LSP message dispatch                          |
| `src/lsp_adapter.rs`   | LSP JSON-RPC transport (Content-Length framing), diagnostics-to-JSON conversion |
| `src/query.rs`         | `wado query` subcommand: arg parsing, dispatch                                  |
| `src/query_adapter.rs` | Engine invocation, text/JSON output formatting                                  |
| `tests/lsp.rs`         | Integration tests for both `wado lsp` and `wado query`                          |

## References

See [lsp.md](lsp.md) for the specification of the latest LSP.
