# Wado Project

This document describes how to develop the Wado compiler toolchain.

Wado is a Rust-like programming language targeting Wasm/WASI.

## The Language

Read @docs/cheatsheet.md to understand Wado language.

If you need detailed specification, read the `docs/spec.md`.

## The Compiler

The compiler is implemented in `wado-compiler/`.

Standard libraries (stdlib) are implemented in `wado-compiler/lib`, with `wasi/` for WASI interface and `core/` for the core library.

See `docs/compiler.md` in order to develop the compiler. See `docs/optimizer.md` for optimization passes and future plans.

Builtin functions that are directly mapped to wasm instructions or external functions are implemented in `wado-compiler/lib/core/builtin.wado`.

Internal functions that are used to provide language features are implemented in `wado-compiler/lib/core/internal.wado`.

### Wasm Compatibility

`wado-compiler` must compile for `wasm32-unknown-unknown`. Do not use OS-dependent `std` modules in production code. CI enforces this with a wasm32 build check.

### E2E Test Specification (Compiler Tests)

E2E tests verify **compiler behavior** (codegen, error messages, optimization). They are `.wado` files in `wado-compiler/tests/fixtures/` with a `__DATA__` section containing JSON test expectations.

Each test fixture group has the same prefix in their filenames.

By default, only O0 and O2 run locally; O1/O3/Os require `CI=1` or `WADO_FULL_TEST=1`.

#### Data Section Schema

The target world is indicated by the top-level key in the JSON object:

- No world key → `wasi:cli/command` (default)
- `"test": {}` → test world (runs test block exports)
- `"wasi:http/service": {...}` → HTTP service world

| Field                 | Type                 | Description                                                 |
| --------------------- | -------------------- | ----------------------------------------------------------- |
| `"test"`              | `{}`                 | Run as test world (`wasi:test`), executing test exports     |
| `"wasi:http/service"` | `object`             | Run as HTTP service (see HTTP sub-fields below)             |
| `stdout`              | `string`             | Expected stdout (exact match)                               |
| `stderr`              | `string`             | Expected stderr (exact match)                               |
| `stdout_contains`     | `string[]`           | Strings that must appear in stdout                          |
| `stderr_contains`     | `string[]`           | Strings that must appear in stderr                          |
| `trapped`             | `bool`               | Whether the program should trap                             |
| `compile_error`       | `string`             | Expected compile error (substring match)                    |
| `TODO`                | `bool`               | Mark as TODO test - must fail until feature is implemented  |
| `skip_os`             | `bool`               | Skip this test under `-Os` (e.g. tests relying on names)    |
| `preopened_dirs`      | `[string, string][]` | Preopened directories `[host_path, guest_path]`             |
| `allocator`           | `string`             | Override allocator: `"bump"` (default) or `"debug"`         |
| `wir_expect:Ox`       | `string[]`           | Patterns that must appear in WIR at `-Ox` (substring match) |
| `wir_not_expect:Ox`   | `string[]`           | Patterns that must NOT appear in WIR at `-Ox`               |
| `outgoing_mocks`      | `object`             | Mock responses for outgoing HTTP requests (see below)       |

HTTP sub-fields (inside `"wasi:http/service": {...}`):

| Field             | Type                 | Description                                   |
| ----------------- | -------------------- | --------------------------------------------- |
| `request`         | `object`             | Injected HTTP request (defaults to `GET /`)   |
| `request.method`  | `string`             | HTTP method (default: `"GET"`)                |
| `request.path`    | `string`             | Request path (default: `"/"`)                 |
| `request.headers` | `[string, string][]` | Request headers                               |
| `request.body`    | `string`             | Request body as UTF-8                         |
| `status`          | `number`             | Expected HTTP status code                     |
| `body`            | `string`             | Expected response body (exact UTF-8 match)    |
| `body_contains`   | `string[]`           | Strings that must appear in the response body |
| `headers_contain` | `[string, string][]` | Response headers that must be present         |

Outgoing mock sub-fields (inside each entry of `"outgoing_mocks": {...}`):

Keys are URL patterns matched against the request URI (exact match on full URI or path).

| Field     | Type                 | Description                             |
| --------- | -------------------- | --------------------------------------- |
| `status`  | `number`             | HTTP status code (default: 200)         |
| `body`    | `string`             | Response body as UTF-8 (default: empty) |
| `headers` | `[string, string][]` | Response headers                        |

#### Examples

```wado
test "Hello, world!" {
    let x = 1;
    assert x == 1;
}

__DATA__
{"test": {}}
```

