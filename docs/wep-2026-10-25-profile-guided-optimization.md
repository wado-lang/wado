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
Every later compile reads the profile and marks each
region by what it measured, so the optimizer's existing cold-path machinery
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
format is in the [specification](./spec-pgo.md#the-profile-format). A file under
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

### Rarity is two questions, as in LLVM and GCC

`cold_path()` says two things at once. Its branch is rarely taken, which the
branch hints read. And its code rarely runs, which the inliner, `cold_outline`,
`param_spec`, `const_object_globalization` and the remarks read. A guess has no
way to tell the two apart, but a measurement does, and they part often: a `?`
that fails in 0.1% of a hot function's calls runs on a rare side of its branch,
yet thousands of times in the whole program.

LLVM and GCC answer the two questions with two yardsticks over the same counts.

- Whether a branch is biased is a ratio within the branch. LLVM's
  [`BranchProbabilityInfo`][llvm-bpi] reads branch weights, and a profile's
  weights replace the static guesses. [HotColdSplitting][llvm-hcs], when it has
  no profile, calls a successor cold at a probability of at most 1%, the
  complement of [`predictable-branch-threshold`][llvm-tti] (99%). GCC's
  `-fprofile-use` [sets each edge's probability][gcc-profile] to its count over
  its block's count, which overrides every [static predictor][gcc-predict-def],
  `__builtin_expect` and the Fortran front end's failure-path predictors
  included. Its if-conversion calls a branch predictable at
  [`predictable-branch-outcome`][gcc-params] (2%).
- Whether code runs rarely is an absolute count against the whole profile.
  LLVM's [`ProfileSummaryInfo`][llvm-psi] places hot and cold lines from shares
  of all runs, and with a profile, HotColdSplitting outlines a block, and the
  [inliner][llvm-inline] prices a call site, by `isColdBlock` and
  `isHotCallSite`. GCC's [`maybe_hot_bb_p`][gcc-predict] reads
  `hot-bb-count-ws-permille` the same way, and its `probably_never_executed`
  treats a count of zero as never run.

[llvm-bpi]: https://github.com/llvm/llvm-project/blob/main/llvm/lib/Analysis/BranchProbabilityInfo.cpp
[llvm-hcs]: https://github.com/llvm/llvm-project/blob/main/llvm/lib/Transforms/IPO/HotColdSplitting.cpp
[llvm-tti]: https://github.com/llvm/llvm-project/blob/main/llvm/lib/Analysis/TargetTransformInfo.cpp
[llvm-psi]: https://github.com/llvm/llvm-project/blob/main/llvm/lib/Analysis/ProfileSummaryInfo.cpp
[llvm-inline]: https://github.com/llvm/llvm-project/blob/main/llvm/lib/Analysis/InlineCost.cpp
[gcc-profile]: https://github.com/gcc-mirror/gcc/blob/master/gcc/profile.cc
[gcc-predict]: https://github.com/gcc-mirror/gcc/blob/master/gcc/predict.cc
[gcc-predict-def]: https://github.com/gcc-mirror/gcc/blob/master/gcc/predict.def
[gcc-params]: https://github.com/gcc-mirror/gcc/blob/master/gcc/params.opt

Neither reads the summary to decide a branch's direction. A summary line says
nothing about a branch: a profile of under a million runs puts the cold line on
about its smallest count, so a ratio of 0.1% and one of 50% look the same to
it.

So a compile asks both. A region is unlikely when it ran at most 1% of the
times the code around it did, under
`--optimize-predictable-branch-threshold`, LLVM's name and default. It is cold
when its count is cold against the summary, under LLVM's
`--optimize-profile-summary-cutoff-hot` and `-cold`, or zero, as GCC holds.
[Rarity](./spec-pgo.md#rarity) states both rules. For a path past a branch, or
past a statement that can leave, that ratio is the branch's probability. A
loop's test also runs once per pass through its body, so for a loop's body the
ratio bounds the probability from above, and a body it calls unlikely is. A
count on both summary lines is hot, since LLVM's inliner asks hot first. A share
stays put when a workload runs twice, so a workload need not be large, only like
real use. The thresholds are LLVM's for now; the benchmark machine
settles Wado's own.

### The profile marks each region by its answers

In every function the workloads entered, reify inserts a marker at the start of
each region by its answers: `builtin::cold_path()` where it is unlikely and
cold, `builtin::unlikely_path()` where it is only unlikely, and
`builtin::cold_code()` where it is only cold. The branch hints read
`cold_path()` and `unlikely_path()`, and every other pass reads `cold_path()`
and `cold_code()`. WIR keeps the split: its marker carries the halves it
states, so a WIR pass reads the one it needs as a NIR pass does. The two halves are `internal` to `core:`, so no author
writes one. Reify is where coverage inserts its probes, so a region means the
same thing to both.

A region is marked cold only where all of the code under it is cold. A marker
covers what follows it in its block, so one over a loop the workload ran a
million times would price the loop as cold. Under a region marked cold, a
region that is also unlikely gets `unlikely_path()`, so its branch is still
hinted.

A function whose body region never ran gets no marker. Workloads are tests, so
a build's own entry never runs under them, and neither does whatever only it
reaches, such as a benchmark's timing loop: a function they never entered says
nothing about how often it runs. Marking such functions at their start made
every benchmark slower, by 24% to 74%. The optimizer does not inline into a
function only cold sites reach, and that took in the whole path the benchmark
measures. A function the workloads entered but whose code is cold everywhere is
another matter: it gets `cold_code()` at its start, as LLVM treats a function
whose entry count is cold.

Each worker thread seeds its compiles from a snapshot of the reified standard
library, built once. The thread builds it with the `core:` counts already
applied, so a profiled compile seeds from the snapshot as a plain one does.

### The inliner prices a call site by the summary

The inliner prices a call site by what the summary says of the region holding
it. A hot site may inline a callee of up to
`--optimize-inline-hot-callsite-threshold` statements, a cold one of up to
`--optimize-inline-cold-callsite-threshold`, and any other site keeps
`--optimize-inline-threshold`. They mirror LLVM's `hot-callsite-threshold`
(3000) and `inline-cold-callsite-threshold` (45) beside its `inline-threshold`
(225), but count statements as Wado's inliner does, so the benchmarks settle
their defaults.

### The profile overrides the source's markers

An author's `cold_path()` and the marker the compiler puts on a `?`'s failure
path are both guesses, and a workload modelled on real use measures better than
either. So in a region the profile has a count for, both are dropped, and the
region's own marks stand. GCC does the same: `-fprofile-use` replaces the
probability a `__builtin_expect` gives with the measured one. So does LLVM's
[IR profile][llvm-pgo-use], which overwrites the weights an expect lowered to.
Clang's front-end profile is the exception: there
[`LowerExpectIntrinsic`][llvm-lower-expect] replaces the measured weights with
the expect's.

Where what the profile found is not both unlikely and cold, the compile says so
in an optimizer remark at `--log-level info`. It names the marker, the region's
count and the count of the code around it, and what the region was found to be. The remark audits
a guess against measurement, as LLVM's [`-pgo-warn-misexpect`][llvm-misexpect]
does, so an author learns which markers the workload contradicts. A `?`'s guess
is usually right, since an HTTP service answers well under 1% of its requests
with a 4xx or 5xx, and then the profile marks the failure path as the guess did
and says nothing.

[llvm-pgo-use]: https://github.com/llvm/llvm-project/blob/main/llvm/lib/Transforms/Instrumentation/PGOInstrumentation.cpp
[llvm-lower-expect]: https://github.com/llvm/llvm-project/blob/main/llvm/lib/Transforms/Scalar/LowerExpectIntrinsic.cpp
[llvm-misexpect]: https://github.com/llvm/llvm-project/blob/main/llvm/lib/Transforms/Utils/MisExpect.cpp

The other markers the compiler places sit ahead of a trap, or past a global's
once-guard. A workload that reaches a trap fails the recording, and a once-guard
runs once by construction, so a profile has nothing to correct in them. The
marker `match_to_switch` puts on the default arm of an exhaustive `match` is one
of these: that arm is `unreachable`.

### Flags switch the marks

`-f pgo` reads the profile and `-f no-pgo` ignores it. It is on by default in
every world, the test world included. A profile never changes what a program
computes, but the markers it places steer inlining and outlining, so a test
world without them would test code no build ships, and a pass that miscompiles
only under a profile would go unseen.

`wado test --pgo` reads no profile. It rewrites the file, so reading it would
make the run depend on what it overwrites, and the counts it records depend on
no marker.

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
as the author does, and any other that states both halves
`builtin::$known_cold_path()`, which lowering turns into `cold_path()`. The
marker on a `?` propagating a failure is a guess. A global's once-guard runs
once, and the profile's marker states a measurement. The spelling decides
nothing about the profile, which overrides the author's and the `?`'s markers
alike.

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
6. [x] Unlikely and cold. A compile derives the summary's hot and cold lines
   under `--optimize-profile-summary-cutoff-hot` and `-cold`, and judges each
   region unlikely under `--optimize-predictable-branch-threshold`. It writes
   `cold_path()`, `unlikely_path()` or `cold_code()` by the answers, and
   overrides the author's and the `?`'s markers, with the remark. An e2e
   fixture states each mark and each remark.
7. [ ] Hot and cold call sites. The inliner prices a site by the summary under
   `--optimize-inline-hot-callsite-threshold` and
   `--optimize-inline-cold-callsite-threshold`, and the benchmarks, against
   `-f no-pgo`, settle their defaults and those of the summary's cutoffs and
   `--optimize-predictable-branch-threshold`.
8. [ ] The random-profile differential test.
9. [ ] The `benchmark/` pilot. Each benchmark gains a workload and a committed
   profile, `http_routing` a test to carry it first, and a table compares the
   three builds the flags give. A workload runs a smaller input than the
   benchmark measures, so the table does not judge a profile on the run that
   trained it.
10. [ ] The design weighed against LLVM, GCC and Go. Each option below is
    measured on the pilot's table, then adopted, or recorded as a known gap
    with what it lost to. These are options, not the decision.
    - The counts reach the optimizer. Three markers carry two bits per region,
      so nothing weighs one side against the other, and the summary's hot line
      only keeps a count out of cold. LLVM keeps branch weights (`!prof`) and
      function entry counts on the IR, and its inliner raises the threshold at
      a hot call site.
    - Counts per calling context. One function holds one set of counts, so a
      function cold overall but hot from one caller is cold everywhere, and a
      cold call inside a hot region has nowhere to be marked. LLVM's CSPGO
      records again after inlining, and Go's PGO keys its counts by call edge.
    - Code the workloads never entered. It is compiled as unprofiled, so a
      library's paths no workload takes are never cold. GCC's
      `-fprofile-partial-training` lets the author choose between the two.
    - Stale profiles. A changed hash is said once per function at `info`, and
      counts that drift through a caller's change go unsaid. Clang warns at
      `-Wprofile-instr-out-of-date` with the share of functions that no longer
      match.
    - The stdlib snapshot. It is keyed by the whole profile's summary, so a
      change to the user's counts rebuilds it. Keying it by the marks it
      receives would rebuild it only when they change.
    - The inliner's `branch_weight` guard reads the cold half, while the layout
      it protects follows the unlikely hint. The table settles which half it
      should read.

## Known gaps

- [ ] The profile finds the author's markers and the `?`s in the source,
  where the plan finds its regions, so nothing reads the difference
  `$known_cold_path` keeps through TIR. The distinction may have no reader
  left.
- [ ] An author's `cold_path()` in the right operand of `&&` or `||` stays
  when the profile measured it, since the profile has no statement position
  there for a marker of its own.
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
