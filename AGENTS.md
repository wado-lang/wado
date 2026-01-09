# Wado Project

This is the specification and implementation of **Wado**, a new programming language targeting Wasm/WASI.

## The Spec

Read @spec.md to understand the new language.

## The Compiler

The compiler is implemented in `wado-compiler/` with a hand-written recursive descent parser.

Standard libraries are implemented in `wado-compiler/lib`, whre `wasi/` for WASI and `core/` for the core library.

## The CLI

The CLI is implemented in `wado-cli/` with sub-command style CLI:

```sh
wado compile -o file.wasm file.wado # generates Wasm
wado compile -o file.wat file.wado  # generates WAT
wado run file.wado
```

## Wasm and WASI

Because this language is only targeting Wasm with WASI, this project has git submodules for wasi and wasm.

Wasm: `wasm/`
WASI: `wasi/`

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
  - See wasmtime P3 support: `find ../../bytecodealliance/wasmtime/crates/wasi/src/p3/wit -name '*.wit'`

## General Rules

* All the documents and comments must be written in English.
* Everything is under discussion. We can change the spec at any time.
* When referring to WAT, use folded style syntax.

## Terminology

* Wasm: WebAssembly (not WASM)
* WASI: WebAssembly System Interface
* module: a Wado file
* project: a collection of modules
* Wado standard library: consists of the core library and the WASI library

## Rules for Rust Code

* Do not use wildcard imports (`use ...::*;`).
* Write tests in implementation files just for examples. For complete tests, write them in the `tests/` directory.

## Rules for Markdown

* Format markdown files with `prettier`.

## Project Development

```sh
cargo build
cargo test
cargo fmt    # formatting
cargo clippy # linting

make hello-run # simple smoke test
```

