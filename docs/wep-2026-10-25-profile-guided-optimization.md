# WEP: Profile-Guided Optimization

## Context

The optimizer already leans on cold paths. `builtin::cold_path()` marks code
that rarely runs, and every pass that reads the marker acts on it:

- `nir/cold_outline` moves the marker's basic block into a function of its own.
- `nir/inline` prices a callee without its cold code. It does not inline at a
  cold call site, nor anywhere in a function that runs only once a cold path
  is taken.
- `param_spec` mints no specialized clone for a cold call site unless the call
  returns and binds a field, and `const_object_globalization` skips a function
  that runs only on a cold path.
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

A deterministic workload's counts come out the same on every run, so the
committed profile changes only when the code or the workload does. A workload that reads a clock or randomness still works, but its
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
fetched to.

Under each module the profile lists each function that holds a measured region,
with:

- its name and its line, for a reader;
- the hash of its source text and its region plan;
- the count of each of its regions, numbered from zero within the function.

Coverage numbers regions across the whole program, so editing one function
would move every later function's numbers. Numbering within the function keeps
an edit to one from reaching the counts of another.

### A compile finds a function's counts by its hash alone

A compile matches a function to its counts by the hash and nothing else. The
module, the name and the line are for a reader.

The first design matched a function by its module's name and then by its own.
A packaged module's name holds the package's version, so every version bump
cut the profile off from the code it measured. The bump left the code itself
unchanged. A release raises the version of every package in this repository,
so each release built its packages without their profiles, and the fuel
benchmarks failed on the release pull request. The hash already decides whether
counts apply, so the name added nothing but this. Matching by hash alone
carries a profile across a version bump, a file move and a rename of a module.
Two versions of one package in one build need no rule either.

Functions of one hash share their counts, summed, as a generic function's
instances already did. Sharing cannot make cold what ran: a region is cold only
where the sum is zero, so none of the copies ran it. But it costs speed. A copy
no workload entered is judged by another's runs, a branch rare in one copy and
common in another is judged on the sum, and the summary's hot and cold lines
move. So the hash covers where a function is declared, keeping apart what text
alone cannot:

- A method's `impl` head, in full. Its type's name alone left `impl P for &T`
  and `impl P for &mut T` one owner, and so the two traits' `end` of one type.
- A closure's place: the hash of its enclosing function, and its line relative
  to that function's start and its column, as the function's regions are
  placed. The function's name would not do, since two modules may each have a
  `parse` with the same closure in the same place. Short closures recur, and
  one of the same text and types runs as often as the place it is written in.
  On `gale_gen` the type-name owner folded
  `|alt: &Alternative| self.alt_has_gate(alt)` in `rule_overlap`, which no
  workload entered, into the one in `choice_route`, which ran, and cost 1167
  fuel against matching by name. Gale's `keyword_chars_admitted` passes one
  closure text to `all` on one branch and to `any` on the other.

What still shares counts is a function copied into two modules under one name.

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
an author learns when to record again. With matching by hash, the compile says
so of a function whose hash the profile lacks while the profile lists a
function of its name in its module. The module is compared without its
package's version, so a bump does not hide an edit. A name may stand for two
functions, two impls' `fmt` of one type say, so the remark calls the function
new or changed rather than claiming it changed.

Changing what a hash covers changes the format's version in the header: a
profile of another version would apply none of its counts, so the compile
refuses it and says to record again.

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

