# Fuel Benchmarks

## Context

The `Benchmark` workflow timed each benchmark's throughput on a GitHub-hosted
runner and compared it with the history on gh-pages. A hosted runner is a
different machine from one run to the next, so the history mixed machines. A
pull request was warned at 2x and failed at 4x, since anything finer was noise.
No regression short of 2x could show.

Wall clock is still the only measure that compares Wado with C, Rust and
JavaScript. That comparison needs a quiet machine, not a shared runner.

## Decision

CI measures fuel, not time. Fuel is wasmtime's count of the Wasm instructions
the guest ran, about one unit each. It does not depend on the host, so one run
on any machine gives the figure that only a change to the program or to
wasmtime moves.

Fuel leaves out what the host does for the guest, GC included, by design: that
work depends on the host, and counting it would give up the determinism. A
change that allocates more costs no fuel for the allocation itself. The
wall-clock benchmarks in `benchmark/` stay as they are and show that work.

### `core:fuel` measures a region of a test

`measure(label, f)` runs `f` and reports the fuel it spent as region `label` of
the running test. `f`'s result passes through `builtin::black_box`, so its work
stays in the region. The setup around the call is left out.

`wado test --report-fuel` shows each region on its test's line and writes every
metered test to `build/fuel.json`. Without `--report-fuel`, `measure` just runs
`f`, so the block stays an ordinary test.

The host interface is `core:fuel/fuel-host`: `spent` answers the fuel the
running test has spent, and `report` records a region. wasmtime writes its fuel
counter back to the store before every call, so `spent` is exact at the call.
Only `wado test` links the interface. A program for any other world that
reaches `core:fuel` does not compile, as with `core:eval`
(`TEST_WORLD_ONLY_IMPORT`).

### A metered run fixes the insecure randomness

Under `--report-fuel`, `wasi:random/insecure` and `insecure-seed` answer the
same on every run, as Kiln generators and `core:eval` programs already get.
`HashMap::new` hashes under `insecure-seed`, and the seed changes how far its
probes go, so without this a program using one spent different fuel on each
run. `insecure-seed` promises no unpredictability, so this changes no program's
meaning. `wasi:random/random` stays unpredictable.

### The benchmarks keep both measures

Each benchmark program keeps `export fn run()`, timed by `core:benchmark`
against the other implementations, and gains a `test "fuel"` block that
measures the same phases once. The block reads its input with `#include_bytes`,
since a test reaches no file.

`benchmark/fuel.ts` runs the blocks at `-O1`, `-O2` and `-O3`, as the
wall-clock series did, with `--allocator freelist`, the allocator a shipped
program gets. It names each region `<benchmark directory>/<label> (<level>)`.

### CI holds fuel exactly and records it

`benchmark/fuel.json` is the baseline, every level included. A pull request
fails on any difference from it, either way, and names each series that moved.
Fuel has no noise to tolerate, so every level is held, not only `-O2` as the
wall-clock gate was. A push to main records the series on gh-pages under
`benchmarks/fuel`.

`mise run update-baselines` rewrites it together with the stdlib coverage
baseline, since a change to the compiler or the stdlib moves either. A change
that moves fuel commits the result, so the diff shows its cost.

The wall-clock series on gh-pages stops. Its history stays where it is.

## Known gaps

- Fuel prices every instruction alike, so a branch shape or a cache effect does
  not show.
- A region that reads `wasi:random/random`, a clock, or I/O can spend different
  fuel on each run.
- Two pull requests that each update the baseline can merge into a main whose
  fuel matches neither. The push still records main's figures, and the next
  pull request fails until the baseline is updated.
