# wado-lsp

Language service engine for the Wado compiler toolchain.

## Rules

- Types follow LSP semantics (0-based positions, standard severity levels).
- See [LSP Architecture WEP](../docs/wep-2026-04-18-lsp-architecture.md) for crate scope and build targets. The WEP supersedes earlier rules about this crate being IO-free, `wasm32-unknown-unknown`-only, or protocol-agnostic.

## Architecture

| File                        | Role                                                                                                                      |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `src/lib.rs`                | `Engine` struct: document management + query dispatch                                                                     |
| `src/host.rs`               | `FilesystemCompilerHost`: default `CompilerHost` for disk-backed source loading                                           |
| `src/diagnostics.rs`        | Compiler `Diagnostic` to LSP-compatible `Diagnostic` conversion                                                           |
| `src/semantic_tokens.rs`    | Semantic token computation (lexer + AST classification)                                                                   |
| `src/definition.rs`         | Go-to-definition via `Cursor::{def_key, def_span}` and a file-path matcher for `use`/`#include` paths                     |
| `src/hover.rs`              | Hover info; `Cursor::def_symbol` selects the binding, locals render from the AST node, items via `wado_compiler::unparse` |
| `src/references.rs`         | Find-references via `Cursor::references_to_def`                                                                           |
| `src/document_highlight.rs` | Document highlight; Read/Write classification consults `Annotated::is_write_target`                                       |
| `src/location.rs`           | URI / span helpers for translating compiler positions to LSP types                                                        |
| `src/server.rs`             | `run_stdio()`: blocking stdin/stdout loop feeding the async dispatcher                                                    |
| `src/server/transport.rs`   | Content-Length framing + typed JSON-RPC send/receive helpers                                                              |
| `src/server/dispatch.rs`    | LSP method routing and server-lifecycle enforcement                                                                       |
| `src/server/rpc.rs`         | LSP wire types (params, capabilities, notifications)                                                                      |
| `src/bin/wado-lsp.rs`       | Binary entrypoint; drives `run_stdio()` via `futures::executor::block_on`                                                 |

### Engine

`Engine` manages open documents (`IndexMap<String, String>`) and provides query methods. Each query takes a `&impl CompilerHost` to load imported modules.

### DiagnosticCollector

`DiagnosticCollector` in `lib.rs` wraps any `CompilerHost`, delegating `load_source` while silently collecting all emitted diagnostics. This avoids modifying or depending on a specific host implementation.

### Stdio server

`server::run_stdio` is the binary's only entrypoint and is also re-used by `wado-cli/src/lsp.rs`. I/O is synchronous (`std::io`) so the crate builds for `wasm32-wasip2`, where tokio's `io-std` is unavailable; the dispatcher still awaits `Engine` futures between messages. Async is driven by `futures::executor::block_on` — the crate does not depend on tokio.

### Bundled stdlib content

Jump-to-definition into bundled stdlib modules emits `core:<name>` / `wasi:<interface>` URIs from `module_uri` (`src/location.rs`). The server advertises a `workspace.textDocumentContent` capability for the `core` and `wasi` schemes; clients call `workspace/textDocumentContent` to retrieve the source. The handler in `src/server/dispatch.rs` resolves it via `Engine::text_document_content` (`src/lib.rs`), which looks the URI up in `wado_compiler::stdlib::get_stdlib_module`. Both `core:cli` and the rfc3986-normalised form `core:/cli` are accepted.

The VS Code extension (`wado-vscode/src/extension.ts`) bridges this with a `TextDocumentContentProvider` registered for both schemes, and forces opened documents to language `wado` because opaque URIs (e.g. `core:cli`) carry no extension for VS Code's language detector.

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
- [ ] File operations (`willCreateFiles` / `didCreateFiles` / `willRenameFiles` / `didRenameFiles` / `willDeleteFiles` / `didDeleteFiles`)

### Window Features

- [ ] `window/showMessage` / `window/showMessageRequest`
- [ ] `window/showDocument`
- [ ] `window/logMessage`
- [ ] `window/workDoneProgress` (create / cancel)
- [ ] `telemetry/event`

## TODO: Jump-to-Definition Follow-ups

Known gaps and quality debts in `src/definition.rs` / `src/location.rs` not tied
to a specific LSP request kind. Each item identifies the symptom and the
concrete code location involved.

- [ ] **Jump-to-def for non-`Simple` `UseItem` variants.**
      `Resolver::record_use_specifier_references` (`wado-compiler/src/resolver.rs`)
      skips `UseItem::{EffectFunctions, Namespace}`. Give `UseItemSimple`
      (effect functions) and `UseItem::Namespace` their own `AstId` + name
      `Span` in `wado-compiler/src/ast.rs` so cursor-on-name works for:
      - `use foo from "./foo.wado"` (namespace import)
      - `use { Eff::{f, g} } from "..."` (effect function list — both the
      effect name and the function names)
- [ ] **Narrow `#include_str` / `#include_bytes` cursor match to the path
      literal.** `Literal::IncludeStr(String)` / `IncludeBytes(String)` store
      only the path text, so `IncludePathFinder` in `src/definition.rs`
      matches against the full `#include_str(...)` `LiteralExpr::span`. This
      causes `#`, the macro name, and the parentheses to all jump. Either
      store the path literal's `Span` on the variant, or emit a separate
      `AstId` for the string literal.
- [ ] **Define `UseItem::Simple.alias` jump-to-def semantics.** Tests cover
      only clicks on the imported `name`. Decide and test: clicking the alias
      should jump to the alias's use-site definition (so callers that use the
      alias go to the alias line), while clicks on the original `name` still
      go to the source module. Update `record_use_specifier_references` and
      add coverage in `tests/definition.rs`.
- [ ] **Make `name_span_of` total enough to drop the `def_span` fallback chain.**
      `Cursor::def_span` falls through three levels (`name_span_of` →
      `symbol.span` → `span_of_key`) because `name_span_of` does not cover
      every addressable `AstId` (e.g. anonymous `impl` blocks, `Item::Resource`,
      `Item::Test` have no dedicated `name_span` field). Either give every
      decl-bearing AST node a `name_span` so `name_span_of` becomes total,
      or accept the fallback and remove this TODO.
- [ ] **Serve bundled `.wat` / `.wasm` assets via `workspace/textDocumentContent`.**
      `core:` / `wasi:` source modules are now openable, but the
      `ModuleSource::Wasm { path, .. }` arm of `module_uri` in
      `src/location.rs` still returns the import path verbatim — clients have
      no way to open `core:libm.wat`. Extend `Engine::text_document_content`
      to dispatch to `wado_compiler::stdlib::get_stdlib_wasm_asset` for `.wat`
      assets (text) and either disassemble or skip `.wasm` (binary). Decide
      whether to advertise additional schemes or reuse `core:` / `wasi:`.
