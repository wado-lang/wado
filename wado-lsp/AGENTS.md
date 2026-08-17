# wado-lsp

Language service engine for the Wado compiler toolchain.

## Rules

- Types follow LSP semantics (0-based positions, standard severity levels).
- See [LSP Architecture WEP](../docs/wep-2026-04-18-lsp-architecture.md) for crate scope and build targets. The WEP supersedes earlier rules about this crate being IO-free, `wasm32-unknown-unknown`-only, or protocol-agnostic.
- The crate still compiles for `wasm32-unknown-unknown` (CI-enforced: `cargo check -p wado-lsp --target wasm32-unknown-unknown`), keeping the `Engine` and query path OS-independent so the browser playground can run the language service without a WASI host. Runtime I/O (filesystem source loading) is delegated to the host — `wado-cli` on the desktop, an in-memory host in the browser.

## Architecture

| File                        | Role                                                                                                                                                                                             |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `src/lib.rs`                | `Engine` struct: document state + per-document `Semantics` snapshot cache + query dispatch                                                                                                       |
| `src/ast_search.rs`         | `FirstMatch` + `find_in_module`: the short-circuiting module walk that hover's local renderer and definition's `#include` path finder share                                                      |
| `src/host.rs`               | `FilesystemCompilerHost`: default `CompilerHost` for disk-backed source loading                                                                                                                  |
| `src/uri.rs`                | Typed `Uri` + `UriScheme` for parsing `file:` / `core:` / `wasi:` / `kiln:` URIs once instead of inline string splitting; percent-decodes and re-encodes `file:` paths                           |
| `src/text.rs`               | `PositionEncoding`, LSP `Position` ↔ compiler 1-based codepoint `(line, col)` conversion, `range_from_codepoints` (the one span→`Range` conversion), and the shared `LineIndex`                  |
| `src/diagnostics.rs`        | Compiler `Diagnostic` to LSP-compatible `Diagnostic` conversion (re-encodes spans in the negotiated position encoding; tags unused / dead-code lints with `DiagnosticTag::Unnecessary`)          |
| `src/semantic_tokens.rs`    | Semantic token computation. Classifies identifiers by resolved `SymbolKind` from the `Semantics` snapshot, falling back to lexer + AST heuristics. Re-encodes start/length at delta-encode time. |
| `src/definition.rs`         | Go-to-definition via `Cursor::{def_key, def_span}` and a file-path matcher for `use`/`#include` paths                                                                                            |
| `src/hover.rs`              | Hover info; `Cursor::def_symbol` selects the binding, locals render from the AST node, items via `wado_compiler::unparse`                                                                        |
| `src/inlay_hints.rs`        | Inlay hints: inferred-type hints on `let` / closure / `for-of` bindings, plus parameter-name hints at call sites                                                                                 |
| `src/references.rs`         | Find-references via `Cursor::references_to_def`                                                                                                                                                  |
| `src/document_highlight.rs` | Document highlight; Read/Write classification consults `Semantics::is_write_target`                                                                                                              |
| `src/location.rs`           | `ModuleSource` → LSP URI translation, including lexical `.`/`..` normalisation of relative imports                                                                                               |
| `src/query.rs`              | `QueryContext`: per-query bundle (`&Semantics` + source + URI + encoding) consumed by every position-bearing feature                                                                             |
| `src/server.rs`             | `run_stdio()`: blocking stdin/stdout loop feeding the async dispatcher; returns the LSP exit code                                                                                                |
| `src/server/transport.rs`   | Content-Length framing + typed JSON-RPC send/receive helpers                                                                                                                                     |
| `src/server/dispatch.rs`    | LSP method routing, position-encoding negotiation, and server-lifecycle enforcement                                                                                                              |
| `src/server/rpc.rs`         | LSP wire types (params, capabilities, notifications)                                                                                                                                             |
| `src/bin/wado-lsp.rs`       | Binary entrypoint; drives `run_stdio()` via `futures::executor::block_on`                                                                                                                        |
| `src/test_support.rs`       | Shared in-memory `MapHost` and the `open` / `open_files` fixture builders for unit + integration tests (`#[doc(hidden)] pub`); replaces per-file `TestHost` and per-file setup duplication       |

