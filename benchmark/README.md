# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-03, wasmtime 47.0.2, gcc 13.3.0, rustc 1.97.1,
Node.js v24.14.1, Bun 1.3.11, Linux x86_64.

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
| C              | 5.54 M nums/s | 180.384 ms | 1.00x   |
| **Wado**       | 4.58 M nums/s | 218.414 ms | 1.21x   |
| JavaScript     | 3.83 M nums/s | 261.197 ms | 1.45x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| JavaScript     | 4.13 M px/s | 190.306 ms | 1.00x   |
| **Wado**       | 4.12 M px/s | 190.802 ms | 1.00x   |
| C              | 4.10 M px/s | 191.775 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter    | vs best |
| -------------- | --------------- | ---------- | ------- |
| C              | 167.01 M nums/s | 59.878 ms  | 1.00x   |
| JavaScript     | 132.03 M nums/s | 75.738 ms  | 1.26x   |
| **Wado**       | 97.13 M nums/s  | 102.951 ms | 1.72x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput    | ms/iter    | vs best |
| ---------------- | ------------- | ---------- | ------- |
| Rust (core::fmt) | 9.87 M conv/s | 101.368 ms | 1.00x   |
| **Wado** (fpfmt) | 7.63 M conv/s | 131.145 ms | 1.29x   |
| C (printf)       | 5.58 M conv/s | 179.301 ms | 1.77x   |

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
| serde_cbor (Rust)    | 1.48 GB/s   | 0.428 ms | 1.00x   |
| serde_json (Rust)    | 1.16 GB/s   | 0.543 ms | 1.27x   |
| **core:cbor** (Wado) | 377.60 MB/s | 1.672 ms | 3.91x   |
| JSON (JS)            | 191.80 MB/s | 3.293 ms | 7.69x   |
| **core:json** (Wado) | 139.90 MB/s | 4.513 ms | 10.54x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_json (Rust)    | 470.20 MB/s | 1.343 ms  | 1.00x   |
| serde_cbor (Rust)    | 426.56 MB/s | 1.480 ms  | 1.10x   |
| JSON (JS)            | 302.32 MB/s | 2.089 ms  | 1.56x   |
| **core:cbor** (Wado) | 66.44 MB/s  | 9.504 ms  | 7.08x   |
| **core:json** (Wado) | 57.87 MB/s  | 10.912 ms | 8.13x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.46 GB/s   | 1.541 ms  | 1.00x   |
| serde_json (Rust)    | 580.19 MB/s | 3.880 ms  | 2.52x   |
| **core:cbor** (Wado) | 129.91 MB/s | 17.328 ms | 11.24x  |
| JSON (JS)            | 117.06 MB/s | 19.229 ms | 12.48x  |
| **core:json** (Wado) | 61.26 MB/s  | 36.748 ms | 23.85x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 580.34 MB/s | 3.879 ms  | 1.00x   |
| JSON (JS)            | 195.28 MB/s | 11.527 ms | 2.97x   |
| serde_json (Rust)    | 183.70 MB/s | 12.254 ms | 3.16x   |
| **core:cbor** (Wado) | 107.36 MB/s | 20.967 ms | 5.41x   |
| **core:json** (Wado) | 75.43 MB/s  | 29.843 ms | 7.69x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 2.36 GB/s   | 0.733 ms | 1.00x   |
| serde_json (Rust)    | 2.30 GB/s   | 0.751 ms | 1.02x   |
| JSON (JS)            | 520.20 MB/s | 3.320 ms | 4.53x   |
| **core:cbor** (Wado) | 492.76 MB/s | 3.505 ms | 4.78x   |
| **core:json** (Wado) | 253.49 MB/s | 6.813 ms | 9.29x   |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.36 GB/s   | 1.270 ms  | 1.00x   |
| serde_json (Rust)    | 596.61 MB/s | 2.895 ms  | 2.28x   |
| JSON (JS)            | 390.20 MB/s | 4.427 ms  | 3.49x   |
| **core:cbor** (Wado) | 198.83 MB/s | 8.686 ms  | 6.84x   |
| **core:json** (Wado) | 91.17 MB/s  | 18.945 ms | 14.92x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 143.48 MB/s | 4.401 ms  | 1.00x   |
| JavaScript (node:zlib) | 124.03 MB/s | 5.092 ms  | 1.16x   |
| **Wado** (core:zlib)   | 30.18 MB/s  | 20.921 ms | 4.75x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.32 GB/s   | 0.479 ms | 1.00x   |
| JavaScript (node:zlib) | 669.76 MB/s | 0.943 ms | 1.97x   |
| **Wado** (core:zlib)   | 136.57 MB/s | 4.624 ms | 9.65x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput | ms/iter    | vs best |
| ------------------- | ---------- | ---------- | ------- |
| Rust (sqlparser-rs) | 5.91 MB/s  | 2.263 ms   | 1.00x   |
| **Wado** (Gale)     | 4.02 MB/s  | 3.323 ms   | 1.47x   |
| Java (ANTLR4)       | 0.05 MB/s  | 279.347 ms | 123.44x |