```wado
// WIR pattern test - verify optimization effects at a specific -Ox level
// Use `wado dump [-O0|-O2] file.wado` to discover WIR patterns
export fn run() {
    let a: Array<i32> = [10, 20, 30];
    assert a.len() == 3;
}

__DATA__
{
    "stdout": "",
    "wir_expect:O1": ["SequenceLiteralBuilder::push_literal("]
    "wir_expect:O2": ["array.new_fixed<i32>(10, 20, 30)"],
}
```

### Adding New Test Fixtures

After adding new `.wado` files to `wado-compiler/tests/fixtures/`, you must touch `wado-compiler/tests/e2e.rs` to trigger `datatest_mini` to rediscover test files:

```sh
touch wado-compiler/tests/e2e.rs
```

Without this, `cargo test` will not detect the new fixture because `datatest_mini` discovers files at compile time.

### Standard Library Tests (Library Logic)

Tests for **standard library logic** (e.g., `zlib_test.wado`, `string_test.wado`) live alongside implementations in `wado-compiler/lib/`. These are `.wado` files with `test` blocks, run with `wado test`.

### The `wasi:*` Modules

`wasi:*` modules are part of the Wado standard library.

Those modules are generated from WIT files by the `wado-from-wit` tool, so if `wasi/*.wado` files need to be updated, edit `wado-from-wit` instead, and run:

```sh
mise run update-stdlib-wasi
```

It requires a git submodule `vendor/wasmtime` to be initialized.

## The Package Manifest

`wado-manifest/` handles `wado.toml` parsing, validation, and `wado.lock` lock file management.

This crate must compile for `wasm32-unknown-unknown` (same constraint as `wado-compiler`). CI enforces this.

## The CLI

The CLI is implemented in `wado-cli/` with sub-command style CLI:

```sh
cargo run --bin wado -- compile -o file.wasm file.wado    # generates Wasm
cargo run --bin wado -- compile -o file.wat file.wado     # generates WAT
cargo run --bin wado -- compile --wat-to-stdout file.wado # outputs WAT to stdout
cargo run --bin wado -- run file.wado                     # run CLI program using wasmtime
cargo run --bin wado -- serve file.wado                   # serve HTTP service using wasmtime
```

To inspect invalid Wasm when debugging codegen bugs, use `--no-validate`:

```sh
# Skip validation and output raw Wasm bytes even if invalid
wado compile --no-validate --wat-to-stdout file.wado
```

### Debug Allocator

Use `--allocator debug` to enable the debug allocator, which never reuses freed memory and poisons freed regions with `0xFF` bytes for use-after-free detection:

```sh
wado compile --allocator debug file.wado
wado run --allocator debug file.wado      # not yet wired, use compile + wasmtime
```

The debug allocator is **automatically selected** when compiling for tests.

### Serve Command

Use `wado serve` to run a Wado HTTP service (wasi:http/service world):

```sh
wado serve file.wado                        # serve on 0.0.0.0:8080 (default)
wado serve --addr 127.0.0.1:3000 file.wado  # serve on custom address
```

### Dump Command

Use `wado dump` to inspect compiler internal state for debugging.
Output defaults to unparsed Wado source code; use `--inspect` for internal Debug format. See `wado dump --help` for the full help.

```sh
wado dump file.wado                  # show final WIR (default)
wado dump --tir file.wado            # show final TIR (after optimization)
wado dump --tir -O0 file.wado        # show TIR without optimization
wado dump --ast file.wado            # show parsed AST
wado dump --desugared file.wado      # show desugared AST
wado dump --modules file.wado        # show loaded modules
wado dump --symbols file.wado        # show symbol table
wado dump --types file.wado          # show type table
wado dump --tir-resolved file.wado   # show TIR after type resolution
wado dump --tir-monomorphized file.wado  # show TIR after monomorphization
wado dump --tir-lowered file.wado    # show TIR after lowering
```

Optimization levels: `-O0` (none), `-O1` (development), `-O2` (production, default), `-O3` (aggressive), `-Os` (`-O2` + strip symbols).

## The Formatter

There's `wado format` command to format Wado source code.

`mise run format-wado` formats all the fixtures for compiler tests.

CAUTION: `mise run format-wado` may break uncommitted test fixtures. So if the syntax is updated, make sure adding tests to `wado-compiler/tests/format.rs`.

## VS Code Extension

The VS Code extension is implemented in `wado-vscode/`.

Their syntax files are generated by:

```sh
mise run update-wado-vscode-grammar
```

which depends on `wado-compiler/src/syntax.rs`. If syntax is updated, keep it up-to-date.

