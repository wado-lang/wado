# Wado Project

This is the specification and implementation of Wado, a programming language targeting Wasm/WASI.

## The Spec

Read @spec.md to understand the new language.

When updating spec.md, keep it mutually exclusive and collectively exhaustive (MECE).

## The Compiler

The compiler is implemented in `wado-compiler/` with a hand-written recursive descent parser.

Standard libraries are implemented in `wado-compiler/lib`, where `wasi/` for WASI and `core/` for the core library.

See also `docs/compiler.md` for the implementation details and the feature checklist.

There are E2E test fixtures in `wado-compiler/tests/fixtures/*.wado`.

## The CLI

The CLI is implemented in `wado-cli/` with sub-command style CLI:

```sh
cargo run --bin wado -- compile -o file.wasm file.wado    # generates Wasm
cargo run --bin wado -- compile -o file.wat file.wado     # generates WAT
cargo run --bin wado -- compile --wat-to-stdout file.wado # outputs WAT to stdout
cargo run --bin wado -- run file.wado                     # run it directly using wasmtime
```

## Bundled Library

`wado-bundled/` is a Rust crate that provides bundled Wasm modules for Wado, providing:

- [x] float-to-string conversion (fts)
- [ ] math functions (libm)
- [ ] sort
- [ ] hash map

## Wasm and WASI

There are external references in the module for convenience:

- `vendor/wasm/` - WebAssembly/spec
- `vendor/wasi/` - WebAssembly/WASI
- `vendor/wasmtime/` - a Wasm runtime with WASI P3 support

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
- Avoid using well-known floating point number constants like PI, E, etc. in tests not to violate the Clippy `approx_constant` rule.

## Rules for Rust

- Do not use wildcard imports (`use ...::*;`).
- Write tests in implementation files just for examples. For comprehensive tests, write them in the `tests/` directory.
- Manage dependencies in the workspace `Cargo.toml`.
- Avoid using well-known floating point numbers in tests not to violate the Clippy `approx_constant` rule.

## Rules for Markdown

- Do not use `**...**` (bold) for sub-sections. Use markdown sections instead.
- Use markdown checklist for TODOs (`- [ ] ...`) and what's done (`- [x] ...`), instead of `~~...~~` (strike-through).

## Architecture Decision Records (ADR)

Significant architectural decisions are documented as ADRs in `docs/adr-{yyyy-mm-dd}-{feature}.md`.

Format: `docs/adr-YYYY-MM-DD-feature-name.md`

### Structure

- Title: Short description of the decision
- Status: Proposed | Accepted | Deprecated | Superseded
- Context: Background and problem statement
- Decision: What was decided and why
- Consequences: Impact and trade-offs

## Project Development

```sh
make build
make test
make clippy-fix

make hello    # generates example/hello.wat
make hello-run # simple smoke test

make format # format code and documents
```

See `Makefile` for all the development tasks.

## On Your Task Done

```sh
make on-task-done # format, clippy-fix, update-bundled, test
```
