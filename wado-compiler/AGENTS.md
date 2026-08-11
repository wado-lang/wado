# wado-compiler

The Wado compiler crate: frontend, IR pipeline, optimizer, and codegen.

## Rules

- `codegen.rs` emits the `Package` as is; it knows nothing of the earlier phases.
- Name mangling and monomorphization go through `name.rs`. No other component knows a name format.
- Walk IR through the visitor utilities, not by hand.
- Escalate the test scope as the work matures: `cargo check` while iterating,
  `mise run test` during development, `mise run test-wado` when wrapping up, and
  the `on-task-done` skill only when finishing a task.

## NIR Optimize

Design, soundness invariants, and open work:
[`docs/wep-2026-06-05-nir-optimizer-architecture.md`](../docs/wep-2026-06-05-nir-optimizer-architecture.md).
What a contributor trips over:

- The value graph is built once per function (`Body::value_graph`) and
  maintained in place. Never clear-and-rebuild it, and never key a cache or
  side-table by `ExprId` — a pass needing a value reads it off the operand
  (born-as-operands) or takes a scratch walk (`Engine::scoped_const_reads`).
- A promoted read lives in the pool, not the skeleton, so a pass deciding a
  local is unused must count it (`arena_query`'s `promoted_*` queries), scoped
  to the reachable operands — the pool is append-only and still holds reads that
  folded away.
- Inside a session that census is `Engine::reads_promoted_local` /
  `promoted_read_count`, memoized: recomputing it per rule application is
  quadratic. The memo holds only because the edit API sees every operand
  written, so write operands through `Engine` (`map_operands` included), never
  through `engine.body`, and have a new mutating edit report itself
  (`census_note_operand` / `census_note_node_operands` /
  `census_note_structure`).
- niri's `scratch_folds` is the one surviving `ExprId` memo. The rewrite that
  commits an aggregate consumes the node that produced it, so an enclosing fold
  cannot re-derive the value from the tree — folding a string-building region to
  a literal depends on it.
- Do not narrow niri's trackability read-position whitelist to the reachable
  tree: two attempts each lost the string-builder folds. See
  `aggregate_safe_locals` and the two `still_vouches` tests in
  `tests/integration/niri.rs`.

## Standard Libraries

`lib/core/` and `lib/wasi/`, embedded into the binary by `include_str!` in
`src/stdlib.rs` — editing a `.wado` file there has no effect until the crate is
rebuilt. `lib/core/builtin.wado` holds the compiler intrinsics and
`lib/core/rt.wado` the runtime helpers (panic, assert, CM ABI glue).
`lib/wasi/` and `lib/core/kiln/` are generated from WIT: read
`wado-from-idl/AGENTS.md` first. `wado-bundled-libm/` is a prebuilt Wasm module
(`mise run update-bundled` rebuilds it).

Tests for stdlib logic live beside the implementation as `.wado` files with
`test` blocks (`zlib_test.wado`, …) and run under `wado test`.

## E2E Tests

`.wado` files in `tests/fixtures/`, one filename prefix per group, run at each
optimization level — O0 and O2 locally, O1/O3/Os under `WADO_FULL_TEST=1`. A
cross-module fixture puts the modules it loads in `tests/fixtures/sub`.

A fixture's expectations live in a trailing `__DATA__` JSON section. The fields
are the `serde` structs in `tests/e2e.rs` (`TestSpec`, `HttpServiceSpec`,
`HttpRequestSpec`) — read them there rather than from a copy that drifts. What
those structs do not say:

- No `__DATA__` at all means the test world, so a library-shaped source can
  double as a fixture verbatim (`cm_catalog.wado` is byte-identical to
  `package-cm-catalog/src/lib.wado`). With a `__DATA__`, the world comes from the
  top-level key: none → `wasi:cli/command`, `"test"` → test world,
  `"wasi:http/service"` → HTTP service.
- Each `preopened_dirs` entry gets a fresh temp directory, deleted afterwards,
  so filesystem tests stay hermetic across the parallel per-level runs. Its
  template is `""` for an empty scratch dir, or a workspace-relative path copied
  in as a seed corpus (`tests/fixtures/testdata`).
- An unmatched `tls_mocks` server name fails the handshake, so a test cannot
  silently reach the real network. The default empty map allows none.
- `wado dump [-O0|-O2] file.wado` is how you find the patterns for
  `wir_expect:Ox` / `wir_not_expect:Ox`.

`datatest_mini` resolves fixtures when the `harness!` macro in `tests/e2e.rs`
expands, and an incremental `cargo test` will not re-expand it on its own. Run
`touch tests/e2e.rs` after adding or removing a fixture, or after changing
`WADO_FULL_TEST` (the level gates read it at expansion time). CI builds from
scratch, so this is local-only; `cargo test -- --ignored` also runs the gated
levels without it.

## Wasm Compatibility

This crate must compile for `wasm32-unknown-unknown`, enforced by a CI build
check. Keep OS-dependent `std` modules out of production code.
