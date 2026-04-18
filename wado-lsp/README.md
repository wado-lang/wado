# wado-lsp

Language server and language service engine for the Wado programming language.

This crate ships two deliverables:

- An `Engine` library that manages open documents and answers LSP-style queries (diagnostics, hover, go-to-definition, references, document highlight, semantic tokens). Results follow LSP wire conventions (0-based positions, standard severity levels).
- A `wado-lsp` binary that speaks the LSP over stdio (Content-Length framed JSON-RPC 2.0). The same source builds for native hosts and for `wasm32-wasip2`, so one implementation serves the desktop CLI (`wado lsp`), the VS Code extension, and the forthcoming browser playground.

See [LSP Architecture](../docs/wep-2026-04-18-lsp-architecture.md) for the rationale behind the dual-target layout and the `wado-cli` ↔ `wado-lsp` split.

## Architecture

```
stdio (Content-Length + JSON-RPC 2.0)
               |
    +----------v-----------+
    |  src/bin/wado-lsp.rs |   native & wasm32-wasip2
    +----------+-----------+
               |
    +----------v-----------+
    |      server.rs       |   blocking std::io loop;
    |  transport/dispatch  |   sync framing, async dispatch
    |        /rpc          |
    +----------+-----------+
               |
    +----------v-----------+
    |       Engine         |   open_document / diagnostics /
    |      (lib.rs)        |   hover / definition / ...
    +----------+-----------+
               |
    +----------v-----------+
    |    CompilerHost      |   FilesystemCompilerHost, or
    |    (wado-compiler)   |   any caller-provided impl
    +----------------------+
```

`wado-cli/src/lsp.rs` is a thin delegator that calls `wado_lsp::server::run_stdio().await`. `wado-cli/src/compiler_host.rs` wraps `FilesystemCompilerHost` with CLI-only decorations (timestamps, stderr printing, log-level filtering).

## Library usage

```rust
use std::path::PathBuf;
use wado_lsp::{Engine, FilesystemCompilerHost};

let mut engine = Engine::new();
engine.open_document("file:///path/to/file.wado", source);

let host = FilesystemCompilerHost::new(PathBuf::from("/path/to"));
let diagnostics = engine.diagnostics("file:///path/to/file.wado", &host).await;
```

Each query takes a `&impl wado_compiler::CompilerHost` so the caller controls how imported modules are loaded (filesystem, in-memory, or a VS Code workspace API).

## Building

```sh
cargo build -p wado-lsp                                   # native
cargo build -p wado-lsp --target wasm32-wasip2 --release  # VS Code / playground
```

`mise run build-wado-lsp-wasm` builds the release Wasm component and copies it to `wado-vscode/out/wado_lsp.wasm`. `mise run watch-wado-lsp-wasm` keeps that output in sync via `cargo-watch`.

## References

- [LSP Architecture WEP](../docs/wep-2026-04-18-lsp-architecture.md)
- [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
- [lsp.md](./lsp.md) — local copy of the LSP 3.18 specification
