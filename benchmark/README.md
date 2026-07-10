# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-10, wasmtime 46.0.1, gcc 13.3.0, rustc 1.96.1,
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
| C              | 7.95 M nums/s | 125.765 ms | 1.00x   |
| **Wado**       | 7.80 M nums/s | 128.168 ms | 1.02x   |
| JavaScript     | 7.70 M nums/s | 129.791 ms | 1.03x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| JavaScript     | 4.07 M px/s | 193.000 ms | 1.00x   |
| **Wado**       | 4.06 M px/s | 193.552 ms | 1.00x   |
| C              | 3.99 M px/s | 197.158 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 272.79 M nums/s | 36.658 ms | 1.00x   |
| JavaScript     | 196.78 M nums/s | 50.818 ms | 1.39x   |
| **Wado**       | 174.38 M nums/s | 57.345 ms | 1.56x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 13.24 M conv/s | 75.513 ms  | 1.00x   |
| **Wado** (fpfmt) | 8.62 M conv/s  | 115.992 ms | 1.54x   |
| C (printf)       | 7.57 M conv/s  | 132.159 ms | 1.75x   |

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
| serde_cbor (Rust)    | 1.92 GB/s   | 0.329 ms | 1.00x   |
| serde_json (Rust)    | 1.61 GB/s   | 0.392 ms | 1.19x   |
| **core:cbor** (Wado) | 484.45 MB/s | 1.303 ms | 4.06x   |
| JSON (JS)            | 284.29 MB/s | 2.221 ms | 6.92x   |
| **core:json** (Wado) | 204.73 MB/s | 3.084 ms | 9.60x   |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 637.25 MB/s | 0.991 ms | 1.00x   |
| serde_cbor (Rust)    | 616.37 MB/s | 1.025 ms | 1.03x   |
| JSON (JS)            | 493.24 MB/s | 1.280 ms | 1.29x   |
| **core:cbor** (Wado) | 101.96 MB/s | 6.193 ms | 6.25x   |
| **core:json** (Wado) | 93.81 MB/s  | 6.732 ms | 6.79x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.78 GB/s   | 1.265 ms  | 1.00x   |
| serde_json (Rust)    | 670.79 MB/s | 3.356 ms  | 2.72x   |
| **core:cbor** (Wado) | 198.74 MB/s | 11.326 ms | 9.17x   |
| JSON (JS)            | 130.38 MB/s | 17.266 ms | 13.98x  |
| **core:json** (Wado) | 108.88 MB/s | 20.675 ms | 16.74x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 876.50 MB/s | 2.568 ms  | 1.00x   |
| serde_json (Rust)    | 279.68 MB/s | 8.049 ms  | 3.13x   |
| JSON (JS)            | 275.09 MB/s | 8.183 ms  | 3.19x   |
| **core:json** (Wado) | 115.97 MB/s | 19.409 ms | 7.56x   |
| **core:cbor** (Wado) | 97.16 MB/s  | 23.167 ms | 9.02x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 3.21 GB/s   | 0.537 ms | 1.00x   |
| serde_cbor (Rust)    | 3.07 GB/s   | 0.563 ms | 1.05x   |
| JSON (JS)            | 715.83 MB/s | 2.413 ms | 4.59x   |
| **core:cbor** (Wado) | 632.93 MB/s | 2.728 ms | 5.19x   |
| **core:json** (Wado) | 301.38 MB/s | 5.731 ms | 10.91x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.89 GB/s   | 0.915 ms  | 1.00x   |
| serde_json (Rust)    | 787.13 MB/s | 2.194 ms  | 2.46x   |
| JSON (JS)            | 584.17 MB/s | 2.957 ms  | 3.31x   |
| **core:cbor** (Wado) | 221.84 MB/s | 7.785 ms  | 8.72x   |
| **core:json** (Wado) | 148.79 MB/s | 11.608 ms | 13.01x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 224.47 MB/s | 2.813 ms  | 1.00x   |
| JavaScript (node:zlib) | 157.23 MB/s | 4.016 ms  | 1.43x   |
| **Wado** (core:zlib)   | 42.93 MB/s  | 14.711 ms | 5.23x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 2.02 GB/s   | 0.313 ms | 1.00x   |
| JavaScript (node:zlib) | 1.01 GB/s   | 0.624 ms | 2.00x   |
| **Wado** (core:zlib)   | 208.97 MB/s | 3.022 ms | 9.90x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 7.75 MB/s  | 1.724 ms | 1.00x   |
| **Wado** (Gale)     | 5.10 MB/s  | 2.621 ms | 1.52x   |

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
| JavaScript (Prism)           | 8.90 MB/s   | 1.501 ms  | 1.00x   |
| **Wado** (Gale)              | 3.60 MB/s   | 3.712 ms  | 2.47x   |
| JavaScript (Lezer)           | 2.71 MB/s   | 4.934 ms  | 3.28x   |
| Rust (tree-sitter)           | 2.65 MB/s   | 5.050 ms  | 3.36x   |
| JavaScript (web-tree-sitter) | 1.56 MB/s   | 8.548 ms  | 5.71x   |
| JavaScript (Shiki)           | 634.12 KB/s | 21.078 ms | 14.37x  |

## Application Server

### HTTP Routing

End-to-end HTTP throughput of `wado serve` vs [Hono](https://hono.dev/) on
Node.js and Bun, vs native-Rust [Axum](https://github.com/tokio-rs/axum), over
Hono's official router benchmark route set driven with `oha`. See
`http_routing/README.md` for the full table and methodology.

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                     |       50,313 |      27,612 |     55,221 |        98,360 |
| `GET /user/lookup/username/hey` |       46,511 |      29,981 |     52,317 |        99,693 |
| `GET /event/abcd1234/comments`  |       46,766 |      26,159 |     52,291 |        99,961 |
| `POST /event/abcd1234/comment`  |       47,376 |      21,810 |     48,651 |        98,581 |
| `GET /static/index.html`        |       45,725 |      23,629 |     48,842 |        96,481 |

HTTP routing needs `oha` and Bun, and is measured separately
(`SLICE=4 ROUNDS=5 CONNECTIONS=50 mise run benchmark-http-routing`).

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
