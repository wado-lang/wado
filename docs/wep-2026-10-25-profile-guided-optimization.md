# WEP: Profile-Guided Optimization

## Context

The optimizer already leans on cold paths. `builtin::cold_path()` marks code
that rarely runs, and every pass that reads the marker acts on it. Among them:

- `nir/cold_outline` moves the marked region into a function of its own.
- `nir/inline` prices a callee without its cold tail. It does not inline at a
  cold call site, nor anywhere in a function that runs only once a cold path
  is taken.
- `const_object_globalization`, `param_spec`, `match_to_switch` and `peephole`
  leave cold code as it is.
- `infer_branch_hints` turns the marker into a Wasm branch hint, and wasmtime
  lays out the cold side out of line.

Every one of these marks is a guess. An author writes `cold_path()` where they
believe a path is rare. `infer_branch_hints` adds the paths that end in a trap.
Nothing tells the compiler which paths a real run takes, so a guess that is
wrong stays wrong, and a rare path nobody marked is optimized as if it were hot.

The compiler already measures which code runs. `wado test --coverage` plans a
region for each branch and body from the AST, inserts a probe at the start of
each one, and reports which ones ran
([WEP: Test Coverage](./wep-2026-09-28-test-coverage.md)). Regions do not depend
on `-O`, so a region measured in one build names the same source in any other.
A probe records only that a region ran, not how often.

`benchmark/` measures Wado on real programs, and most benchmarks already call
their kernel from a `test` block. `http_routing`, a `wasi:http/service`, has
none yet. `benchmark/` is where a change to the optimizer is judged, so
it is where profile-guided optimization should prove itself first.

## Decision

A package states its real workload as tests marked `#[workload]`. Running them
records how often each region ran into a profile, and the package commits it.
Every later compile reads the profile and marks each region that never ran as
cold, so the optimizer's existing cold-path machinery acts on measurement
instead of guesses.

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
`#[timeout_ms]` and `#[synopsis]`. With `#[expect_trap]` or `#[TODO]` it is a
compile error: such a test traps by design, and a workload that traps fails
the recording (below).

A workload should be deterministic and small. Its counts then come out the same
on every run, so the committed profile changes only when the code or the
workload does. A workload that reads a clock or randomness still works, but its
profile churns.

An attribute was chosen over a new `workload` block. A block would add a
keyword to the reserved vocabulary, and it would not run under `wado test`.

### `wado test --pgo` records the profile

`wado test --pgo` compiles the package's workloads with probes and runs only
them. Recording belongs to the test runner, as coverage does, since a workload
is a test. The flag is not named `--profile`, which already means the sampling
profiler of `wado run --profile` and `wado test --profile`. A probe counts
every run of its region. It measures the package's modules, its
dependencies' and the `core:` modules, since a workload's hot path runs through
all three.

The recording compile does not read an existing profile. A profile is then a
function of the source and the workloads alone, and recording twice gives the
same file.

A workload that traps, or fails an assertion, fails the recording. Its counts
would stop at the trap, and a profile from a broken workload would mislead.

### Coverage counts too

The probe that coverage inserts counts every run, in every coverage build. The
first run of a region still calls the host at once, so a test that traps keeps
its hits. A test that returns hands its counts to the runner when it ends. A
test that traps contributes only its hits, which count as one run each. LCOV's
`DA` and `BRDA` lines carry the counts.

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

The profile is a text file, so the repository holds no binary for it. A file
under `build/` would not reach CI or a release build, so their output would
differ from the author's.

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

Every name in a profile resolves the same on any machine and from any entry:

- A module of the package is a file relative to the package root, or for a
  program without a manifest, relative to the program's directory.
- A dependency's module is the dependency's package name and the file relative
  to its root, never the cache path it was fetched to.
- A `core:` module is its import path.

Coverage names a module relative to the entry module's directory instead, which
two entries in different directories would name apart.

Under each module the profile lists each function that holds a measured region,
with:

- its name and its line, for a reader;
- the hash of its region plan;
- the count of each of its regions, numbered from zero within the function.

Coverage numbers regions across the whole program, so editing one function
would move every later function's numbers. Numbering within the function keeps
an edit to one from reaching the counts of another.

### A function whose plan changed has no profile

