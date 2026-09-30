# WEP: Test Coverage

## Context

`wado test` says which tests passed. It cannot say which code they ran. A
package author has no way to find the function no test calls, the `match` arm
no input reaches, or the error path a `?` never took.

The first user is the standard library. `mise run test-stdlib` runs its tests,
and every Wado program runs its code, so the standard library should be fully
covered: every region run by some test, or marked as one no test can reach.

The compiler has what coverage needs, but in pieces:

- Every AST, TIR and NIR node carries a `Span`: byte range, line and column.
  WIR carries none, so a Wasm offset cannot be mapped back to a line. The
  function-level DWARF that [`dwarf.md`](./dwarf.md) plans is not implemented.
- The elaborator already rewrites source for instrumentation. Power-assert
  records a plan per `assert`, keyed by AST node, and reify emits the rewritten
  TIR from it (`elaborator/assert.rs`, `reify_assert`).
- The test world already carries compile-time metadata to the runner in a
  custom section, `org.wado-lang.test-names`.
- The test world already has a host import that no other world gets:
  `core:eval`, linked in `create_test_linker`.
- The runner instantiates a fresh component for every test, so anything the
  guest records starts from nothing in each test.

Two properties of the pipeline decide the design.

The optimizer deletes, duplicates and moves code. Inlining copies a body into
each caller, monomorphization makes one function per instantiation,
`const_branch_prune` and DCE delete, `cold_outline` moves a path into a new
function, and niri folds calls at compile time. A mapping from the emitted Wasm
back to source would have to survive all of that.

The compiler never sees every function. Liveness keeps reify from emitting a
function nothing reaches, and `prelower_reach` prunes more before lowering. A
function no test reaches is exactly what coverage must report, so the list of
what could have run cannot come from the compiled program.

## Decision

### `wado test --coverage` reports lines, branches and functions

```sh
wado test --coverage                  # summary on stdout, LCOV in build/coverage/
wado test --coverage=lcov,json        # choose the files written
wado test --coverage --coverage-include=deps
wado test --coverage --coverage-baseline scripts/stdlib-coverage.json
```

After the run, the summary lists each file and the totals. Under
`--format tap`, each of its lines is a `#` comment.

```text
coverage: lines 182/204 (89.2%), branches 61/74 (82.4%), functions 40/41 (97.6%)
  src/lexer.wado   lines     62/64  branches     20/22  functions   14/14
  src/parser.wado  lines   120/140  branches     41/52  functions   26/27
```

Three files can be written to `build/coverage/`:

- `lcov.info`: the LCOV trace format (`FN`/`FNDA`, `BRDA`, `DA`). Codecov,
  Coveralls, `genhtml` and the VS Code coverage extensions read it.
- `coverage.json`: the same data with region spans and, per region, the tests
  that reached it.
- `baseline.json`: the regions left unrun, in the form `--coverage-baseline`
  reads.

`--coverage` changes no test outcome. It fails the run only when the
instrumentation itself fails, which is a compiler bug.

### The unit is a region, planned from the AST

A region is a span of source that runs as a unit: when its first statement runs,
the rest runs too, unless a trap stops it. Each region gets one probe. Region
boundaries are:

| Construct                                      | Regions it starts                                 |
| ---------------------------------------------- | ------------------------------------------------- |
| `fn`, method, closure                          | the body                                          |
| `if`, `if let`                                 | each branch, including an `else` the source omits |
| `match`                                        | each arm                                          |
| `for`, `while`, `while let`, `loop`            | the body                                          |
| `let … else`                                   | the `else` block                                  |
| `&&`, `\|\|`                                   | the right operand                                 |
| `?`                                            | the early return                                  |
| a statement that can leave its block early[^1] | the statements after it                           |

[^1]: One that contains `return`, `break`, `continue` or `?` outside a closure.
A call typed `!` starts no region: it traps or never returns, so nothing
after it runs.

The three reports derive from regions:

- A function is covered when its body region ran.
- A branch is a region that one side of a choice starts: each `if` branch, each
  `match` arm, the `else` of `let … else`, the right operand of `&&` and `||`,
  and the early return of `?`. LCOV's `BRDA` takes one line per branch. A
  report names one by where its choice is written and its kind: `14:5 else`,
  `20:9 arm 2`.
- A line counts when a statement starts on it. It is covered when the innermost
  region holding that statement ran. Blank lines, comments and lone braces do
  not count.

