---
name: benchmark
description: Run performance benchmarks and wasm size reports, then update README files with new results.
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
- wasm-size needs `rustup target add wasm32-wasip1` and Moonbit
  (`curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash`, then
  `moon update` in each `wasm-size/*` dir).

## Procedure

1. Run `mise run benchmark-all` **three times**; per implementation keep the
   fastest value (throttling only ever slows things down). It runs 10
   benchmarks serially: count-prime, mandelbrot, sieve, zlib, fts,
   json-{twitter,canada,catalog}, sqlite-parse, syntax-highlight.
2. Run http-routing separately (needs `oha` + pinned cores):
   `SLICE=4 ROUNDS=5 CONNECTIONS=50 mise run benchmark-http-routing`. It keeps
   the per-(server, request) max internally, so one invocation suffices.
3. Refresh the README Environment line versions: `mise exec -- node --version`,
   `mise exec -- bun --version`, `rustc --version`, `cc --version | head -1`
   (wasmtime version is the vendored `vendor/wasmtime` workspace version).
4. Update the tables, following README.md's existing layout. http-routing is
   req/s (higher is better) and lists a curated subset of the measured requests.
5. wasm-size, when asked: `mise run report-wasm-size`, then update
   `wasm-size/README.md`.

## Reading output

Each program prints a throughput line — `<rate> <unit>/s   (<ms> ms/iter,
<n> iter)` — per phase (zlib prints two phases, `Compress:`/`Decompress:`).
Read the rate and the ms/iter straight off; the iteration count auto-calibrates
to ~1s, so there is no total to report. Units: numbers/s (count-prime, sieve),
px/s (mandelbrot), conversions/s (fts), MB/s (zlib, json-\*, sqlite-parse,
syntax-highlight), req/s (http-routing). `vs best` = fastest rate / this rate.
Implementations per benchmark:

- count-prime / mandelbrot / sieve: C, JavaScript, Wado
- fts: Rust, C, Wado
- zlib: zlib-rs, Wado
- json-\*: serde_json, JSON.parse, Wado (catalog also Wado v2)
- sqlite-parse: sqlparser-rs, Wado
- syntax-highlight: Prism, Lezer, tree-sitter, Shiki, Wado
- http-routing: wado serve, Hono (Node/Bun), Axum

## Workload sizing

Benchmarks auto-calibrate their iteration count to run ~1s, so no manual
denomination is needed. Keep each single iteration well under 1s: count-prime
(limit) and fts (conversion count) are sized so one run is ~100ms, giving ~10
calibrated iterations. If a workload's one-shot cost exceeds ~1s it will report
a single iteration — shrink its problem size across every language
implementation of that benchmark to fix it.

## Notes

- Tool versions come from mise, not the system.
- Cloud VMs are noisy; best-of-three absorbs the drift.
