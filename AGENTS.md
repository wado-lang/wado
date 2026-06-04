# Wado Project

This document describes how to develop the Wado compiler toolchain.

Wado is a statically-typed programming language targeting Wasm/WASI.

Note: `CLAUDE.md` is a symlink to `AGENTS.md`.

## Development

This project uses [mise](https://mise.jdx.dev/) to manage dev tools. Project tasks are defined in `mise.toml`. Run `mise tasks` to discover available tasks.

Install mise first if you don't have it:

```sh
curl -fsSL https://mise.run | sh
```

### When Starting a Task

Run the following to set up your development environment:

```sh
mise trust                 # trust the mise.toml config (first time only)
mise run on-task-started   # install project tools
```

### When Completing a Task

When you are completing a task, use the `on-task-done` skill to finish it.

### Common Development Tasks

```sh
mise run test        # test Rust crates
mise run test-wado   # test Wado modules
mise run format      # format Rust files and Markdown files

mise run benchmark-all     # count-prime, mandelbrot, sieve, fts, zlib, and so on
mise run report-wasm-size  # hello_world, pi_approx, zlib, and so on
```

## General Rules

- Documentation and comments must be written in English.
- Avoid ad-hoc workarounds. Write proper code based on a sound design.
- Do not use comment sections to separate or organize code.
- Perform red/green TDD.
- A compiler bug is always P0 — no exceptions. Stop, write a minimal reproducible e2e fixture, and fix it before continuing if it blocks the current task.
- A pre-existing issue — whether you find it or a reviewer points it out — must be fixed, with TDD when practical.
- Don't pipe long-running commands (`mise run …`, `cargo test`, etc.) into `tail` or `head`. Redirect to a file and inspect it afterwards if you need to trim output.
- Use plain `cargo build` / `cargo run` / `cargo test` (the `dev` profile) for iteration. `Cargo.toml` raises `opt-level` on `wado-compiler`, `wado-dev-tools`, and `cranelift-codegen` so dev-build runtime is close to release for the parts that matter, while compile time stays much lower. `--release` is for distributing binaries, not for the inner dev loop.
- Use the `rust` skill when writing Rust.
- Use the `wado` skill when writing Wado code or designing Wado language features.

## The Wado Language

For the detailed specification, read `docs/spec.md`.

### Quick tour of the Wado language

Unlike Rust:

- No lifetimes. No borrow checker. No ownership. Just GC.
- Value semantics: every value is deeply copied when assigned or passed to a function, except for references and `builtin::array<T>` (an internal type, not user-facing).
- Wado splits Rust's `enum` into `variant` for sum types with payloads and `enum` for plain discriminants (no payload). Bitmask types use `flags`.
- No macros.
- No `unsafe`. No raw pointers.
- Semicolons are just separators. Functions that return values must use `return`.

Like Rust:

- Full generics and traits, but no dynamic dispatch (yet).
- Full pattern matching:
  - `match` statements and expressions.
  - `if let` and `while let`.

Wado-specific features:

- Effect system: effect signatures and handlers.
- Wasm CM builtins and direct WASI P3 bindings.
- `scrutinee matches { PATTERN }` operator, similar to Rust's `matches!` macro.
- `task return` for Wasm async functions.
- `assert` statements with power-assert-like diagnostics. Assertions cannot be disabled, so they are always reliable.
- ES-Modules-like import statements.
- Template string literals with Rust-like `{expr:specifier}` formatting.

## The Compiler

The compiler is implemented in `wado-compiler/`.

See also:

- `docs/compiler.md` for the compiler internals.
- `docs/optimizer.md` for the optimization passes.

### Bundled Library

The compiler bundles Wasm modules for language features:

- `wado-bundled-libm/` — deterministic math functions using the `libm` crate.

## The CLI

The CLI is implemented in `wado-cli/` as a subcommand-style CLI. The sections below describe each subcommand.

In the examples below, `wado` is shorthand for `cargo run --bin wado --`.

A Wado program targets a Wasm _world_: the CLI command (`wasi:cli/command`, the default), the HTTP service (`wasi:http/service`, run via `wado serve`), or the test world (`wasi:test`, used by E2E tests). Several defaults — including the allocator — depend on the target world.

### Compile Command

```sh
wado compile -o file.wasm file.wado    # generate Wasm
wado compile -o file.wat file.wado     # generate WAT
wado compile --wat-to-stdout file.wado # output WAT to stdout
```

To inspect invalid Wasm when debugging codegen bugs, use `--no-validate`:

```sh
# Skip validation and output raw Wasm bytes even if invalid
wado compile --no-validate --wat-to-stdout file.wado
```

Optimization levels: `-O0` (none), `-O1` (development), `-O2` (production, default), `-O3` (aggressive), `-Os` (`-O2` + strip symbols).

#### Allocators

Three allocators are available via `--allocator <mode>`:

- `bump` (default for CLI): Bump pointer; never frees. Fast, minimal code.
- `freelist` (default for HTTP world): Reclaims freed memory via a free list. For long-running processes.
- `debug` (default for test world): Never reuses freed memory; poisons freed memory with `0xFF`. For use-after-free detection.

```sh
wado compile --allocator bump file.wado      # bump allocator
wado compile --allocator freelist file.wado  # free-list allocator
wado compile --allocator debug file.wado     # debug allocator
```

`wado compile` selects the `debug` allocator automatically when targeting the test world; E2E tests rely on this.

### Run Command

```sh
wado run file.wado  # run a CLI program with wasmtime
```

### Serve Command

Use `wado serve` to run a Wado HTTP service (wasi:http/service world):

```sh
wado serve file.wado                        # serve on 0.0.0.0:8080 (default)
wado serve --addr 127.0.0.1:3000 file.wado  # serve on a custom address
```

### Dump Command

Use `wado dump` to inspect compiler internal state for debugging.
See `wado dump --help` for the full help.

```sh
wado dump file.wado                  # show final WIR (default)
wado dump --nir file.wado            # show final NIR (after optimization)
wado dump --nir -O0 file.wado        # show NIR without optimization
wado dump --ast file.wado            # show parsed AST
wado dump --modules file.wado        # show loaded modules
wado dump --symbols file.wado        # show symbol table
wado dump --types file.wado          # show type table
wado dump --tir-resolved file.wado       # show TIR after type resolution
wado dump --tir-monomorphized file.wado  # show TIR after monomorphization
wado dump --nir-lowered file.wado        # show NIR right after lowering (before optimize)
```

### The Formatter

The `wado format` command formats Wado source code.

`mise run format-wado` formats all the fixtures used by compiler tests.

**Caution:** `mise run format-wado` may break uncommitted test fixtures. When the syntax is updated, make sure to add tests to `wado-compiler/tests/format.rs`.

### Compilation Log and Timing

The compiler emits timestamped diagnostics to stderr. Use `--log-level` to control verbosity.

```sh
wado compile --log-level debug file.wado
```

## The Standard Library

`wasi:*` modules are part of the Wado standard library.

These modules are generated from WIT files by the `wado-from-idl` tool. To update any `wasi/*.wado` file, edit `wado-from-idl` and run:

```sh
mise run update-stdlib-wasi
```

This requires the `vendor/wasmtime` git submodule to be initialized.

## Package Manifest

`wado-manifest/` handles `wado.toml` parsing, validation, and `wado.lock` lock file management.

This crate must compile for `wasm32-unknown-unknown`. CI enforces this.

## Language Server Protocol

The LSP engine is implemented in `wado-lsp/`.

## VS Code Extension

The VS Code extension is implemented in `wado-vscode/`.

The syntax files are generated from `wado-compiler/src/syntax.rs` by:

```sh
mise run update-wado-vscode-grammar
```

Whenever the syntax changes, regenerate the grammar and update the formatter fixtures in `wado-compiler/tests/format.fixtures/`.

See `wado-vscode/README.md` for more details.

## Dependencies

The wasm-tools crates (`wasmparser`, `wasm-encoder`, `wasmprinter`, `wit-parser`, `wat`) are pinned in `[workspace.dependencies]` to the same generation wasmtime depends on, so cargo dedupes them instead of compiling parallel 0.x trees. `mise run check-deps` enforces this (also a CI job) and lists the irreducible exceptions.

When bumping wasmtime, re-align them:

1. Find wasmtime's generation, e.g. `cargo tree -i wasmparser@<ver>`.
2. Re-pin the wasm-tools crates in `Cargo.toml` to that generation (`wat = "~1.<gen>"`, the rest `"0.<gen>"`).
3. `cargo update`, then `mise run check-deps`.

## References

### Wasm and WASI

Wado targets the following Wasm features:

- Wasm 3.0 (2025-09-17), including GC
- Wasm Component Model (CM)
  - Design: `vendor/component-model/design/mvp/`
  - Canonical ABI: `vendor/component-model/design/mvp/CanonicalABI.md`
  - Concurrency (async, streams, futures): `vendor/component-model/design/mvp/Concurrency.md`
- WASI 0.3.0 (P3)
  - P3 is supported by wasmtime.
  - See wasmtime's P3 support: `find vendor/wasmtime/crates/wasi/src/p3/wit -name '*.wit'`
- Wasm Stack Switching

### Vendor Submodules

`vendor/` contains reference repositories: the specifications for Wasm and the Component Model, plus runtimes such as wasmtime.

To initialize:

```sh
git submodule update --init --recommend-shallow
```

### Wado Evolution Proposals (WEP)

Wado uses Wado Evolution Proposals (WEPs) to document significant language features and architecture decisions. See `docs/` for existing WEPs.
