# wado-vscode

The VS Code extension, hosting `wado-lsp` as a bundled Wasm module. `README.md`
covers the setup, inner loop, tests, and settings.

## Rules

- The TextMate grammar and `language-configuration.json` are generated from
  `wado-compiler/src/syntax.rs`. Whenever the syntax changes, run
  `mise run update-wado-vscode-grammar`.
