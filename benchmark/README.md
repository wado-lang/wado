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
| C              | 7.97 M nums/s | 125.525 ms | 1.00x   |
| **Wado**       | 7.83 M nums/s | 127.776 ms | 1.02x   |
| JavaScript     | 7.83 M nums/s | 127.767 ms | 1.02x   |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| JavaScript     | 4.08 M px/s | 192.519 ms | 1.00x   |
| **Wado**       | 4.07 M px/s | 193.297 ms | 1.00x   |
| C              | 4.01 M px/s | 196.317 ms | 1.02x   |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter    | vs best |
| -------------- | --------------- | ---------- | ------- |
| C              | 271.58 M nums/s | 36.822 ms  | 1.00x   |
| JavaScript     | 197.02 M nums/s | 50.757 ms  | 1.38x   |
| **Wado**       | 95.84 M nums/s  | 104.339 ms | 2.83x   |

## Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 13.58 M conv/s | 73.644 ms  | 1.00x   |
| **Wado** (fpfmt) | 9.33 M conv/s  | 107.219 ms | 1.46x   |
| C (printf)       | 7.67 M conv/s  | 130.374 ms | 1.77x   |

## Compression: compress

zlib compression of twitter.json (631514 bytes).

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 228.29 MB/s | 2.766 ms  | 1.00x   |
| JavaScript (node:zlib) | 160.29 MB/s | 3.940 ms  | 1.42x   |
| **Wado** (core:zlib)   | 31.66 MB/s  | 19.945 ms | 7.21x   |

## Compression: decompress

zlib decompression of twitter.json (631514 bytes).

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 2.07 GB/s   | 0.305 ms | 1.00x   |
| JavaScript (node:zlib) | 1.04 GB/s   | 0.609 ms | 1.99x   |
| **Wado** (core:zlib)   | 171.26 MB/s | 3.687 ms | 12.09x  |

## JSON: twitter

Deserialize twitter.json (631514 bytes).

| Implementation          | Throughput  | ms/iter  | vs best |
| ----------------------- | ----------- | -------- | ------- |
| Rust (serde_json)       | 973.00 MB/s | 0.649 ms | 1.00x   |
| JavaScript (JSON.parse) | 450.70 MB/s | 1.260 ms | 2.16x   |
| **Wado** (core:json)    | 189.02 MB/s | 3.340 ms | 5.15x   |

## JSON: canada

Deserialize canada.json (2251051 bytes, geographic coordinates).

| Implementation          | Throughput  | ms/iter   | vs best |
| ----------------------- | ----------- | --------- | ------- |
| Rust (serde_json)       | 292.17 MB/s | 7.705 ms  | 1.00x   |
| JavaScript (JSON.parse) | 287.25 MB/s | 7.836 ms  | 1.02x   |
| **Wado** (core:json)    | 87.01 MB/s  | 25.872 ms | 3.36x   |

## JSON: catalog

Deserialize citm_catalog.json (1727204 bytes, event catalog).

| Implementation             | Throughput  | ms/iter   | vs best |
| -------------------------- | ----------- | --------- | ------- |
| Rust (serde_json)          | 808.56 MB/s | 2.136 ms  | 1.00x   |
| JavaScript (JSON.parse)    | 597.51 MB/s | 2.890 ms  | 1.35x   |
| **Wado** (v2, hand-rolled) | 271.50 MB/s | 6.361 ms  | 2.98x   |
| **Wado** (core:json)       | 166.82 MB/s | 10.353 ms | 4.85x   |

## SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 7.92 MB/s  | 1.688 ms | 1.00x   |
| **Wado** (Gale)     | 6.08 MB/s  | 2.200 ms | 1.30x   |

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
| JavaScript (Prism)           | 9.10 MB/s   | 1.469 ms  | 1.00x   |
| **Wado** (Gale)              | 3.56 MB/s   | 3.755 ms  | 2.56x   |
| JavaScript (Lezer)           | 2.77 MB/s   | 4.831 ms  | 3.29x   |
| Rust (tree-sitter)           | 2.71 MB/s   | 4.929 ms  | 3.36x   |
| JavaScript (web-tree-sitter) | 1.60 MB/s   | 8.343 ms  | 5.69x   |
| JavaScript (Shiki)           | 664.30 KB/s | 20.120 ms | 13.70x  |

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
