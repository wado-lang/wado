# wado-lsp

Language service engine for the Wado compiler toolchain.

## Rules

- Types follow LSP semantics (0-based positions, standard severity levels).
- See [LSP Architecture WEP](../docs/wep-2026-04-18-lsp-architecture.md) for crate scope and build targets. The WEP supersedes earlier rules about this crate being IO-free, `wasm32-unknown-unknown`-only, or protocol-agnostic.

## Architecture

| File                        | Role                                                                                             |
| --------------------------- | ------------------------------------------------------------------------------------------------ |
| `src/lib.rs`                | `Engine` struct: document management + query dispatch                                            |
| `src/host.rs`               | `FilesystemCompilerHost`: default `CompilerHost` for disk-backed source loading                  |
| `src/diagnostics.rs`        | Compiler `Diagnostic` to LSP-compatible `Diagnostic` conversion                                  |
| `src/semantic_tokens.rs`    | Semantic token computation (lexer + AST classification)                                          |
| `src/definition.rs`         | Go-to-definition via `Annotated::{ast_id_at, referenced_symbol, symbol_at}`                      |
| `src/hover.rs`              | Hover info; locals render from the resolved AST node, items delegate to `wado_compiler::unparse` |
| `src/references.rs`         | Find-references, walks `Annotated::iter_references` and collects matching use-sites              |
| `src/document_highlight.rs` | Document highlight; classifies each occurrence as Read or Write via AST walk                     |
| `src/location.rs`           | Shared cursor→`SymbolKey` resolution and span/URI helpers                                        |
| `src/server.rs`             | `run_stdio()`: blocking stdin/stdout loop feeding the async dispatcher                           |
| `src/server/transport.rs`   | Content-Length framing + typed JSON-RPC send/receive helpers                                     |
| `src/server/dispatch.rs`    | LSP method routing and server-lifecycle enforcement                                              |
| `src/server/rpc.rs`         | LSP wire types (params, capabilities, notifications)                                             |
| `src/bin/wado-lsp.rs`       | Binary entrypoint; drives `run_stdio()` via `futures::executor::block_on`                        |

### Engine

`Engine` manages open documents (`IndexMap<String, String>`) and provides query methods. Each query takes a `&impl CompilerHost` to load imported modules.

### DiagnosticCollector

`DiagnosticCollector` in `lib.rs` wraps any `CompilerHost`, delegating `load_source` while silently collecting all emitted diagnostics. This avoids modifying or depending on a specific host implementation.

### Stdio server

`server::run_stdio` is the binary's only entrypoint and is also re-used by `wado-cli/src/lsp.rs`. I/O is synchronous (`std::io`) so the crate builds for `wasm32-wasip2`, where tokio's `io-std` is unavailable; the dispatcher still awaits `Engine` futures between messages. Async is driven by `futures::executor::block_on` — the crate does not depend on tokio.

## Related Files in wado-cli

| File                   | Role                                                                           |
| ---------------------- | ------------------------------------------------------------------------------ |
| `src/lsp.rs`           | `wado lsp` subcommand — delegates to `wado_lsp::server::run_stdio`             |
| `src/compiler_host.rs` | CLI-side wrapper over `wado_lsp::FilesystemCompilerHost` (stderr + timestamps) |
| `src/query.rs`         | `wado query` subcommand: arg parsing, dispatch                                 |
| `src/query_adapter.rs` | Engine invocation, text/JSON output formatting                                 |
| `tests/lsp.rs`         | Integration tests for both `wado lsp` and `wado query`                         |

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
- [ ] `textDocument/callHierarchy` (prepare / incomingCalls / outgoingCalls)
- [ ] `textDocument/typeHierarchy` (prepare / supertypes / subtypes)

### Language Features — Comprehension

- [ ] `textDocument/signatureHelp`
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