Similarly, if syntax is updated, update the formatter fixtures in `wado-compiler/tests/format.fixtures/` (e.g. `all.dirty.wado`, `all.clean.wado`, `mess.dirty.wado`, `mess.clean.wado`).

See also `wado-vscode/README.md` for more details.

## Bundled Library

The compiler bundles Wasm modules for the language futures:

- `wado-bundled-libm/` - deterministic Math functions with `libm` crate

### Wasm and WASI

Wado is designed on the following Wasm features:

- Wasm 3.0 (2025-09-17)
- Wasm GC
- Wasm Component Model
  - CM spec: `vendor/component-model/design/mvp/`
  - Canonical built-ins: `vendor/component-model/design/mvp/CanonicalABI.md`
  - Concurrency (async, streams, futures): `vendor/component-model/design/mvp/Concurrency.md`
- Wasm Stack Switching (not yet implemented in wasmtime)
- WASI 0.3.0 (P3)
  - P3 is supported by wasmtime v41
  - See wasmtime P3 support: `find vendor/wasmtime/crates/wasi/src/p3/wit -name '*.wit'`

## General Rules

- Don't be anchored by existing implementations or conventions. Always design from first principles toward the optimal solution.
- All the documents and comments must be written in English.
- When referring to WAT, use folded style syntax.
- If you find a compiler bug, limitation, or awkward behavior, fix it. Such a problem must be treated as the highest priority.
- Use sub-agents only for research tasks (searching, reading, exploring). Never use sub-agents for editing files.
- `CLAUDE.md` is a symlink to `AGENTS.md`. Editing either one is sufficient.

## Rules for Rust

- Write tests in implementation files just for simple smoke tests. For comprehensive tests, write tests in the `tests/`.
- Manage dependencies in the workspace `Cargo.toml`.
- Do not use `#![allow(deprecated)]`; use newer alternatives instead.
- Use `panic!` for things that are not yet implemented or not supported.
- YAGNI. Do the simplest thing that could possibly work.
- Use `IndexMap` and `IndexSet` from the `indexmap` crate in order to ensure deterministic iteration order.
- Do not use any comment sections to separate or organize code. Use Rust's natural structure instead.
- Follow Test-Driven Development: write a failing test case first, then implement the concern.

### Rules for the Compiler Code Base

- The principle: `codegen.rs` emits the `Project` as is, which does not have the knowledge of the previous phases.
- Use utilities in `name.rs` to handle name mangling and monomorphization. Other components must not know the details of name formats.
- Do not parse mangled / formatted names even in `name.rs`. Use parsed objects instead.
- Minimize hard-coded logic for compiler builtins. Define builtin and internal functions in Wado source files in `lib/core/*.wado`.
- Minimize hard-coded logic for WASI. Use metadata extracted from `lib/wasi/*.wado`.

## Wado Evolution Proposals (WEP)

Wado has a set of document for significant language features and architecture decisions.

See [docs/WEP.md] for details and existing WEPs.

## Project Development

### Tool Management

This project uses [mise](https://mise.jdx.dev/) to manage dev tools. Project tasks are defined in `mise.toml`. Run `mise tasks ls` to list available tasks.

Install mise first if you don't have it:

```sh
curl -fsSL https://mise.run | sh
# Then add to your shell profile:
#   eval "$(~/.local/bin/mise activate bash)"  # for bash
#   eval "$(~/.local/bin/mise activate zsh)"   # for zsh
```

Then run `mise run on-task-started` to install all required development tools.

### Development Tasks

```sh
mise run test        # test Rust crates (included in on-task-done)
mise run test-wado   # test Wado modules (included in on-task-done)
mise run format      # format Rust files and Markdown files
mise run format-wado # format Wado source files

mise run benchmark-all # count-prime, mandelbrot, sieve, fts, and zlib
mise run report-wasm-size # hello_world, pi_approx, and zlib
```

## Compilation Log and Timing

The compiler emits timestamped diagnostics to stderr. Use `--log-level` to control verbosity.

```sh
wado compile --log-level debug file.wado # all messages including phase spans
```

## Development Workflow

### When Starting a Task

Run the following to set up your development environment:

```sh
mise trust                 # trust the mise.toml config (first time only)
mise run on-task-started   # install project tools
```

### When Completing a Task

When you have completed a task, make sure everything is up-to-date and tested:

- Update the docs if necessary:
  - docs/spec.md
  - docs/cheatsheet.md
  - docs/compiler.md
  - docs/optimizer.md
- Run `mise run on-task-done` to format, clippy-fix, update golden fixtures, regenerate stdlib docs, and test. It will take 15+ minutes.
  - Run in foreground and commit the results. CI's integrity check fails if generated files are uncommitted.
