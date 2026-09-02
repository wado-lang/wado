# wado-compiler

The Wado compiler crate: frontend, IR pipeline, optimizer, and codegen. The NIR
optimizer has its own guide: [`docs/optimizer.md`](../docs/optimizer.md).

## Rules

- `codegen.rs` emits the `Package` as is; it knows nothing of the earlier phases.
- Name mangling and monomorphization go through `name.rs`. No other component knows a name format.
- A declaration is identified by its `DefId`, never by its name — see
  [WEP: Declaration Identity](../docs/wep-2026-08-12-declaration-identity.md).
- Walk IR through the visitor utilities, not by hand, and answer a question
  once: one resolver total over the IR, rather than partial walkers, which
  multiply until each misses a different shape.
- Take a finding one altitude up: it names a line, so ask whether that shape
  occurs elsewhere in the IR before fixing there.
- Optimize as far as correctness allows. A conservatism is a claim about
  precision: measure what it buys before keeping it, and drop the ones that buy
  nothing.
- Escalate the test scope as the work matures: `cargo check` while iterating,
  `mise run test` during development, `mise run test-wado` when wrapping up.
- This crate must compile for `wasm32-unknown-unknown` (checked in CI). Keep
  OS-dependent `std` modules out of production code.

## Standard Libraries

`lib/core/` and `lib/wasi/` are embedded by `include_str!` in `src/stdlib.rs` —
editing one has no effect until the crate is rebuilt. `lib/wasi/` and
`lib/core/kiln/` are generated from WIT: read `wado-from-idl/AGENTS.md` first.

`src/stdlib.rs` names no `_test.wado`, so the white-box tests beside a stdlib
module are **not** embedded: `wado test lib/core/prelude/fpfmt_test.wado` picks
one up from disk and runs in seconds. Iterate on the test there, and spend the
rebuild only on the module it tests.

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
