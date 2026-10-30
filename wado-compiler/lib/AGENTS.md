# The Standard Library

`../src/stdlib.rs` maps every import to its file here. A dev build reads them
from disk, so an edit takes effect on the next `wado` run with no rebuild. A
release build embeds them, as does any `wasm32` build.

`wasi/`, `core/kiln/`, `core/eval/`, `core/coverage/`, `core/fuel/` and
`core/process/` are generated from WIT: read `wado-from-idl/AGENTS.md` first.

## Rules

- No inline hints (`#[inline(...)]`). A hint that makes code faster marks a case
  the optimizer misses, so the fix belongs in the optimizer. A hint that trades
  run time for compile time hides no such case, and may stay: `core:rt`'s
  coverage probe is the one.
- A module re-exports an effect only where it owns it: `core:cli` hands out
  `Stdout`, `core:fs` hands out `Preopens`. A module that merely performs an
  ambient effect imports it privately, and its callers take the name from
  `wasi:*` too. `pub use` keeps the identity, so re-exporting an effect
  abstracts nothing: it adds a second path to one type, and a reader of
  `use { MonotonicClock } from "core:benchmark"` has to go and find out which
  clock that is.
- Choose between `builtin::select` and `if` on eagerness, not on copies.
  `select` evaluates both operands and is planned as the merge it is, so it pays
  the same copy the `if` does. `src/optimize/select_lowering.rs` goes the other
  way, rewriting an `if` into a branchless select only where both arms are
  duplicable pure leaves.
- A module carries a `#[synopsis]` test, in the module itself, since `wado doc`
  renders it as the module's `## Synopsis`. Write the shortest program that
  shows what the module is for, not a tour of its API. Leave it out only where
  such a program cannot be written. `core:prelude` owes none: every program
  already imports it.

## Tests

Tests for a module live in `<module>_test.wado` beside it. The test file joins
`core:*`'s package, so it reaches `internal` items as well as `pub` ones. A
private item cannot be reached from there, so its test goes in the module. A
facade puts each test beside the file that implements it, which is why
`core:collections` is tested from `collections/treemap_test.wado` and
`collections/treeset_test.wado`.

A language feature is tested from an e2e fixture, even where the prelude
implements it. The same holds for a prelude item the compiler knows by name,
such as a trait an operator calls. The rest of the prelude is ordinary library
code, tested from `<module>_test.wado` like any other module.

The stdlib tests aim at 100% coverage of the stdlib, whatever the fixtures
cover. `mise run test-stdlib-coverage` holds the stdlib to
`scripts/stdlib-coverage.json`, the regions its tests leave unrun, and fails on
a difference either way. `mise run update-baselines` rewrites it, with the
fuel baseline (`benchmark/AGENTS.md`).

Coverage serves the code, not the reverse: never bend correct, fast code to fit
the baseline. New code gets a test. A region that never runs says so in the code
(`unreachable()`, `#[coverage(off)]`). One that runs, but under no stdlib test,
is added to the baseline.