ANTLR4 (Java) is the head-to-head for Gale's generated parser, on the JVM and
JIT-warmed to steady state (per-parse time flattens after ~50 parses, so the gap
is algorithmic, not a warmup artifact). The cost is full-context LL — this
grammar's ambiguities defeat the two-stage SLL fast path. Needs `java`; skipped
if absent.

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
| JavaScript (Prism)           | 5.67 MB/s   | 2.357 ms  | 1.00x   |
| **Wado** (Gale)              | 2.93 MB/s   | 4.561 ms  | 1.94x   |
| JavaScript (Lezer)           | 2.07 MB/s   | 6.455 ms  | 2.74x   |
| Rust (tree-sitter)           | 2.05 MB/s   | 6.515 ms  | 2.76x   |
| JavaScript (web-tree-sitter) | 1.23 MB/s   | 10.876 ms | 4.61x   |
| JavaScript (Shiki)           | 488.48 KB/s | 27.363 ms | 11.61x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  | Throughput | ms/iter     | vs best |
| --------------- | ---------- | ----------- | ------- |
| **Wado** (Gale) | 97.16 KB/s | 353.946 ms  | 1.00x   |
| Java (ANTLR4)   | 25.32 KB/s | 1358.275 ms | 3.84x   |

Gale is measured in-process (grammar assembly + code generation) and emits a
Wado recursive-descent parser; ANTLR4 runs its reference jar
(`java -jar antlr-4.13.2-complete.jar -Dlanguage=Java`, ~0.14 s of which is JVM
startup) over the same two files and emits Java. The ANTLR4 row needs `java` and
is skipped if it is absent.

## Application Server

### HTTP Routing

End-to-end HTTP throughput of `wado serve` vs [Hono](https://hono.dev/) on
Node.js and Bun, vs native-Rust [Axum](https://github.com/tokio-rs/axum), over
Hono's official router benchmark route set driven with `oha`. See
`http_routing/README.md` for the full route set and methodology.

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                     |       30,835 |      18,660 |     31,979 |        73,465 |
| `GET /user/lookup/username/hey` |       26,835 |      15,513 |     31,223 |        75,372 |
| `GET /event/abcd1234/comments`  |       24,982 |      15,931 |     27,405 |        72,029 |
| `POST /event/abcd1234/comment`  |       26,181 |      13,138 |     31,410 |        72,488 |
| `GET /static/index.html`        |       26,019 |      15,048 |     31,427 |        70,690 |

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
mise run benchmark-gale-gen         # Gale generator over the Rust grammar

# application server
mise run benchmark-http-routing     # HTTP routing (wado serve vs Hono vs Axum)
```

Prerequisites: `cc` and `cargo` (system); `node` and `bun` (managed by
`mise install`). The ANTLR4 reference rows (gale-gen, sqlite-parse) need `java`
(sqlite-parse also `javac`); the jar is fetched to `~/.cache/gale`. Those rows
are skipped if the tool is absent.

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
