# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-06-25, wasmtime 46.0.0, gcc 13.3.0, rustc 1.96.0,
Node.js v26.3.1, Bun 1.3.11, Linux x86_64.

All figures report **throughput** (work per second; higher is better) with
the per-iteration time in parentheses. Each phase warms up once, then
auto-calibrates its iteration count so the timed loop runs for about a
second; the total wall time is therefore ~constant and omitted, leaving the
per-iteration time (the mean over the calibrated iterations). Native rows are
optimized builds — C with `gcc -O3`, Rust release, Wado `-O2` — and
JavaScript runs on Node.js. Pure-computation benchmarks name the language
alone; library benchmarks name it as `language (library)`. Throughput uses
each workload's natural unit: numbers/s (prime counting, sieve), px/s
(mandelbrot), conversions/s (float-to-string), and MB/s for the byte-oriented
workloads (JSON, zlib, SQL parsing, syntax highlighting). `vs best` is the
ratio of the fastest row's throughput to this row's (1.00x = fastest).

## Prime Counting

Count primes up to 1M (integer arithmetic, trial division).

| Implementation | Throughput    | ms/iter    | vs best |
| -------------- | ------------- | ---------- | ------- |
| C              | 7.94 M nums/s | 125.880 ms | 1.00x   |
| **Wado**       | 7.77 M nums/s | 128.653 ms | 1.02x   |
| JavaScript     | 7.75 M nums/s | 129.078 ms | 1.03x   |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| JavaScript     | 4.07 M px/s | 193.400 ms | 1.00x   |
| **Wado**       | 4.05 M px/s | 194.049 ms | 1.00x   |
| C              | 3.99 M px/s | 197.342 ms | 1.02x   |

## Sieve

Sieve of Eratosthenes up to 10M (array operations). The sieve buffer is
allocated once and reset each iteration, so allocation and first-touch page
faults stay out of the timed region — the loop measures steady-state array
traffic, which keeps run-to-run spread within ~1-2%.

| Implementation | Throughput      | ms/iter    | vs best |
| -------------- | --------------- | ---------- | ------- |
| C              | 282.51 M nums/s | 35.397 ms  | 1.00x   |
| JavaScript     | 204.19 M nums/s | 48.973 ms  | 1.38x   |
| **Wado**       | 99.08 M nums/s  | 100.925 ms | 2.85x   |

## Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 13.46 M conv/s | 74.268 ms  | 1.00x   |
| **Wado** (fpfmt) | 7.84 M conv/s  | 127.501 ms | 1.72x   |
| C (printf)       | 7.58 M conv/s  | 131.980 ms | 1.78x   |

## Compression: compress

zlib compression of twitter.json (631514 bytes).

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 226.51 MB/s | 2.788 ms  | 1.00x   |
| JavaScript (node:zlib) | 155.78 MB/s | 4.054 ms  | 1.45x   |
| **Wado** (core:zlib)   | 31.27 MB/s  | 20.197 ms | 7.24x   |

## Compression: decompress

zlib decompression of twitter.json (631514 bytes).

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 2.05 GB/s   | 0.308 ms | 1.00x   |
| JavaScript (node:zlib) | 1.10 GB/s   | 0.573 ms | 1.86x   |
| **Wado** (core:zlib)   | 165.52 MB/s | 3.815 ms | 12.39x  |

## JSON: twitter

Deserialize twitter.json (631514 bytes).

| Implementation          | Throughput  | ms/iter  | vs best |
| ----------------------- | ----------- | -------- | ------- |
| Rust (serde_json)       | 951.67 MB/s | 0.664 ms | 1.00x   |
| JavaScript (JSON.parse) | 432.13 MB/s | 1.314 ms | 1.98x   |
| **Wado** (core:json)    | 172.59 MB/s | 3.659 ms | 5.51x   |

## JSON: canada

Deserialize canada.json (2251051 bytes, geographic coordinates).

| Implementation          | Throughput  | ms/iter   | vs best |
| ----------------------- | ----------- | --------- | ------- |
| Rust (serde_json)       | 288.40 MB/s | 7.805 ms  | 1.00x   |
| JavaScript (JSON.parse) | 280.53 MB/s | 8.024 ms  | 1.03x   |
| **Wado** (core:json)    | 64.17 MB/s  | 35.080 ms | 4.49x   |

## JSON: catalog

Deserialize citm_catalog.json (1727204 bytes, event catalog).

| Implementation              | Throughput  | ms/iter   | vs best |
| --------------------------- | ----------- | --------- | ------- |
| Rust (serde_json)           | 795.90 MB/s | 2.170 ms  | 1.00x   |
| JavaScript (JSON.parse)     | 592.41 MB/s | 2.915 ms  | 1.34x   |
| **Wado** (v2, hand-rolled¹) | 265.84 MB/s | 6.497 ms  | 2.99x   |
| **Wado** (core:json)        | 151.91 MB/s | 11.370 ms | 5.24x   |

¹ `json_catalog/json_catalog_v2.wado` is a hand-rolled CitmCatalog parser
PoC (no `core:json` / `core:serde`). Kept as a marker of the upper bound
that's currently reachable without changes to `core:json`'s
sub-access-struct architecture. See its source for design notes.

## SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 7.64 MB/s  | 1.749 ms | 1.00x   |
| **Wado** (Gale)     | 5.53 MB/s  | 2.417 ms | 1.38x   |

## Syntax Highlight

Highlight 81 SQL statements (13366 bytes). Gale-generated highlighter vs
four reference SQL highlighters:

- **Prism.js** — regex-based, the speed reference (ultimate goal)
- **tree-sitter (Rust native)** — same `tree-sitter-sequel` grammar
  used by the JS row below, run as a Rust binary
- **Lezer (CodeMirror)** — `@codemirror/lang-sql` + `@lezer/highlight`,
  pure-JS LR parser
- **tree-sitter (web-tree-sitter)** — official JS WASM binding, same
  `@derekstride/tree-sitter-sql` grammar as the Rust row
- **Shiki (JS engine)** — TextMate grammars, VSCode-quality output

| Implementation               | Throughput  | ms/iter   | vs best |
| ---------------------------- | ----------- | --------- | ------- |
| JavaScript (Prism)           | 8.99 MB/s   | 1.486 ms  | 1.00x   |
| **Wado** (Gale)              | 3.09 MB/s   | 4.330 ms  | 2.91x   |
| JavaScript (Lezer)           | 2.69 MB/s   | 4.970 ms  | 3.34x   |
| Rust (tree-sitter)           | 2.67 MB/s   | 5.006 ms  | 3.37x   |
| JavaScript (web-tree-sitter) | 1.58 MB/s   | 8.441 ms  | 5.68x   |
| JavaScript (Shiki)           | 663.36 KB/s | 20.149 ms | 13.56x  |

Notes:

- Regex-based Prism wins on raw speed for a token-poor language like SQL.
- The Wado (Gale) highlighter now edges just ahead of pure-JS Lezer and
  the tree-sitter Rust native parser, and clearly ahead of the same grammar
  through tree-sitter's JS WASM binding — V8 optimises plain JS more
  aggressively than the WASM↔JS boundary crossings cost.
- Shiki is the slowest but produces the richest output (identifier-level
  coloring, VSCode-quality themes). The Oniguruma (WASM) engine is omitted
  because it is ~2.5x slower than the JS engine on this input while
  producing byte-identical output.

## HTTP Routing

End-to-end HTTP throughput of `wado serve` vs an equivalent
[Hono](https://hono.dev/) server on Node.js and on Bun, vs an
equivalent [Axum](https://github.com/tokio-rs/axum) server in native
Rust. The route and request set is Hono's own official router benchmark
(`honojs/hono`, `benchmarks/routers/`), driven over HTTP with `oha`.
Servers and the load generator run on disjoint pinned cores; each
request is measured in rotating slices and the fastest is kept.

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                     |       52,979 |      30,595 |     61,858 |        98,406 |
| `GET /user/lookup/username/hey` |       49,963 |      30,536 |     54,222 |       100,359 |
| `GET /event/abcd1234/comments`  |       51,269 |      30,646 |     54,752 |       100,362 |
| `POST /event/abcd1234/comment`  |       50,210 |      21,421 |     53,639 |        99,944 |
| `GET /static/index.html`        |       50,816 |      28,582 |     55,317 |        98,739 |

`wado serve` leads Hono on Node on every request (~50k–53k vs ~21k–31k
req/s) but trails Hono on Bun (~54k–62k) — Bun's HTTP server is
markedly faster than Node's. Native-Rust Axum is the ceiling; its
figure here is load-generator-limited (`oha` saturates before Axum
does, staying flat at ~98k–100k). `wado serve` runs a
`wasi:http/service` component on wasmtime, dispatching through
`core:router`, with pooled instance reuse + periodic recycling — a
cross-runtime comparison of a Wasm component on wasmtime vs JS on
Node.js/Bun vs native Rust. See `http_routing/README.md` for the full
table and methodology.

## Running

```sh
mise run benchmark-all              # run all

mise run benchmark-count-prime      # integer arithmetic
mise run benchmark-mandelbrot       # float arithmetic
mise run benchmark-sieve            # array operations
mise run benchmark-fts              # float-to-string
mise run benchmark-zlib             # compression
mise run benchmark-json-twitter     # JSON (631 KB)
mise run benchmark-json-canada      # JSON (2.3 MB)
mise run benchmark-json-catalog     # JSON (1.7 MB)
mise run benchmark-sqlite-parse     # SQL parsing
mise run benchmark-syntax-highlight # syntax highlighting
mise run benchmark-http-routing     # HTTP routing (wado serve vs Hono vs Axum)
```

Prerequisites: `cc` and `cargo` (system); `node` and `bun` (managed by
`mise install`).

## Profiling

```sh
# Guest profiling (all platforms) — view at https://profiler.firefox.com/
wado run --profile guest prog.wado
wado run --profile guest,output.json,5 prog.wado  # custom path, 5ms interval

# Linux perf
perf record -k mono wado run --profile jitdump prog.wado  # detailed (jitdump)
perf record -k mono wado run --profile perfmap prog.wado  # simple (perfmap)
samply record wado run --profile perfmap prog.wado         # samply
```
