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
   `SLICE=4 ROUNDS=5 CONNECTIONS=50 mise run benchmark-http-routing`. It keeps
   the per-(server, request) max internally, so one invocation suffices.
3. Refresh the README Environment line versions: `mise exec -- node --version`,
   `mise exec -- bun --version`, `rustc --version`, `cc --version | head -1`
   (wasmtime version is the vendored `vendor/wasmtime` workspace version).
4. Update the tables, following README.md's existing layout. http-routing is
   req/s (higher is better) and lists a curated subset of the measured requests.
5. wasm-size: `mise run report-wasm-size`, then update `wasm-size/README.md`.

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