A compile reads a function's counts only when the hash of its current region
plan equals the recorded one. The plan is each region's kind and its span
relative to the function's start. It changes with the function's text, and also
with what outside the text decides its regions: a callee becoming `!`-typed
starts or ends a region, and `#[coverage(off)]` on the enclosing `impl` or
module removes them all.

A function whose hash differs is compiled as if no profile existed. A stale
profile therefore costs only the gain on what changed, and it never marks the
wrong region.

A compile that discards a function's counts says so at `--log-level info`, so
an author learns when to record again.

### A region that never ran is marked cold

Reify inserts a `builtin::cold_path()` at the start of each region whose count
is zero and whose enclosing region ran, in every function the profile covers.
Reify is where coverage inserts its probes, so a region means the same thing to
both. From there, the optimizer's existing passes act on the marker as they do
on one the author wrote.

A function whose body region never ran is cold as a whole, with one marker at
its start. A region inside one already marked gets none: `cold_outline` would
otherwise outline each branch of a function that never ran into a function of
its own, and both sides of its `if`s would carry a marker. A region that ran
any number of times gets no marker.

Each worker thread seeds its compiles from a snapshot of the reified standard
library, built once. The thread builds it with the `core:` counts already
applied, so a profiled compile seeds from the snapshot as a plain one does. A
run whose entries carry different `core:` counts builds one snapshot per set of
counts. Using the counts beyond zero, to optimize a
hot path harder than the rest, is left to a later proposal.

### The source wins over the profile, and says so

An author's `cold_path()` in a region the workload ran stays in force. It
states an intent the workload may not cover, such as an error path a test
exercises often. The compile reports the conflict as an optimizer remark at
`--log-level info`, naming the marker and the count. The remark audits the
author's guesses against measurement.

### Two flags switch the marks off

| Flag               | Effect                                                      |
| ------------------ | ----------------------------------------------------------- |
| `-f no-pgo`        | Ignore the profile.                                         |
| `-f no-cold-paths` | Ignore every `cold_path()`, the author's and the profile's. |

`-f no-cold-paths` drops the markers before any pass reads them, so
`cold_outline`, the inliner and the branch hints all see none. With both flags,
three builds of one program can be compared: no cold paths, the author's only,
and the author's plus the profile's.

`-f no-cold-paths` leaves alone the hints `infer_branch_hints` derives from
traps. Those follow from what the program does when it traps, not from a guess.

The existing `-f no-branch-hinting` is a different cut. It drops the markers
only at WIR build, after every NIR pass has read them, and it also skips the
hints derived from traps. It answers what branch hints are worth to wasmtime;
`-f no-cold-paths` answers what the markers are worth to the whole pipeline.

### A profile changes no output

A profile, stale or wrong, only moves where the optimizer spends its effort. It
never changes what a program computes. A test checks this the way
[EMI](./optimizer.md#differential-testing-emi) checks injected dead code: the
same program compiled with random profiles prints the same output at every
level.

## Roadmap

1. [ ] Counting probes. The probe counts, the runner reads the counts when a
   test returns, and LCOV carries them. `mise run test-stdlib` under
   `--coverage`, before and after, in a table with the command beside it,
   shows what counting costs.
2. [ ] `#[workload]`. The attribute is parsed and run as a test, and spec
   fixtures state it.
3. [ ] `wado test --pgo`. It records the workloads into `pgo/`, with the
   per-function hash.
4. [ ] Applying the profile. Reify marks the regions that never ran, a function
   whose hash differs is skipped, and the conflict remark is emitted. An e2e
   fixture can name a profile in its `__DATA__`, so a fixture states which
   regions end up cold.
5. [ ] `-f no-pgo` and `-f no-cold-paths`.
6. [ ] The random-profile differential test.
7. [ ] The `benchmark/` pilot. Each benchmark gains a workload and a committed
   profile, `http_routing` a test to carry it first, and a table compares the
   three builds the flags give. A workload
   runs a smaller input than the benchmark measures, so the table does not
   judge a profile on the run that trained it.

## Known gaps

- [ ] A region that starts in an expression, the right operand of `&&` or
  `||`, has no statement position for a marker.
- [ ] A region is measured once for all of a generic function's
  instantiations, so an instantiation the workload never ran shares the counts
  of one it ran.
- [ ] A workload that traps fails the recording, so a package cannot profile a
  path that ends in a trap.
