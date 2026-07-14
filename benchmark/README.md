# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-14, wasmtime 46.0.1, gcc 13.3.0, rustc 1.96.1,
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
| C              | 5.90 M nums/s | 169.375 ms | 1.00x   |
| **Wado**       | 4.69 M nums/s | 213.007 ms | 1.26x   |
| JavaScript     | 3.92 M nums/s | 254.948 ms | 1.51x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| JavaScript     | 4.24 M px/s | 185.291 ms | 1.00x   |
| **Wado**       | 4.18 M px/s | 188.263 ms | 1.01x   |
| C              | 4.15 M px/s | 189.361 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 235.88 M nums/s | 42.394 ms | 1.00x   |
| JavaScript     | 173.93 M nums/s | 57.494 ms | 1.36x   |
| **Wado**       | 114.04 M nums/s | 87.688 ms | 2.07x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 10.13 M conv/s | 98.714 ms  | 1.00x   |
| **Wado** (fpfmt) | 6.75 M conv/s  | 148.186 ms | 1.50x   |
| C (printf)       | 5.87 M conv/s  | 170.305 ms | 1.73x   |

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
| serde_cbor (Rust)    | 1.70 GB/s   | 0.372 ms | 1.00x   |
| serde_json (Rust)    | 1.06 GB/s   | 0.596 ms | 1.60x   |
| **core:cbor** (Wado) | 398.99 MB/s | 1.582 ms | 4.36x   |
| JSON (JS)            | 241.87 MB/s | 2.611 ms | 7.20x   |
| **core:json** (Wado) | 163.99 MB/s | 3.850 ms | 10.62x  |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 506.52 MB/s | 1.247 ms | 1.00x   |
| serde_cbor (Rust)    | 443.91 MB/s | 1.423 ms | 1.14x   |
| JSON (JS)            | 315.08 MB/s | 2.004 ms | 1.61x   |
| **core:cbor** (Wado) | 70.72 MB/s  | 8.930 ms | 7.16x   |
| **core:json** (Wado) | 67.70 MB/s  | 9.328 ms | 7.48x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.53 GB/s   | 1.468 ms  | 1.00x   |
| serde_json (Rust)    | 639.36 MB/s | 3.521 ms  | 2.45x   |
| JSON (JS)            | 135.33 MB/s | 16.633 ms | 11.58x  |
| **core:cbor** (Wado) | 125.57 MB/s | 17.926 ms | 12.48x  |
| **core:json** (Wado) | 60.75 MB/s  | 37.056 ms | 25.79x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 615.25 MB/s | 3.659 ms  | 1.00x   |
| JSON (JS)            | 207.57 MB/s | 10.845 ms | 2.96x   |
| serde_json (Rust)    | 199.77 MB/s | 11.268 ms | 3.08x   |
| **core:json** (Wado) | 63.41 MB/s  | 35.499 ms | 9.70x   |
| **core:cbor** (Wado) | 53.03 MB/s  | 42.444 ms | 11.60x  |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 2.58 GB/s   | 0.669 ms | 1.00x   |
| serde_cbor (Rust)    | 2.58 GB/s   | 0.669 ms | 1.00x   |
| JSON (JS)            | 629.53 MB/s | 2.744 ms | 4.20x   |
| **core:cbor** (Wado) | 569.61 MB/s | 3.032 ms | 4.64x   |
| **core:json** (Wado) | 258.08 MB/s | 6.692 ms | 10.24x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.39 GB/s   | 1.241 ms  | 1.00x   |
| serde_json (Rust)    | 626.76 MB/s | 2.756 ms  | 2.27x   |
| JSON (JS)            | 419.19 MB/s | 4.120 ms  | 3.40x   |
| **core:cbor** (Wado) | 193.18 MB/s | 8.940 ms  | 7.37x   |
| **core:json** (Wado) | 106.25 MB/s | 16.255 ms | 13.40x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 154.49 MB/s | 4.088 ms  | 1.00x   |
| JavaScript (node:zlib) | 131.56 MB/s | 4.800 ms  | 1.17x   |
| **Wado** (core:zlib)   | 32.48 MB/s  | 19.440 ms | 4.76x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.47 GB/s   | 0.429 ms | 1.00x   |
| JavaScript (node:zlib) | 777.69 MB/s | 0.812 ms | 1.94x   |
| **Wado** (core:zlib)   | 129.96 MB/s | 4.859 ms | 11.58x  |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Gale-generated parser vs sqlparser-rs.

| Implementation      | Throughput | ms/iter  | vs best |
| ------------------- | ---------- | -------- | ------- |
| Rust (sqlparser-rs) | 6.80 MB/s  | 1.965 ms | 1.00x   |
| **Wado** (Gale)     | 3.92 MB/s  | 3.405 ms | 1.73x   |

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
| JavaScript (Prism)           | 6.72 MB/s   | 1.989 ms  | 1.00x   |
| **Wado** (Gale)              | 2.67 MB/s   | 5.006 ms  | 2.52x   |
| JavaScript (Lezer)           | 2.26 MB/s   | 5.923 ms  | 2.97x   |
| Rust (tree-sitter)           | 2.24 MB/s   | 5.972 ms  | 3.00x   |
| JavaScript (web-tree-sitter) | 1.34 MB/s   | 9.944 ms  | 5.01x   |
| JavaScript (Shiki)           | 573.18 KB/s | 23.319 ms | 12.01x  |

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
