# wado-lsp

Language service engine for the Wado compiler toolchain. Scope and build
targets: [WEP: LSP Architecture](../docs/wep-2026-04-18-lsp-architecture.md).
The protocol: [lsp.md](lsp.md).

## Rules

- Types follow LSP semantics (0-based positions, standard severities).
- The `Engine` and query path compile for `wasm32-unknown-unknown` (CI checks
  it), so the browser playground runs them. I/O belongs to the host:
  `wado-cli` on the desktop, an in-memory host in the browser.
- No `catch_unwind` around compiler calls: a panic reaching the server is a
  compiler bug, and taking the server down says so.
- Every AST walk goes through `wado_compiler::ast::AstVisitor`.
- A wrapper over `CompilerHost` (`DiagnosticCollector`) delegates every method;
  a trait default drops the wrapped host's dependency index.
- The server is synchronous `std::io` driven by `futures::executor::block_on`,
  with no tokio, so it builds for `wasm32-wasip2`. `run_stdio` returns the exit
  code rather than exiting; `wado lsp` reuses it.

## Behaviour Worth Knowing

- Queries run on a partial `Semantics`: a failed phase degrades to "no answer"
  for what it would have filled. Lexing and parsing recover; only a loader
  failure yields `Semantics::empty`, where semantic tokens fall back to lexer
  and AST classification. `tests/parse_error.rs` pins it.
- A contextual keyword is whichever the parse read it as, recorded in
  `Module::contextual_keywords`: no AST node says whether `type` was a keyword
  or a name. A field name (`AstSpans::field_names`) outranks symbol resolution,
  so shorthand `{ state }` colours the same with or without a snapshot.
- Position encoding prefers UTF-32, a passthrough of the compiler's codepoint
  columns, then UTF-8, then UTF-16. Every conversion lives in `text.rs`.
- `Engine::diagnostics` reports only the requested document's: an imported
  file's line and column would land on unrelated code. One with no span is kept
  at the document start, since the loader's hard failures carry none. The
  compiler owes every located diagnostic its file, which the e2e host asserts.
- `file:` URIs are percent-decoded on the way in and re-encoded on the way out,
  and relative imports normalised lexically, since clients key documents by URI
  string.
- `core:` / `wasi:` sources are served through `workspace/textDocumentContent`,
  which accepts the rfc3986-normalised `core:/cli` as well as `core:cli`. The VS
  Code extension registers a provider for both schemes and forces language
  `wado`, since an opaque URI carries no extension.

## TODO

Remaining LSP 3.18 kinds:

- [ ] Lifecycle: `client/registerCapability` / `unregisterCapability`,
      `$/setTrace` / `$/logTrace`, `$/cancelRequest`
- [ ] Sync: incremental `didChange`, `willSave`, `willSaveWaitUntil`, `didSave`,
  `didRename`
- [ ] Pull diagnostics: `textDocument/diagnostic`, `workspace/diagnostic`
- [ ] Navigation: `declaration`, `typeDefinition`, `implementation`,
  `callHierarchy`, `typeHierarchy`
- [ ] Comprehension: `signatureHelp`, `documentLink`, `codeLens`, `inlineValue`,
  `moniker`
- [ ] Structure: `documentSymbol`, `foldingRange`, `selectionRange`,
  `linkedEditingRange`
- [ ] Editing: `completion`, `codeAction`, `formatting`, `rangeFormatting`,
  `onTypeFormatting`, `rename` / `prepareRename`, `inlineCompletion`,
  `documentColor` / `colorPresentation`
- [ ] Workspace: `symbol`, `configuration`, `didChangeConfiguration`,
  `workspaceFolders`,
  `didChangeWatchedFiles` (which caching `dependency_index` across
  requests waits on), `executeCommand`, `applyEdit`, file operations
- [ ] Window: `showMessage` / `showMessageRequest`, `showDocument`, `logMessage`, `workDoneProgress`,
  `telemetry/event`

Definition gaps:

- [ ] `#include_str(...)` jumps from anywhere in the call, not just its path
  literal.
- [ ] A click on a `use` alias has no defined target.
- [ ] `Cursor::def_span` falls back through three lookups because
  `name_span_of` is not total.
- [ ] Bundled `.wat` / `.wasm` assets (`core:libm.wat`) cannot be opened.
