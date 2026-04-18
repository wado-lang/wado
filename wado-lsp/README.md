# wado-lsp

Language service engine for the Wado programming language.

The crate exposes an `Engine` that manages open documents and answers LSP-style queries (diagnostics, hover, go-to-definition, references, document highlight, semantic tokens). Types follow LSP wire conventions (0-based positions, standard severity levels) so adapters convert with minimal friction.

See [LSP Architecture](../docs/wep-2026-04-18-lsp-architecture.md) for the overall LSP direction — notably, the plan to host the stdio LSP server inside this crate and ship a `wado-lsp` binary that builds for both native and `wasm32-wasip2`, powering the VS Code extension and the browser playground from a single implementation.

## Architecture (current state)

```
wado-lsp                      wado-cli
+--------------------+        +------------------+
|  Engine            |<-------| lsp.rs           |  wado lsp (stdio LSP server)
|    open_document() |        | lsp_adapter.rs   |
|    update_document()|       | lsp_rpc.rs       |
|    close_document() |       +------------------+
|    diagnostics()    |
|    hover()          |
|    definition()     |
|    references()     |
|    document_highlight()
|    semantic_tokens()
+--------------------+
         |
         v
  wado-compiler
  (CompilerHost)
```

Per the WEP, the adapter files in `wado-cli/src/lsp*.rs` migrate into `wado-lsp/src/server/` and `wado-cli lsp` becomes a thin delegation. Until that migration lands, `wado-cli` remains the home of the stdio dispatcher.

## Usage

```rust
use wado_lsp::Engine;

let mut engine = Engine::new();
engine.open_document("file:///path/to/file.wado", source);
let diagnostics = engine.diagnostics("file:///path/to/file.wado", &host).await;
```

Each query takes a `&impl wado_compiler::CompilerHost` so the caller controls how imported modules are loaded (filesystem, in-memory, or VS Code workspace API).

## References

- [LSP Architecture WEP](../docs/wep-2026-04-18-lsp-architecture.md)
- [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
- [lsp.md](./lsp.md) — local copy of the LSP 3.18 specification
