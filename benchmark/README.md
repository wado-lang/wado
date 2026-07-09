# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-09, wasmtime 46.0.1, gcc 13.3.0, rustc 1.96.1,
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
| C              | 6.93 M nums/s | 144.340 ms | 1.00x   |
| **Wado**       | 6.75 M nums/s | 148.238 ms | 1.03x   |
| JavaScript     | 6.40 M nums/s | 156.306 ms | 1.08x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| C              | 4.70 M px/s | 167.209 ms | 1.00x   |
| JavaScript     | 4.52 M px/s | 174.180 ms | 1.04x   |
| **Wado**       | 4.28 M px/s | 183.605 ms | 1.10x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 249.24 M nums/s | 40.122 ms | 1.00x   |
| JavaScript     | 183.94 M nums/s | 54.366 ms | 1.36x   |
| **Wado**       | 162.07 M nums/s | 61.703 ms | 1.54x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 12.91 M conv/s | 77.470 ms  | 1.00x   |
| **Wado** (fpfmt) | 9.69 M conv/s  | 103.247 ms | 1.33x   |
| C (printf)       | 7.84 M conv/s  | 127.550 ms | 1.65x   |

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
| serde_json (Rust)    | 1.79 GB/s   | 0.353 ms | 1.00x   |
| serde_cbor (Rust)    | 1.72 GB/s   | 0.367 ms | 1.04x   |
| **core:cbor** (Wado) | 553.42 MB/s | 1.141 ms | 3.23x   |
| JSON (JS)            | 316.28 MB/s | 1.997 ms | 5.66x   |
| **core:json** (Wado) | 150.21 MB/s | 4.204 ms | 11.91x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 678.59 MB/s | 0.931 ms | 1.00x   |
| serde_cbor (Rust)    | 594.39 MB/s | 1.062 ms | 1.14x   |
| JSON (JS)            | 546.99 MB/s | 1.155 ms | 1.24x   |
| **core:cbor** (Wado) | 104.17 MB/s | 6.062 ms | 6.51x   |
| **core:json** (Wado) | 95.50 MB/s  | 6.612 ms | 7.10x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.72 GB/s   | 1.309 ms  | 1.00x   |
| serde_json (Rust)    | 701.36 MB/s | 3.210 ms  | 2.45x   |
| **core:cbor** (Wado) | 250.25 MB/s | 8.995 ms  | 6.87x   |
| JSON (JS)            | 172.17 MB/s | 13.075 ms | 9.99x   |
| **core:json** (Wado) | 104.23 MB/s | 21.597 ms | 16.50x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 914.65 MB/s | 2.461 ms  | 1.00x   |
| JSON (JS)            | 308.08 MB/s | 7.307 ms  | 2.97x   |
| serde_json (Rust)    | 291.25 MB/s | 7.729 ms  | 3.14x   |
| **core:cbor** (Wado) | 153.72 MB/s | 14.643 ms | 5.95x   |
| **core:json** (Wado) | 143.30 MB/s | 15.708 ms | 6.38x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 3.23 GB/s   | 0.535 ms | 1.00x   |
| serde_cbor (Rust)    | 2.73 GB/s   | 0.633 ms | 1.18x   |
| JSON (JS)            | 797.42 MB/s | 2.166 ms | 4.05x   |
| **core:cbor** (Wado) | 770.10 MB/s | 2.242 ms | 4.19x   |
| **core:json** (Wado) | 287.76 MB/s | 6.002 ms | 11.22x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 1.91 GB/s   | 0.905 ms | 1.00x   |
| serde_json (Rust)    | 810.30 MB/s | 2.132 ms | 2.36x   |
| JSON (JS)            | 621.54 MB/s | 2.779 ms | 3.07x   |
| **core:cbor** (Wado) | 309.21 MB/s | 5.585 ms | 6.17x   |
| **core:json** (Wado) | 177.70 MB/s | 9.719 ms | 10.74x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 210.92 MB/s | 2.994 ms  | 1.00x   |
| JavaScript (node:zlib) | 144.50 MB/s | 4.370 ms  | 1.46x   |
| **Wado** (core:zlib)   | 44.46 MB/s  | 14.204 ms | 4.74x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.77 GB/s   | 0.356 ms | 1.00x   |
| JavaScript (node:zlib) | 1.05 GB/s   | 0.601 ms | 1.69x   |
| **Wado** (core:zlib)   | 242.46 MB/s | 2.604 ms | 7.32x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 7.52 MB/s  | 1.778 ms | 1.00x   |
| **Wado** (Gale)     | 4.47 MB/s  | 2.988 ms | 1.68x   |

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
| JavaScript (Prism)           | 10.46 MB/s  | 1.278 ms  | 1.00x   |
| **Wado** (Gale)              | 3.44 MB/s   | 3.891 ms  | 3.05x   |
| JavaScript (Lezer)           | 3.19 MB/s   | 4.187 ms  | 3.28x   |
| Rust (tree-sitter)           | 2.60 MB/s   | 5.133 ms  | 4.02x   |
| JavaScript (web-tree-sitter) | 1.76 MB/s   | 7.593 ms  | 5.94x   |
| JavaScript (Shiki)           | 715.34 KB/s | 18.685 ms | 14.62x  |

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
