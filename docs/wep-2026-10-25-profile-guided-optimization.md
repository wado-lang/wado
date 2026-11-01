# WEP: Profile-Guided Optimization

## Context

The optimizer already leans on cold paths. `builtin::cold_path()` marks code
that rarely runs, and every pass that reads the marker acts on it:

- `nir/cold_outline` moves the marker's basic block into a function of its own.
- `nir/inline` prices a callee without its cold code. It does not inline at a
  cold call site, nor anywhere in a function that runs only once a cold path
  is taken.
- `param_spec` mints no specialized clone for a cold call site, and
  `const_object_globalization` skips a function that runs only on a cold path.
- `wir_build` turns the marker into a Wasm branch hint, and wasmtime lays out
  the cold side out of line.

An author's marker is a guess: it goes where they believe a path is rare. Only
a path that ends in a trap is known to be cold. `infer_branch_hints` hints
those, and `match_to_switch` marks the default arm of an exhaustive switch,
which traps. Nothing tells the compiler which paths a real run takes, so a guess
that is wrong stays wrong, and a rare path nobody marked is optimized as if it
were hot.

The compiler already measures which code runs. `wado test --coverage` plans a
region for each branch and body from the AST, inserts a probe at the start of
each one, and reports which ones ran
([WEP: Test Coverage](./wep-2026-09-28-test-coverage.md)). Regions do not depend
on `-O`, so a region measured in one build names the same source in any other.
A probe records only that a region ran, not how often.

`benchmark/` measures Wado on real programs, and most benchmarks already call
their kernel from a `test` block. `http_routing`, a `wasi:http/service`, has
none yet. `benchmark/` is where a change to the optimizer is judged, so it is
where profile-guided optimization should prove itself first.

## Decision

A package states its real workload as tests marked `#[workload]`. Running them
records how often each region ran into a profile, and the package commits it.
Every later compile outside the test world reads the profile and marks each
region that never ran as cold, so the optimizer's existing cold-path machinery
acts on measurement instead of guesses.

### A workload is a test marked `#[workload]`

```wado
#[workload]
test "sieve to 200k" {
    let mut is_prime = List::<bool>::filled(200_001, true);
    assert sieve_count(&mut is_prime, 200_000) == 17_984;
}
```

`wado test` runs a workload like any other test, so a workload that breaks
fails the suite instead of rotting. The attribute combines with
`#[timeout_ms]` and `#[synopsis]`. With `#[expect_trap]` or `#[TODO]`, or in a
module marked `#![TODO]`, it is a compile error: such a test traps by design,
and a workload that traps fails the recording (below).

A workload should be deterministic and small. Its counts then come out the same
on every run, so the committed profile changes only when the code or the
workload does. A workload that reads a clock or randomness still works, but its
profile churns.

An attribute was chosen over a new `workload` block. A block would add a
keyword to the reserved vocabulary, and it would not run under `wado test`.

### `wado test --pgo` records the profile

`wado test --pgo` compiles the package's workloads with probes and runs only
them. A probe counts every run of its region. It measures the package's
modules, its dependencies' and the `core:` modules, since a workload's hot path
runs through all three.

Recording belongs to the test runner, as coverage does, since a workload is a
test. The flag is not named `--profile`, which already means the sampling
profiler of `wado run --profile` and `wado test --profile`.

The recording compile does not read an existing profile. A profile is then a
function of the source and the workloads alone, and recording a deterministic
workload twice gives the same file.

A workload that traps, or fails an assertion, fails the recording. Its counts
would stop at the trap, and a profile from a broken workload would mislead.

A run that leaves out part of a package's workloads is an error, since it would
overwrite the package's profile with part of them. `--filter`, `--test-name`
and `--exclude` are therefore errors with `--pgo`. For the same reason, so is a
path argument naming part of a package.

A run that records no workload is an error as well, and the existing profile is
kept. An empty profile would read as code no workload ran.

A run that records some workloads removes the profile of each package it holds
no workload of. Such a profile carries counts from workloads that are gone, and
its hashes still match every function that did not change.

### Coverage counts too

