# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-02, wasmtime 46.0.1, gcc 13.3.0, rustc 1.96.1,
Node.js v24.14.1, Linux x86_64.

Throughput is work per second (higher is better), with per-iteration time in
parentheses. Native rows are optimized builds (C `gcc -O3`, Rust release, Wado
`-O2`); JavaScript runs on Node.js. `vs best` is the fastest row's throughput
over this row's (1.00x = fastest). Absolute throughput is machine-dependent, so
compare by `vs best`. Each figure is the best of three runs.

Benchmarks are grouped into four sections: pure computation, serialization &
compression, parsing, and application server.

## Pure Computation

### Prime Counting

Count primes up to 1M (integer arithmetic, trial division).

| Implementation | Throughput    | ms/iter    | vs best |
| -------------- | ------------- | ---------- | ------- |
| C              | 7.34 M nums/s | 136.194 ms | 1.00x   |
| **Wado**       | 7.20 M nums/s | 138.801 ms | 1.02x   |
| JavaScript     | 6.88 M nums/s | 145.359 ms | 1.07x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| C              | 5.03 M px/s | 156.355 ms | 1.00x   |
| JavaScript     | 4.85 M px/s | 162.244 ms | 1.04x   |
| **Wado**       | 4.47 M px/s | 176.011 ms | 1.13x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 259.98 M nums/s | 38.465 ms | 1.00x   |
| JavaScript     | 188.95 M nums/s | 52.923 ms | 1.38x   |
| **Wado**       | 144.60 M nums/s | 69.156 ms | 1.80x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 13.75 M conv/s | 72.743 ms  | 1.00x   |
| **Wado** (fpfmt) | 11.89 M conv/s | 84.117 ms  | 1.16x   |
| C (printf)       | 8.49 M conv/s  | 117.764 ms | 1.62x   |

## Serialization & Compression

Each dataset is measured under two codecs — JSON (`core:json`) and CBOR
(`core:cbor`) — over the same Wado data types, so serialization and
deserialization compare both across languages and across codecs. `serde_json` /
`serde_cbor` (Rust) and `JSON.stringify` / `JSON.parse` (JS) are the references.
Throughput for both phases is reported over the JSON source size (the shared
denominator across codecs).

### twitter

`twitter.json` (631514 bytes): a Twitter API search response with 100 statuses.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 1.91 GB/s   | 0.331 ms | 1.00x   |
| serde_json (Rust)    | 1.87 GB/s   | 0.337 ms | 1.02x   |
| **core:cbor** (Wado) | 543.06 MB/s | 1.162 ms | 3.52x   |
| JSON (JS)            | 333.60 MB/s | 1.893 ms | 5.73x   |
| **core:json** (Wado) | 139.03 MB/s | 4.542 ms | 13.74x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 749.46 MB/s | 0.843 ms | 1.00x   |
| serde_cbor (Rust)    | 644.58 MB/s | 0.980 ms | 1.16x   |
| JSON (JS)            | 582.95 MB/s | 1.083 ms | 1.29x   |
| **core:cbor** (Wado) | 115.84 MB/s | 5.451 ms | 6.47x   |
| **core:json** (Wado) | 92.76 MB/s  | 6.807 ms | 8.08x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.81 GB/s   | 1.241 ms  | 1.00x   |
| serde_json (Rust)    | 730.58 MB/s | 3.081 ms  | 2.48x   |
| **core:cbor** (Wado) | 199.14 MB/s | 11.304 ms | 9.09x   |
| JSON (JS)            | 179.21 MB/s | 12.561 ms | 10.10x  |
| **core:json** (Wado) | 72.39 MB/s  | 31.094 ms | 25.00x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 922.26 MB/s | 2.441 ms  | 1.00x   |
| JSON (JS)            | 323.74 MB/s | 6.953 ms  | 2.85x   |
| serde_json (Rust)    | 306.23 MB/s | 7.351 ms  | 3.01x   |
| **core:cbor** (Wado) | 152.68 MB/s | 14.743 ms | 6.04x   |
| **core:json** (Wado) | 128.80 MB/s | 17.476 ms | 7.16x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 3.32 GB/s   | 0.521 ms | 1.00x   |
| serde_cbor (Rust)    | 2.77 GB/s   | 0.623 ms | 1.20x   |
| JSON (JS)            | 858.84 MB/s | 2.011 ms | 3.87x   |
| **core:cbor** (Wado) | 745.71 MB/s | 2.316 ms | 4.45x   |
| **core:json** (Wado) | 282.05 MB/s | 6.123 ms | 11.77x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 1.96 GB/s   | 0.883 ms | 1.00x   |
| serde_json (Rust)    | 868.26 MB/s | 1.989 ms | 2.26x   |
| JSON (JS)            | 666.43 MB/s | 2.592 ms | 2.94x   |
| **core:cbor** (Wado) | 316.62 MB/s | 5.455 ms | 6.19x   |
| **core:json** (Wado) | 192.12 MB/s | 8.990 ms | 10.20x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 222.47 MB/s | 2.839 ms  | 1.00x   |
| JavaScript (node:zlib) | 153.51 MB/s | 4.114 ms  | 1.45x   |
| **Wado** (core:zlib)   | 35.26 MB/s  | 17.908 ms | 6.31x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.87 GB/s   | 0.337 ms | 1.00x   |
| JavaScript (node:zlib) | 1.12 GB/s   | 0.562 ms | 1.67x   |
| **Wado** (core:zlib)   | 225.98 MB/s | 2.794 ms | 8.27x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 7.86 MB/s  | 1.701 ms | 1.00x   |
| **Wado** (Gale)     | 5.31 MB/s  | 2.516 ms | 1.48x   |

