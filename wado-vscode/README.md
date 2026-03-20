# Wado Language Support for VS Code

Language support for the [Wado programming language](https://github.com/wado-lang/wado).

## Features

- Syntax highlighting for `.wado` files
- Bracket matching and auto-closing
- Comment toggling (`Ctrl+/` for line, `Shift+Alt+A` for block)
- Code folding
- Auto-indentation

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

## Roadmap

- [ ] LSP integration (when wado-compiler supports LSP)
  - [ ] Diagnostics (errors/warnings)
  - [ ] Go to definition
  - [ ] Find references
  - [ ] Hover information
  - [ ] Code completion
  - [ ] Formatting
  - [ ] Linting
