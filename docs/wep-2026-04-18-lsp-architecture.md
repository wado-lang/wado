# LSP Architecture

## Context

`wado-vscode` currently provides only TextMate syntax highlighting. Wado needs a Language Server Protocol (LSP) integration so editors can surface diagnostics, hover, go-to-definition, references, and semantic tokens on `.wado` files. The same language server must also power the browser-based playground on the official Wado site.

### Personas with opposing needs

- Compiler developers (this repository): edit `wado-compiler`, want the local build reflected in the editor within seconds, want native debuggers (`gdb`, `eprintln!`) and short rebuild cycles.
- Application developers (marketplace): install the VS Code extension and immediately get diagnostics on `.wado` files, including in `vscode.dev` and `github.dev`, with no separate toolchain install.

### What exists today

- `wado-lsp` exposes an `Engine` with typed methods (`open_document`, `diagnostics`, `hover`, `definition`, `references`, `document_highlight`, `semantic_tokens`). It is I/O-free and compatible with `wasm32-unknown-unknown`.
- `wado-cli` hosts the stdio LSP adapter in `src/lsp.rs`, `src/lsp_adapter.rs`, `src/lsp_rpc.rs`, and a filesystem `CompilerHost` in `src/compiler_host.rs`. `wado lsp` runs the LSP server on the native process.
- `wado-vscode` registers syntax highlighting only; no LSP client.

The earlier `wado-lsp/README.md` proposed a Wasm-only, subprocess-less architecture with a handwritten C ABI bridge. That proposal is withdrawn by this WEP.

### Constraints

- Web support is mandatory. `vscode.dev`, `github.dev`, and the official Wado-site playground must all work. A native-subprocess-only architecture (rust-analyzer, gopls) cannot be the sole solution.
- JSON-RPC overhead is acceptable. It is the normal LSP wire format.
- Modern Wasm performance is adequate. Wasmtime and V8 typically reach ~80% of native; Wasm speed is not a reason to avoid it.
- `wado-cli` cannot be compiled to Wasm. It depends on `wasmtime` and `hyper-util`, which are host runtimes. Building `wado-cli` for `wasm32-wasip2` fails at `io-lifetimes` due to unstable-feature gating and is fundamentally blocked by `wasmtime` itself.

### Build feasibility verification

To establish feasibility before this WEP was written, the following was verified against the current tree (2026-04-18):

- `cargo build -p wado-lsp --target wasm32-wasip2` succeeds with zero source changes (cold: 61 seconds).
- A throw-away binary combining `wado-lsp` + `wado-compiler` + `tokio` (current-thread) + `serde_json` compiles to a `wasm32-wasip2` component, runs under `wasmtime`, reads `.wado` source from stdin, and writes LSP-shaped JSON diagnostics to stdout. The round-trip reports the expected type-mismatch diagnostic with correct zero-based LSP ranges.
- Release `.wasm` size: 12 MB raw, 2.7 MB gzipped.
- Incremental rebuild after touching `wado-compiler/src/lib.rs`: 43 seconds for `wasm32-wasip2` release, 60 seconds for native dev. The Wasm build is actually faster because native dev pays `opt-level = 2` on `cranelift-codegen`.

### Prerequisites to confirm at implementation time

- [ ] `@vscode/wasm-wasi-core` or `@vscode/wasm-component-model` can host a `wasm32-wasip2` component as an LSP server with bidirectional stdio. If neither supports preview 2 components cleanly, fall back to `wasm32-wasip1` (which `wado-lsp` also builds for without source changes).
- [ ] `vscode-languageclient` can be wired to a custom transport that bridges to the VS Code Wasm host.

### Considered alternatives

- Native subprocess only (rust-analyzer, gopls, clangd style). Rejected: no web support, and platform-specific VSIXes are a distribution burden.
- Wasm with handwritten C ABI bridge (the current `wado-lsp/README.md` proposal). Rejected: loses LSP-standard features (cancellation, progress, streaming) offered by `vscode-languageclient`; duplicates JSON marshalling; complicates the compiler-developer inner loop.
- Wasm-only as virtual subprocess, no native path. Rejected: compiler-developer inner loop becomes 40–60 seconds per iteration and loses `eprintln!` / `gdb` ergonomics.
- Keep stdio server in `wado-cli`, ship a separate `wado-lsp-server` crate for the Wasm build. Rejected: two implementations of the same server, inviting drift.

## Decision

### Single LSP server implementation, two build targets