### AST walking

Every AST walk goes through `wado_compiler::ast::AstVisitor` and its `walk_*`
functions — never a hand-written traversal, which silently skips whatever a
later AST node adds.

The contextual keywords (`test`, `do`, `resume`) lex as identifiers, so only
`TestDecl::span` / `ResumeExpr::span` / `WithHandlerExpr::do_span` can classify
them as keywords.

### Engine

`Engine` owns per-document state: source text and a cached
`Rc<Snapshot>` built by composing `wado_compiler::parse` →
`wado_compiler::load` → `wado_compiler::semantics_of`, with kiln
invocation discovery (`kiln::prepare_invocations`) interleaved between
parse and load. A runtime-backed host that already ran the generators
can instead supply a precomputed `InvocationIndex` via
`open_document_with_invocations`; when present it is used verbatim and
consume-only discovery is skipped (native `wado query` does this — see
the Kiln WEP). `update_document` drops an injected index with the
snapshot, so a changed document falls back to consume-only.
The snapshot is built on first query and shared across back-to-back
queries on the same document version; `update_document` /
`close_document` invalidates it. The negotiated `PositionEncoding`
lives on `Engine` and is consulted by every position-bearing query.

Each query takes a `&impl CompilerHost` so the caller decides how
imported modules are loaded. Pass the same host across queries to
keep cross-file resolution consistent; the snapshot cache itself is
keyed by document text, not by host identity.

### Partial-result `Semantics`

The compiler frontend always produces a `Semantics` — even when an
analysis phase bails. LSP queries operate on whatever partial state
the phases produced (e.g. hover still works on a well-formed function
even when another function has a type error). Batch compilation
checks `Semantics::is_complete()` and aborts on partial results;
LSP-side queries simply degrade to "no answer" for fields the bailed
phase would have populated.

Both lexing and parsing are error-recovering. Recovered lex errors
surface as `lexer error: …` diagnostics, parser errors as
`parse error: …`, and the partial `Semantics` still drives position
queries on the surrounding healthy regions. The loader path is the
only remaining hard failure: if a missing/broken import or a
downstream phase bails, `build_semantics` (`src/lib.rs`) emits the
failure diagnostic via the host and returns [`Semantics::empty`],
and every position-bearing query returns `None` / `[]` for that
snapshot. Semantic-token highlighting still works in that case:
`Engine::semantic_tokens` passes `None` semantics to
`semantic_tokens::compute`, which falls back to lexer + AST
classification. When a snapshot _is_ available, identifiers are
classified by their resolved `SymbolKind` instead, with the same
heuristic path as the per-token fallback for names the elaborator
recorded no use→def edge for. The recovery behaviour is pinned by
`tests/parse_error.rs`.

### Position encoding

`PositionEncoding` (UTF-8 / UTF-16 / UTF-32) is negotiated at
`initialize` from the client's `general.positionEncodings`
capability. Server preference: UTF-32 (cheapest — the compiler's
`Span::column` is already a 1-based codepoint index, so UTF-32 is a
passthrough) → UTF-8 → UTF-16 (LSP default; only chosen when the
client offers nothing else). The codepoint semantics come from
`lexer.rs::Lexer::advance`, which increments `column` per Unicode
scalar value, not per byte and not per UTF-16 code unit. Every
conversion lives in `text.rs` and routes through
`codepoint_offset_to_character` / `character_to_codepoint_offset`.

### Diagnostics

`Engine::diagnostics` returns only the diagnostics belonging to the
requested document. LSP publishes per URI, so a diagnostic whose
`span.file` names an imported module cannot be reported on the importer:
its line and column are the _imported_ file's, and painting them onto the
open document draws the squiggle over unrelated code. The imported file
reports its own errors when the client opens it.

A diagnostic with no span is kept and anchored at the document start.
The loader reports its hard failures — `ModuleNotFound` above all —
without one, and the same failure returns `Semantics::empty`, silencing
every position query. Dropping the diagnostic left a file that looked
clean and answered nothing.

