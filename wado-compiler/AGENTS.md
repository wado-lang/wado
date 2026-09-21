# wado-compiler

The Wado compiler crate.

## Rules

- Nothing in this crate writes to a stream: `println!`, `eprintln!` and `dbg!`
  are denied at the crate root. A user-facing message goes through `Logger` and
  a developer trace through `compiler_trace!`. An `assert!` is for what only a
  broken stdlib can reach.
- A phase error words itself once, in its `Diagnostic`. It carries no `Display`:
  nothing in the crate can print one, and a second wording drifts from the one
  the user reads. `Display` is for an error the CLI prints itself
  (`CompileError`, `WitEmitError`), or one whose text a `Diagnostic` builder
  reads (`LexError`, `LoadError`).
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
- `cargo check --all-targets` while iterating, or a type the crate changes
  breaks the uncompiled unit tests where nothing looks. Run
  `cargo test -p wado-compiler --test e2e` for anything the language touches: it
  covers O0 and O2 and leaves the rest to CI, whose `ignored` lines are that
  split and not a gap, so never set `WADO_FULL_TEST` unless asked by name.
  `mise run test` and `mise run test-wado` take an hour: run them at the end.
- This crate must compile for `wasm32-unknown-unknown` (checked in CI). Keep
  OS-dependent `std` modules out of production code.

## Standard Libraries

`src/stdlib.rs` maps every import to its file under `lib/`. A dev build reads
them from disk, so editing one takes effect on the next `wado` run with no
rebuild. A release build embeds them, as does any `wasm32` build, which has no
filesystem. `lib/wasi/` and `lib/core/kiln/` are generated from WIT, `lib/web/`
from a WebIDL snapshot: read `wado-from-idl/AGENTS.md` first.

A module re-exports every effect its public signatures carry. A caller has to
name an effect to install it or to declare it onward, and the module that
demands it is the one place to get it from, so `core:uuid` hands out `Random`
and `SystemClock` and no user of it imports `wasi:*`. An effect a caller never
names stays private: `core:log` reads the clock under `#[ambient]`, so it
re-exports nothing.

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
