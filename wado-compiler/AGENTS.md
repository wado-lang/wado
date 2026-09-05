# wado-compiler

The Wado compiler crate. The NIR optimizer has its own guide:
[`docs/optimizer.md`](../docs/optimizer.md).

## Rules

- `src/codegen.rs` emits the `Package` as is; it knows nothing of the earlier
  phases.
- Only `src/name.rs` knows a name format. Mangling and monomorphization go
  through it.
- A declaration is identified by its `DefId`, never by its name — see
  [WEP: Declaration Identity](../docs/wep-2026-08-12-declaration-identity.md).
- Walk IR through the visitor utilities, and answer a question with one resolver
  over the IR rather than partial walkers, which each miss a different shape.
- Optimize as far as correctness allows. A conservatism is a claim about
  precision: measure what it buys, and drop the ones that buy nothing.
- Escalate the test scope as the work matures: `cargo check` while iterating,
  `mise run test` during development, `mise run test-wado` when wrapping up.
- This crate must compile for `wasm32-unknown-unknown` (checked in CI). Keep
  OS-dependent `std` modules out of production code.

## Standard Libraries

`src/stdlib.rs` lists `lib/core/` and `lib/wasi/`. A dev build reads them from
disk, so editing one takes effect on the next `wado` run with no rebuild. A
release build embeds them, as does any `wasm32` build, which has no filesystem.
`lib/wasi/` and `lib/core/kiln/` are generated from WIT, `lib/web/` from a
WebIDL snapshot: read `wado-from-idl/AGENTS.md` first.

`builtin::select` returns one of its operands rather than a copy, so write the
`if` for `i128`, `u128` and any composite: `src/optimize/select_lowering.rs`
rewrites what qualifies and refuses the rest on that ground.

## E2E Tests

`.wado` files in `tests/fixtures/`, expectations in a trailing `__DATA__` JSON
section whose fields are the `serde` structs in `tests/e2e.rs`.

- Run `touch tests/e2e.rs` after adding or removing a fixture, or after changing
  `WADO_FULL_TEST`. `datatest_mini` resolves fixtures at macro-expansion time and
  an incremental `cargo test` will not re-expand on its own.
- `wado dump -Ox file.wado` is how you find `wir_expect:Ox` patterns.
- `wado dump --assert-plan file.wado` shows which operands an `assert` captures
  and which of them a short-circuit can skip.
- `builtin::black_box(value)` returns `value` opaquely, keeping a fixture's input
  off the constant-folding path.