`wado-lsp` owns the entire LSP server: the `Engine`, the stdio JSON-RPC transport, the request dispatcher, the LSP wire types, and a `FilesystemCompilerHost`. A new `[[bin]] wado-lsp` is added. The binary builds for both native and `wasm32-wasip2`:

- The native build powers the desktop CLI (`wado lsp` delegates to it) and the compiler-developer override (`wado.serverPath`).
- The `wasm32-wasip2` build is embedded in the VS Code extension VSIX and reused by the browser playground.

Both targets speak the same LSP JSON-RPC wire protocol over stdio.

### Crate layout

```
wado-lsp/
├── src/
│   ├── lib.rs                # Engine (existing)
│   ├── diagnostics.rs, ...   # (existing)
│   ├── host.rs               # FilesystemCompilerHost (moved from wado-cli)
│   └── server/
│       ├── mod.rs            # pub async fn run_stdio()
│       ├── transport.rs      # Content-Length header + JSON-RPC framing
│       ├── dispatch.rs       # LSP method -> Engine routing
│       └── rpc.rs            # LSP wire types
└── src/bin/
    └── wado-lsp.rs           # #[tokio::main(flavor = "current_thread")] run_stdio()
```

The "protocol-agnostic" framing in the current `wado-lsp/README.md` is removed. `wado-lsp` is the LSP package; its library surface exposes a typed `Engine` that other consumers may reuse, but its primary deliverable is the LSP server binary.

No Cargo features gate the server. All dependencies (`tokio`, `serde_json`) are unconditional. The `wado-compiler` dependency dominates both binary size and build time, so splitting the server behind a feature flag would not meaningfully shrink the `Engine`-only consumption profile.

### `wado-cli` is a thin client

- `wado-cli/src/lsp.rs` reduces to calling `wado_lsp::server::run_stdio().await`.
- `wado-cli/src/lsp_adapter.rs` and `wado-cli/src/lsp_rpc.rs` are deleted; their content moves to `wado-lsp/src/server/`.
- `wado-cli/src/compiler_host.rs` either re-exports `wado_lsp::FilesystemCompilerHost` or keeps only CLI-specific decorations (log-level filtering, phase timing) on top of it.

### `wado-cli mcp` and `wado-cli query` are out of scope

The MCP server and the one-shot diagnostics CLI stay inside `wado-cli`. Both are native-only tools; neither currently has a compelling Wasm use case. A future WEP may revisit their placement. If that happens, it should follow the same pattern as this WEP: move the server implementation into a dedicated crate that builds for both native and `wasm32-wasip2`, and keep `wado-cli` as a thin dispatcher.

### VS Code extension

`wado-vscode` gains an LSP client built on the official `vscode-languageclient` package. The client's `ServerOptions` callback has two branches:

1. If `wado.serverPath` is set in settings, spawn that executable as a native subprocess with stdio piped to `LanguageClient`.
2. Otherwise, launch the bundled `wado-lsp.wasm` via `@vscode/wasm-wasi-core` (or `@vscode/wasm-component-model`, depending on prerequisite validation). The Wasm host exposes the process as stdio streams; `LanguageClient` drives them identically to the native case.

Settings contributed by the extension:

- `wado.serverPath` (string, default unset) — absolute path to a native `wado-lsp` binary. Compiler developers set this to `${workspaceFolder}/target/debug/wado-lsp`.
- `wado.trace.server` (standard LSP trace setting) — trace verbosity.

Commands:

- `wado.restartLanguageServer` — calls `client.restart()`. Bound to a default keybinding.

A `vscode.workspace.createFileSystemWatcher` on the bundled `wado-lsp.wasm` path triggers `client.restart()` automatically when the file changes, so a rebuild on disk reloads the running LSP with no user action.

### Compiler-developer inner loop

Two complementary paths share the same server implementation:

- Native override (primary). Set `wado.serverPath` to a local build. `cargo watch -x 'build --bin wado-lsp'` rebuilds in a few seconds on edit; `wado.restartLanguageServer` or an auto-restart hook reloads the LSP. `eprintln!` appears in the "Wado Language Server" output channel; `gdb`/`lldb` can attach to the subprocess.
- Wasm hot-reload (secondary). `mise run watch-wado-lsp-wasm` rebuilds and copies `wado_lsp.wasm` into `wado-vscode/out/` on change. The `FileSystemWatcher` described above triggers `client.restart()` automatically. No keystrokes. Incremental rebuild takes 40–60 seconds.

Both workflows use the exact same server implementation, so observed behavior does not diverge between paths.