The plan is computed per module from its AST, in the annotate phase, as
power-assert's is. It is a function of the source text alone, so every
compilation that loads a module plans the same regions with the same numbers.
It covers every function in the module, including those liveness never
reifies, which report as not run.

A `test` block, including a `#[synopsis]` one, is not planned: it is the test,
not the code under test. Neither is synthesized code (derived impls, bridges,
anything with a `FRESH` span), nor a global's initializer. An `assert`'s
condition is not split into regions: power-assert rewrites it, so its operands
are not the source's.

### Reify inserts one probe per region

When coverage is on, reify consults the plan and puts a probe at the start of
each region: a call to `core:rt::coverage_probe(id)`, a compiler item. The
compile plans every measured module before reify, in load order, and numbers
the regions across them, so `id` is already global. Instrumenting at reify has
three consequences:

- Every copy the pipeline makes of a region keeps its probe with the same `id`.
  An inlined body, a generic function's instances and an unrolled pack loop all
  report the one region they came from.
- Only packages being measured are instrumented.
- The optimizer sees probes and never needs to know what they mean.

Reify checks itself: for each function one of whose probes it emitted, every
probed region of that function must have taken its probe. A region reify
reached without one would report as never run. The one exception is a region
with no instance to take one: the body of a tuple `for-of` over no elements,
which never runs and reports so.

### A region its children account for takes no probe

A region needs no probe of its own when every run of it enters one of its
probed child regions. The common case is a body that reaches an `if` or a
`match` before anything that can leave it early: every run takes one branch,
and every branch is a region, including an `else` the source omits. The body
ran exactly when one of its branches ran.

The plan marks such a region as derived and names the children it derives
from. Reify inserts no probe for it, and the runner reports it as run when any
of those children ran. A child may itself be derived.

The choice is a statement that is an `if` or a `match`, or binds one with `let`
or returns one with `return`. Its condition, scrutinee and guards must not be
able to leave the block, and neither may any statement before it. A region
whose statements start with a `?`, a `return` or a `break` before any choice
keeps its probe. So does one written as an expression rather than a block, such
as a `match` arm `=> f(x)`.

A trap between the start of the region and the choice is the one case the
derivation gets wrong: the region ran, but reports as not run.

Over the standard library, 1,874 of 10,960 regions are derived, so a measured
compile inserts 17% fewer probes.

### A compile that measures the standard library skips its snapshot

Each worker thread elaborates the standard library once and seeds every compile
from that snapshot, reified TIR included. Probes inserted at reify would be part
of it, so a compile that measures the standard library elaborates it afresh
instead. The snapshot stays plain, and a program `core:eval` compiles in the
same process, which is never measured, seeds from it as before.

A compile whose entry is itself a stdlib file (`lib/core/json.wado`, which
declares `#![stdlib("core:json")]`) plans that module as `core:json`, the same
plan an import of it gets, since the plan depends on the source text alone.

### A probe is an effect, so the optimizer keeps what ran

`coverage_probe` writes a mutable global and calls a host import, so the
optimizer sees it as it sees any other call with a side effect. It declares the
`CoverageHost` effect `#[benign]`, so a pure function keeps its signature once
probed. No pass that preserves program behavior can then remove a probe that
could run:

- DCE cannot remove a call whose body holds a probe, since that call is no
  longer pure.
- niri cannot fold such a call at compile time, so a function that would
  have run only inside the compiler still runs, and reports, at run time.
- `const_branch_prune` may delete a branch together with its probe. That branch
  can never run, so reporting it as not run is correct.
- Hoisting, CSE and select lowering skip what holds a probe.

A pass may still call a function the source does not. The append fusion in
`string_push` writes a run of appends with `String::internal_write_str_at`,
which nothing else calls, so whether it runs says what `-O` did. Such a
function is `#[coverage(off)]`.

Coverage is therefore counted from source, and it does not depend on `-O`. The
test suite checks that invariant (see Testing). The price is speed: an
instrumented build runs slower than the plain one, most of all when the
standard library is measured and every prelude call holds a probe. Timeouts
stay as declared, so a test near its limit may need a larger `#[timeout_ms]`
under coverage.

### A probe tells the host once, the first time it runs

