# wado-compiler

The Wado compiler crate: frontend, IR pipeline, optimizer, and codegen. The NIR
optimizer has its own guide: [`docs/optimizer.md`](../docs/optimizer.md).

## Rules

- `codegen.rs` emits the `Package` as is; it knows nothing of the earlier phases.
- Name mangling and monomorphization go through `name.rs`. No other component knows a name format.
- A declaration is identified by its `DefId`, never by its name — see
  [WEP: Declaration Identity](../docs/wep-2026-08-12-declaration-identity.md).
- Walk IR through the visitor utilities, not by hand.
- Escalate the test scope as the work matures: `cargo check` while iterating,
  `mise run test` during development, `mise run test-wado` when wrapping up.
- This crate must compile for `wasm32-unknown-unknown` (a CI build check). Keep
  OS-dependent `std` modules out of production code.

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
- `wado dump --assert-plan file.wado` shows which operands an `assert` captures
  and which of them a short-circuit can skip.
- `builtin::black_box(value)` returns `value` opaquely, keeping a fixture's input
  off the constant-folding path.
