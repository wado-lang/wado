# Wado Language Support for VS Code

Editor integration for the [Wado programming language](https://github.com/wado-lang/wado).

## Overview

This extension contributes the `wado` language to VS Code and hosts the Wado
language server (`wado-lsp`) as a bundled WebAssembly module. One build works
across desktop VS Code and the browser (`vscode.dev` / `github.dev`) without a
platform-specific binary. A native subprocess override is available for
compiler development.

Runtime shape:

- Grammar and language configuration are generated from
  `wado-compiler/src/syntax.rs` so highlighting stays in sync with the lexer.
- `src/extension.ts` loads `out/wado_lsp.wasm` via `@vscode/wasm-wasi-lsp` and
  speaks LSP over stdio through `vscode-languageclient`.
- `ms-vscode.wasm-wasi-core` is declared as an `extensionDependencies` entry
  and provides the Wasm host.

## Development

### Prerequisites

- Node.js 18+, npm
- `mise` (installs Rust toolchains + the `wasm32-wasip1` target)

### First-time setup

```bash
cd wado-vscode
npm install
npm run compile
mise run build-wado-lsp-wasm   # produces out/wado_lsp.wasm
```

### Install into your local VS Code

From the repository root:

```bash
mise run install-wado-vscode-dev   # symlink into ~/.vscode/extensions
mise run clean-wado-vscode-dev     # remove the symlink
```

Reload VS Code after install (`Cmd+Shift+P` → "Developer: Reload Window").

### Inner loop

```bash
mise run watch-wado-lsp-wasm       # rebuild the Wasm server on change
```

The extension watches `out/wado_lsp.wasm` and restarts the client
automatically (250 ms debounced). For raw `eprintln!` / debugger workflows,
point `wado.serverPath` at a native build
(`${workspaceFolder}/target/debug/wado-lsp`) and the extension launches that
as a subprocess instead.

Manual restart: `Wado: Restart Language Server`
(`Ctrl+Shift+F5` / `Cmd+Shift+F5`).

### Tests

```bash
mise run test-wado-vscode          # full suite (unit + E2E)
```

From `wado-vscode/`:

```bash
npm run test:unit                  # tokenization, no VS Code
npm run test                       # E2E: launches VS Code
xvfb-run -a npm test               # headless Linux
```

E2E coverage: Wasm server diagnostics, native subprocess diagnostics, and
cross-file import resolution through the `workspaceFolder` mount.

### Regenerating the grammar

```bash
mise run update-wado-vscode-grammar
```

This regenerates `syntaxes/wado.tmLanguage.json` and
`language-configuration.json` from `wado-compiler/src/syntax.rs` and
validates them against the TextMate / language-config JSON schemas. Run it
whenever the lexer keyword set changes.

### Packaging

```bash
npm run vscode:prepublish
npx vsce package
```

## Settings

- `wado.serverPath` — absolute path to a native `wado-lsp` binary. Empty
  (default) uses the bundled Wasm server.
- `wado.trace.server` — `off` | `messages` | `verbose`. Standard LSP trace
  verbosity forwarded to the language client.

## Implemented

Editor contributions:

- `.wado` file association, bracket matching, auto-closing, comment toggling,
  folding, auto-indent.
- Canonical TextMate grammar generated from the compiler's syntax definition.

Language server features (via `wado-lsp`):

- Diagnostics (push model, incremental on `didOpen` / `didChange`).
- Hover with resolved types and item signatures.
- Go-to-definition and find-references across imports.
- Document highlight (reads vs. writes).
- Semantic tokens (full document).

Infrastructure:

- Bundled `wasm32-wasip1` server, subprocess override via `wado.serverPath`.
- Serialized start/stop/restart lifecycle, graceful activation on failure,
  debounced auto-restart on Wasm changes.
- Cross-platform CI matrix (ubuntu / macos / windows) with `xvfb-run` on
  Linux.

## Roadmap

- Completion, formatting, rename, code actions.
- Inlay hints for inferred types.
- Workspace symbols and call hierarchy.
- Single-file mount for imports outside any workspace folder (tracked as a
  follow-up WEP).
- Preview 2 (component) hosting when `@vscode/wasm-wasi-lsp` gains support;
  see [LSP Architecture WEP](../docs/wep-2026-04-18-lsp-architecture.md).
