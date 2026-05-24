# Overview

This is the wado-compiler crate.

## Rules

- The principle: `codegen.rs` emits the `Package` as is, which does not have the knowledge of the previous phases.
- Use utilities in `name.rs` to handle name mangling and monomorphization. Other components must not know the details of name formats.
- If applicable, use visitor utilities instead of walking IR nodes by hand.

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
| `tls_mocks`           | `object`             | Mock responses for `wasi:tls` handshakes (see below)        |

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

TLS mock sub-fields (inside each entry of `"tls_mocks": {...}`):

Keys are matched against the `server_name` argument the guest passes to `Connector::connect`. Unmatched server names fail the handshake with a clear error so tests cannot silently reach the real network. Empty `tls_mocks` is the default and behaves as "no server name allowed."

| Field   | Type     | Description                                                                |
| ------- | -------- | -------------------------------------------------------------------------- |
| `recv`  | `string` | Cleartext bytes delivered to the guest's `Connector::receive` stream       |
| `error` | `string` | If set, fails the handshake with this message (`Connector::connect → Err`) |

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

`datatest_mini` resolves everything when the `harness!` macro in `tests/e2e.rs`
expands at compile time, and an incremental `cargo test` will not re-expand it
unless that file changes. So run `touch tests/e2e.rs` (or `cargo clean`) whenever
the macro's compile-time inputs change:

- **After adding or removing `.wado` files under `tests/fixtures/`** — otherwise
  the new/deleted fixtures are not discovered.
- **When changing the env vars that gate optimization levels** — O1/O3/Os carry
  `ignore_unless_env = ["CI", "WADO_FULL_TEST"]`, evaluated at expansion time. To
  flip them between running and `#[ignore]` (e.g. set `WADO_FULL_TEST=1` to run
  them locally), touch the file so the new env value is read.

This only matters for local development — CI builds from scratch, so it always
sees the current fixtures and env. Locally you can also run the ignored levels
without touching or rebuilding via `cargo test -- --ignored`.

`tests/fixtures` requires data-section test spec, so if you test cross-module features, place the loaded modules in `tests/fixtures/sub`.

## Standard Library Tests (Library Logic)

Tests for standard library logic live alongside implementations in `lib/`. These are `.wado` files with `test` blocks (e.g., `zlib_test.wado`, `string_test.wado`) , run with `wado test`.

## Wasm Compatibility

This crate must compile for `wasm32-unknown-unknown`. Do not use OS-dependent `std` modules in production code. CI enforces this with a wasm32 build check.

## LSP-Friendly Compiler Architecture

Pipeline:

```
parse → bind → load → analyze → annotate → lower_tir → monomorphize → lower → optimize → codegen
```

- `annotate` is AST-preserving type resolution; returns `Annotated` (see `src/annotate.rs`). Used by both LSP and batch compilation. The desugar-replacement surface rewrites (`x += y`, `while`, C-style `for`, `assert`, `matches`, comparison chain) are TIR-direct: the resolver builds TIR from the parsed AST without producing synthetic AST nodes for them, so the parsed AST stays parser-shaped end to end. (`for x of expr` and `Self::method` / `T::method` static-call rewriting still synthesise AST under the hood; that is a pre-existing implementation detail and is on a separate cleanup track.)
- `lower_tir` emits TIR from `&Annotated`; used only by batch compilation.
- `(ModuleSource, AstId)` (`SymbolKey`) is the canonical identity for every semantic entity that originates in source. `AstId` is dense over `Block` / `Stmt` / `Expr` / `Pattern` / `Type` / `Item` / `Decl`, so every source position resolves to a key via `Module::ast_id_at`. Builtins live in `ModuleSource::Builtin` with their own dense ID range.
- AST is the source of truth: `annotate` attaches facts, never mutates or moves AST nodes. Decl-backed `ResolvedType` variants and `Symbol` both carry `defined_at: SymbolKey`.
- Use→def edges are recorded by the real resolver as it performs name resolution (`resolve_ident`, `resolve_call`, …). `Annotated::referenced_symbol` is the single source of truth; there is no separate lexical re-scan. `annotate_loaded` always drives `lower_tir` so the edges exist for both LSP and batch compilation.
- Structural facts that derive purely from the AST (per-`AstId` spans, declaration name spans, assignment write targets, position-to-`AstId` lookup) live on a per-module `AstIndex` (`src/ast_index.rs`), built once during `annotate_loaded`. Powers `Annotated::ast_id_at` / `span_of_key` / `name_span_of` / `is_write_target` without AST re-walks.
- LSP queries entry through `Annotated::cursor_at(module, line, col) -> Cursor`, which exposes `def_key` / `def_symbol` / `def_name_span` / `def_span` / `references_to_def` / `is_write_target` / `span` / `key` / `module`. Each LSP feature is a thin pass over those query methods.
- The `codegen.rs` principle still holds: codegen consumes `Package` without knowledge of earlier phases.

Entry points:

- `wado_compiler::annotate(source, host, filename) -> Annotated` — LSP path; skips `monomorphize` / `lower` / `optimize` / `codegen`.
- `wado_compiler::compile_with_options(...)` — batch path; calls `annotate_loaded` + `lower_tir` + `Package::new`, so registries build once.

`Engine::{definition, hover, diagnostics}` all go through `annotate` — cross-file navigation falls out for free because `Annotated` already contains every transitively-loaded module.

### Next

1. **Build out LSP features on the query API:** completion, rename, references, call-hierarchy. The infrastructure is in place; these are additive.
2. **(Deferred)** Salsa / demand-driven incrementalization, only if per-file reanalysis becomes a bottleneck. The architecture is designed to be wrappable in salsa queries with minimal restructuring; not planned.

Out of scope for this track: language changes, `Package` format changes, codegen output changes.
