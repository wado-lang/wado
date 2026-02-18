# Wado Project

This is the specification and the toolchain of Wado, a programming language targeting Wasm/WASI.

## The Spec

Read @docs/cheatsheet.md to understand Wado syntax and standard library.

If you need detailed specification, read spec.md.

When updating spec.md, keep it mutually exclusive and collectively exhaustive (MECE).

## The Compiler

The compiler is implemented in `wado-compiler/`.

Standard libraries (a.k.a. stdlib) are implemented in `wado-compiler/lib`, with `wasi/` for WASI interface and `core/` for the core library.

See `docs/compiler.md` in order to develop the compiler.

Builtin functions that are directly mapped to wasm instructions or external functions are implemented in `wado-compiler/lib/core/builtin.wado`.

Internal functions that are used to provide language features are implemented in `wado-compiler/lib/core/internal.wado`.

### wasm32 Compatibility

`wado-compiler` must compile for `wasm32-unknown-unknown`. Do not use OS-dependent `std` modules in production code. CI enforces this via `cargo check -p wado-compiler --target wasm32-unknown-unknown`.

### E2E Test Specification

E2E tests are `.wado` files in `wado-compiler/tests/fixtures/` with a `__DATA__` section containing JSON test expectations.

Each test fixture group has the same prefix in their filenames.

#### Data Section Schema

| Field             | Type       | Description                                                |
| ----------------- | ---------- | ---------------------------------------------------------- |
| `stdout`          | `string`   | Expected stdout (exact match)                              |
| `stderr`          | `string`   | Expected stderr (exact match)                              |
| `stdout_contains` | `string[]` | Strings that must appear in stdout                         |
| `stderr_contains` | `string[]` | Strings that must appear in stderr                         |
| `trapped`         | `bool`     | Whether the program should trap                            |
| `compile_error`   | `string`   | Expected compile error (substring match)                   |
| `TODO`            | `bool`     | Mark as TODO test - must fail until feature is implemented |

#### Examples

```wado
// Success test
export fn run() {
    let x = 1;
    assert x == 1; // prefer `assert` for test expectations

    println("ok");
}

__DATA__
{"stdout": "ok\n"}
```

```wado
// Error test - expects compilation to fail
export fn run() {
    let a = 1 != 2 != 3;
}

__DATA__
{"compile_error": "!= operator cannot be chained"}
```

```wado
// TODO test - for unimplemented features
// Test runs but MUST fail (compile/runtime error or wrong output)
// If it passes, the test fails to remind you to remove TODO
export fn run() {
    let r: Result<i32, String> = Result::<i32, String>::Ok(42);
    if let Ok(value) = r {  // Result pattern matching not yet implemented
        println(`{value}`);
    }
}

__DATA__
{"TODO": true, "stdout": "value\n"}
```

### Adding New Test Fixtures

After adding new `.wado` files to `wado-compiler/tests/fixtures/`, you must touch `wado-compiler/tests/e2e.rs` to trigger `datatest_mini` to rediscover test files:

```sh
touch wado-compiler/tests/e2e.rs
```

Without this, `cargo test` will not detect the new fixture because `datatest_mini` discovers files at compile time.

### The `wasi:*` Modules

`wasi:*` modules are part of the Wado standard library.

Those modules are generated from WIT files by the `wado-from-wit` tool, so if `wasi/*.wado` files need to be updated, edit `wado-from-wit` instead, and run:

```sh
make update-stdlib-wasi
```

It requires a git submodule `vendor/wasmtime` to be initialized.

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
wado compile --no-validate -o output.wasm file.wado
# Combine with --wat-to-stdout to inspect the generated WAT
wado compile --no-validate --wat-to-stdout file.wado
```

### Serve Command

Use `wado serve` to run a Wado HTTP service (wasi:http/service world):

```sh
wado serve file.wado                 # serve on 0.0.0.0:8080 (default)
wado serve --addr 127.0.0.1:3000 file.wado  # serve on custom address
```

The source file must export an HTTP handler function:

```wado
use { Request, Response, ErrorCode } from "wasi:http";

