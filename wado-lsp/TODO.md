# wado-lsp TODO

## LSP Feature Implementation

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
- [x] `textDocument/inlayHint`
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
  Also the prerequisite for caching `FilesystemCompilerHost::dependency_index`
  across requests: the walk-up + `wado.toml` + `wado.lock` parse currently
  re-runs once per document version (the loader asks for the index exactly
  once, when it is constructed). Caching it without watching the manifest
  would serve a stale dependency set after an edit to `wado.toml`.
- [ ] `workspace/executeCommand`
- [ ] `workspace/applyEdit`
- [ ] File operations (`willCreateFiles` / `didCreateFiles` / `willRenameFiles` / `didRenameFiles` / `willDeleteFiles` / `didDeleteFiles`)

### Window Features

- [ ] `window/showMessage` / `window/showMessageRequest`
- [ ] `window/showDocument`
- [ ] `window/logMessage`
- [ ] `window/workDoneProgress` (create / cancel)
- [ ] `telemetry/event`

## Jump-to-Definition Follow-ups

Known gaps and quality debts in `src/definition.rs` / `src/location.rs` not tied
to a specific LSP request kind. Each item identifies the symptom and the
concrete code location involved.

- [ ] Narrow the `#include_str` / `#include_bytes` cursor match to the path
  literal. `Literal::IncludeStr(String)` / `IncludeBytes(String)` store
  only the path text, so `IncludePathFinder` in `src/definition.rs`
  matches against the full `#include_str(...)` `LiteralExpr::span`. This
  causes `#`, the macro name, and the parentheses to all jump. Either
  store the path literal's `Span` on the variant, or emit a separate
  `AstId` for the string literal.
- [ ] Define the jump-to-def semantics of `UseItem::Simple.alias`. Tests cover
  only clicks on the imported `name`. Decide and test: clicking the alias
  should jump to the alias's use-site definition (so callers that use the
  alias go to the alias line), while clicks on the original `name` still
  go to the source module. Update `record_use_specifier_references` and
  add coverage in `tests/definition.rs`.
- [ ] Make `name_span_of` total enough to drop the `def_span` fallback chain.
  `Cursor::def_span` falls through three levels (`name_span_of` →
  `symbol.span` → `span_of_key`) because `name_span_of` does not cover
  every addressable `AstId` (e.g. anonymous `impl` blocks, `Item::Resource`,
  `Item::Test` have no dedicated `name_span` field). Either give every
  decl-bearing AST node a `name_span` so `name_span_of` becomes total,
  or accept the fallback and remove this TODO.
- [ ] Serve bundled `.wat` / `.wasm` assets via `workspace/textDocumentContent`.
  `core:` / `wasi:` source modules are now openable, but the
  `ModuleSource::Wasm { path, .. }` arm of `module_uri` in
  `src/location.rs` still returns the import path verbatim — clients have
  no way to open `core:libm.wat`. Extend `Engine::text_document_content`
  to dispatch to `wado_compiler::stdlib::get_stdlib_wasm_asset` for `.wat`
  assets (text) and either disassemble or skip `.wasm` (binary). Decide
  whether to advertise additional schemes or reuse `core:` / `wasi:`.
