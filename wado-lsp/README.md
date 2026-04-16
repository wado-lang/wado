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

## VS Code Integration

The VS Code extension embeds wado-lsp as a Wasm module. There is no subprocess mode — Wasm-only keeps the architecture simple and enables both desktop and web with a single path.

### Why Wasm-only

- **Zero-install**: Users install only the VS Code extension. No separate `wado` binary needed.
- **VS Code Web**: Works on github.dev and vscode.dev out of the box.
- **Sandbox & tutorials**: Enables rich browser-based playground and interactive tutorials without a server.
- **Single code path**: One integration to maintain, not two.

### How it works

```
VS Code Extension (TypeScript)
  │
  ├── wado_lsp.wasm          (Engine compiled to Wasm)
  │     ↑
  │     │  open_document(), diagnostics(), ...
  │     │
  ├── CompilerHost (TS)       Implements load_source via VS Code workspace API
  │
  └── VS Code Adapter (TS)    Converts Engine results → VS Code Diagnostics API
```

1. **Build**: `cargo build --target wasm32-unknown-unknown` + `wasm-bindgen` CLI to generate JS/TS bindings for `Engine`.
2. **CompilerHost in TypeScript**: Implement `load_source` using `vscode.workspace.fs` (desktop) or in-memory files (web/sandbox).
3. **VS Code adapter**: Convert `Diagnostic`, `Position`, `Range` to `vscode.Diagnostic`, `vscode.Position`, `vscode.Range`.

The `CompilerHost` trait is async, so the TypeScript side returns Promises naturally.

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