export fn handle(request: Request) -> Result<Response, ErrorCode> {
    // Handle HTTP request and return response
}
```

### Dump Command

Use `wado dump` to inspect compiler internal state for debugging:

```sh
wado dump file.wado                  # show all phases (Debug format)
wado dump --ast file.wado            # show AST structure (Debug format)
wado dump --ast --unparse file.wado  # show AST as Wado source code
wado dump --desugar file.wado        # show desugared AST (Debug format)
wado dump --desugar --unparse file.wado  # show desugared AST as source
wado dump --modules file.wado        # show loaded modules only
wado dump --symbols file.wado        # show symbol table only
wado dump --tir file.wado            # show TIR (Typed IR)
wado dump --tir --unparse file.wado  # show TIR as pseudo-Wado source
wado dump --lower file.wado          # show lowered TIR
wado dump --lower --unparse file.wado  # show lowered TIR as pseudo-Wado source
wado dump --optimize file.wado       # show optimization hints
wado dump --optimize --unparse -O2 file.wado  # show optimized TIR as pseudo-Wado
```

Available phases (in compilation order):

1. `--tokens` - Lexer output
2. `--ast` - Parser output (supports `--unparse`)
3. `--desugar` - Desugared AST (supports `--unparse`)
4. `--modules` - Loaded modules
5. `--symbols` - Symbol table
6. `--tir` - Typed IR (supports `--unparse`)
7. `--lower` - Lowered TIR (supports `--unparse`)
8. `--optimize` - Optimized TIR (supports `--unparse`)
9. `--wir` - Wasm IR (supports `--unparse`)

Optimization levels for `--optimize` phase: `-O0` (none), `-O1` (development), `-O2` (production, default), `-O3` (aggressive), `-Os` (`-O2` + strip names).

### Golden Fixtures (Lowered TIR Tests)

Golden fixtures in `tests/fixtures.golden/*.lowered.wado` capture expected optimized TIR output. The `lowered` test suite (`tests/lowered.rs`) compares current compiler output against these golden files to detect unintended optimizer changes.

```sh
make update-golden-fixtures  # regenerate golden files after optimizer changes
cargo test -p wado-compiler --test lowered  # run golden file comparison tests
```

## The Formatter

There's `wado format` command to format Wado source code.

`make format-wado` formats all the fixtures for compiler tests.

CAUTION: `make format-wado` may break uncommitted test fixtures. So if the syntax is updated, make sure adding tests to `wado-compiler/tests/format.rs`.

## VS Code Extension

The VS Code extension is implemented in `wado-vscode/`.

Their syntax files are generated by:

```sh
make update-wado-vscode-grammar
```

which depends on `wado-compiler/src/syntax.rs`. If syntax is updated, keep it up-to-date.

See also `wado-vscode/README.md` for more details.

## Bundled Library

`wado-bundled/` is a Rust crate that provides bundled Wasm modules for Wado, providing:

- [x] float-to-string conversion (fts)
- [x] math functions (libm)

## Wasm and WASI

There are external references in the module for convenience:

- `vendor/wasm/` - WebAssembly/spec
- `vendor/wasi/` - WebAssembly/WASI
- `vendor/wasmtime/` - a Wasm runtime with WASI P3 support
- `vendor/wasm-tools/` - a Wasm toolchain, where the Wado compiler relies on.

### Wasm and WASI Features

Wado is designed with the following Wasm features:

- Wasm 3.0 (2025-09-17)
- Wasm GC
- Wasm Component Model
- Wasm Stack Switching (not yet implemented in wasmtime)
- WASI 0.3.0 (P3)
  - P3 is supported by wasmtime v41
  - See wasmtime P3 support: `find vendor/wasmtime/crates/wasi/src/p3/wit -name '*.wit'`

## General Rules

- All the documents and comments must be written in English.
- When referring to WAT, use folded style syntax.
- Do not commit changes unless the user requests so. When commit, no need to explain the implementation details.
- If you find a compiler bug, limitation, or awkward behavior, fix it. Such a problem must be treated as the highest priority.
- Use sub-agents only for research tasks (searching, reading, exploring). Never use sub-agents for editing files.
- `CLAUDE.md` is a symlink to `AGENTS.md`. Editing either one is sufficient.

## Rules for Rust

- Follow `clippy::pedantic` lint rules. The workspace is configured with pedantic lints enabled.
- Write tests in implementation files just for examples. For comprehensive tests, write them in the `tests/`.
- Manage dependencies in the workspace `Cargo.toml`.
- Do not use `#![allow(deprecated)]`; use newer alternatives instead.
- Use `panic!("not yet implemented")` for things that are not yet implemented.
- YAGNI. Do the simplest thing that could possibly work.
- Do not use `HashMap` or `HashSet` from `std::collections`. Use `IndexMap` and `IndexSet` from the `indexmap` crate instead, to ensure deterministic iteration order.

### Rules for the Compiler Code Base

- The principle: `codegen.rs` emits the `Project` as is, which does not have the knowledge of the previous phases.
- Use utilities in `name.rs` to handle name mangling and monomorphization. Other components must not know the details of name formats.
- Do not parse mangled / formatted names even in `name.rs`. Use parsed objects instead.
- Minimize hard-coded logic for compiler builtins. Define builtin and internal functions in Wado source files in `lib/core/*.wado`.
- Minimize hard-coded logic for WASI. Use metadata extracted from `lib/wasi/*.wado`.

## Rules for Markdown

- Do not use `**...**` (bold) for sub-sections. Use markdown sections instead.
- Use markdown checklist for TODOs (`- [ ] ...`) and what's done (`- [x] ...`), instead of `~~...~~` (strike-through) and emojis.

## Wado Evolution Proposals (WEP)

Significant language features and architectural decisions are documented as WEPs in `docs/wep-{yyyy-mm-dd}-{feature}.md`.

Format: `docs/wep-YYYY-MM-DD-feature-name.md`

WEPs combine language specification and implementation strategy in a single document, covering both user-visible features and compiler architecture decisions.

### List of WEPs

- [Target WASI P3 Only](./docs/wep-2026-01-11-wasi-p3-only.md)
- [Deterministic Math Library (libm) Integration](./docs/wep-2026-01-10-deterministic-libm.md)
- [Tagged Template Literals for Compile-Time Execution](./docs/wep-2026-01-10-tagged-template-literals.md)
- [WebAssembly Module Import Support](./docs/wep-2026-01-10-wasm-import.md)
- [Operator Precedence and Associativity](./docs/wep-2026-01-11-operator-precedence.md)
- [Ambient Logging Functions](./docs/wep-2026-01-12-ambient-logging.md)
- [Data Section (`__DATA__`)](./docs/wep-2026-01-12-data-section.md)
- [Literal Type Conversion Rules](./docs/wep-2026-01-12-literal-type-conversion.md)
- [Resource Lifecycle Management (RAII)](./docs/wep-2026-01-12-resource-lifecycle.md)
- [Value Semantics and Reference Stores](./docs/wep-2026-01-12-value-semantics-and-stores.md)
- [Struct and Trait System](./docs/wep-2026-01-13-struct-and-trait.md)
- [Compiler Pipeline Refactoring](./docs/wep-2026-01-14-compiler-pipeline-refactoring.md)
- [Tuple and Array Literal Syntax](./docs/wep-2026-01-15-tuple-and-array-literals.md)
- [World Conformance and Export Syntax](./docs/wep-2026-01-16-world-conformance-and-export.md)
- [Closure Implementation](./docs/wep-2026-01-16-closure-implementation.md)
- [Function Return Type Syntax](./docs/wep-2026-01-16-function-return-type-syntax.md)
- [CompilerHost Abstraction for Compiler I/O](./docs/wep-2026-01-16-source-provider-abstraction.md)
- [Type Stringification](./docs/wep-2026-01-16-type-stringification.md)
- [Template Format Specifiers](./docs/wep-2026-01-17-template-format-specifiers.md)
- [JSON Literal Compatibility](./docs/wep-2026-01-18-json-literal-compatibility.md)
- [JSON Module Import](./docs/wep-2026-01-18-json-module-import.md)
- [Operator Overloading](./docs/wep-2026-01-18-operator-overloading.md)
- [Iterator-Based Literal Coercion](./docs/wep-2026-01-18-iterator-based-literal-coercion.md)
- [Effect System and Randomness in Collections](./docs/wep-2026-01-20-effect-system-randomness.md)
- [Associated Types in Traits](./docs/wep-2026-01-20-associated-types.md)
- [Indexing Traits Design](./docs/wep-2026-01-20-indexing-traits.md)
- [String Template Desugaring](./docs/wep-2026-01-20-string-template-desugaring.md)
- [Compile-Time Location Literals](./docs/wep-2026-01-23-compile-time-location-literals.md)
- [Iterator Traits Design](./docs/wep-2026-01-24-iterator-traits.md)
- [Module Loader Design](./docs/wep-2026-01-24-module-loader.md)
- [Closure Parameter Monomorphization](./docs/wep-2026-01-25-closure-parameter-monomorphization.md)
- [128-bit Integer Types (i128/u128)](./docs/wep-2026-01-24-i128-u128-types.md)
- [Re-export Syntax (`pub use`)](./docs/wep-2026-01-25-pub-use-reexport.md)
- [Variant Payload Design](./docs/wep-2026-01-25-variant-payload-design.md)
- [Effect System Design](./docs/wep-2026-01-27-effect-system-design.md)
- [Global Variables](./docs/wep-2026-01-27-global-variables.md)
- [WIT and Wado Mapping](./docs/wep-2026-01-29-wit-wado-mapping.md)
- [Newtype Semantics](./docs/wep-2026-01-29-newtype-semantics.md)
- [SIMD v128 Types](./docs/wep-2026-01-31-simd-v128.md)
- [Format Traits](./docs/wep-2026-02-01-format-traits.md)
- [Wasm Plan Phase](./docs/wep-2026-02-03-wasm-plan-phase.md)
- [Trait Bounds Enforcement](./docs/wep-2026-02-07-trait-bounds.md)
- [Variant Wasm GC Representation](./docs/wep-2026-02-08-variant-representation.md)
- [Variant-Independent Types](./docs/wep-2026-02-09-variant-independent-types.md)
- [Compile-Time Tuple Enumeration](./docs/wep-2026-02-10-compile-time-tuple-enumeration.md)
- [Package Manifest (`wado.toml`)](./docs/wep-2026-02-14-package-manifest.md)
- [Wasm IR (WIR) Layer](./docs/wep-2026-02-14-wir-layer.md)
- [TIR-Level CM Adapter Synthesis](./docs/wep-2026-02-15-cm-adapter-synthesis.md)

### Structure

- Title: Short description of the proposal
- Context: Background and problem statement
- Decision: What was decided and why
- Consequences: Impact and trade-offs

## Project Development

### Tool Management

This project uses [mise](https://mise.jdx.dev/) for development tool version management.

Run `make on-task-started` to install mise and all required development tools automatically.

### Development Tasks

```sh
make test
make build
make format # format Rust files and markdown files
make format-wado # format Wado source files

make hello     # generates example/hello.wat and example/hello.wasm
make hello-run # simple smoke test

make benchmark-count-prime # use integer arithmetic
make benchmark-mandelbrot  # use float arithmetic
make benchmark-sieve       # use arrays

make report-wasm-size
```

## Compilation Log and Timing

The compiler emits timestamped diagnostics to stderr. Use `--log-level` to control verbosity.

### Log Levels

```sh
wado compile --log-level debug file.wado   # all messages including phase spans
wado compile --log-level info file.wado    # info, warnings, errors (default)
wado compile --log-level warn file.wado    # warnings and errors only
wado compile --log-level error file.wado   # errors only
wado compile --log-level off file.wado     # silent
```

`--log-level` is available on `compile`, `run`, and `serve` subcommands.

## Development Workflow

### When Starting a Task

Run the following to set up your development environment:

```sh
make on-task-started  # install mise and project tools
```

If this is your first time running mise in this repository, you may need to trust the configuration file:

```sh
mise trust
```

### Syncing Vendor Submodules

Run the following to sync all vendor submodules:

```sh
mise run sync-vendor
```

This syncs `vendor/wasmtime` to the exact version in `Cargo.lock` (required for WASI P3 compatibility), and updates other vendor submodules (`vendor/wasm`, `vendor/wasi`, `vendor/wasm-tools`) to their latest remote HEAD.

### When Completing a Task

When you have completed a task, make sure everything is up-to-date and tested:

- Update docs if necessary:
  - spec.md if the language specification is updated.
  - docs/compiler.md if the new features are implemented.
  - docs/cheatsheet.md if the syntax/stdlib is updated.
- Run `make on-task-done` to format, clippy-fix, update-bundled, update-golden-fixtures, and test.
