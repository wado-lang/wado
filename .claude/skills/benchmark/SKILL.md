---
name: benchmark
description: Run Wado performance benchmarks (count-prime, mandelbrot, zlib, json, http-routing, …) and wasm-size reports, then update the benchmark/ and wasm-size/ README files. Use when asked to benchmark Wado, measure performance, or refresh the benchmark/wasm-size results.
---

# Benchmark

Run the benchmarks and update `benchmark/README.md` (and `wasm-size/README.md`).

## Prerequisites

```sh
mise run on-task-started
```

- `vendor/wasmtime` submodule must exist (the SessionStart hook handles it;
  otherwise `git submodule update --init --recommend-shallow vendor/wasmtime`).
- http-routing needs `oha` (`cargo install oha`); `bun` is mise-managed.
- gale-gen's and sqlite-parse's ANTLR4 references need `java` (sqlite-parse also
  needs `javac`); the jar is fetched to `~/.cache/gale`. Those rows are skipped
  if the tool is absent.
- wasm-size needs `rustup target add wasm32-wasip1` and Moonbit
  (`curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash`, then
  `moon update` in each `wasm-size/*` dir).

## Procedure

1. Run `mise run benchmark-all` **three times**, each to its own log, then pick
   per row with `node benchmark/pick.ts run1.log run2.log run3.log` (throttling
   only ever slows things down). Use that tool rather than reading the logs by
   eye: it keys rows by (task, implementation, phase) and selects on ms/iter, so
   a rate that rounds to a tie across runs cannot pair with the wrong ms/iter.
   Which benchmarks run, and in what order, is `benchmark/mise.toml`'s `all`
   task.
2. Run http-routing separately (needs `oha` + pinned cores):
   `SLICE=10 ROUNDS=3 SHAPES="1 4" mise run benchmark-http-routing`. It keeps
   the per-(server, request) max internally, so one invocation suffices, and it
   measures one worker shape per entry in `SHAPES`. Add `HEADROOM_CHECK=1` when
   `CONNECTIONS_PER_WORKER`, `OHA_CORE_COUNT` or `SHAPES` changed, to confirm
   `oha` was not the ceiling.
3. Refresh the README Environment line versions: `mise exec -- node --version`,
   `mise exec -- bun --version`, `rustc --version`, `cc --version | head -1`
   (wasmtime version is the vendored `vendor/wasmtime` workspace version).
4. Update the tables, following README.md's existing layout. http-routing is
   req/s (higher is better), one table per worker shape. Keep measured figures
   inside tables — prose around them is not re-measured and drifts.
5. wasm-size: `mise run report-wasm-size`, then update `wasm-size/README.md`.

## Sweeping a compiler knob

`WADO_BENCH_FLAGS` is appended to every `wado compile` / `wado run` the harness
issues, so an arm costs a benchmark run rather than a release rebuild:

```sh
for t in 13 20 32; do
  WADO_BENCH_FLAGS="--optimize-inline-threshold $t" mise run benchmark-all > thr$t.log 2>&1
done
node benchmark/pick.ts thr13.log thr20.log thr32.log
```

Read the sweep with `pick.ts` the same way as a best-of-three: it keys rows by
(task, implementation, phase), so the "best" column names the winning arm per
row. Only a knob every compiling subcommand accepts can be swept this way — the
harness spends the flags on `wado run`, so one added to `compile` alone is one
the sweep cannot reach. The knob a sweep settles on is a default in
`optimize.rs`, not a flag the README's numbers were taken under — re-run the
suite unflagged before updating the tables.

Comparing the settled default against `origin/main` is a different measurement,
and `WADO_BIN` plus `ab.ts` is how: see the `wado-performance` skill.

## Reading output

Each program prints a throughput line — `<rate> <unit>/s   (<ms> ms/iter,
<n> iter)` — per phase (zlib prints two phases, `Compress:`/`Decompress:`).
Read the rate and the ms/iter straight off; the iteration count auto-calibrates
to ~1s, so there is no total to report. The unit is the benchmark's own
(numbers/s, px/s, conversions/s, MB/s, req/s); `vs best` = fastest rate / this
rate.

Each benchmark prints the implementations it compared, so read them off the run
rather than from a list here. Two are conditional: the ANTLR4 rows for
`sqlite-parse` and `gale-gen` are skipped when `java` is absent, and the run
says `SKIP:` when it drops one.

## Workload sizing

Benchmarks auto-calibrate their iteration count to run ~1s, so no manual
denomination is needed. A workload whose single iteration approaches that
reports one iteration and stops averaging — shrink its problem size, across
every language implementation of that benchmark, until it calibrates again.

## Notes

- Tool versions come from mise, not the system.
- Cloud VMs are noisy; best-of-three absorbs the drift.