### Syntax Highlight

Highlight 81 SQL statements (13366 bytes). Gale-generated highlighter vs four
reference SQL highlighters:

- **Prism.js** — regex-based, the speed reference (ultimate goal)
- **tree-sitter (Rust native)** — same `tree-sitter-sequel` grammar used by the
  JS row below, run as a Rust binary
- **Lezer (CodeMirror)** — `@codemirror/lang-sql` + `@lezer/highlight`, a
  pure-JS LR parser
- **tree-sitter (web-tree-sitter)** — official JS WASM binding, same
  `@derekstride/tree-sitter-sql` grammar as the Rust row
- **Shiki (JS engine)** — TextMate grammars, VSCode-quality output

| Implementation               | Throughput  | ms/iter   | vs best |
| ---------------------------- | ----------- | --------- | ------- |
| JavaScript (Prism)           | 9.07 MB/s   | 1.474 ms  | 1.00x   |
| **Wado** (Gale)              | 3.88 MB/s   | 3.446 ms  | 2.34x   |
| JavaScript (Lezer)           | 2.74 MB/s   | 4.874 ms  | 3.31x   |
| Rust (tree-sitter)           | 2.69 MB/s   | 4.962 ms  | 3.37x   |
| JavaScript (web-tree-sitter) | 1.58 MB/s   | 8.482 ms  | 5.74x   |
| JavaScript (Shiki)           | 639.48 KB/s | 20.901 ms | 14.52x  |

## Application Server

### HTTP Routing

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

These figures are carried over from a previous run; HTTP routing needs `oha`
and Bun, and is measured separately (`SLICE=4 ROUNDS=5 CONNECTIONS=50 mise run
benchmark-http-routing`).

## Running

```sh
mise run benchmark-all              # run all

# pure computation
mise run benchmark-count-prime      # integer arithmetic
mise run benchmark-mandelbrot       # float arithmetic
mise run benchmark-sieve            # array operations
mise run benchmark-fts              # float-to-string

# serialization & compression
mise run benchmark-json-twitter     # JSON ser/de (631 KB)
mise run benchmark-json-canada      # JSON ser/de (2.3 MB)
mise run benchmark-json-catalog     # JSON ser/de (1.7 MB)
mise run benchmark-cbor             # CBOR ser/de (twitter, canada, catalog)
mise run benchmark-zlib             # compression

# parsing
mise run benchmark-sqlite-parse     # SQL parsing
mise run benchmark-syntax-highlight # syntax highlighting

# application server
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
