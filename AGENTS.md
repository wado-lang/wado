# Wado Project

This document describes how to develop the Wado compiler toolchain.

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

mise run benchmark-all     # runs all benchmarks and reports the results
mise run report-wasm-size  # measures the size of the generated Wasm files and reports the results
```

## Tooling

- Never `pgrep` to check whether a job is alive — it matches the watcher's own command line, so the loop never exits. Have the job record its own completion: `cmd > run.log 2>&1; echo $? > run.done`.
- Always redirect output to a file and read the file. Filtering a live command (`| tail`, `| grep`) discards everything you did not anticipate, and a filter that misses costs a full re-run — tens of minutes.
- Run long jobs (`mise run test`, `test-wado`, `update-golden-fixtures`, `on-task-done`) through the harness's background mechanism, not `nohup ... &`, so completion is notified. Never foreground `sleep` to wait.

## General Rules

- Write all documentation and comments in English, and keep them concise — cut filler and low-information words.
  - Comments: don't write them. Make them unnecessary through clear structure, naming, and function decomposition that make intent obvious.
  - Docs: keep them concise. Don't document implementation details.
- Avoid ad-hoc workarounds. Write proper code based on a sound design.
- Perform red/green TDD.
- A compiler bug is always P0 — no exceptions. The instant you suspect one, stop all other work, and as the top priority write a minimal reproducible e2e fixture and fix it. A workaround that lets the current task proceed is never a reason to skip or defer any of these.
- A pre-existing issue — whether you find it or a reviewer points it out — must be fixed, with TDD when practical.
- Use plain `cargo build` / `cargo run` / `cargo test` (the `dev` profile) for iteration. `Cargo.toml` raises `opt-level` on `wado-compiler`, `wado-dev-tools`, and deps so dev-build runtime is close to release for the parts that matter. `--release` is only for distributing binaries, not for the inner dev loop.
- Cloud sessions run with `CARGO_INCREMENTAL=0`: `target/debug/incremental` does not fit the session's fixed disk allowance. Every rebuild is a full recompile of the touched crates, so scope the inner loop with `cargo check` and `-p <crate>` instead of relying on incremental relinks. Never set `CARGO_INCREMENTAL=1` in a task or script.
- Use the `rust` skill when writing Rust.
- Use the `wado` skill when writing Wado code or designing Wado language features.

## The Wado Language

For the detailed specification, read `docs/spec.md`.

### Quick tour of the Wado language

Wado is a statically-typed programming language targeting Wasm/WASI, strongly affected by Rust and TypeScript.

Like Rust:

- Full types, generics and traits.
  - String, List (Rust's Vec), TreeMap (Rust's IndexMap), and primitives types.
  - traits: Display, Inspect (Rust's Debug), Eq (no PartialEq), Ord.
  - No dynamic dispatch (yet)
- Full pattern matching:
  - `match` statements and expressions.
  - `if let` and `while let`.

Unlike Rust:

- No lifetimes. No borrow checker. Just GC.
  - Partial ownership is only implemented for Wasm CM resources.
- Value semantics: every value is deeply copied when assigned or passed to a function, except for references.
  - And thus no Copy or Clone traits.
- Wado splits Rust's `enum` into `variant` for sum types with payloads and `enum` for plain discriminants (no payload). Bitmask types use `flags`.
- No macros.
- No user-defined attributes.
- No `unsafe` and no raw pointers.
- Semicolons are just separators. Functions that return values must use `return`.

Like TypeScript & JavaScript:

- ES modules like module system: `use { to_string} from "core:json"`.
- Template string literals with backtick + `${expr}` interpolation, but with Rust-like formatting specifiers: `${expr:specifier}`.

Wado-specific features:

- Effect system: effect signatures and handlers.
- Wasm CM builtins and direct WASI P3 bindings.
- `scrutinee matches { PATTERN }` operator, similar to Rust's `matches!` macro.
- `task return` for Wasm async functions.
- `assert` statements with power-assert-like diagnostics. Assertions cannot be disabled, so they are always reliable.
- Literal spread `..base`: JS-leaning (anonymous composition, key-value merge, last-wins); a named struct allows only a single leading `..base`.

## Repository Map

- `wado-compiler/` — the compiler: frontend, IR pipeline, optimizer, codegen. The Wado standard library (`core:*`, `wasi:*`) lives in `wado-compiler/lib/`. Internals: `docs/compiler.md`, `docs/optimizer.md`.
- `wado-cli/` — the `wado` binary (see below).
- `wado-lsp/` — the language service engine, also compiled to Wasm for the browser.
- `wado-vscode/` — the VS Code extension.
- `wado-from-idl/` — generates the `wasi:*` and `core:kiln` stdlib modules from WIT.
- `wado-manifest/` — `wado.toml` / `wado.lock` parsing, validation, and dependency resolution.
- `wado-bundled-libm/` — deterministic math, bundled into the compiler as a Wasm module. (`wado-bundled-icu/` is a not-yet-wired spike.)
- `docs/` — the language spec (`docs/spec.md`), stdlib docs, and the Wado Evolution Proposals (`docs/wep-*.md`) recording significant language and architecture decisions.
- `benchmark/`, `wasm-size/` — performance and code-size measurement.
- `package-*/` — packages written in Wado; `package-gale/` is the ANTLR4 port.
- `vendor/` — reference specs and runtimes, as git submodules (see References).

## The CLI

The `wado` binary is implemented in `wado-cli/`. Below, `wado` is shorthand for `cargo run --bin wado --`. `wado --help` lists every subcommand and `wado <command> --help` its flags; the `wado-cli` skill covers the workflows.

The ones you reach for while developing the toolchain:

- `compile` — compile one source file to Wasm or WAT. `-O0` (none) … `-O3` (aggressive), `-Os` (`-O2` + strip symbols); default `-O2`.
- `check` — verify a source file (and its Kiln generators) without emitting Wasm.
- `run` — compile and run a CLI program with wasmtime.
- `test` — run the `test` blocks in Wado source files.
- `serve` — compile and serve an HTTP service.
- `dump` — dump compiler internal state at every stage: AST, modules, symbols, types, TIR, NIR, WIR.
- `query` — ask the language service for hover / definition / references / diagnostics, by position or by `MODULE#SYMBOL` notation.
- `format` — format Wado source code.

