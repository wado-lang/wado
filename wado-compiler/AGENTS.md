# Overview

This is the wado-compiler crate.

## Rules

- The principle: `codegen.rs` emits the `Package` as is, which does not have the knowledge of the previous phases.
- Use utilities in `name.rs` to handle name mangling and monomorphization. Other components must not know the details of name formats.

## Standard Libraries

Standard libraries (stdlib) are implemented in `lib`, with `lib/wasi/` for WASI interface and `lib/core/` for the core library.

For other important files:

- `lib/core/builtin.wado` for compiler intrinsics.
- `lib/core/internal.wado` for utilities to implement language features

## E2E Test Specification (Compiler Tests)

E2E tests verify language features and compiler behaviors (codegen, error messages, optimization). They are `.wado` files in `tests/fixtures/` with a `__DATA__` section containing test specification in JSON.

Each test fixture group has the same prefix in their filenames.

E2E tests are run in each optimization level. By default, O0 and O2 are executed locally; O1/O3/Os require `WADO_FULL_TEST=1`.

### Data Section Test Spec

The target world is indicated by the top-level key in the JSON:

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
| `body`            | `string`             | Expected response body (exact match)          |
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

### Adding Test Fixtures

After adding new `.wado` files to `tests/fixtures/`, you must touch `tests/e2e.rs` to trigger `datatest_mini` to rediscover test files.

Without this, `cargo test` will not detect the new fixture because `datatest_mini` discovers files at compile time.

`tests/fixtures` requires data-section test spec, so if you test cross-module features, place the loaded modules in `tests/fixtures/sub`.

## Standard Library Tests (Library Logic)

Tests for standard library logic live alongside implementations in `lib/`. These are `.wado` files with `test` blocks (e.g., `zlib_test.wado`, `string_test.wado`) , run with `wado test`.

## Wasm Compatibility

This crate must compile for `wasm32-unknown-unknown`. Do not use OS-dependent `std` modules in production code. CI enforces this with a wasm32 build check.

## Refactoring Plan: LSP-Friendly Compiler Architecture

### Motivation

The current pipeline (`parse → bind → desugar → load → analyze → resolve → TIR → monomorphize → lower → optimize → codegen`) treats **resolve as AST → TIR lowering**. This is fine for batch compilation but hostile to LSP:

- TIR is a transformed tree, losing 1:1 correspondence with source AST. `position → type` / `position → symbol` queries have no direct path.
- Symbols lack source-file/URI info, so cross-file navigation cannot be assembled.

### Design Context

Coding agents write most of the code, so keystroke-level incremental analysis is low priority. However humans read code extensively, so full navigation and comprehension features (hover, go-to-definition, find-references, semantic tokens) are essential. Occasional human editing means some completion support is desirable but not critical.

This means a simple **"recompute on file change, cache per-document"** model is sufficient: parse + bind + resolve runs once per `didChange`/`didSave` (typically tens of milliseconds for a single file), and results are cached until the next change. No demand-driven incremental framework (salsa) is needed at this scale.

### Target Architecture (remaining work)

- Introduce stable `AstId` (module-local, parse-stable) and `AstPtr` (position-resolvable).
- Rework `SymbolTable` / `TypeTable` to be keyed by `AstId` rather than consumed during TIR construction.
- Split `resolve` into: (a) _annotate_ AST with `SymbolTable`/`TypeTable` (no lowering), (b) _lower_ to TIR as a later phase.
- Expose a query API: `position → AstId`, `AstId → Symbol`, `AstId → ResolvedType`, `Symbol → defining AstId + source URI`.
- Add a **lightweight analysis entry point** (parse + bind + resolve, no monomorphize/lower/codegen) for LSP use.
- Add source URI to `Symbol` so cross-file definition results can be returned.
- LSP caches analysis results per document and invalidates on change.

### Future: Incremental Analysis (salsa)

If the project grows to hundreds of files and per-file reanalysis becomes a bottleneck, the architecture above is designed to be wrappable in salsa queries with minimal restructuring. This is not planned for the foreseeable future.

### Principles

- **AST is the source of truth.** Semantic info is attached, not substituted.
- **Every semantic entity points back to source.** `Symbol`, `ResolvedType`, TIR nodes carry `AstId` (and transitively `Span` + URI).
- **The `codegen.rs` principle still holds.** Codegen consumes TIR without knowledge of earlier phases; what changes is how and when TIR is produced.

### Scope Boundaries

- This plan does **not** change the language, `Package` format, or codegen output.
- Batch compilation (`wado compile`) continues to work by driving the query chain end-to-end.