### Playground compatibility

This WEP does not design the browser playground integration. It requires only that the produced `wado-lsp.wasm` remain compatible with a generic browser-worker environment: no VS Code-specific imports leak into the Wasm module. A follow-up WEP will design the JS/TS host and transport (likely `MessagePort`-based JSON-RPC) for the public site.

### Distribution and sizing

- VSIX is a single cross-platform artifact containing `wado-lsp.wasm`. No per-platform VSIX is needed.
- CI enforces a gzip size ceiling on the Wasm artifact (initial target: 5 MB). Current measurement: 2.7 MB gzip.
- The extension is published to both the Microsoft Marketplace and Open VSX as `wado-lang.wado`.

## Consequences

### Benefits

- One LSP implementation, not two. The same Rust code serves desktop (native), web (Wasm-in-VS-Code), and playground (Wasm-in-browser). No FFI duplication and no wire-protocol divergence.
- Zero-install for end users. A single ~2.7 MB gzipped Wasm works on Windows, macOS, Linux, `vscode.dev`, and `github.dev` without platform-specific VSIXes.
- Fast inner loop for compiler developers. Native override gives sub-second rebuilds and full debugger access; Wasm hot-reload gives 40–60-second cycles with zero manual intervention.
- Standard tooling. `vscode-languageclient` handles cancellation, progress, streaming, and future LSP features; no handwritten JSON-RPC on the TypeScript side.

### Trade-offs

- `wado-lsp` is no longer a pure library. It ships a binary and depends unconditionally on `tokio` and `serde_json`. Consumers of the `Engine` API pick up these dependencies even if they only want the library.
- The VS Code extension requires a Rust build step (`cargo build -p wado-lsp --target wasm32-wasip2 --release`) before packaging. A `mise` task orchestrates this; CI runs it on every PR.
- Compiler-developer native override requires a `target/debug/wado-lsp` build on disk. The mise-managed workflow already builds all binaries, but documentation must make this explicit.
- `wasm32-wasip2` is added to `rust-toolchain.toml`. The first `rustup` sync after update pulls an extra standard library.

### Breaking changes

- `wado-lsp/README.md` is rewritten. The "Wasm-only, no subprocess" line is withdrawn.
- `wado-cli/src/lsp_adapter.rs` and `wado-cli/src/lsp_rpc.rs` are removed. No external consumer is known.

### Rollout

- [ ] Validate `@vscode/wasm-wasi-core` or `@vscode/wasm-component-model` can host `wasm32-wasip2` components as stdio LSP servers. If not, pivot to `wasm32-wasip1`.
- [ ] Add `wasm32-wasip2` to `rust-toolchain.toml` targets.
- [ ] Move the stdio LSP server from `wado-cli` to `wado-lsp`. Add `[[bin]] wado-lsp`. Delete `wado-cli/src/lsp_adapter.rs` and `wado-cli/src/lsp_rpc.rs`. Reduce `wado-cli/src/lsp.rs` to a delegation call.
- [ ] Move `FilesystemCompilerHost` to `wado-lsp`. Wire `wado-cli` to re-export or wrap it.
- [ ] Add `mise` tasks `build-wado-lsp-wasm` and `watch-wado-lsp-wasm`.
- [ ] Add a CI job that runs `cargo build -p wado-lsp --target wasm32-wasip2 --release` and asserts gzip size ≤ 5 MB.
- [ ] Implement the VS Code LSP client in `wado-vscode` with both subprocess and Wasm paths, the `wado.serverPath` setting, the `wado.restartLanguageServer` command, and the `FileSystemWatcher` auto-restart.
- [ ] Add an end-to-end test that drives the Wasm build through an `initialize` / `didOpen` / `publishDiagnostics` exchange.
- [ ] Rewrite `wado-lsp/README.md` to reflect the new architecture.

## See Also

- [`wep-2026-01-16-source-provider-abstraction.md`](./wep-2026-01-16-source-provider-abstraction.md) — `CompilerHost` abstraction that makes the compiler I/O-free and Wasm-buildable.
- [`wep-2026-01-11-wasi-p3-only.md`](./wep-2026-01-11-wasi-p3-only.md) — Wado's position on WASI versions; the LSP server targets p2 for VS Code hosting reasons, which is independent of the runtime targets used by compiled Wado programs.
- [Language Server Protocol specification](https://microsoft.github.io/language-server-protocol/) — wire protocol for the transport implemented in `wado-lsp/src/server/`.
