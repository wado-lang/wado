# wado-lsp

The language service engine and the stdio LSP server. Scope and build targets
are in [WEP: LSP Architecture](../docs/wep-2026-04-18-lsp-architecture.md); how
each part works is in its module doc. Open work is in [TODO.md](./TODO.md), and
the LSP specification at
<https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/>.
[lsp.md](./lsp.md) is its source, whose message definitions are unexpanded
`{% include %}`s.

## Rules

- The crate compiles for `wasm32-unknown-unknown` (CI-enforced), so the browser
  playground runs it without a WASI host. Runtime I/O is the host's:
  `wado-cli` on the desktop, an in-memory host in the browser.
- Every AST walk goes through `wado_compiler::ast::AstVisitor` and its `walk_*`
  functions. A hand-written traversal silently skips whatever a later AST node
  adds.
- A query degrades to no answer on a partial `Semantics`, never fails: the
  frontend always produces one, and batch compilation alone checks
  `Semantics::is_complete()`.
- A `CompilerHost` wrapper delegates every capability. A trait default in its
  place drops the wrapped host's dependency index (`DiagnosticCollector`).
- A `file:` URI is percent-decoded on the way in (`Uri::to_filename`) and
  re-encoded on the way out (`location::filename_to_uri`). Clients key
  documents by URI string, so a path that skips either names a different
  document.
