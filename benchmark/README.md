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
| C              | 7.25 M nums/s | 137.874 ms | 1.00x   |
| **Wado**       | 7.16 M nums/s | 139.641 ms | 1.01x   |
| JavaScript     | 6.83 M nums/s | 146.504 ms | 1.06x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| C              | 4.97 M px/s | 158.307 ms | 1.00x   |
| JavaScript     | 4.86 M px/s | 161.675 ms | 1.02x   |
| **Wado**       | 4.53 M px/s | 173.569 ms | 1.10x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 264.41 M nums/s | 37.820 ms | 1.00x   |
| JavaScript     | 193.29 M nums/s | 51.736 ms | 1.37x   |
| **Wado**       | 148.64 M nums/s | 67.277 ms | 1.78x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 13.55 M conv/s | 73.814 ms  | 1.00x   |
| **Wado** (fpfmt) | 11.77 M conv/s | 84.938 ms  | 1.15x   |
| C (printf)       | 8.29 M conv/s  | 120.610 ms | 1.63x   |

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
| serde_cbor (Rust)    | 1.88 GB/s   | 0.336 ms | 1.00x   |
| serde_json (Rust)    | 1.86 GB/s   | 0.339 ms | 1.01x   |
| **core:cbor** (Wado) | 553.34 MB/s | 1.141 ms | 3.40x   |
| JSON (JS)            | 344.20 MB/s | 1.835 ms | 5.46x   |
| **core:json** (Wado) | 134.00 MB/s | 4.712 ms | 14.03x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 723.77 MB/s | 0.873 ms | 1.00x   |
| serde_cbor (Rust)    | 640.77 MB/s | 0.986 ms | 1.13x   |
| JSON (JS)            | 583.93 MB/s | 1.081 ms | 1.24x   |
| **core:cbor** (Wado) | 115.49 MB/s | 5.468 ms | 6.27x   |
| **core:json** (Wado) | 91.86 MB/s  | 6.874 ms | 7.88x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.83 GB/s   | 1.233 ms  | 1.00x   |
| serde_json (Rust)    | 747.93 MB/s | 3.010 ms  | 2.45x   |
| **core:cbor** (Wado) | 204.58 MB/s | 11.003 ms | 8.95x   |
| JSON (JS)            | 180.39 MB/s | 12.479 ms | 10.14x  |
| **core:json** (Wado) | 73.13 MB/s  | 30.779 ms | 25.02x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 923.04 MB/s | 2.439 ms  | 1.00x   |
| JSON (JS)            | 323.62 MB/s | 6.956 ms  | 2.85x   |
| serde_json (Rust)    | 307.87 MB/s | 7.312 ms  | 3.00x   |
| **core:cbor** (Wado) | 151.01 MB/s | 14.906 ms | 6.11x   |
| **core:json** (Wado) | 128.88 MB/s | 17.465 ms | 7.16x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 3.34 GB/s   | 0.517 ms | 1.00x   |
| serde_cbor (Rust)    | 2.84 GB/s   | 0.608 ms | 1.18x   |
| JSON (JS)            | 851.43 MB/s | 2.029 ms | 3.92x   |
| **core:cbor** (Wado) | 743.26 MB/s | 2.323 ms | 4.49x   |
| **core:json** (Wado) | 268.90 MB/s | 6.423 ms | 12.42x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 1.97 GB/s   | 0.875 ms | 1.00x   |
| serde_json (Rust)    | 861.09 MB/s | 2.006 ms | 2.29x   |
| JSON (JS)            | 661.56 MB/s | 2.611 ms | 2.98x   |
| **core:cbor** (Wado) | 325.44 MB/s | 5.307 ms | 6.05x   |
| **core:json** (Wado) | 193.31 MB/s | 8.934 ms | 10.19x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 217.40 MB/s | 2.905 ms  | 1.00x   |
| JavaScript (node:zlib) | 156.27 MB/s | 4.041 ms  | 1.39x   |
| **Wado** (core:zlib)   | 35.72 MB/s  | 17.681 ms | 6.09x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.87 GB/s   | 0.337 ms | 1.00x   |
| JavaScript (node:zlib) | 1.14 GB/s   | 0.552 ms | 1.64x   |
| **Wado** (core:zlib)   | 225.38 MB/s | 2.801 ms | 8.30x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 8.05 MB/s  | 1.661 ms | 1.00x   |
| **Wado** (Gale)     | 4.66 MB/s  | 2.869 ms | 1.73x   |

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
| JavaScript (Prism)           | 10.86 MB/s  | 1.231 ms  | 1.00x   |
| JavaScript (Lezer)           | 3.32 MB/s   | 4.026 ms  | 3.27x   |
| Rust (tree-sitter)           | 2.88 MB/s   | 4.644 ms  | 3.77x   |
| **Wado** (Gale)              | 2.52 MB/s   | 5.313 ms  | 4.31x   |
| JavaScript (web-tree-sitter) | 1.85 MB/s   | 7.233 ms  | 5.87x   |
| JavaScript (Shiki)           | 743.69 KB/s | 17.973 ms | 14.60x  |

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