So a compile asks both, and [Rarity](./spec-pgo.md#rarity) states the rules. For
a path past a branch, or past a statement that can leave, the ratio behind
"unlikely" is the branch's probability. A loop's test also runs once per pass
through its body, so for a loop's body the ratio bounds the probability from
above, and a body it calls unlikely is.

A region that ran is not cold, however rarely it ran. The optimizer reads a
cold region as rarely run together with everything it calls: it inlines into
no function that only cold sites reach. A count says how often the region's
own code ran, not its callees'. An entry such as `core:json`'s `to_bytes` runs
once per document and calls the whole serializer. A cold line placed as LLVM's
`ProfileSummaryInfo` places one, at the smallest counts of the profile, marked
it cold. Then nothing under it was inlined, and serializing cost up to 2.9
times the fuel it costs without a profile, even trained on the input it
measured.

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

A marker covers what follows it in its block, so one marker at the outermost
cold region covers everything under it.

A function whose body region never ran gets no marker. Workloads are tests, so
a build's own entry never runs under them, and neither does whatever only it
reaches, such as a benchmark's timing loop: a function they never entered says
nothing about how often it runs. Marking such functions at their start made
every benchmark slower, by 24% to 74%. The optimizer does not inline into a
function only cold sites reach, and that took in the whole path the benchmark
measures.

Each worker thread seeds its compiles from a snapshot of the reified standard
library, built once. The thread builds it with the `core:` counts already
applied, so a profiled compile seeds from the snapshot as a plain one does.

### The inliner prices a call site by the summary

The inliner prices a call site by what a summary of the profile says of the
region holding it. The summary places hot and cold lines from shares of all
runs, as LLVM's `ProfileSummaryInfo` does. With every count sorted from the
largest down, the hot line is the smallest count still inside
`--optimize-profile-summary-cutoff-hot` parts per million of all runs, 990000
by default, and the cold line the same under
`--optimize-profile-summary-cutoff-cold`, 999999 by default. A share that holds
no count places no line. A site in a region that ran and reaches the hot line is
hot, and one at or below the cold line is cold. Unlike a cold mark, a cold price
stays at its site and reaches no callee. A hot site may inline a callee of up to
`--optimize-inline-hot-callsite-threshold` statements, a cold one of up to
`--optimize-inline-cold-callsite-threshold`, and any other site keeps
`--optimize-inline-threshold`, every site of a function the profile does not
judge included. A hot site in or under an unlikely region keeps it too: the
branch into it is predicted not taken. They mirror LLVM's `hot-callsite-threshold`
(3000) and `inline-cold-callsite-threshold` (45) beside its `inline-threshold`
(225), but count statements as Wado's inliner does, so the benchmarks settle
their defaults: 4 times the level's threshold at a hot site and a quarter of it
at a cold one. LLVM's ratios are about 13 and a fifth.

The hot ratio was measured at 4, 8 and 16 on the fuel benchmarks against
`-f no-pgo`. Fuel favours the larger ratios, but it counts Wasm instructions,
not what a huge function costs once compiled. At 8, `cbor_twitter`'s
deserializer spliced its record decoders into one function and ran 58% longer
by the wall clock for the same fuel, and `json_twitter`'s 29% longer. At 4 both
ran within a few percent of `-f no-pgo`. The cold ratio was measured at a half
down to an eighth, and moved no fuel region. A quarter grew `gale_gen` least.
`--optimize-predictable-branch-threshold` keeps LLVM's 99: at 90, 95 and 98 the
geometric mean moved by under 0.03 points.

A hot site weighs the callee's cold paths, though the plain price leaves them
out. A splice copies them all the same. Priced plain, `CitmCatalog::deserialize`
spliced its field decoders and grew 42-fold, and `json_catalog`'s module more
than doubled, for 4% less fuel.

A call under a `?` is priced by the region around the `?`. The `?`'s own region
spans its operand but counts only the failure path, which the call runs before,
so every such call read as never run and was priced cold.

The heat of each measured region is kept in a table beside the IR, keyed by
the region's source span, and the inliner looks a call up by its span. A spliced
body keeps its spans, so a site keeps its heat in whatever caller it lands in.
A marker statement would also have carried the heat, but a statement in a hot
loop is in the way of the passes that match the loop's shape.

A site inside a function the same round may splice is priced plain, not hot.
Splicing a large callee there grows a small helper past what its own callers
splice, and the hot loop then calls the helper instead of its callee.
`json/catalog/ser` cost 32% more fuel that way, with `write_json_i64` grown
fifteenfold.

### A profile drops a marker only where it refutes it

A marker on a path that ran is dropped only when the path ran at least a tenth
as often as the code around it. A count of zero is what cold reads, but it is
the wrong test for a marker. `List::push`'s growth path ran in 1.4% of pushes,
over the 1% that is unlikely, so dropping its `cold_path()` left it unmarked.
`grow` was then spliced at every push, and JITted code ran 2.2% more
instructions on `syntax_highlight` with no change in fuel.

Against dropping every marker that ran, over three rotated rounds of
`mise run all-wado`, with `+` meaning faster and `noise` where the ranges
overlap:

| Marker dropped at | `syntax_highlight` | `cbor_catalog` ser | `microgpt` infer | `json_twitter` de |
| ----------------- | -----------------: | -----------------: | ---------------: | ----------------: |
| 5%                |              +2.1% |              -0.7% |            -0.6% |             +2.8% |
| 10%               |              +2.9% |              +4.9% |            -0.9% |             +2.7% |
| 20%               |              +2.6% |              +4.6% |            noise |             +2.8% |
| 50%               |              +2.5% |              +4.9% |            -0.5% |             +2.6% |
| Never             |              +2.5% |              +4.9% |            -1.4% |             noise |

Fuel cannot place this line. It leaves out what the JIT makes of a splice, so it
ranked 2% above 5%, where the wall clock ranked them the other way.

A marker at the top of a function or closure body has no count around it, so a
run cannot refute it and only a count of zero overrides it. A marker that stands
keeps what it means: the path stays cold, along with every function only it
reaches, even if such a function runs often. That is the cost of this rule, and
[Reading a rare path as cold](#reading-a-rare-path-as-cold) is why a count alone
does not grant the same reading.

### A remark audits the source's markers

Where the profile drops the marker of a path that ran, the compile says so in an
optimizer remark at `--log-level info`. It names the marker, the region's count
and the count of the code around it, and what the region was found to be. The
remark audits a guess against measurement, as LLVM's
[`-pgo-warn-misexpect`][llvm-misexpect] does, so an author learns which markers
the workload contradicts.

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

Every marker stating both halves is spelled `builtin::cold_path()`, whoever
placed it. The profile finds the author's and the `?`s' markers in the source,
so nothing after reify needs to tell them from the compiler's.

The existing `-f no-branch-hinting` is a different cut. Every pass sees the
same markers and hints with it as without, and only the emitter leaves out the
hint section, the trap-derived hints included. It answers what branch hints are
worth to wasmtime; `-f no-cold-paths` answers what the markers are worth to the
whole pipeline.

### A profile changes no output

A profile, stale or wrong, only moves where the optimizer spends its effort. It
never changes what a program computes. EMI's profile stage checks this
([WEP: Compiler Fuzzing](./wep-2026-08-19-compiler-fuzzing.md#a-profile-is-a-third-axis)):
the program compiled under its recorded profile and random ones prints the same
output at every level. It is a nightly campaign, not a gate.

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
   and the compile looks each function up by its hash.
6. [x] Unlikely and cold. A compile judges each region unlikely under
   `--optimize-predictable-branch-threshold`, and cold where it never ran. It
   writes
   `cold_path()`, `unlikely_path()` or `cold_code()` by the answers, and
   overrides the author's and the `?`'s markers, with the remark. An e2e
   fixture states each mark and each remark.
7. [x] Hot and cold call sites. The summary places its hot and cold lines
   under `--optimize-profile-summary-cutoff-hot` and `-cold`. The inliner
   prices a site by it under `--optimize-inline-hot-callsite-threshold` and
   `--optimize-inline-cold-callsite-threshold`, and the benchmarks, against
   `-f no-pgo`, settle their defaults and that of
   `--optimize-predictable-branch-threshold`.
8. [x] The random-profile differential test, as EMI's profile stage.
9. [x] The `benchmark/` pilot. Each benchmark gains a workload and a committed
   profile, `http_routing` a test to carry it first, and a table compares the
   three builds the flags give.
10. [ ] The design weighed against LLVM, GCC and Go. Each option below is
    measured on the pilot's table, then adopted, or recorded as a known gap
    with what it lost to. These are options, not the decision.
    - The counts reach the optimizer. Three markers carry two bits per region,
      so nothing weighs one side against the other. LLVM keeps branch weights
      (`!prof`) and function entry counts on the IR, and its inliner raises the
      threshold at a hot call site.
    - Counts per calling context. One function holds one set of counts, so a
      function that runs rarely overall but often from one caller reads the
      same from every caller, and a rare call inside a hot region has nowhere
      to be marked. LLVM's CSPGO
      records again after inlining, and Go's PGO keys its counts by call edge.
    - Code the workloads never entered. It is compiled as unprofiled, so a
      library's paths no workload takes are never cold. GCC's
      `-fprofile-partial-training` lets the author choose between the two.
    - Stale profiles. A changed hash is said once per function at `info`, and
      counts that drift through a caller's change go unsaid. Clang warns at
      `-Wprofile-instr-out-of-date` with the share of functions that no longer
      match.
    - The stdlib snapshot. Each thread holds one profiled snapshot, so under
      `wado test` every worker builds its own for each package whose profile
      names a `core:` module.
    - The inliner's `branch_weight` guard reads the cold half, while the layout
      it protects follows the unlikely hint. The table settles which half it
      should read.
11. [x] Call-site heat in context. The inliner prices each case below by where
    the site's code now stands, and an e2e fixture states each. A third case,
    the lookahead at a hot site, is a [dead end](#dead-ends).
    - A spliced callee's sites. A site keeps the span it was written at, so its
      region still names the callee's heat. Each call carries a ceiling, the
      coolest site a splice carried it through, and its heat is the cooler of
      its region's and the ceiling (`pgo_spliced_site_heat`). A call a later
      pass rebuilds, such as an indirect call made direct, keeps the ceiling
      of the one it replaces (`pgo_rebuilt_call_heat`).
    - A candidate's own hot sites. A caller that may itself be spliced this
      round prices its hot sites plain, waiting to move into its callers. The
      wait holds only while some site this round visits may admit it, judged
      from the site's loop depth and heat, so a candidate that never moves
      takes the hot price (`pgo_unmoved_caller_hot_site`).
12. [x] Matching by hash alone. A function's hash covers where it is declared,
    a profile sums the counts of one hash, and a compile finds counts by the
    hash (`pgo_hash_matched`).

## Known gaps

- [ ] A region is measured once for all of a generic function's
  instantiations, so an instantiation the workload never ran shares the counts
<<<<<<< HEAD
  of one it ran.
- [ ] On the `benchmark/` pilot `fts` and `json_canada` ser run longer by the
  wall clock than `-f no-pgo`, for less fuel. In `fts` a hot site splices
  `f64::fmt_fixed` into a loop: native instructions fall and cycles rise, and
  what the splice costs is not known. The tables are in
||||||| 648fe5563d1
  of one it ran.
- [ ] On the `benchmark/` pilot the profile is mixed. Against `-f no-pgo` it buys
  up to 19% of fuel on the serde rows. But `microgpt`'s inference runs 46%
  longer by the wall clock for 4% more fuel: with a 2 GiB heap the PGO build is
  faster, so the hot splices make its collections dearer, and why is not known.
  `zlib` decompression and `json_twitter` serialization run about 10% longer
  for slightly less fuel. The tables are in
=======
  of one it ran. So does a function copied into another module under its
  name.
- [ ] On the `benchmark/` pilot the profile is mixed. Against `-f no-pgo` it buys
  up to 19% of fuel on the serde rows. But `microgpt`'s inference runs 46%
  longer by the wall clock for 4% more fuel: with a 2 GiB heap the PGO build is
  faster, so the hot splices make its collections dearer, and why is not known.
  `zlib` decompression and `json_twitter` serialization run about 10% longer
  for slightly less fuel. The tables are in
>>>>>>> origin/main
  [the benchmark README](../benchmark/README.md#profile-guided-optimization).
  `http_routing` records a profile but is in neither table: its `oha` run was
  not repeated under the three builds.
- [ ] The inliner reads a caller's cold code once, before it splices. A callee
  whose whole body is cold, spliced in, covers its block on both sides of the
  call, yet the calls after it are still inlined at hot-path prices.
- [ ] A splice's ceiling bounds every call it copies at the landing site's
  heat, a loop inside the callee included, though that loop runs more often
  than the site does. In `microgpt`, `softmax` runs 130 times and the closure
  its `collect` maps runs once per element, yet the closure is priced at
  `softmax`'s plain heat and stays a call. That costs inference 0.55% of fuel.

## Dead ends

### The lookahead at a hot site

The table of what a round splices is built at the plain threshold. A hot
site's price therefore counts a loopy callee that only a hot site would splice
as a call, not as its size. A table per heat, charging each call at the cooler
of its own heat and the pricing site's, closes that gap. It gained no
benchmark.

It lost on `cbor/canada/de`. `CborDeserializer::begin_seq` priced 84 at its hot
sites, over the hot threshold of 64, because its callee `begin_container`
counts as loopy, and so `begin_seq` stayed a call. That loop's back edge never
runs in the profile. Under the plain table, `begin_seq` is spliced first and
`begin_container` follows a round later. That is the faster code.

Neither a higher hot threshold nor a hot table that leaves out the cold code
recovered it. Fuel against the build before cases 1 and 2 of roadmap item 11,
with `+` meaning less fuel, measured by
`wado test -O2 --allocator freelist --report-fuel --test-name fuel benchmark`:

| Variant                             | `cbor/canada/de` | `gale/generate` | Mean over the rows |
| ----------------------------------- | ---------------: | --------------: | -----------------: |
| Plain table                         |           +0.00% |          -0.51% |             +0.98% |
| Table per heat                      |           -1.95% |          -1.04% |             +0.75% |
| Table per heat, hot threshold 96    |           -1.26% |          -0.43% |             +0.84% |
| Table per heat, hot threshold 128   |          -10.49% |          -0.45% |             +0.22% |
| Table per heat, hot price cold-free |           +0.00% |          -1.68% |             +0.87% |

### Holding a hot site to the safepoint rule

A plain site keeps a callee that reaches a safepoint out of a loop. A hot site
admits one on its price alone, which is how `f64::fmt_fixed`, priced 62 against
the hot threshold of 64, reaches the loop in `fts_run`. Every way of refusing
that splice cost more elsewhere than it gained. Against the build with the
splice, with `+` meaning faster and `-` where a row was not measured:

| Variant                                | `fts` | `json_canada` ser | `json_canada` de | `cbor_twitter` ser | `cbor_catalog` ser | `zlib` compress | Mean over the rows |
| -------------------------------------- | ----: | ----------------: | ---------------: | -----------------: | -----------------: | --------------: | -----------------: |
| Refuse any callee over the plain price |  3.8% |              0.3% |             -24% |                  - |                  - |               - |                  - |
| Only one returning a scalar or nothing |  3.8% |              4.5% |             -16% |                  - |                  - |               - |                  - |
| Only one returning nothing             |  3.6% |              0.9% |            -0.5% |             -16.0% |             -23.6% |           -3.3% |             -1.89% |
| Hot threshold 48                       |  2.2% |              0.3% |            -5.5% |               0.0% |              -0.7% |           -4.5% |             -1.30% |
| Hot threshold 60                       |  0.2% |              0.0% |            -5.5% |               0.4% |               0.5% |           -1.0% |              0.06% |

The first two rows are three or four alternating `wado run`s of those benchmarks
alone. The others are the best of two `mise run benchmark-all`.

### Training on little of the program

A region the workloads never ran is cold, so what they miss costs the run that
reaches it. `gale_gen` measures the Rust grammar, and its `-f no-pgo` build
spends 1,169M fuel. Trained on each input below, by
`wado test -O2 --allocator freelist --report-fuel --test-name fuel benchmark/gale_gen/gale_gen.wado`
and three alternating `wado run`s, with `+` meaning better than `-f no-pgo`:

| Workload                   |  Fuel | Wall clock |
| -------------------------- | ----: | ---------: |
| `JSON.g4`                  | -3.1% |      -7.5% |
| `SQLite.g4`                | -1.0% |      -4.6% |
| SQLite, TypeScript and CSS | +1.2% |      -1.5% |
| The Rust grammar itself    | +1.7% |       0.0% |

### Training on the measured input

Coverage does not explain the other rows that read slower than `-f no-pgo`.
Trained on the input it measures, `syntax_highlight` stayed where it was, as
`fts` did on a hundred times its `fts_run`, and `json_canada`'s serialization
ran 3% slower.

### Reading a zero count as not cold

A region that never ran could be only unlikely, so that code the workloads miss
would keep its inlining. It moved fuel by at most 0.5%, and the mean over the
wall clock rows by under 0.1%. Against the marks as they stand, over three
alternating rounds of `mise run all-wado`, these rows moved most:

| Row                | Wall clock |
| ------------------ | ---------: |
| `json_catalog` de  |      +4.1% |
| `cbor_twitter` ser |      +4.0% |
| `gale_gen`         |      +2.2% |
| `cbor_twitter` de  |      -2.6% |
| `fts`              |      -3.2% |
| `json_canada` de   |      -3.5% |

### Reading a rare path as cold

A path under a share of the code around it could count as cold and unlikely,
whether or not the source marks it, so a marker would decide nothing. It
recovered `syntax_highlight`, but cold covers what a region calls, and the
serializers and deserializers paid. Against the marks as they stood, over three
rotated rounds of `mise run all-wado`, with `+` meaning faster:

| Row                | Marker dropped at 10% | Cold under 5% | Cold under 10% | Cold under 20% |
| ------------------ | --------------------: | ------------: | -------------: | -------------: |
| `syntax_highlight` |                 +2.6% |         +2.2% |          +2.4% |          +1.9% |
| `gale_gen`         |                 +2.1% |         -0.6% |          -1.0% |          -5.0% |
| `json_twitter` de  |                 noise |        -29.1% |         -35.0% |         -39.9% |
| `cbor_twitter` de  |                 noise |        -71.8% |         -73.4% |         -79.4% |
| `json_twitter` ser |                 +4.2% |        -82.1% |         -83.7% |         -85.7% |