The probe that coverage inserts counts every run, in every coverage build. The
first run of a region still calls the host at once, so a test that traps keeps
its hits. A test that returns hands its counts to the runner when it ends. A
test that traps contributes only its hits, which count as one run each. LCOV's
`DA` and `BRDA` lines carry the counts.

The hand-over runs no measured code. It walks the counters with builtins and
reports only the regions that ran more than once, since a hit already counts
one. Passing the counters as a `list` would lower it through `List`'s prelude
methods, which `--coverage-include=stdlib` measures, so the hand-over would
count runs of its own after reading theirs.

A count is an `i64`. One probe and one counter serve both `--coverage` and
`--pgo`.

[WEP: Test Coverage](./wep-2026-09-28-test-coverage.md) rejected counting for
two reasons. A count read at the end is lost when a test traps, and a host call
on every run is too slow. Keeping the first-hit call answers the first, and
reading the counts once at the end answers the second. What counting adds to
the probe's hot path is one add and one store to an array that holds no
references, so it needs no write barrier. Today's probe already pays for the
call, the length load, the bounds check and the load on every run.

### The profile is committed

The profile is a text file, so the repository holds no binary for it. It holds
one record per line, so a merge conflicts only where the counts moved. Its
format is in the [specification](./spec-testing.md#profiles). A file under
`build/` would not reach CI or a release build, so their output would differ
from the author's.

A package keeps one profile, `pgo/default.pgo` under its root, merged from all
of its workloads. A single-file program without a manifest keeps
`pgo/<stem>.pgo` in its directory, since one directory may hold several such
programs. `benchmark/` is mostly such programs. The name follows Go, whose
`default.pgo` is likewise committed with the main package and read without a
flag. A directory keeps the profiles out of the source listing, and the
extension is not `.json`, so no formatter rewrites what `--pgo` writes.

Only the entry's profile is read. A dependency's own `pgo/` is ignored, as Go
ignores a non-main package's `default.pgo`: the entry's workloads already
measure the dependency's code as the entry uses it.

Every count moves when the code does, so `.gitattributes` marks a profile
`linguist-generated -diff`, as it marks the repository's other generated files.
A routine diff then skips it, and `git diff --text` shows it to a reader who
wants it.

### A profile names what it measured without machine paths

A profile names a module by the module's name, the one the compiler gives it
everywhere (see
[WEP: Module Loader Design](./wep-2026-01-24-module-loader.md#a-module-has-one-name)),
so every name resolves the same on any machine and from any entry of one
package. A packaged
module's name holds its package's name and version, never the cache path it was
fetched to. Two versions of one package can be two packages in one build, so the
version keeps their names apart. After an upgrade the profile holds no counts
for the dependency until the workloads are recorded again.

Under each module the profile lists each function that holds a measured region,
with:

- its name and its line, for a reader;
- the hash of its source text and its region plan;
- the count of each of its regions, numbered from zero within the function.

Coverage numbers regions across the whole program, so editing one function
would move every later function's numbers. Numbering within the function keeps
an edit to one from reaching the counts of another.

### A function whose source or plan changed has no profile

A compile reads a function's counts only when the hash of its current source
text and region plan equals the recorded one. The text catches an edit that
keeps every region where it was but changes which one runs, such as `x == 0`
becoming `x <= 0`. The plan, each region's kind and its span relative to the
function's start, catches what outside the text decides its regions: a callee
becoming `!`-typed starts or ends a region, and `#[coverage(off)]` on the
enclosing `impl` or module removes them all.

A function whose hash differs is compiled as if no profile existed. A stale
profile therefore costs only the gain on what changed, and it never applies
counts to a region they were not recorded for. Counts still go stale through an
edit elsewhere, such as a caller that now passes other values, and only
recording again refreshes them.

A compile that discards a function's counts says so at `--log-level info`, so
an author learns when to record again.

### A region that never ran is marked cold

In every function the workloads entered, reify inserts a `builtin::cold_path()`
at the start of each outermost region whose count is zero. Reify is where
coverage inserts its probes, so a region means the same thing to both. From
there, the optimizer's passes act on the marker as they do on one the author
wrote.

A function whose body region never ran gets no marker. Workloads are tests, so
a build's own entry never runs under them, and neither does whatever only it
reaches, such as a benchmark's timing loop: a function they never entered says
nothing about how often it runs. Marking such functions at their start made
every benchmark slower, by 24% to 74%. The optimizer does not inline into a
function only cold sites reach, and that took in the whole path the benchmark
measures. A region that ran any number of times gets no marker; what its count
says beyond that is the profile summary's to decide (below).

Each worker thread seeds its compiles from a snapshot of the reified standard
library, built once. The thread builds it with the `core:` counts already
applied, so a profiled compile seeds from the snapshot as a plain one does.

### The profile summary says what is hot and what is cold

A count alone says little: a million runs is a sliver of one program and most
of another. So a compile reads each count against a summary of the whole
profile, as LLVM's `ProfileSummaryInfo` does. With every count the profile holds
sorted from the largest down, the hot line is the smallest count still inside
the share of all runs that `--optimize-profile-summary-cutoff-hot` names, and
the cold line the same for `--optimize-profile-summary-cutoff-cold`. A count at
or above the hot line is hot, and one at or below the cold line is cold. Both
shares are in parts per million, with LLVM's defaults for its options of the
same names: 990000 and 999999. A share of all runs stays put when a workload
runs twice, so a workload need not be large, only like real use.

The inliner prices a call site by what the summary says of the region holding
it. A hot site may inline a callee of up to
`--optimize-inline-hot-callsite-threshold` statements, a cold one of up to
`--optimize-inline-cold-callsite-threshold`, and any other site keeps
`--optimize-inline-threshold`. They mirror LLVM's `hot-callsite-threshold`
(3000) and `inline-cold-callsite-threshold` (45) beside its `inline-threshold`
(225), but count statements as Wado's inliner does, so the benchmarks settle
their defaults.

### The author wins over the profile; the compiler's guess does not

An author's `cold_path()` in a region the workload ran stays in force. It
states an intent the workload may not cover, such as an error path a test
exercises often. The compile reports the conflict as an optimizer remark at
`--log-level info`, naming the marker and the count. The remark audits the
author's guesses against measurement, as LLVM's `-pgo-warn-misexpect` audits a
`__builtin_expect`.

The marker the compiler puts on a `?`'s failure path is its own guess, and a
workload modelled on real use measures better than any guess. The guess is
usually right: an HTTP service answers well under 1% of its requests with a 4xx
or 5xx. So the marker stays while the summary calls the failure path's count
cold. Above the cold line the profile wins: the marker is dropped, and the same
remark names the count. Comparing with zero would drop a right guess, since a
rare path still runs now and then. LLVM settles the same question the same way,
letting a measured count override a `__builtin_expect`.

The other markers the compiler places sit ahead of a trap, or past a global's
once-guard. A workload that reaches a trap fails the recording, and a once-guard
runs once by construction, so a profile has nothing to correct in them. The
marker `match_to_switch` puts on the default arm of an exhaustive `match` is one
of these: that arm is `unreachable`.

### Flags switch the marks

`-f pgo` reads the profile and `-f no-pgo` ignores it. The default depends on
the world:

| Build                        | Default |
| ---------------------------- | ------- |
| the test world (`wado test`) | off     |
| any other world              | on      |

A test checks what a program computes, which a profile never changes, so the
test world has no use for one by default. `-f pgo` turns it on there, mainly for
fuzzing the application of profiles.

A build that finds no profile compiles as one with `-f no-pgo`. Under an
explicit `-f pgo` it also says so at `--log-level info`, since the author asked
for a profile that is not there.

`-f no-cold-paths` drops every marker before any pass reads one: the author's,
the profile's, and the compiler's, ahead of a trap, past a global's once-guard,
and in the default arm `match_to_switch` writes. A build with it shows what the
optimizer does with no marker to go on. The hints `infer_branch_hints` derives
from traps stay, since they read no marker. The two flags give three builds of
one program: `-f no-cold-paths` has no markers, `-f no-pgo` has the author's
and the compiler's, and the default adds the profile's.

Reify and lowering spell a marker that states a guess `builtin::cold_path()`,
as the author does, and any other `builtin::$known_cold_path()`, so the conflict
remark can tell the two apart until NIR, which has one marker. The marker on a
`?` propagating a failure is a guess. A global's once-guard runs once, and the
profile's marker states a measurement. The spelling decides nothing about the
profile: a `?`'s marker yields to it, and an author's never does.

The existing `-f no-branch-hinting` is a different cut. Every pass sees the
same markers and hints with it as without, and only the emitter leaves out the
hint section, the trap-derived hints included. It answers what branch hints are
worth to wasmtime; `-f no-cold-paths` answers what the markers are worth to the
whole pipeline.

A marker in an arm that `match_to_switch` turns into a `br_table` target still
reaches every optimizer pass, but no hint: Wasm's branch-hint section has no
form for `br_table`. That is accepted. A hint only moves a block in wasmtime's
code layout, and across every shape tried, branch hints have not made wasmtime's
code measurably faster. Carrying one there would take a guard ahead of the
table, a compare on every dispatch, for a gain nobody has measured.

### A profile changes no output

A profile, stale or wrong, only moves where the optimizer spends its effort. It
never changes what a program computes. A test checks this the way
[EMI](./optimizer.md#differential-testing-emi) checks injected dead code: the
same program compiled with random profiles prints the same output at every
level.

## Roadmap

1. [x] Counting probes. The probe counts, the runner reads the counts when a
   test returns, and LCOV carries them. `mise run test-stdlib-coverage`, before
   and after, in a table with the command beside it, shows what counting costs.
2. [x] `#[workload]`. The attribute runs as a test, is an error beside
   `#[expect_trap]` or `#[TODO]`, and e2e fixtures state both.
3. [x] `wado test --pgo`. It records the workloads into `pgo/`, with the
   per-function hash.
4. [x] Applying the profile. Reify marks the regions that never ran, a function
   whose hash differs is skipped, and the conflict remark is emitted. An e2e
   fixture can name a profile in its `__DATA__`, so a fixture states which
   regions end up cold.
5. [x] `-f pgo` and `-f no-pgo`. The CLI hands a compile the entry's profile,
   and the compile looks each module up by its name.
6. [ ] The profile summary. A compile derives the hot and cold lines from its
   profile under `--optimize-profile-summary-cutoff-hot` and
   `--optimize-profile-summary-cutoff-cold`, and a `?`'s failure path above the
   cold line loses its marker, with the remark. An e2e fixture states both
   sides of the cold line.
7. [ ] Hot and cold call sites. The inliner prices a site by the summary under
   `--optimize-inline-hot-callsite-threshold` and
   `--optimize-inline-cold-callsite-threshold`, and the benchmarks, against
   `-f no-pgo`, settle their defaults.
8. [ ] The random-profile differential test.
9. [ ] The `benchmark/` pilot. Each benchmark gains a workload and a committed
   profile, `http_routing` a test to carry it first, and a table compares the
   three builds the flags give. A workload runs a smaller input than the
   benchmark measures, so the table does not judge a profile on the run that
   trained it.

## Known gaps

- [ ] The conflict remark reads the author's markers and the `?`s from the
  source, where the plan finds its regions, so nothing reads the difference
  `$known_cold_path` keeps through TIR. Once a `?`'s marker yields by its share
  of runs, no marker `$known_cold_path` writes can be contradicted, so the
  distinction may have no reader left.
- [ ] A region that starts in an expression, the right operand of `&&` or
  `||`, has no statement position for a marker, so the regions inside one that
  never ran are marked in its place.
- [ ] A region is measured once for all of a generic function's
  instantiations, so an instantiation the workload never ran shares the counts
  of one it ran.
- [ ] A NIR `match` is read as if every arm's guard ran only on some paths. The
  first arm's guard runs on every path through the `match`, so a marker under
  it covers the `match` but is not read that way.
- [ ] The inliner reads a caller's cold code once, before it splices. A callee
  whose whole body is cold, spliced in, covers its block on both sides of the
  call, yet the calls after it are still inlined at hot-path prices.
