# Wado Language Support for VS Code

Language support for the [Wado programming language](https://github.com/wado-lang/wado).

## Features

- Syntax highlighting for `.wado` files
- Bracket matching and auto-closing
- Comment toggling (`Ctrl+/` for line, `Shift+Alt+A` for block)
- Code folding
- Auto-indentation
- Language Server Protocol integration (via bundled `wado-lsp` Wasm server)
  - Diagnostics
  - Hover, go-to-definition, find-references
  - Document highlight
  - Semantic tokens

## Installation (Local Development)

From the repository root:

```bash
mise run install-wado-vscode-dev  # Install extension via symlink
mise run clean-wado-vscode-dev    # Remove the symlink
```

Then restart VS Code (Cmd+Q, then reopen).

Changes to the grammar or configuration take effect after reloading VS Code (`Cmd+Shift+P` → "Developer: Reload Window").

## Development

### Prerequisites

- Node.js 18+
- npm

### Setup

```bash
cd wado-vscode
npm install
npm run compile
```

### Testing

From the repository root:

```bash
mise run test-wado-vscode  # Run all tests (unit + E2E)
```

Or from `wado-vscode/`:

```bash
npm run test:unit  # Run unit tests (no VS Code needed)
npm run test       # Run E2E tests (launches VS Code)
```

Press `F5` in VS Code to launch the Extension Development Host.

### Updating Syntax Files

The TextMate grammar (`syntaxes/wado.tmLanguage.json`) and language configuration (`language-configuration.json`) are generated from the Wado compiler's canonical syntax definitions.

To regenerate after language changes:

```bash
mise run update-wado-vscode-grammar
```

This ensures both files stay in sync with the compiler's keyword and syntax definitions.

### Maintenance Pipeline

The syntax highlighting pipeline has multiple layers of validation:

1. **Canonical Syntax Definition** (`wado-compiler/src/syntax.rs`)
   - Language-agnostic definition of keywords, operators, types, etc.
   - Consistency tests verify it matches the lexer's keyword recognition

2. **Grammar Generation** (`wado-cli/src/syntax.rs`)
   - Transforms canonical definitions into TextMate grammar and VS Code language config
   - Output validated against official JSON schemas

3. **JSON Schema Validation**
   - TextMate grammar validated against `tmlanguage.schema.json`
   - Language config validated against `language-configuration.schema.json`
   - To update local schema files: `mise run update-json-schema-files`

4. **Tokenization Tests** (`src/test/unit/tokenization.test.ts`)
   - Uses `vscode-textmate` to verify actual tokenization behavior
   - Tests keyword highlighting, comments, strings, `__DATA__` section handling

When adding new keywords or syntax:

1. Add to lexer (`wado-compiler/src/lexer.rs`) and token (`token.rs`)
2. Add to `SyntaxDefinition` (`wado-compiler/src/syntax.rs`)
3. Run `cargo test -p wado-compiler syntax` to verify consistency
4. Run `mise run update-wado-vscode-grammar` to regenerate
5. Run `mise run test-wado-vscode` to verify tokenization

### Packaging

```bash
npm run vscode:prepublish
npx vsce package
```

## Language Server

The extension ships the Wado language server (`wado-lsp`) compiled to WebAssembly. On activation it loads `out/wado_lsp.wasm` via `@vscode/wasm-wasi-lsp` and drives it as a standard LSP server over stdio. The same server implementation also powers `vscode.dev` and `github.dev` without a platform-specific binary.

### Settings

- `wado.serverPath` — Absolute path to a native `wado-lsp` binary. When set, the extension launches that binary as a subprocess instead of the bundled Wasm. Compiler developers set this to `${workspaceFolder}/target/debug/wado-lsp` for a fast inner loop (`eprintln!`, `gdb`).
- `wado.trace.server` — `off` / `messages` / `verbose`. Standard LSP trace verbosity.

### Commands

- `Wado: Restart Language Server` (default binding `Ctrl+Shift+F5` / `Cmd+Shift+F5` on `.wado` files). The extension also auto-restarts when `out/wado_lsp.wasm` changes on disk, so `mise run watch-wado-lsp-wasm` produces a hot-reload loop.

### Building the bundled Wasm

```bash
mise run build-wado-lsp-wasm   # produces out/wado_lsp.wasm
mise run watch-wado-lsp-wasm   # rebuild on source changes
```

The build targets `wasm32-wasip1` because `@vscode/wasm-wasi-lsp` currently hosts only preview 1 modules. Rationale: [LSP Architecture WEP](../docs/wep-2026-04-18-lsp-architecture.md).

## Roadmap

- [ ] Additional LSP features — completion, formatting, rename, code actions (tracked in `wado-lsp/CLAUDE.md`).