The rest (`init`, `update`, `fetch`, `build`, `publish`, `doc`, `wit`, `syntax`, `lsp`, `clean`) serve packaging, registry, and editor integration.

Behaviour that no `--help` will remind you of:

- A program targets a Wasm _world_: `wasi:cli/command` (default), `wasi:http/service`, or the synthetic `test` world. `--world test` exports the entry module's `test` blocks and drops everything else; `serve` and `test` pick their world automatically.
- The world selects the allocator: `bump` for CLI (never frees), `freelist` for HTTP (long-running), `debug` for the test world (never reuses freed memory, poisons it with `0xFF`). E2E tests rely on the test world picking `debug`.
- `wado run` reaches only the directories granted to it: the current one, or exactly the `--dir` grants once any is given. Paths open relative to a grant, so an absolute path never opens.
- `mise run format-wado` formats every compiler test fixture and may break uncommitted ones. When the syntax changes, add tests to `wado-compiler/tests/format.rs`.

## Dependencies

The wasm-tools crates (`wasmparser`, `wasm-encoder`, `wasmprinter`, `wit-parser`, `wat`) are pinned in `[workspace.dependencies]` to the same generation wasmtime depends on, so cargo dedupes them instead of compiling parallel 0.x trees. `mise run check-deps` enforces this (also a CI job) and lists the irreducible exceptions.

When bumping wasmtime, re-align them:

1. Find wasmtime's generation, e.g. `cargo tree -i wasmparser@<ver>`.
2. Re-pin the wasm-tools crates in `Cargo.toml` to that generation (`wat = "~1.<gen>"`, the rest `"0.<gen>"`).
3. `cargo update`, then `mise run check-deps`.

## References

### Wasm and WASI

Wado targets the following Wasm features:

- Wasm 3.0 (released on 2025-09-17), including GC and JSPI
- Wasm Component Model (CM)
  - Design: `vendor/component-model/design/mvp/`
  - Canonical ABI: `vendor/component-model/design/mvp/CanonicalABI.md`
  - Concurrency (async, streams, futures): `vendor/component-model/design/mvp/Concurrency.md`
- WASI 0.3 (or p3, released on 2026-06-11)
  - Fully supported by wasmtime.
  - See wasmtime's P3 support: `find vendor/wasmtime/crates/wasi/src/p3/wit -name '*.wit'`

### Vendor Submodules

`vendor/` contains reference repositories: the specifications for Wasm and the Component Model, plus runtimes such as wasmtime.

To initialize:

```sh
git submodule update --init --recommend-shallow
```
