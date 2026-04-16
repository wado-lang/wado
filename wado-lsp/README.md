# wado-lsp

Protocol-agnostic language service engine for the Wado programming language.

## Design Principles

- **Protocol-agnostic**: The engine does not know about LSP JSON-RPC, MCP, or any specific wire protocol. It exposes typed Rust methods (`diagnostics()`, `open_document()`, etc.) that protocol adapters call.
- **IO-free**: All file access goes through `wado_compiler::CompilerHost`. The crate performs no filesystem, network, or stdio operations directly. This makes it compatible with `wasm32-unknown-unknown` for browser-based tooling.
- **LSP-aligned types**: The data types (`Diagnostic`, `Range`, `Position`, `Severity`) follow LSP semantics (0-based positions, same severity levels) so that protocol adapters can convert with minimal friction.

## Architecture

```
wado-lsp (this crate)          wado-cli (consumers)
+---------------------+        +------------------+
|  Engine             |<-------| lsp.rs           |  wado lsp (stdio LSP server)
|    open_document()  |<-------| lsp_adapter.rs   |  JSON-RPC transport
|    update_document()|        +------------------+
|    close_document() |<-------| query.rs         |  wado query (one-shot CLI)
|    diagnostics()    |<-------| query_adapter.rs  |  output formatting
+---------------------+        +------------------+
         |                              future:
         v                      +------------------+
  wado-compiler                 | mcp.rs           |  wado mcp (MCP server)
  (CompilerHost)                | mcp_adapter.rs   |
                                +------------------+
```

The engine manages open document state and delegates compilation to `wado-compiler`. Protocol adapters in `wado-cli` handle wire formats and IO.

## Usage

```rust
use wado_lsp::Engine;

let mut engine = Engine::new();
engine.open_document("file:///path/to/file.wado", source);
let diagnostics = engine.diagnostics("file:///path/to/file.wado", &host).await;
```

## CLI Commands

```sh
wado lsp                              # Start LSP server over stdio
wado query diagnostics file.wado      # One-shot diagnostics (human-readable)
wado query diagnostics --json file.wado  # One-shot diagnostics (JSON)
```
