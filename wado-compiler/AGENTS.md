# wado-compiler

The Wado compiler crate.

## Rules

- Nothing in this crate writes to a stream: `println!`, `eprintln!` and `dbg!`
  are denied at the crate root. A user-facing message goes through `Logger` and
  a developer trace through `compiler_trace!`. An `assert!` is for what only a
  broken stdlib can reach.
- `src/codegen.rs` emits the `Package` as is; it knows nothing of the earlier
  phases.
- Only `src/name.rs` knows a name format. Mangling and monomorphization go
  through it.
- Every name the compiler mints for itself starts with one `$`
  (`name::INTERNAL_PREFIX`), which no Wado identifier can spell.
  `mise run check-internal-names` gates it.
- What makes such a name unique is a serial that advances on read
  (`FunctionContext::fresh_serial`), never a local index the site has yet to
  allocate. A step that reads one before recursing hands its own names to
  whatever nests inside it (issue #1987).
- A declaration is identified by its `DefId`, never by its name. See
  [WEP: Declaration Identity](../docs/wep-2026-08-12-declaration-identity.md).
- Walk IR through the visitor utilities, and answer a question with one resolver
  over the IR rather than partial walkers, which each miss a different shape.
- Optimize to the limit correctness allows, and nothing short of it. Wrong code
  is never a trade for speed. A conservatism is not caution but a defect:
  measure what it buys, and delete it when that is nothing.
- `cargo check` while iterating. The e2e fixtures cover the language, so run
  `cargo test -p wado-compiler --test e2e` for anything the language touches. It
  runs O0 and O2, and the other levels only under `CI` or `WADO_FULL_TEST`.
  `mise run test` and `mise run test-wado` take an hour, so they belong at the
  end and not in the loop.
- This crate must compile for `wasm32-unknown-unknown` (checked in CI). Keep
  OS-dependent `std` modules out of production code.

## Standard Libraries

`src/stdlib.rs` maps every import to its file under `lib/`. A dev build reads
them from disk, so editing one takes effect on the next `wado` run with no
rebuild. A release build embeds them, as does any `wasm32` build, which has no
filesystem. `lib/wasi/` and `lib/core/kiln/` are generated from WIT, `lib/web/`
from a WebIDL snapshot: read `wado-from-idl/AGENTS.md` first.

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
