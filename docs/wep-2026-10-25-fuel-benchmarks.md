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
the guest ran, each priced near the time it takes. It does not depend on the
host, so one run on any machine gives the figure that only a change to the
program, to the prices or to wasmtime moves.

### Fuel prices each operator near its time

wasmtime's default prices every operator at one, so an integer division, a
call and an allocation each cost what an `i32.add` does. `wado-host/src/fuel.rs`
gives `Config::operator_cost` a table instead, and every metered engine takes
it: `--report-fuel`, Kiln generators, `core:eval` and the e2e harness. A fuel
budget then bounds time, which is what the harness and `core:eval` use one for.

- An `i32.add` costs 2. A price is a `u8`, so a finer unit would push the
  heaviest operators past 255. A coarser one could not price a local or a
  constant at half an add, which is about what they cost once Cranelift has
  them in registers.
- The classes are the Internet Computer's instruction weights
  (`instruction_to_cost` in dfinity/ic), benchmarked per operator and in line
  with Agner Fog's tables: a division costs 10 adds, a call 5, an `f64.sqrt` 8.
  No price list is shared across runtimes, and that one is measured on a JIT
  rather than set adversarially as a chain's gas is.
- What that table does not price for wasmtime is priced from microbenchmarks
  of each operator against `i32.add` under Cranelift with the copying
  collector: `struct.new` and the `array.new` family, casts, `call_ref`, and
  bulk memory. wasmtime charges a bulk operator a flat price plus a whole
  number per element. It fills an array one element at a time, so those
  operators pay per element. A copy or a fill of linear memory is a `memmove`
  or `memset` whose time per byte rounds to nothing, so it pays flat.

The `Benchmark` workflow stores the series under a suite name that changes with
the prices, since figures under two price lists do not compare. Each list starts
a history of its own on the page, and the older ones stay in its data.

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

Under `wado test --report-fuel`, `wasi:random/insecure` and `insecure-seed`
answer the same on every run, as Kiln generators and `core:eval` programs
already get. `HashMap::new` hashes under `insecure-seed`, and the seed changes
how far its probes go, so without this a program using one spent different fuel
on each run. `wasi:random/random` stays unpredictable.

Only a test gets this. `insecure-seed` exists to seed a hash map's DoS
protection, so `wado run` and `wado serve` keep the host's seed under
`--report-fuel`, and their fuel can vary with it.

### The benchmarks keep both measures

Each benchmark program keeps `export fn run()`, timed by `core:benchmark`
against the other implementations, and gains a `test "fuel"` block that
measures the same phases once. The block reads its input with `#include_bytes`,
since a test reaches no file.

`benchmark/fuel.ts` runs the blocks at `-O1`, `-O2` and `-O3`, as the
wall-clock series did, with `--allocator freelist`, the allocator a shipped
program gets. It names each series `<label> (<level>)`. A label is a path that
starts with the benchmark's category, so the series of one category sort
together: `serde/` (`serde/cbor/twitter/ser`), `gale/`, `codec/`, `compute/`
and `ml/`.

### CI holds fuel exactly and records it

`benchmark/fuel.json` is the baseline, `-O2` alone. CI's `test-fuel` job, a
test like any other, fails on any difference from it, either way, and names
each series that moved. `-O2` is the level a performance change answers for;
`-O1` and `-O3` are for reference, so the `Benchmark` workflow records them and
nothing holds them.

The `Benchmark` workflow only records: a push to main stores the series on
gh-pages under `benchmarks/fuel`, beside the wasm sizes. It no longer runs on a
pull request. An exact test leaves a threshold nothing to catch for fuel, and
the wasm-size threshold, which failed a pull request at 130%, goes with it.

`mise run update-baselines` rewrites it together with the stdlib coverage
baseline, since a change to the compiler or the stdlib moves either. A change
that moves fuel commits the result, so the diff shows its cost.

The wall-clock series on gh-pages stops. Its history stays where it is.

## Known gaps

- A price is one figure per operator, so a branch shape or a cache effect does
  not show.
- An allocation's share of collection grows with the live set, and a
  surviving object's with its size, which no per-operator price can see. The
  prices assume a small live set; the wall-clock benchmarks in `benchmark/`
  show the rest.
- A copy pays nothing per byte, so a large one is priced low. A copy whose
  length is a constant of at most 128 bytes runs inline at almost no cost, and
  pays the same flat price as one that calls `memmove`.
- An array pays the same per element whatever its element type, though an `i8`
  element costs about a third of an `i32` one.
- No pull request is held to a wasm size. A growth shows only in the record on
  gh-pages, after the merge.
- A region that reads `wasi:random/random`, a clock, or I/O can spend different
  fuel on each run.
- Two pull requests that each update the baseline can merge into a main whose
  fuel matches neither. The push still records main's figures, and the next
  pull request fails until the baseline is updated.
