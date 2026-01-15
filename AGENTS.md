# Wado Project

This is the specification and implementation of Wado, a programming language targeting Wasm/WASI.

## The Spec

Read @spec.md to understand the new language.

When updating spec.md, keep it mutually exclusive and collectively exhaustive (MECE).

## The Compiler

The compiler is implemented in `wado-compiler/` with a hand-written recursive descent parser.

Standard libraries are implemented in `wado-compiler/lib`, with `wasi/` for WASI and `core/` for the core library.

See also `docs/compiler.md` for the implementation details and the feature checklist.

There are E2E test fixtures in `wado-compiler/tests/fixtures/*.wado`.

### E2E Test Specification

E2E tests are `.wado` files in `wado-compiler/tests/fixtures/` with a `__DATA__` section containing JSON test expectations.

Each test fixture group has the same prefix in their filenames.

#### Data Section Schema

| Field             | Type       | Description                              |
| ----------------- | ---------- | ---------------------------------------- |
| `stdout`          | `string`   | Expected stdout (exact match)            |
| `stderr`          | `string`   | Expected stderr (exact match)            |
| `stdout_contains` | `string[]` | Strings that must appear in stdout       |
| `stderr_contains` | `string[]` | Strings that must appear in stderr       |
| `trapped`         | `bool`     | Whether the program should trap          |
| `compile_error`   | `string`   | Expected compile error (substring match) |

#### Examples

```wado
// Success test - expects specific output
fn run() {
    println("Hello");
}

__DATA__
{"stdout": "Hello\n"}
```

```wado
// Error test - expects compilation to fail
fn run() {
    let a = 1 != 2 != 3;
}

__DATA__
{"compile_error": "!= operator cannot be chained"}
```

### The `wasi:*` Modules

`wasi:*` modules are part of the Wado standard library.

Those modules are generated from WIT files by the `wado-from-wit` tool, so if `wasi/*.wado` files need to be updated, edit `wado-from-wit` instead, and run:

```sh
make update-stdlib-wasi
```

## The CLI

The CLI is implemented in `wado-cli/` with sub-command style CLI:

```sh
cargo run --bin wado -- compile -o file.wasm file.wado    # generates Wasm
cargo run --bin wado -- compile -o file.wat file.wado     # generates WAT
cargo run --bin wado -- compile --wat-to-stdout file.wado # outputs WAT to stdout
cargo run --bin wado -- run file.wado                     # run it directly using wasmtime
cargo run --bin wado -- dump file.wado                    # dump compiler internal state
```

### Dump Command

Use `wado dump` to inspect compiler internal state for debugging:

```sh
wado dump file.wado           # show all: modules, symbols, AST
wado dump --modules file.wado # show loaded modules only
wado dump --symbols file.wado # show symbol table only
wado dump --ast file.wado     # show AST only
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

This project relies on the following features:

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
- Do not commit changes unless the user requests so. When commit, no need to explain the implementation details.

## Rules for Rust

- Do not use wildcard imports (`use ...::*;`).
- Write tests in implementation files just for examples. For comprehensive tests, write them in the `tests/` directory.
- Manage dependencies in the workspace `Cargo.toml`.
- Avoid using well-known floating point number constants like PI, E, etc. in tests not to violate the Clippy `approx_constant` rule.
- Do not use `#![allow(deprecated)]`; use newer alternatives instead.
- Use `panic!("not yet implemented")` for things that are not yet implemented.
- Use string interpolation (`print!("foo: {foo}")`) - only variables are allowed inside the interpolation, though.

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

When you have completed a task, make sure everything is up-to-date and tested:

- `make on-task-done` for format, clippy-fix, update-bundled, test.
- Update spec.md if necessary.
- Update docs/compiler.md if necessary.
