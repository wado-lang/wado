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
```

After the run, the summary lists each file and the totals:

```text
coverage: lines 182/204 (89.2%), branches 61/74 (82.4%), functions 40/41 (97.6%)
  src/lexer.wado    lines  62/64   branches 20/22   functions 14/14
  src/parser.wado   lines 120/140  branches 41/52   functions 26/27
```

Two files can be written to `build/coverage/`:

- `lcov.info`: the LCOV trace format (`FN`/`FNDA`, `BRDA`, `DA`). Codecov,
  Coveralls, `genhtml` and the VS Code coverage extensions read it.
- `coverage.json`: the same data with region spans and, per region, the tests
  that reached it.

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

[^1]: One that contains `return`, `break`, `continue`, `?`, or a call typed `!`.

The three reports derive from regions:

- A function is covered when its body region ran.
- A branch is a region that one side of a choice starts: each `if` branch, each
  `match` arm, the right operand of `&&` and `||`, and the early return of `?`.
  LCOV's `BRDA` takes one line per branch.
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
anything with a `FRESH` span).

### Reify inserts one probe per region

When coverage is on, reify consults the plan and puts a probe at the start of
each region: a call to an internal builtin, `builtin::coverage_probe(id)`,
where `id` is the region's number within its module. Instrumenting at reify
has three consequences:

- Every copy the pipeline makes of a region keeps its probe with the same `id`.
  An inlined body, a generic function's instances and an unrolled pack loop all
  report the one region they came from.
- Only packages being measured are instrumented.
- The optimizer sees probes and never needs to know what they mean.

`id` is local to a module. After link, each module's ids are offset by the
count before it, so a probe carries one global index.

### The stdlib snapshot is built once per instrumentation

Each worker thread elaborates the standard library once and seeds every compile
from that snapshot, reified TIR included. Probes inserted at reify are therefore
part of the snapshot, so the snapshot is keyed by whether the standard library
is instrumented.

One run can need both. Measuring the standard library instruments its
snapshot, but a program `core:eval` compiles in the same process is never
measured and has no import for its probes to call. It seeds from the plain one.

A compile whose entry is itself a stdlib file (`lib/core/json.wado`) reparses
that module and does not use the snapshot for it. Its plan is the same, since
the plan depends on the source text alone.

### A probe is an effect, so the optimizer keeps what ran

The optimizer treats `coverage_probe` like a write to a mutable global: it
writes state and is never pure. No pass that preserves program behavior can
then remove a probe that could run:

- DCE cannot remove a call whose body holds a probe, since that call is no
  longer pure.
- niri cannot fold such a call at compile time, so a function that would
  have run only inside the compiler still runs, and reports, at run time.
- `const_branch_prune` may delete a branch together with its probe. That branch
  can never run, so reporting it as not run is correct.
- Hoisting, CSE and select lowering skip what holds a probe.

Coverage is therefore counted from source, and it does not depend on `-O`. The
test suite checks that invariant (see Testing). The price is speed: an
instrumented build runs slower than the plain one, most of all when the
standard library is measured and every prelude call holds a probe. Timeouts
stay as declared,
so a test near its limit may need a larger `#[timeout_ms]` under coverage.

### A probe tells the host once, the first time it runs

WIR lowers each probe to a check of one bit and, the first time, a call:

```wat
global.get $cov_k        ;; 32 regions per global
i32.const  <bit>
i32.and
if                       ;; not yet hit in this instance
  i32.const <id>
  call $cov_first_hit    ;; cold: sets the bit, calls the host
end
```

`$cov_first_hit` sets the bit and calls a host import, `wado:coverage/hit(id:
u32)`. Like `core:eval`, the import exists only in the test world, and the
runner links it in `create_test_linker`.

The host must hear about a hit when it happens, not when the test ends. A
trapping test cannot be entered again, so a dump at the end would lose every
`#[expect_trap]` and `#[TODO]` test. The first-hit call pays one host call per
region per test, and the hot path stays a global load and a branch.

The runner collects the ids in the test's store, the way `EvalSession` lives
there. A fresh instance per test gives per-test hit sets at no extra cost.
Probes that run in `$initialize_module` report as the test that instantiated
the module.

### The plan travels with the component

The compiler writes the plan as a custom section, `org.wado-lang.coverage`,
beside `org.wado-lang.test-names`. For each instrumented module it holds:

- the module's path, relative to the package root; a `core:` module is named
  by its file in the stdlib package, so `core:json` imported by a test and
  `lib/core/json.wado` compiled as an entry are one module;
- a hash of its source text;
- every region: its global id, kind, parent function, and span;
- every countable line, with the region that holds it.

The runner merges plans and hits across files by path and region number, since
every test file is its own entry point and the same module is compiled once
per file that reaches it. Two plans for one path with different hashes mean
the source changed during the run, and the runner reports an error rather than
a merged number.

A module that no compiled test file reaches still belongs in the denominator.
`wado test` already parses and compiles every `.wado` file it discovers, so
each of them contributes its plan.

### What is measured

The package under test: its `EntryPoint` and `Local` modules, and its `core:`
modules when that package is the standard library (`wado test --coverage
wado-compiler`). `--coverage-include=deps` adds `Dependency` and `Remote`
modules. Kiln output (`Redirected`) is never measured, and neither is the
standard library under another package's tests.

A Wasm asset such as `libm.wat` has no plan, since it is not Wado source.

