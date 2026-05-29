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
   `mise exec -- zig version`, `wasmtime --version`, `rustc --version`,
   `cc --version | head -1`.
4. Update the tables, following README.md's existing layout. http-routing is
   req/s (higher is better) and lists a curated subset of the measured requests.
5. wasm-size, when asked: `mise run report-wasm-size`, then update
   `wasm-size/README.md`.

## Reading output

Each program prints `Elapsed: … ms` (zlib also `Compress:`/`Decompress:`).
Round Wado's fractional ms to whole; report the total for looped benchmarks.
Implementations per benchmark:

- count-prime / mandelbrot / sieve: C, JavaScript, Wado
- fts: Zig, Rust, C, Wado
- zlib: zlib-rs, Wado
- json-\*: serde_json, JSON.parse, Wado (catalog also Wado v2)
- sqlite-parse: sqlparser-rs, Wado
- syntax-highlight: Prism, Lezer, tree-sitter, Shiki, Wado
- http-routing: wado serve, Hono (Node/Bun), Axum

## Denomination

Light workloads are scaled in-program so results land in whole ms — just run
and report. sieve → 100M, fts → 5M (problem size); JSON → ×10, zlib → ×100
(`iterations` / `b.iterations`). JSON/zlib totals are warm steady-state. To
retune, change the factor across every language implementation of that benchmark.

## Notes

- Tool versions come from mise, not the system.
- Cloud VMs are noisy; best-of-three absorbs the drift.