`coverage_probe` is Wado source in `core:rt`, itself `#[coverage(off)]`. It
keeps one byte per region in a global array, grown on demand, and the first
time a region runs it sets the byte and calls the host import
`core:coverage/coverage-host@0.1.0#hit(id: u32)`. The interface is WIT in
`lib/core/coverage/`, generated like `core:eval`'s. A component imports it only
when a probe is emitted, and the runner links it in the test linker.

The host must hear about a hit when it happens, not when the test ends. A
trapping test cannot be entered again, so a dump at the end would lose every
`#[expect_trap]` and `#[TODO]` test. The first-hit call pays one host call per
region per test, and the hot path is an array load and a branch. The call is
Wado rather than a WIR lowering, so the inliner decides where the check lands,
as it does for any other small function.

The runner collects the ids in the test's store, the way `EvalSession` lives
there. A fresh instance per test gives per-test hit sets at no extra cost.
Probes that run in `$initialize_module` report as the test that instantiated
the module.

### The plan travels with the component

The compiler writes the plan as a custom section, `org.wado-lang.coverage`,
beside `org.wado-lang.test-names`. For each instrumented module it holds:

- the module's path: a file relative to the entry module's directory, as the
  loader reads it, and a `core:` module by its import path;
- its first global id;
- every function: its name, line and body region;
- every region: its kind, span and function, for a branch the position of its
  choice and its side, and for a derived region the children it derives from;
- every countable line, with the region that holds it.

The runner resolves each path to a file and reports it relative to the
directory `wado test` ran in. A `core:` module resolves to its file under
`wado-compiler/lib/`, so `core:json` imported by a test and
`lib/core/json.wado` compiled as an entry are one module. A release build
embeds the standard library and keeps the import path.

The runner merges plans and hits across files by path and region number, since
every test file is its own entry point and the same module is compiled once
per file that reaches it. Two plans for one path that differ mean the source
changed during the run, and the runner reports an error rather than a merged
number. The plan itself is the check, so the section carries no hash.

A module that no compiled test file reaches still belongs in the denominator.
`wado test` already parses and compiles every `.wado` file it discovers, so
each of them contributes its plan.

### What is measured

The package under test: its `EntryPoint` and `Local` modules, the `*_test.wado`
files' own helpers included. `--coverage-include` adds more, as a list:
`deps` adds `Dependency` and `Remote` modules, and `stdlib` adds `core:`
modules, which is how the standard library's own tests measure it
(`wado test --coverage --coverage-include=stdlib wado-compiler`). Kiln output
(`Redirected`) is never measured.

Within the standard library, two modules have no plan:

- `core:allocator`, a `#![wasm_module("mem")]` module. The host calls it as the
  `realloc` canonical option, and the Canonical ABI clears `may_leave` for that
  call (`LiftLowerContext.reallocate`), so a probe calling the host there would
  trap. A build also links only one of its three allocators.
- A Wasm asset such as `libm.wat`, which is not Wado source.

The Canonical ABI clears `may_leave` in `post-return` as well. Coverage is
compiled only for the test world, which lifts every export `async`, and an
`async` lift has no `post-return`, so no probe runs there. Codegen asserts that
a measured component lifts no export with `post-return`.

`#[coverage(off)]` on a `fn`, an `impl` or a module (`#![coverage(off)]`)
removes it from the plan, as Rust's attribute of the same name does. It is for
code that cannot be reached from a test, such as a `run` entry point that only
the CLI world calls. It is the only way to exempt code, so every exemption is
visible where the code is.

### The standard library gates on a baseline

`--coverage-baseline <file>` fails the run on a difference between the regions
left uncovered and the ones the file lists. A new uncovered region fails, and
so does a listed region that is now covered, so the file only shrinks. It is the
pattern `scripts/rust-inline-paths.json` already follows.

An entry names its region by path, function and position within the function,
not by line, so an edit elsewhere in the file leaves it valid:

```json
{ "wado-compiler/lib/core/json.wado": { "Parser::parse_value": ["arm 7", "rest 12"] } }
```

`arm 7` is the function's region 7, a `match` arm; region 0 is the body. A
function that never ran lists its body alone, `fn 0`, since nothing in it ran
either. A name repeated within a file takes `#2`, `#3`, … in source order.
`--coverage=baseline` writes the file.

`mise run test-stdlib-coverage` runs the stdlib tests with `--coverage` and
`scripts/stdlib-coverage.json`, and CI runs it in the `O2` job. Coverage does
not depend on `-O`, so one optimization level answers for all.
`mise run update-stdlib-coverage-baseline` rewrites the file.
The baseline starts as whatever the first run leaves uncovered, and the work
toward 100% is emptying it: a test for each region that can run, and
`#[coverage(off)]` for each that cannot. Once it is empty, 100% is what the gate
holds.