`#[coverage(off)]` on a `fn`, an `impl` or a module (`#![coverage(off)]`)
removes it from the plan, as Rust's attribute of the same name does. It is for
code that cannot be reached from a test, such as a `run` entry point that only
the CLI world calls. It is the only way to exempt code, so every exemption is
visible where the code is.

### The allocator reports through its own module

`core:allocator` is `#![wasm_module("mem")]`: it compiles into a core module of
its own, which defines the linear memory the main module imports. It runs in
two situations:

- The guest calls it, through `builtin::realloc` or the glue that lowers an
  argument for an import. Calling an import from there is allowed.
- The host calls it as the `realloc` canonical option, to place a value it
  hands the guest. The Canonical ABI clears `may_leave` for that call
  (`LiftLowerContext.reallocate`), and calling any import while it is clear
  traps.

A probe in `mem` must therefore know which situation it is in:

- `mem` imports `wado:coverage/hit` as the main module does. The import takes
  one `u32`, so lowering it needs no memory or `realloc`, and the module that
  defines the memory can import it.
- In a coverage build, the `realloc` canonical option names a wrapper that
  `mem` exports. The wrapper sets a flag for the length of the call, and it is
  the only way the host enters `mem`.
- On a first hit, a probe in `mem` calls the host when the flag is clear. When
  it is set, the probe records the id as pending.
- The host can have called `realloc` only while the guest was waiting on it.
  After every call the main module makes to the host, it checks a pending flag
  `mem` exports, and when it is set, calls a function `mem` exports that
  reports the pending ids.

The Canonical ABI also clears `may_leave` in `post-return`. The test world lifts
every export `async`, and an `async` lift has no `post-return`, so no probe
runs there. The WIR lowering asserts it.

### A test file chooses its allocator

A build links exactly one allocator, and `wado test` picks `debug`. The `bump`
and `freelist` allocators would stay uncovered. Their tests today are e2e
fixtures (`allocator_freelist_*.wado`), which choose the allocator in
`__DATA__`, and which `wado test` never runs.

A test file names its allocator with `#![allocator("freelist")]`, and the
allocator's tests move to `lib/core/allocator_*_test.wado`. Each file compiles
on its own, so one run covers all three allocators, and merging by path joins
them into one plan for `core:allocator`. An explicit `--allocator` that
disagrees with the file's is an error for that file.

### The standard library gates on a baseline

`--coverage-baseline <file>` fails the run on a difference between the regions
left uncovered and the ones the file lists. A new uncovered region fails, and
so does a listed region that is now covered, so the file only shrinks. It is the
pattern `scripts/rust-inline-paths.json` already follows.

An entry names its region by path, function and position within the function,
not by line, so an edit elsewhere in the file leaves it valid.

A CI job runs the stdlib tests with `--coverage` and the stdlib's baseline.
Coverage does not depend on `-O`, so one optimization level answers for all.
The baseline starts as whatever the first run leaves uncovered, and the work
toward 100% is emptying it: a test for each region that can run, and
`#[coverage(off)]` for each that cannot. Once it is empty, 100% is what the gate
holds.

### Testing

- An e2e fixture states the coverage it expects in `__DATA__`, as it states a
  compile error today: `{"uncovered_lines": [12, 14]}`, and
  `{"uncovered_branches": ["14:5 else"]}` for branches. Each region kind gets a
  fixture.
- `wado dump --coverage-plan` prints the plan, as `--assert-plan` prints
  power-assert's.
- A corpus check runs every fixture under coverage at `-O0` and `-O3` and
  requires identical reports. It is the check that no pass moves a probe out of
  its region or drops one that could run.
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

1. [ ] The region plan: the AST walk, `#[coverage(off)]`, and
       `wado dump --coverage-plan`.
2. [ ] Probes: the `coverage_probe` builtin, reify inserting it, its effect in
       `mod_ref`, and the WIR lowering to bit globals and `$cov_first_hit`.
3. [ ] The custom section and the `wado:coverage/hit` import in the test world.
4. [ ] The runner: collecting hits, merging plans, the summary, LCOV and JSON.
5. [ ] Fixtures with `uncovered_lines` and `uncovered_branches`, and the
       `-O0`/`-O3` corpus check.
6. [ ] The standard library: the snapshot keyed by instrumentation,
       `--coverage-baseline`, and the CI job with its first baseline.
7. [ ] The allocator: the import and the `realloc` wrapper in `mem`, the
       pending drain, `#![allocator]`, and its fixtures moved to test files.
8. [ ] 100% for the standard library: the baseline emptied.

## Known gaps

- [ ] A trap in the middle of a region marks the whole region as run. Only a
      statement that can leave its block starts a new region, and any call can
      trap. A 100% report can therefore include a statement no test ran to its
      end.
- [ ] Hit or not only, no execution counts. `FNDA` and `DA` report `1` or `0`.
- [ ] A hit in the allocator is lost when the allocator traps while the host
      is calling it: the pending ids are never reported.
- [ ] Kiln generators run at compile time and are not measured, nor is a
      program `core:eval` compiles.
- [ ] `wado run` and `wado serve` have no `--coverage`.

## References

- [DWARF Metadata](./dwarf.md): the source mapping this design does without.
- [Power-Assert Coverage](./wep-2026-08-19-power-assert-coverage.md): the
  plan-then-reify pattern the probes follow.
- [Eval](./wep-2026-09-26-eval.md): the precedent for a test-world-only host
  import.
- [Test Discovery](./wep-2026-05-02-test-discovery.md): how `wado test` finds and
  runs test files.
- LLVM source-based code coverage: the region model this design adapts.
