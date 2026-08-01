# wado-compiler

The Wado compiler crate: frontend, IR pipeline, optimizer, and codegen.

## Rules

- The principle: `codegen.rs` emits the `Package` as is, which does not have the knowledge of the previous phases.
- Use utilities in `name.rs` to handle name mangling and monomorphization. Other components must not know the details of name formats.
- If applicable, use visitor utilities instead of walking IR nodes by hand.

## NIR Optimize

The live value graph is the source of truth for pure values: built once per
function (`Body::value_graph`, set lazily on first query and reused across every
pass) and rewritten eagerly in place via e-class union (the aegraph model —
build once, rewrite, extract once), not re-derived per pass. Operand promotion
("born as operands") makes a pure skeleton position carry its `ValueId` directly
as `Operand::Value` in the pool (`body.values`), so a pass reads the value off
the operand instead of looking it up — there is no `ExprId`→value side-table.
BCE recognisers are structural.

Never reintroduce, regardless of perf:

- a rebuild of the value graph mid-pipeline. Build-once is structural: nothing
  clears `Body::value_graph`. Keep it that way — maintain the graph in place,
  never clear-and-rebuild.
- an `ExprId`-keyed cache / side-table. A pass needing a value uses
  born-as-operands or a scratch walk (`Engine::scoped_const_reads`).

The one `ExprId`-keyed memo that remains is niri's `scratch_folds`, holding what
the current frame folded each node to. It is not a cache to be scoped away: the
rewrite that commits an aggregate consumes the node that produced it, so the
value is no longer derivable from the tree and an enclosing fold reading through
that node needs the memo to continue. Folding a string-building region to a
literal depends on it. It is confined to the frame that wrote it and cleared
wherever the environment restarts.

Details: `docs/wep-2026-06-15-live-value-graph.md`.

## Development Cycle

Escalate the test scope as the work matures, so the fast feedback stays fast:

- `cargo check` — lightest. Just confirms the crate still compiles. Run it
  constantly while iterating.
- `mise run test` — the compiler dev-cycle test. Run it during development to
  verify the Rust crates (including the E2E fixtures) still pass.
- `mise run test-wado` — broader: exercises the Wado standard library and other
  `.wado` modules. Run it when wrapping up a dev cycle.
- `mise run on-task-done` (via the `on-task-done` skill) — the full finish pass
  (format, clippy-fix, golden fixtures, stdlib docs, the test suites). It takes a
  long time, so run it only when explicitly instructed to finish a task.

## Standard Libraries

Standard libraries (stdlib) are implemented in `lib`, with `lib/wasi/` for WASI interface and `lib/core/` for the core library.

The stdlib sources are embedded into the compiler binary at build time (`include_str!` in `src/stdlib.rs`), so editing a `.wado` file under `lib/` has no effect until the crate is rebuilt.

For other important files:

- `lib/core/builtin.wado` for compiler intrinsics.
- `lib/core/rt.wado` for runtime support helpers (panic, assert, CM ABI glue)

## E2E Test Specification (Compiler Tests)

E2E tests verify language features and compiler behaviors (codegen, error messages, optimization). They are `.wado` files in `tests/fixtures/` with a `__DATA__` section containing test specification in JSON.

Each test fixture group has the same prefix in their filenames.

E2E tests are run in each optimization level. By default, O0 and O2 are executed locally; O1/O3/Os require `WADO_FULL_TEST=1`.

### Data Section Test Spec

A fixture with no `__DATA__` section at all defaults to the test world (as if
`{"test": {}}`), so a library-shaped source — `export fn`s plus a `test` block —
can double as a fixture verbatim (see `cm_catalog.wado`, kept byte-identical to
`package-cm-catalog/src/lib.wado`). A fixture _with_ `__DATA__` selects its world
by the top-level key:

- No world key → `wasi:cli/command` (default)
- `"test": {}` → test world (runs test block exports)
- `"wasi:http/service": {...}` → HTTP service world

| Field                   | Type                 | Description                                                 |
| ----------------------- | -------------------- | ----------------------------------------------------------- |
| `"test"`                | `{}`                 | Run as test world (`wasi:test`), executing test exports     |
| `"wasi:http/service"`   | `object`             | Run as HTTP service (see HTTP sub-fields below)             |
| `stdout`                | `string`             | Expected stdout (exact match)                               |
| `stderr`                | `string`             | Expected stderr (exact match)                               |
| `stdout_contains`       | `string[]`           | Strings that must appear in stdout                          |
| `stderr_contains`       | `string[]`           | Strings that must appear in stderr                          |
| `warnings_contains`     | `string[]`           | Substrings each appearing in some compile-time warning      |
| `warnings_not_contains` | `string[]`           | Substrings appearing in no compile-time warning (FP guard)  |
| `trapped`               | `bool`               | Whether the program should trap                             |
| `exit_code`             | `number`             | Expected `wasi:cli/exit` status code (CLI world)            |
| `compile_error`         | `string`             | Expected compile error (substring match)                    |
| `skip_os`               | `bool`               | Skip this test under `-Os` (e.g. tests relying on names)    |
| `preopened_dirs`        | `[string, string][]` | Preopened dirs `[template, guest_path]` (see note below)    |
| `allocator`             | `string`             | Override allocator: `"bump"` (default) or `"debug"`         |
| `wir_expect:Ox`         | `string[]`           | Patterns that must appear in WIR at `-Ox` (substring match) |
| `wir_not_expect:Ox`     | `string[]`           | Patterns that must NOT appear in WIR at `-Ox`               |
| `outgoing_mocks`        | `object`             | Mock responses for outgoing HTTP requests (see below)       |
| `tls_mocks`             | `object`             | Mock responses for `wasi:tls` handshakes (see below)        |

Every `preopened_dirs` entry is backed by a fresh temp directory (deleted when
the test finishes), keeping filesystem tests hermetic across the parallel
per-optimization-level runs. The first element is a `template` seeding it:

- `""` — empty scratch directory.
- a workspace-relative path — copied in as a seed corpus (e.g. binary fixtures
  that cannot be expressed inline, such as `tests/fixtures/testdata`).

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
    let a: List<i32> = [10, 20, 30];
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
