# wado-lsp

Language service engine for the Wado compiler toolchain.

## Rules

- This crate must be IO-free. No filesystem, network, or stdio operations. All IO goes through `CompilerHost`.
- This crate must compile for `wasm32-unknown-unknown`.
- Types follow LSP semantics
- Protocol handling (LSP JSON-RPC, MCP, CLI one-shot query) belongs in `wado-cli`

## Architecture

| File                     | Role                                                                                             |
| ------------------------ | ------------------------------------------------------------------------------------------------ |
| `src/lib.rs`             | `Engine` struct: document management + query dispatch                                            |
| `src/diagnostics.rs`     | Compiler `Diagnostic` to LSP-compatible `Diagnostic` conversion                                  |
| `src/semantic_tokens.rs` | Semantic token computation (lexer + AST classification)                                          |
| `src/definition.rs`      | Go-to-definition via `Annotated::{ast_id_at, referenced_symbol, symbol_at}`                      |
| `src/hover.rs`           | Hover info; locals render from the resolved AST node, items delegate to `wado_compiler::unparse` |

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

## TODO: LSP Feature Implementation

Progress tracker for LSP 3.18 feature kinds. Each item represents a protocol kind (request/notification group), not individual fields or options. Only remaining work is listed.

### Server Lifecycle

- [ ] `client/registerCapability` / `client/unregisterCapability`
- [ ] `$/setTrace` / `$/logTrace`
- [ ] `$/cancelRequest`

### Text Document Synchronization

- [ ] `textDocument/didChange` (incremental sync)
- [ ] `textDocument/willSave`
- [ ] `textDocument/willSaveWaitUntil`
- [ ] `textDocument/didSave`
- [ ] `textDocument/didRename`

### Diagnostics

- [ ] `textDocument/diagnostic` (pull)
- [ ] `workspace/diagnostic` (pull)

### Language Features — Navigation

- [ ] `textDocument/declaration`
- [ ] `textDocument/typeDefinition`
- [ ] `textDocument/implementation`
- [ ] `textDocument/references`
- [ ] `textDocument/callHierarchy` (prepare / incomingCalls / outgoingCalls)
- [ ] `textDocument/typeHierarchy` (prepare / supertypes / subtypes)

### Language Features — Comprehension

- [ ] `textDocument/signatureHelp`
- [ ] `textDocument/documentHighlight`
- [ ] `textDocument/documentLink`
- [ ] `textDocument/codeLens`
- [ ] `textDocument/inlayHint`
- [ ] `textDocument/inlineValue`
- [ ] `textDocument/moniker`

### Language Features — Structure

- [ ] `textDocument/documentSymbol`
- [ ] `textDocument/foldingRange`
- [ ] `textDocument/selectionRange`
- [ ] `textDocument/linkedEditingRange`

### Language Features — Editing

- [ ] `textDocument/completion`
- [ ] `textDocument/codeAction`
- [ ] `textDocument/formatting`
- [ ] `textDocument/rangeFormatting`
- [ ] `textDocument/onTypeFormatting`
- [ ] `textDocument/rename` / `textDocument/prepareRename`
- [ ] `textDocument/inlineCompletion`
- [ ] `textDocument/documentColor` / `textDocument/colorPresentation`

### Workspace Features

- [ ] `workspace/symbol`
- [ ] `workspace/configuration`
- [ ] `workspace/didChangeConfiguration`
- [ ] `workspace/workspaceFolders` / `workspace/didChangeWorkspaceFolders`
- [ ] `workspace/didChangeWatchedFiles`
- [ ] `workspace/executeCommand`
- [ ] `workspace/applyEdit`
- [ ] `workspace/textDocumentContent`
- [ ] File operations (`willCreateFiles` / `didCreateFiles` / `willRenameFiles` / `didRenameFiles` / `willDeleteFiles` / `didDeleteFiles`)

### Window Features

- [ ] `window/showMessage` / `window/showMessageRequest`
- [ ] `window/showDocument`
- [ ] `window/logMessage`
- [ ] `window/workDoneProgress` (create / cancel)
- [ ] `telemetry/event`
