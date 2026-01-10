# Wado Project

This is the specification and implementation of Wado, a programming language targeting Wasm/WASI.

## The Spec

Read @spec.md to understand the new language.

## The Compiler

The compiler is implemented in `wado-compiler/` with a hand-written recursive descent parser.

Standard libraries are implemented in `wado-compiler/lib`, whre `wasi/` for WASI and `core/` for the core library.

See also `docs/compiler.md` for the implementation details and the feature checklist.

## The CLI

The CLI is implemented in `wado-cli/` with sub-command style CLI:

```sh
wado compile -o file.wasm file.wado # generates Wasm
wado compile -o file.wat file.wado  # generates WAT
wado run file.wado
```

The CLI can run a Wado module directly using wasmtime as a library.

## Wasm and WASI

There are external references in the module for convenience:

- `vendor/wasm/` - WebAssembly/spec
- `vendor/wasi/` - WebAssembly/WASI
- `vendor/wasmtime/` - wasmtime, a Wasm runtime
- `vendor/wasm-tools/` - the backend of wasmtime, handling Wasm, WAT, and WIT format

### Wasm and WASI Features

This project relays on the following features:

- Wasm GC
- Wasm Reference Types
- Wasm Wide Arithmetic for i128 and u128
- Wasm Threads
- Wasm Stack Switching
- Wasm Component Model
- WASI
  - Current target: WASI 0.3 (P3) with native stream/future types
  - P3 (`0.3.0-rc-2025-09-16`) is supported by wasmtime v40 with `-W component-model-async=y`
  - See wasmtime P3 support: `find vendor/wasmtime/crates/wasi/src/p3/wit -name '*.wit'`

## General Rules

- All the documents and comments must be written in English.
- Everything is under discussion. We can change the spec at any time.
- When referring to WAT, use folded style syntax.

## Terminology

- Wasm: WebAssembly (not WASM)
- WASI: WebAssembly System Interface
- module: a Wado file
- project: a collection of modules
- Wado standard library: consists of the core library and the WASI library

## Rules for Rust Code

- Do not use wildcard imports (`use ...::*;`).
- Write tests in implementation files just for examples. For comprehensive tests, write them in the `tests/` directory.

## Rules for Markdown

- Format markdown files with `prettier`.

## Architecture Decision Records (ARD)

Significant architectural decisions are documented as ARDs in `docs/ard-{yyyy-mm-dd}-{feature}.md`.

**Format**: `docs/ard-YYYY-MM-DD-feature-name.md`

**Structure**:

- **Title**: Short description of the decision
- **Status**: Proposed | Accepted | Deprecated | Superseded
- **Context**: Background and problem statement
- **Decision**: What was decided and why
- **Consequences**: Impact and trade-offs

## Project Development

```sh
cargo build
cargo test
cargo clippy --fix --allow-dirty --allow-staged
cargo fmt

make hello # generates example/hello.wat
make hello-run # simple smoke test

make format # format code and documents
```
