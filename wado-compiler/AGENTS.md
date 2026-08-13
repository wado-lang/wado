# wado-compiler

The Wado compiler crate: frontend, IR pipeline, optimizer, and codegen.

## Rules

- `codegen.rs` emits the `Package` as is; it knows nothing of the earlier phases.
- Name mangling and monomorphization go through `name.rs`. No other component knows a name format.
- Walk IR through the visitor utilities, not by hand.
- Escalate the test scope as the work matures: `cargo check` while iterating,
  `mise run test` during development, `mise run test-wado` when wrapping up, and
  the `on-task-done` skill only when finishing a task.
- This crate must compile for `wasm32-unknown-unknown` (a CI build check). Keep
  OS-dependent `std` modules out of production code.

## NIR Optimize

Design and soundness invariants:
[`docs/wep-2026-06-05-nir-optimizer-architecture.md`](../docs/wep-2026-06-05-nir-optimizer-architecture.md).

- Never clear-and-rebuild the value graph, and never key a cache by `ExprId`.
  Key a memo by what the work consumed.
- Write operands through `Engine`, never through `engine.body`, and have a new
  mutating edit report itself (`census_note_*`). The promoted-read census is
  memoized and holds only on that.
- Do not narrow niri's trackability read-position whitelist to the reachable
  tree: two attempts each lost the string-builder folds.

## Standard Libraries

`lib/core/` and `lib/wasi/` are embedded by `include_str!` in `src/stdlib.rs` —
editing one has no effect until the crate is rebuilt. `lib/wasi/` and
`lib/core/kiln/` are generated from WIT: read `wado-from-idl/AGENTS.md` first.

## E2E Tests

`.wado` files in `tests/fixtures/`, expectations in a trailing `__DATA__` JSON
section whose fields are the `serde` structs in `tests/e2e.rs`.

- Run `touch tests/e2e.rs` after adding or removing a fixture, or after changing
  `WADO_FULL_TEST`. `datatest_mini` resolves fixtures at macro-expansion time and
  an incremental `cargo test` will not re-expand on its own.
- `wado dump [-O0|-O2] file.wado` is how you find `wir_expect:Ox` patterns.
