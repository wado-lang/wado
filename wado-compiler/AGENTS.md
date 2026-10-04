# wado-compiler

The Wado compiler crate.

## Rules

- Nothing in this crate writes to a stream: `println!`, `eprintln!` and `dbg!`
  are denied at the crate root. A user-facing message goes through `Logger` and
  a developer trace through `compiler_trace!`.
- An `assert!` states an invariant the compiler establishes for itself. What a
  source file can violate is a `Diagnostic`: a panic on user input is a crash,
  whatever it asserts.
- A phase error words itself once, in its `Diagnostic`. It carries no `Display`:
  nothing in the crate can print one, and a second wording drifts from the one
  the user reads. `Display` is for an error the CLI prints itself
  (`CompileError`, `WitEmitError`), or one whose text a `Diagnostic` builder
  reads (`LexError`, `LoadError`).
- `src/codegen.rs` emits the `NirPackage` and `WirPackage` as they are; it
  knows nothing of the earlier phases.
- Only `src/name.rs` knows a name format. Mangling and monomorphization go
  through it.
- Every name the compiler mints for itself starts with one `$`
  (`name::INTERNAL_PREFIX`), which no Wado identifier can spell.
  `mise run check-internal-names` gates it.
- What makes such a name unique is a serial that advances on read
  (`FunctionContext::fresh_serial`), never a local index the site has yet to
  allocate. A step that reads one before recursing hands its own names to
  whatever nests inside it (issue #1987).
- A `TypeId` is a slot, not a type identity: newtype erasure and the boxing
  rewrite both leave many ids resolving to one type. Compare and key by the
  `TypeKey` that `TypeTable::type_key` answers, never by the id.
- A declaration is identified by its `DefId`, never by its name. See
  [WEP: Declaration Identity](../docs/wep-2026-08-12-declaration-identity.md).
- Walk IR through the visitor utilities, and answer a question with one resolver
  over the IR rather than partial walkers, which each miss a different shape.
- Optimize to the limit correctness allows, and nothing short of it. Wrong code
  is never a trade for speed. A conservatism is not caution but a defect:
  measure what it buys, and delete it when that is nothing.
- Iterate with `cargo check --all-targets`, so the unit tests compile too, and
  `cargo test -p wado-compiler --test e2e`. Locally e2e covers O0 and O2; its
  `ignored` lines are CI's share, so never set `WADO_FULL_TEST` unless asked.
- This crate must compile for `wasm32-unknown-unknown` (checked in CI). Keep
  OS-dependent `std` modules out of production code.

## Standard Libraries

`src/stdlib.rs` maps every import to its file under `lib/`. A dev build reads
them from disk, so editing one takes effect on the next `wado` run with no
rebuild. A release build embeds them, as does any `wasm32` build, which has no
filesystem. `lib/wasi/`, `lib/core/kiln/`, `lib/core/eval/` and
`lib/core/coverage/` are generated from WIT: read `wado-from-idl/AGENTS.md`
first.

The stdlib carries no inline hints (`#[inline(...)]`). A hint that makes code
faster marks a case the optimizer misses, so the fix belongs in the optimizer.

A module re-exports an effect only where it owns it (`core:cli`'s `Stdout`,
`core:fs`'s `Preopens`). One that merely performs an effect imports it
privately: `pub use` keeps the identity, so a re-export only adds a second path
to one type.

A module carries a `#[synopsis]` test, which `wado doc` renders as its
`## Synopsis`: the shortest program showing what the module is for, not a tour.
Its other tests live in `<module>_test.wado` beside the file that implements
them; that file reaches `internal` items, and a private item's test goes in the
module itself. `core:prelude` owes neither: the e2e fixtures hold it.

`mise run test-stdlib-coverage` holds the stdlib to
`scripts/stdlib-coverage.json`, the regions its tests leave unrun, and fails on
a difference either way. New code gets a test, or `#[coverage(off)]` where no
test can reach it. Remove what a new test covers with
`mise run update-stdlib-coverage-baseline`; never add to it.

`builtin::select` evaluates both operands and hands one back, so it is planned
as the merge it is: the copy keeping a composite result independent lands on the
result, as the equivalent `if` pays. Choose between them on eagerness, not on
copies. `src/optimize/select_lowering.rs` goes the other way, rewriting an `if`
into a branchless select only where both arms are duplicable pure leaves.

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
