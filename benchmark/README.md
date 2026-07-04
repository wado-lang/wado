# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-04, wasmtime 46.0.1, gcc 13.3.0, rustc 1.96.1,
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
| C              | 7.98 M nums/s | 125.352 ms | 1.00x   |
| **Wado**       | 7.80 M nums/s | 128.150 ms | 1.02x   |
| JavaScript     | 7.36 M nums/s | 135.867 ms | 1.08x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| JavaScript     | 4.08 M px/s | 192.768 ms | 1.00x   |
| **Wado**       | 4.06 M px/s | 193.923 ms | 1.01x   |
| C              | 4.00 M px/s | 196.635 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 282.38 M nums/s | 35.413 ms | 1.00x   |
| JavaScript     | 199.79 M nums/s | 50.053 ms | 1.41x   |
| **Wado**       | 153.73 M nums/s | 65.049 ms | 1.84x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 13.56 M conv/s | 73.770 ms  | 1.00x   |
| **Wado** (fpfmt) | 9.82 M conv/s  | 101.841 ms | 1.38x   |
| C (printf)       | 7.68 M conv/s  | 130.162 ms | 1.76x   |

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
| serde_cbor (Rust)    | 2.05 GB/s   | 0.308 ms | 1.00x   |
| serde_json (Rust)    | 1.62 GB/s   | 0.391 ms | 1.27x   |
| **core:cbor** (Wado) | 498.05 MB/s | 1.267 ms | 4.11x   |
| JSON (JS)            | 293.89 MB/s | 2.149 ms | 6.98x   |
| **core:json** (Wado) | 126.59 MB/s | 4.988 ms | 16.19x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 638.85 MB/s | 0.989 ms | 1.00x   |
| serde_cbor (Rust)    | 631.10 MB/s | 1.001 ms | 1.01x   |
| JSON (JS)            | 482.66 MB/s | 1.308 ms | 1.32x   |
| **core:cbor** (Wado) | 104.16 MB/s | 6.063 ms | 6.13x   |
| **core:json** (Wado) | 87.40 MB/s  | 7.225 ms | 7.31x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.81 GB/s   | 1.245 ms  | 1.00x   |
| serde_json (Rust)    | 701.91 MB/s | 3.207 ms  | 2.58x   |
| **core:cbor** (Wado) | 230.62 MB/s | 9.760 ms  | 7.84x   |
| JSON (JS)            | 144.81 MB/s | 15.545 ms | 12.49x  |
| **core:json** (Wado) | 96.91 MB/s  | 23.228 ms | 18.66x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 877.09 MB/s | 2.566 ms  | 1.00x   |
| serde_json (Rust)    | 294.68 MB/s | 7.639 ms  | 2.98x   |
| JSON (JS)            | 282.31 MB/s | 7.974 ms  | 3.11x   |
| **core:cbor** (Wado) | 134.67 MB/s | 16.715 ms | 6.51x   |
| **core:json** (Wado) | 110.30 MB/s | 20.409 ms | 7.95x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 3.24 GB/s   | 0.533 ms | 1.00x   |
| serde_cbor (Rust)    | 3.07 GB/s   | 0.562 ms | 1.05x   |
| JSON (JS)            | 762.09 MB/s | 2.266 ms | 4.25x   |
| **core:cbor** (Wado) | 700.33 MB/s | 2.466 ms | 4.63x   |
| **core:json** (Wado) | 275.17 MB/s | 6.276 ms | 11.77x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 1.95 GB/s   | 0.886 ms | 1.00x   |
| serde_json (Rust)    | 794.07 MB/s | 2.175 ms | 2.45x   |
| JSON (JS)            | 592.39 MB/s | 2.916 ms | 3.29x   |
| **core:cbor** (Wado) | 270.51 MB/s | 6.384 ms | 7.21x   |
| **core:json** (Wado) | 173.69 MB/s | 9.944 ms | 11.22x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 228.61 MB/s | 2.762 ms  | 1.00x   |
| JavaScript (node:zlib) | 160.31 MB/s | 3.939 ms  | 1.43x   |
| **Wado** (core:zlib)   | 30.92 MB/s  | 20.421 ms | 7.39x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 2.06 GB/s   | 0.306 ms | 1.00x   |
| JavaScript (node:zlib) | 1.02 GB/s   | 0.617 ms | 2.02x   |
| **Wado** (core:zlib)   | 173.74 MB/s | 3.634 ms | 11.88x  |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 7.88 MB/s  | 1.696 ms | 1.00x   |
| **Wado** (Gale)     | 5.79 MB/s  | 2.307 ms | 1.36x   |

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
| JavaScript (Prism)           | 9.19 MB/s   | 1.454 ms  | 1.00x   |
| **Wado** (Gale)              | 3.97 MB/s   | 3.365 ms  | 2.31x   |
| JavaScript (Lezer)           | 2.77 MB/s   | 4.832 ms  | 3.32x   |
| Rust (tree-sitter)           | 2.70 MB/s   | 4.943 ms  | 3.40x   |
| JavaScript (web-tree-sitter) | 1.59 MB/s   | 8.432 ms  | 5.80x   |
| JavaScript (Shiki)           | 665.14 KB/s | 20.095 ms | 13.82x  |

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
