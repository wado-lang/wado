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
| serde_cbor (Rust)    | 1.76 GB/s   | 0.359 ms | 1.00x   |
| serde_json (Rust)    | 1.74 GB/s   | 0.363 ms | 1.01x   |
| **core:cbor** (Wado) | 551.36 MB/s | 1.145 ms | 3.19x   |
| JSON (JS)            | 299.89 MB/s | 2.106 ms | 5.87x   |
| **core:json** (Wado) | 220.95 MB/s | 2.858 ms | 7.96x   |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 674.29 MB/s | 0.937 ms | 1.00x   |
| serde_cbor (Rust)    | 585.84 MB/s | 1.078 ms | 1.15x   |
| JSON (JS)            | 517.14 MB/s | 1.221 ms | 1.30x   |
| **core:cbor** (Wado) | 105.41 MB/s | 5.990 ms | 6.39x   |
| **core:json** (Wado) | 92.11 MB/s  | 6.855 ms | 7.32x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.72 GB/s   | 1.312 ms  | 1.00x   |
| serde_json (Rust)    | 697.24 MB/s | 3.228 ms  | 2.46x   |
| **core:cbor** (Wado) | 247.04 MB/s | 9.111 ms  | 6.94x   |
| JSON (JS)            | 166.58 MB/s | 13.513 ms | 10.30x  |
| **core:json** (Wado) | 97.98 MB/s  | 22.973 ms | 17.51x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 829.70 MB/s | 2.713 ms  | 1.00x   |
| serde_json (Rust)    | 287.88 MB/s | 7.819 ms  | 2.88x   |
| JSON (JS)            | 270.94 MB/s | 8.308 ms  | 3.06x   |
| **core:cbor** (Wado) | 146.25 MB/s | 15.391 ms | 5.67x   |
| **core:json** (Wado) | 114.32 MB/s | 19.691 ms | 7.26x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 3.08 GB/s   | 0.561 ms | 1.00x   |
| serde_cbor (Rust)    | 2.65 GB/s   | 0.651 ms | 1.16x   |
| JSON (JS)            | 802.38 MB/s | 2.153 ms | 3.84x   |
| **core:cbor** (Wado) | 788.55 MB/s | 2.190 ms | 3.90x   |
| **core:json** (Wado) | 383.06 MB/s | 4.509 ms | 8.04x   |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 1.86 GB/s   | 0.929 ms | 1.00x   |
| serde_json (Rust)    | 797.87 MB/s | 2.165 ms | 2.33x   |
| JSON (JS)            | 554.88 MB/s | 3.113 ms | 3.35x   |
| **core:cbor** (Wado) | 303.20 MB/s | 5.696 ms | 6.13x   |
| **core:json** (Wado) | 178.11 MB/s | 9.697 ms | 10.44x  |

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
