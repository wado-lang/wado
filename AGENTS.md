This is the specification and implementation of **Wado**, a new programming language targeting Wasm/WASI.

## The Spec

Read @spec.md to understand the new language.

## The Compiler

The compiler is implemented in `wado-compiler/` with a hand-written recursive descent parser.

It generates a WAT file and we run it with `wasmtime`.

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
- Wasm Component Model
- WASI 0.3

## General Rules

* All the documents and comments must be written in English.
* Everything is under discussion. We can change the spec at any time.
* When referring to WAT, use folded style syntax.

## Terminology

* Wasm: WebAssembly (not WASM)
* WASI: WebAssembly System Interface

