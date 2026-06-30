# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-06-30, wasmtime 46.0.1, gcc 13.3.0, rustc 1.96.0,
Node.js v24.14.1, Bun 1.3.14, Linux x86_64.

Throughput is work per second (higher is better), with per-iteration time in
parentheses. Native rows are optimized builds (C `gcc -O3`, Rust release, Wado
`-O2`); JavaScript runs on Node.js. `vs best` is the fastest row's throughput
over this row's (1.00x = fastest). Absolute throughput is machine-dependent;
compare by `vs best`.

## Prime Counting

Count primes up to 1M (integer arithmetic, trial division).

| Implementation | Throughput    | ms/iter    | vs best |
| -------------- | ------------- | ---------- | ------- |
| C              | 5.90 M nums/s | 169.618 ms | 1.00x   |
| **Wado**       | 4.70 M nums/s | 212.823 ms | 1.26x   |
| JavaScript     | 3.93 M nums/s | 254.736 ms | 1.50x   |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| JavaScript     | 4.26 M px/s | 184.695 ms | 1.00x   |
| **Wado**       | 4.19 M px/s | 187.862 ms | 1.02x   |
| C              | 4.18 M px/s | 188.135 ms | 1.02x   |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter    | vs best |
| -------------- | --------------- | ---------- | ------- |
| C              | 239.83 M nums/s | 41.697 ms  | 1.00x   |
| JavaScript     | 170.63 M nums/s | 58.606 ms  | 1.41x   |
| **Wado**       | 62.95 M nums/s  | 158.854 ms | 3.81x   |

## Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 10.28 M conv/s | 97.283 ms  | 1.00x   |
| **Wado** (fpfmt) | 7.55 M conv/s  | 132.389 ms | 1.36x   |
| C (printf)       | 5.95 M conv/s  | 168.198 ms | 1.73x   |

## Compression: compress

zlib compression of twitter.json (631514 bytes).

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 157.79 MB/s | 4.002 ms  | 1.00x   |
| JavaScript (node:zlib) | 135.13 MB/s | 4.673 ms  | 1.17x   |
| **Wado** (core:zlib)   | 23.49 MB/s  | 26.881 ms | 6.72x   |

## Compression: decompress

zlib decompression of twitter.json (631514 bytes).

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.49 GB/s   | 0.424 ms | 1.00x   |
| JavaScript (node:zlib) | 787.13 MB/s | 0.802 ms | 1.89x   |
| **Wado** (core:zlib)   | 131.58 MB/s | 4.799 ms | 11.32x  |

## JSON: twitter

Deserialize twitter.json (631514 bytes).

| Implementation          | Throughput  | ms/iter  | vs best |
| ----------------------- | ----------- | -------- | ------- |
| Rust (serde_json)       | 753.38 MB/s | 0.838 ms | 1.00x   |
| JavaScript (JSON.parse) | 285.76 MB/s | 1.987 ms | 2.64x   |
| **Wado** (core:json)    | 122.37 MB/s | 5.160 ms | 6.16x   |

## JSON: canada

Deserialize canada.json (2251051 bytes, geographic coordinates).

| Implementation          | Throughput  | ms/iter   | vs best |
| ----------------------- | ----------- | --------- | ------- |
| Rust (serde_json)       | 212.59 MB/s | 10.589 ms | 1.00x   |
| JavaScript (JSON.parse) | 211.49 MB/s | 10.644 ms | 1.01x   |
| **Wado** (core:json)    | 43.70 MB/s  | 51.509 ms | 4.87x   |

## JSON: catalog

Deserialize citm_catalog.json (1727204 bytes, event catalog).

| Implementation             | Throughput  | ms/iter   | vs best |
| -------------------------- | ----------- | --------- | ------- |
| Rust (serde_json)          | 648.38 MB/s | 2.664 ms  | 1.00x   |
| JavaScript (JSON.parse)    | 439.14 MB/s | 3.933 ms  | 1.48x   |
| **Wado** (v2, hand-rolled) | 163.45 MB/s | 10.566 ms | 3.97x   |
| **Wado** (core:json)       | 105.27 MB/s | 16.407 ms | 6.16x   |

## SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 6.84 MB/s  | 1.954 ms | 1.00x   |
| **Wado** (Gale)     | 4.81 MB/s  | 2.778 ms | 1.42x   |

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
| JavaScript (Prism)           | 6.88 MB/s   | 1.944 ms  | 1.00x   |
| **Wado** (Gale)              | 2.66 MB/s   | 5.024 ms  | 2.59x   |
| JavaScript (Lezer)           | 2.32 MB/s   | 5.766 ms  | 2.97x   |
| Rust (tree-sitter)           | 2.26 MB/s   | 5.910 ms  | 3.04x   |
| JavaScript (web-tree-sitter) | 1.37 MB/s   | 9.765 ms  | 5.02x   |
| JavaScript (Shiki)           | 559.77 KB/s | 23.878 ms | 12.29x  |

## HTTP Routing

End-to-end HTTP throughput of `wado serve` vs [Hono](https://hono.dev/) on
Node.js and Bun, vs native-Rust [Axum](https://github.com/tokio-rs/axum), over
Hono's official router benchmark route set driven with `oha`. See
`http_routing/README.md` for the full table and methodology.

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                     |       39,797 |      16,337 |     43,691 |        79,415 |
| `GET /user/lookup/username/hey` |       39,193 |      16,353 |     36,390 |        77,111 |
| `GET /event/abcd1234/comments`  |       35,947 |      15,569 |     36,409 |        72,197 |
| `POST /event/abcd1234/comment`  |       39,297 |      12,004 |     36,584 |        71,786 |
| `GET /static/index.html`        |       36,765 |      14,532 |     37,293 |        80,881 |

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