### Testing

- An e2e fixture states the coverage it expects in `__DATA__`, as it states a
  compile error today:
  `"coverage": {"uncovered_lines": [12], "uncovered_branches": ["14:5 else"], "uncovered_functions": ["never"]}`.
  Each list is the whole set for the fixture's own file. Each region kind gets
  a fixture (`coverage_*.wado`), and CI runs them at every level.
- `wado dump --coverage-plan` prints the plan, as `--assert-plan` prints
  power-assert's.
- `mise run check-coverage-levels` runs the stdlib tests under coverage at `-O0`
  and `-O3` and requires the same regions left unrun. It is the check that no
  pass moves a probe out of its region or drops one that could run. Which test
  ran a region may differ, since a test that waits on the host or a clock takes
  the path its timing picks.
- Integration tests cover what no fixture can: the CLI flags, the LCOV and JSON
  files, and merging plans from several test files.

## Alternatives considered

### Map Wasm offsets back through DWARF

Run the plain build under an instruction-level hook and map offsets to lines
through a `.debug_line` table. WIR has no spans to build that table from, and in
optimized code the regions no longer exist to be mapped: a pruned branch, a
folded call and an inlined body are all gone or merged. Coverage would change
with `-O`, and the functions liveness drops would be missing entirely.

### Instrument the NIR after lowering

NIR still has spans, and the probe would not need a plan in the elaborator. But
NIR comes after monomorphization and pruning. The functions never reached are
already gone, so the denominator still needs an AST walk, and the NIR shapes
no longer match the source constructs (a `for` is already a desugared loop).
Two walks over different trees would then have to agree on region identity.

### Count every execution

A counter per region, bumped on every run and read when the test ends, gives
execution counts as well. Reading at the end loses every trapping test, and a
host call on every run is too slow for loops.

## Roadmap

1. [x] The region plan: the AST walk, `#[coverage(off)]`, and
   `wado dump --coverage-plan`.
2. [x] Probes: `core:rt::coverage_probe` and reify inserting it.
3. [x] The custom section and the `core:coverage/coverage-host` import in the
   test world.
4. [x] The runner: collecting hits, merging plans, the summary, LCOV and JSON.
5. [x] Fixtures with `uncovered_lines`, `uncovered_branches` and
   `uncovered_functions`, and the `-O0`/`-O3` check over the stdlib tests
   (`mise run check-coverage-levels`).
6. [x] The standard library: its snapshot skipped when measured,
   `--coverage-baseline`, and the CI job with its first baseline.
7. [ ] 100% for the standard library: the baseline emptied.
8. [x] Derived regions: the plan marks them, reify leaves them without a probe,
   and the runner derives them. The `-O0`/`-O3` check and the fixtures pass
   unchanged, and the probes saved on the standard library are measured.

## Known gaps

- [ ] A trap cuts a region short, and the report does not see where. A trap in
  the middle of a region marks the whole region as run, and a trap before a
  derived region reaches its choice marks it as not run. Wado cannot catch
  a trap, so any other test that traps fails the run; only an
  `#[expect_trap]` or `#[TODO]` test reports coverage past one.
- [ ] Hit or not only, no execution counts. `FNDA` and `DA` report `1` or `0`.
- [ ] `core:allocator` is not measured. Its tests are e2e fixtures, which
  `wado test` does not run.
- [ ] Kiln generators run at compile time and are not measured, nor is a
  program `core:eval` compiles.
- [ ] `wado run` and `wado serve` have no `--coverage`.
- [ ] The `-O0`/`-O3` check runs over the stdlib tests only. The e2e fixtures
  reach optimizer paths the stdlib does not, and only the `coverage_*`
  fixtures are measured, at each level against their own expectations.

## References

- [DWARF Metadata](./dwarf.md): the source mapping this design does without.
- [Power-Assert Coverage](./wep-2026-08-19-power-assert-coverage.md): the
  plan-then-reify pattern the probes follow.
- [Eval](./wep-2026-09-26-eval.md): the precedent for a test-world-only host
  import.
- [Test Discovery](./wep-2026-05-02-test-discovery.md): how `wado test` finds and
  runs test files.
- LLVM source-based code coverage: the region model this design adapts.
