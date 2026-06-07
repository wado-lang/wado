# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-06-06, wasmtime 46.0.0, gcc 13.3.0, rustc 1.95.0,
Node.js v24.14.1, Bun 1.3.14, Linux x86_64.

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

| Implementation |    Throughput |    ms/iter | vs best |
| -------------- | ------------: | ---------: | ------: |
| C              | 7.96 M nums/s | 125.674 ms |   1.00x |
| **Wado**       | 7.82 M nums/s | 127.945 ms |   1.02x |
| JavaScript     | 7.64 M nums/s | 130.938 ms |   1.04x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------: |
| JavaScript     | 4.07 M px/s | 193.077 ms |   1.00x |
| **Wado**       | 4.06 M px/s | 193.652 ms |   1.00x |
| C              | 3.99 M px/s | 196.978 ms |   1.02x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations). The sieve buffer is
allocated once and reset each iteration, so allocation and first-touch page
faults stay out of the timed region — the loop measures steady-state array
traffic, which keeps run-to-run spread within ~1-2%.

| Implementation |      Throughput |   ms/iter | vs best |
| -------------- | --------------: | --------: | ------: |
| C              | 288.29 M nums/s | 34.687 ms |   1.00x |
| JavaScript     | 197.33 M nums/s | 50.676 ms |   1.46x |
| **Wado**       | 115.36 M nums/s | 86.683 ms |   2.50x |

## Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------: |
| Rust (core::fmt) | 13.58 M conv/s |  73.619 ms |   1.00x |
| **Wado** (fpfmt) |  9.53 M conv/s | 104.933 ms |   1.42x |
| C (printf)       |  7.56 M conv/s | 132.236 ms |   1.80x |

## Compression: compress

zlib compression of twitter.json (631514 bytes).

| Implementation         |  Throughput |   ms/iter | vs best |
| ---------------------- | ----------: | --------: | ------: |
| Rust (zlib-rs)         | 225.50 MB/s |  2.801 ms |   1.00x |
| JavaScript (node:zlib) | 157.15 MB/s |  4.019 ms |   1.43x |
| **Wado** (core:zlib)   |  32.86 MB/s | 19.216 ms |   6.86x |

## Compression: decompress

zlib decompression of twitter.json (631514 bytes).

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------: |
| Rust (zlib-rs)         |   2.04 GB/s | 0.310 ms |   1.00x |
| JavaScript (node:zlib) |   1.09 GB/s | 0.581 ms |   1.87x |
| **Wado** (core:zlib)   | 196.06 MB/s | 3.221 ms |  10.40x |

## JSON: twitter

Deserialize twitter.json (631514 bytes).

| Implementation          |  Throughput |  ms/iter | vs best |
| ----------------------- | ----------: | -------: | ------: |
| Rust (serde_json)       | 970.33 MB/s | 0.651 ms |   1.00x |
| JavaScript (JSON.parse) | 449.03 MB/s | 1.265 ms |   2.16x |
| **Wado** (core:json)    | 180.00 MB/s | 3.508 ms |   5.39x |

## JSON: canada

Deserialize canada.json (2251051 bytes, geographic coordinates).

| Implementation          |  Throughput |   ms/iter | vs best |
| ----------------------- | ----------: | --------: | ------: |
| Rust (serde_json)       | 296.21 MB/s |  7.600 ms |   1.00x |
| JavaScript (JSON.parse) | 286.09 MB/s |  7.868 ms |   1.04x |
| **Wado** (core:json)    |  77.62 MB/s | 29.000 ms |   3.82x |

## JSON: catalog

Deserialize citm_catalog.json (1727204 bytes, event catalog).

| Implementation              |  Throughput |   ms/iter | vs best |
| --------------------------- | ----------: | --------: | ------: |
| Rust (serde_json)           | 810.01 MB/s |  2.132 ms |   1.00x |
| JavaScript (JSON.parse)     | 576.70 MB/s |  2.995 ms |   1.40x |
| **Wado** (v2, hand-rolled¹) | 273.65 MB/s |  6.311 ms |   2.96x |
| **Wado** (core:json)        | 154.42 MB/s | 11.184 ms |   5.25x |

¹ `json_catalog/json_catalog_v2.wado` is a hand-rolled CitmCatalog parser
PoC (no `core:json` / `core:serde`). Kept as a marker of the upper bound
that's currently reachable without changes to `core:json`'s
sub-access-struct architecture. See its source for design notes.

## SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput |  ms/iter | vs best |
| ------------------- | ---------: | -------: | ------: |
| Rust (sqlparser-rs) |  7.83 MB/s | 1.706 ms |   1.00x |
| **Wado** (Gale)     |  4.43 MB/s | 3.017 ms |   1.77x |

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

| Implementation               |  Throughput |   ms/iter | vs best |
| ---------------------------- | ----------: | --------: | ------: |
| JavaScript (Prism)           |   9.35 MB/s |  1.429 ms |   1.00x |
| Rust (tree-sitter)           |   2.72 MB/s |  4.905 ms |   3.44x |
| JavaScript (Lezer)           |   2.68 MB/s |  4.985 ms |   3.49x |
| **Wado** (Gale)              |   2.61 MB/s |  5.130 ms |   3.58x |
| JavaScript (web-tree-sitter) |   1.62 MB/s |  8.271 ms |   5.77x |
| JavaScript (Shiki)           | 675.59 KB/s | 19.784 ms |  13.84x |

Notes:

- Regex-based Prism wins on raw speed for a token-poor language like SQL.
- The Wado (Gale) highlighter lands essentially on par with the
  tree-sitter Rust native parser and pure-JS Lezer (all within ~15% of
  each other) and clearly ahead of the same grammar accessed through
  tree-sitter's JS WASM binding — V8 optimises plain JS more aggressively
  than the WASM↔JS boundary crossings cost.
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