The Design-B semantic checks (effect / stores / default-purity / resource
moves) are derived from the snapshot lazily, on the first `diagnostics`
call, and cached on the `Snapshot`. Every other query reuses the same
snapshot without paying for them.

### URIs

Clients send rfc3986-encoded URIs, so `Uri::to_filename` percent-decodes
`file:` paths. Everything this crate derives from a URI — the
`FilesystemCompilerHost` base path, the kiln manifest walk-up, the
`span.file` match in `Engine::diagnostics` — depends on it: without
decoding, a workspace under `my project/` resolved nothing at all.
Non-`file:` schemes are compiler-minted and never encoded, so they pass
through verbatim.

The inverse holds on the way out: `location::filename_to_uri` percent-encodes
via `uri::percent_encode_path` before wrapping a path as `file://…`. The
paths it receives are decoded — from `to_filename`, or from the compiler,
which speaks in real paths — and a URI emitted without re-encoding does not
string-match the document the client already has open.

Relative imports are normalised lexically (`../lib/x.wado` off
`file:///a/b/foo.wado` is `file:///a/lib/x.wado`, not
`file:///a/b/../lib/x.wado`). Clients key open documents by URI string, so
the un-normalised form opened a duplicate tab for an already-open file.

### DiagnosticCollector

`DiagnosticCollector` in `lib.rs` wraps any `CompilerHost`, delegating `load_source` while silently collecting all emitted diagnostics. This avoids modifying or depending on a specific host implementation.

### Stdio server

`server::run_stdio` is the binary's only entrypoint and is also re-used by `wado-cli/src/lsp.rs`. It returns the process exit code (LSP 3.18 §exit) rather than calling `std::process::exit`: `dispatch` records the client's `exit` on `Lifecycle::exit_code` and the loop returns it, so `wado lsp` routes it through the CLI's own exit path and the loop stays testable. I/O is synchronous (`std::io`) so the crate builds for `wasm32-wasip2`, where tokio's `io-std` is unavailable; the dispatcher still awaits `Engine` futures between messages. Async is driven by `futures::executor::block_on` — the crate does not depend on tokio.

### Bundled stdlib content

Jump-to-definition into bundled stdlib modules emits `core:<name>` / `wasi:<interface>` URIs from `module_uri` (`src/location.rs`). The server advertises a `workspace.textDocumentContent` capability for the `core` and `wasi` schemes; clients call `workspace/textDocumentContent` to retrieve the source. The handler in `src/server/dispatch.rs` resolves it via `Engine::text_document_content` (`src/lib.rs`), which looks the URI up in `wado_compiler::stdlib::get_stdlib_module`. Both `core:cli` and the rfc3986-normalised form `core:/cli` are accepted.

The VS Code extension (`wado-vscode/src/extension.ts`) bridges this with a `TextDocumentContentProvider` registered for both schemes, and forces opened documents to language `wado` because opaque URIs (e.g. `core:cli`) carry no extension for VS Code's language detector.

## Related Files in wado-cli

| File                       | Role                                                                           |
| -------------------------- | ------------------------------------------------------------------------------ |
| `src/lsp.rs`               | `wado lsp` subcommand — delegates to `wado_lsp::server::run_stdio`             |
| `src/compiler_host.rs`     | CLI-side wrapper over `wado_lsp::FilesystemCompilerHost` (stderr + timestamps) |
| `src/query.rs`             | `wado query` subcommand: arg parsing, dispatch                                 |
| `src/query_adapter.rs`     | Engine invocation, text/JSON output formatting                                 |
| `tests/integration/lsp.rs` | Integration tests for both `wado lsp` and `wado query`                         |

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

## TODO: Jump-to-Definition Follow-ups

Known gaps and quality debts in `src/definition.rs` / `src/location.rs` not tied
to a specific LSP request kind. Each item identifies the symptom and the
concrete code location involved.

- [ ] **Jump-to-def for non-`Simple` `UseItem` variants.**
      `Elaborator::record_use_specifier_references` (`wado-compiler/src/elaborator.rs`)
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
