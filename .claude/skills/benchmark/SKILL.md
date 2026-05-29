---
name: benchmark
description: Run performance benchmarks and wasm size reports, then update README files with new results.
---

# Benchmark

Run all benchmarks (compute, JSON, compression, parsing, HTTP routing) and
the wasm size comparison, then update the README files.

## Prerequisites

```sh
mise run on-task-started   # install mise and project tools
```

The `wado-compiler` build needs the `vendor/wasmtime` submodule. The
SessionStart hook initializes it automatically; if a build fails with a
missing `vendor/wasmtime/.../Cargo.toml`, run:

```sh
git submodule update --init --recommend-shallow vendor/wasmtime
```

For the **http-routing** benchmark only:

```sh
cargo install oha   # HTTP load generator (compiles from source, ~minutes)
```

`bun` is mise-managed (installed by the benchmark's own `mise.toml`); `taskset`
is used for CPU pinning when available. The Bun row is skipped gracefully if
`bun` is missing.

For the **wasm-size** report only:

```sh
rustup target add wasm32-wasip1                                # Rust wasm builds
curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash  # Moonbit builds
export PATH="$HOME/.moon/bin:$PATH"
cd wasm-size/hello_world && moon update
cd ../pi_approx && moon update
# If `mise trust` errors appear: mise trust wasm-size/mise.toml
```

## Procedure

### 1. Run `benchmark-all` three times (best of three)

```sh
mise run benchmark-all   # run 1 (cold caches — warmup)
mise run benchmark-all   # run 2
mise run benchmark-all   # run 3
```

`benchmark-all` runs these **10** benchmarks serially (`MISE_JOBS=1`):
count-prime, mandelbrot, sieve, zlib, fts, json-twitter, json-canada,
json-catalog, sqlite-parse, syntax-highlight.

Take the **best (fastest) value per implementation** across the three runs
(run 1 is effectively a warmup and usually loses). Cloud VMs throttle, and
throttling only ever lowers throughput, so the fastest run is the cleanest
estimate of true capacity.

### 2. Run the http-routing benchmark (separate)

http-routing is **not** part of `benchmark-all` (it needs `oha` + pinned
cores). Run it on its own:

```sh
SLICE=4 ROUNDS=5 CONNECTIONS=50 mise run benchmark-http-routing
```

Tunables (env vars): `SLICE` (seconds per slice, default 3), `ROUNDS`
(rotation rounds, per-server max is kept, default 3), `CONNECTIONS`
(concurrent connections, default 50), `OHA_CORE_COUNT` (load-generator core
budget, default `nproc/4`). It already keeps the per-(server,request) max
over rounds internally, so a single invocation suffices.

### 3. Get tool versions

```sh
cd benchmark && mise exec -- node --version && mise exec -- zig version
wasmtime --version
rustc --version
cc --version | head -1
```

### 4. Update benchmark/README.md

- **Environment line**: update the Wado date and any changed tool versions.
- **Result tables**: update times and `vs best` ratios (baseline is the
  fastest = 1.00x). Sort each table fastest-first. Use commas for thousands
  (`3,140`). Round ratios to 2 decimals (compute ratios from the raw
  values, then round the displayed time).
- The http-routing table is **throughput (req/s, higher is better)** and
  shows a curated 5-row subset of the 7 measured requests — keep the same 5
  rows, just refresh the numbers (and the prose ranges below it).

### 5. wasm-size report (optional, when asked)

```sh
mise run report-wasm-size
```

Then update `wasm-size/README.md` size tables for all languages.

## Reading benchmark output

All time tables are in **whole milliseconds**. Parse these lines per run:

| Benchmark        | Implementations                                                  | Line to parse                                      |
| ---------------- | ---------------------------------------------------------------- | -------------------------------------------------- |
| count-prime      | C, JavaScript, Wado                                              | `Elapsed: N ms` (Wado: `Elapsed: N.NNN ms`)        |
| mandelbrot       | C, JavaScript, Wado                                              | `Elapsed: N.NN ms`                                 |
| sieve            | C, JavaScript, Wado                                              | `Elapsed: N ms`                                    |
| fts              | Zig, Rust, C, Wado                                               | `Elapsed: N ms`                                    |
| zlib             | zlib-rs, Wado                                                    | `Compress:`, `Decompress:`, total (`Elapsed:`/sum) |
| json-twitter     | serde_json, JSON.parse, Wado                                     | `Elapsed: N ms` (Wado: `... ms total`)             |
| json-canada      | serde_json, JSON.parse, Wado                                     | `Elapsed: N ms total`                              |
| json-catalog     | serde_json, JSON.parse, Wado v2, Wado core:json                  | `Elapsed: N ms total`                              |
| sqlite-parse     | sqlparser-rs, Wado (Gale)                                        | `Elapsed: N ms (100 iterations)` / `... ms total`  |
| syntax-highlight | Prism, Lezer, tree-sitter (native + JS/WASM), Shiki, Wado (Gale) | `Elapsed: N ms (100 iterations)`                   |
| http-routing     | wado serve, Hono (Node/Bun), Axum                                | results table, req/s per request                   |

Round Wado's `N.NNN ms` to whole ms for the table. For looped benchmarks
report the **total** over the iterations (matching the existing tables).

## Denomination (keeping every result in whole ms)

The lightest workloads are scaled so integer-ms rounding never loses signal.
The scale factor is **baked into the benchmark programs** — just run and
report; do not multiply by hand:

- **sieve** counts to **100M** (problem size; one allocation, so no
  GC-churn artifact from looping).
- **fts** does **5M** conversions (problem size).
- **JSON** (twitter/canada/catalog, incl. v2/v3) iterate **×10** — the
  iteration count lives in `iterations` (Rust/JS) / `b.iterations` (Wado).
- **zlib** iterates **×100**.

JSON/zlib totals are warm steady-state (the Wado framework auto-warms 1 of
N), not a single cold parse. count-prime, mandelbrot, sqlite-parse, and
syntax-highlight are already heavy enough and are not scaled.

If a benchmark still reads too coarse (e.g. serde_json twitter ≈ 9 ms at
×10), bump its factor (×10 → ×100) in the program and re-measure. To change
a factor, edit the `iterations` / `b.iterations` (JSON, zlib) or the problem
size (sieve `limit`, fts `n`) consistently across **all** language
implementations of that benchmark.

## Notes

- Benchmarks use mise-managed tool versions, not system versions.
- Cloud VMs are noisy: absolute times drift, but within-run ratios are
  fairly stable. Best-of-three handles the drift.
- `mise run report-wasm-size` builds all language targets with
  size-optimized flags and reports `.wasm` file sizes.
