---
name: benchmark
description: Measure Wado's performance or the size of the Wasm it generates, and refresh the benchmark/ and wasm-size/ README files. Use for any benchmarking or size-measurement request, whichever benchmark it names.
---

# Benchmark

Run the benchmarks and update `benchmark/README.md` and `wasm-size/README.md`.

## Prerequisites

- `mise run on-task-started`, and the `vendor/wasmtime` submodule.
- http-routing needs `oha` (`cargo install oha`).
- The ANTLR4 rows of gale-gen and sqlite-parse need `java` (and `javac`); the
  run prints `SKIP:` without them.
- wasm-size needs `rustup target add wasm32-wasip1` and Moonbit
  (`curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash`, then
  `moon update` in each `wasm-size/*` dir).

## Procedure

1. Run `mise run benchmark-all` three times, each to its own log, and pick per
   row with `node benchmark/pick.ts /tmp/run1.log /tmp/run2.log /tmp/run3.log`.
2. Run http-routing on its own:
   `SLICE=10 ROUNDS=3 SHAPES="1 4" mise run benchmark-http-routing`. Add
   `HEADROOM_CHECK=1` when `CONNECTIONS_PER_WORKER`, `OHA_CORE_COUNT` or
   `SHAPES` changed, to confirm `oha` was not the ceiling.
3. Refresh the README's Environment line from `mise exec -- node --version`,
   `mise exec -- bun --version`, `rustc --version`, `cc --version | head -1`,
   and the `vendor/wasmtime` workspace version.
4. Update the tables in the README's existing layout; http-routing is one req/s
   table per worker shape.
5. `mise run report-wasm-size`, then update `wasm-size/README.md`.

## Sweeping a Compiler Knob

`WADO_BENCH_FLAGS` is appended to every `wado compile` and `wado run` the harness
issues, so an arm costs a run, not a rebuild. Only a knob both accept can be
swept this way.

```sh
set -e  # a failed arm would leave pick.ts choosing among the rest
for t in 13 20 32; do
  WADO_BENCH_FLAGS="--optimize-inline-threshold $t" mise run benchmark-all > /tmp/thr$t.log 2>&1
done
node benchmark/pick.ts /tmp/thr13.log /tmp/thr20.log /tmp/thr32.log
```

The winner becomes a default in `optimize.rs`; re-run the suite unflagged before
updating the tables. Comparing it against `origin/main` is the
`wado-performance` skill's A/B.

## Reading Output

Each program prints `<rate> <unit>/s   (<ms> ms/iter, <n> iter)` per phase. The
iteration count calibrates to about a second; a workload whose single iteration
approaches that stops averaging, so shrink it in every language's
implementation. `vs best` is the fastest rate over this one.
