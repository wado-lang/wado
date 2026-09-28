# WEP: Test Coverage

## Context

`wado test` says which tests passed. It cannot say which code they ran. A
package author has no way to find the function no test calls, the `match` arm
no input reaches, or the error path a `?` never took.

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
| `match`                                        | each arm; a guard's success is its own region     |
| `for`, `while`, `while let`, `loop`            | the body                                          |
| `let … else`                                   | the `else` block                                  |
| labeled block                                  | the statements after a `break LABEL` inside it    |
| `&&`, `\|\|`                                   | the right operand                                 |
| `?`                                            | the early return                                  |
| a statement that can leave its block early[^1] | the statements after it                           |

[^1]: One that contains `return`, `break`, `continue`, `?`, or a call typed `!`.

The three reports derive from regions:

- A function is covered when its body region ran.
- A branch is one row of the table above that picks between paths: each `if`
  branch, each `match` arm, the right operand of `&&` and `||`, and the early
  return of `?`. LCOV's `BRDA` takes one line per branch.
- A line counts when a statement starts on it. It is covered when the innermost
  region holding that statement ran. Blank lines, comments and lone braces do
  not count.

The plan is computed per module from its AST, in the annotate phase, as
power-assert's is. It is a function of the source text alone, so every
compilation that loads a module plans the same regions with the same numbers.
It covers every function in the module, reached or not. A function liveness
never reifies still has its regions in the plan, and reports them as not run.

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
- Only packages being measured are instrumented. The standard library is
  elaborated once per worker and snapshotted; it is never measured, so the
  snapshot stays as it is.
- The optimizer sees probes and never needs to know what they mean.

`id` is local to a module. After link, each module's ids are offset by the
count before it, so a probe carries one global index.

### A probe is an effect, so the optimizer keeps what ran

The optimizer treats `coverage_probe` like a write to a mutable global: it
writes state and is never pure. Nothing that preserves program behavior can
then delete, reorder or fold a probe away:

- DCE cannot remove a call whose body holds a probe, since that call is no
  longer pure.
- niri cannot fold such a call at compile time, so a function that would
  have run only inside the compiler still runs, and reports, at run time.
- `const_branch_prune` may delete a branch together with its probe. That branch
  can never run, so reporting it as not run is correct.
- Hoisting, CSE and select lowering skip what holds a probe.

The last point costs speed: an instrumented build runs slower than the plain
one. It does not change results, and the reported coverage does not depend on
`-O`. That is an invariant the test suite checks (see Testing).

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

- the module's path, relative to the package root;
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

By default: every module of the package under test (`EntryPoint` and `Local`).
`--coverage-include=deps` adds `Dependency` and `Remote` modules. Kiln output
(`Redirected`) and the standard library are never measured.

`#[coverage(off)]` on a `fn`, an `impl` or a module (`#![coverage(off)]`)
removes it from the plan, as Rust's attribute of the same name does. It is for
code that cannot be reached from a test, such as a `run` entry point that only
the CLI world calls.

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

## Consequences

- Coverage is exact per region at every optimization level, because it is
  counted from source, not recovered from Wasm.
- Instrumented builds are slower. Timeouts stay as declared; a test near its
  limit may need a larger `#[timeout_ms]` under coverage.
- The work is independent of DWARF. Line-level DWARF would not make coverage
  simpler, since the optimizer would still have erased the regions coverage
  needs.

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
host call on every run is too slow for loops. Counting can be added later as a
second mode (see Known gaps).

## Roadmap

1. [ ] The region plan: the AST walk, `#[coverage(off)]`, and
       `wado dump --coverage-plan`.
2. [ ] Probes: the `coverage_probe` builtin, reify inserting it, its effect in
       `mod_ref`, and the WIR lowering to bit globals and `$cov_first_hit`.
3. [ ] The custom section and the `wado:coverage/hit` import in the test world.
4. [ ] The runner: collecting hits, merging plans, the summary, LCOV and JSON.
5. [ ] Fixtures with `uncovered_lines` and `uncovered_branches`, and the
       `-O0`/`-O3` corpus check.
6. [ ] Adoption: run `--coverage` over one package in CI, such as
       `package-gale`, and publish its LCOV.

## Known gaps

- [ ] A trap in the middle of a region marks the whole region as run. Only a
      statement that can leave its block starts a new region, and any call can
      trap.
- [ ] Hit or not only, no execution counts. `FNDA` and `DA` report `1` or `0`.
- [ ] The standard library cannot be measured. Its snapshot is elaborated
      without probes.
- [ ] Kiln generators run at compile time and are not measured, nor is a
      program `core:eval` compiles.
- [ ] `wado run` and `wado serve` have no `--coverage`. The same probes would
      work with the import linked in their worlds.

## References

- [DWARF Metadata](./dwarf.md): the source mapping this design does without.
- [Power-Assert Coverage](./wep-2026-08-19-power-assert-coverage.md): the
  plan-then-reify pattern the probes follow.
- [Eval](./wep-2026-09-26-eval.md): the precedent for a test-world-only host
  import.
- [Test Discovery](./wep-2026-05-02-test-discovery.md): how `wado test` finds and
  runs test files.
- LLVM source-based code coverage: the region model this design adapts.
