# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-02, wasmtime 46.0.1, gcc 13.3.0, rustc 1.96.1,
Node.js v24.14.1, Linux x86_64.

Throughput is work per second (higher is better). Native rows are optimized
builds (C `gcc -O3`, Rust release, Wado `-O2`); JavaScript runs on Node.js.
`vs best` is the fastest row's throughput over this row's (1.00x = fastest).
Absolute throughput is machine-dependent, so compare by `vs best`. Each figure
is the best of three runs.

Benchmarks are grouped into four sections: pure computation, serialization &
compression, parsing, and application server.

## Pure Computation

### Prime Counting

Count primes up to 1M (integer arithmetic, trial division).

| Implementation | Throughput    | vs best |
| -------------- | ------------- | ------- |
| C              | 7.32 M nums/s | 1.00x   |
| **Wado**       | 7.18 M nums/s | 1.02x   |
| JavaScript     | 6.70 M nums/s | 1.09x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | vs best |
| -------------- | ----------- | ------- |
| C              | 5.02 M px/s | 1.00x   |
| JavaScript     | 4.85 M px/s | 1.04x   |
| **Wado**       | 4.53 M px/s | 1.11x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | vs best |
| -------------- | --------------- | ------- |
| C              | 263.46 M nums/s | 1.00x   |
| JavaScript     | 188.10 M nums/s | 1.40x   |
| **Wado**       | 143.61 M nums/s | 1.83x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | vs best |
| ---------------- | -------------- | ------- |
| Rust (core::fmt) | 13.72 M conv/s | 1.00x   |
| **Wado** (fpfmt) | 11.97 M conv/s | 1.15x   |
| C (printf)       | 8.48 M conv/s  | 1.62x   |

## Serialization & Compression

Each dataset is measured under two codecs — JSON (`core:json`) and CBOR
(`core:cbor`) — over the same Wado data types, so serialization and
deserialization compare both across languages and across codecs. `serde_json` /
`serde_cbor` (Rust) and `JSON.stringify` / `JSON.parse` (JS) are the references.
Throughput for both phases is reported over the JSON source size (the shared
denominator across codecs). Each cell shows throughput and `vs best` within its
column.

### twitter

`twitter.json` (631514 bytes): a Twitter API search response with 100 statuses.

| Implementation       | Serialize            | Deserialize         |
| -------------------- | -------------------- | ------------------- |
| serde_json (Rust)    | 1.90 GB/s (1.00x)    | 738.19 MB/s (1.00x) |
| JSON (JS)            | 320.42 MB/s (5.93x)  | 584.98 MB/s (1.26x) |
| **core:json** (Wado) | 137.47 MB/s (13.82x) | 92.01 MB/s (8.02x)  |
| serde_cbor (Rust)    | 1.89 GB/s (1.01x)    | 636.05 MB/s (1.16x) |
| **core:cbor** (Wado) | 549.39 MB/s (3.46x)  | 115.17 MB/s (6.41x) |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

| Implementation       | Serialize           | Deserialize         |
| -------------------- | ------------------- | ------------------- |
| serde_json (Rust)    | 726.67 MB/s (2.49x) | 301.33 MB/s (3.04x) |
| JSON (JS)            | 182.71 MB/s (9.91x) | 324.49 MB/s (2.83x) |
| **core:json** (Wado) | 73.54 MB/s (24.61x) | 127.83 MB/s (7.18x) |
| serde_cbor (Rust)    | 1.81 GB/s (1.00x)   | 917.33 MB/s (1.00x) |
| **core:cbor** (Wado) | 208.14 MB/s (8.70x) | 148.16 MB/s (6.19x) |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

| Implementation       | Serialize            | Deserialize         |
| -------------------- | -------------------- | ------------------- |
| serde_json (Rust)    | 3.92 GB/s (1.00x)    | 867.22 MB/s (2.11x) |
| JSON (JS)            | 855.57 MB/s (4.58x)  | 661.74 MB/s (2.77x) |
| **core:json** (Wado) | 280.80 MB/s (13.96x) | 191.65 MB/s (9.55x) |
| serde_cbor (Rust)    | 2.77 GB/s (1.42x)    | 1.83 GB/s (1.00x)   |
| **core:cbor** (Wado) | 726.45 MB/s (5.40x)  | 309.79 MB/s (5.91x) |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | vs best |
| ---------------------- | ----------- | ------- |
| Rust (zlib-rs)         | 219.04 MB/s | 1.00x   |
| JavaScript (node:zlib) | 152.61 MB/s | 1.44x   |
| **Wado** (core:zlib)   | 35.71 MB/s  | 6.13x   |

Decompress:

| Implementation         | Throughput  | vs best |
| ---------------------- | ----------- | ------- |
| Rust (zlib-rs)         | 1.83 GB/s   | 1.00x   |
| JavaScript (node:zlib) | 1.08 GB/s   | 1.69x   |
| **Wado** (core:zlib)   | 217.14 MB/s | 8.43x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | vs best |
| ------------------- | ---------- | ------- |
| Rust (sqlparser-rs) | 7.96 MB/s  | 1.00x   |
| **Wado** (Gale)     | 4.62 MB/s  | 1.72x   |

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

| Implementation               | Throughput  | vs best |
| ---------------------------- | ----------- | ------- |
| JavaScript (Prism)           | 10.51 MB/s  | 1.00x   |
| JavaScript (Lezer)           | 3.15 MB/s   | 3.34x   |
| Rust (tree-sitter)           | 2.82 MB/s   | 3.73x   |
| **Wado** (Gale)              | 2.50 MB/s   | 4.20x   |
| JavaScript (web-tree-sitter) | 1.84 MB/s   | 5.71x   |
| JavaScript (Shiki)           | 735.08 KB/s | 14.30x  |

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
