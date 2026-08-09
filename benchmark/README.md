# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-08, wasmtime 47.0.2, gcc 13.3.0, rustc 1.97.1,
Node.js v26.3.1, Bun 1.3.11, Linux x86_64.

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

| Implementation |    Throughput |    ms/iter | vs best |
| -------------- | ------------: | ---------: | ------- |
| C              | 7.71 M nums/s | 129.720 ms | 1.00x   |
| **Wado**       | 7.57 M nums/s | 132.068 ms | 1.02x   |
| JavaScript     | 7.55 M nums/s | 132.402 ms | 1.02x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| C              | 6.02 M px/s | 130.566 ms | 1.00x   |
| **Wado**       | 5.62 M px/s | 139.924 ms | 1.07x   |
| JavaScript     | 5.60 M px/s | 140.418 ms | 1.07x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation |      Throughput |   ms/iter | vs best |
| -------------- | --------------: | --------: | ------- |
| C              | 237.73 M nums/s | 42.065 ms | 1.00x   |
| **Wado**       | 173.07 M nums/s | 57.779 ms | 1.37x   |
| JavaScript     | 166.10 M nums/s | 60.203 ms | 1.43x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| **Wado** (fpfmt) | 14.75 M conv/s |  67.805 ms | 1.00x   |
| Rust (core::fmt) | 14.29 M conv/s |  69.972 ms | 1.03x   |
| C (printf)       |  8.75 M conv/s | 114.290 ms | 1.69x   |

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

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| serde_cbor (Rust)    |   2.11 GB/s | 0.300 ms | 1.00x   |
| serde_json (Rust)    |   1.92 GB/s | 0.330 ms | 1.10x   |
| **core:cbor** (Wado) | 615.66 MB/s | 1.025 ms | 3.43x   |
| JSON (JS)            | 326.47 MB/s | 1.934 ms | 6.46x   |
| **core:json** (Wado) | 289.80 MB/s | 2.179 ms | 7.28x   |

Deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| serde_json (Rust)    | 805.12 MB/s | 0.784 ms | 1.00x   |
| serde_cbor (Rust)    | 736.87 MB/s | 0.857 ms | 1.09x   |
| JSON (JS)            | 592.70 MB/s | 1.065 ms | 1.36x   |
| **core:cbor** (Wado) | 126.18 MB/s | 5.004 ms | 6.38x   |
| **core:json** (Wado) | 112.96 MB/s | 5.590 ms | 7.13x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| serde_cbor (Rust)    |   2.01 GB/s |  1.120 ms | 1.00x   |
| serde_json (Rust)    | 668.14 MB/s |  3.369 ms | 3.01x   |
| **core:cbor** (Wado) | 303.60 MB/s |  7.414 ms | 6.62x   |
| JSON (JS)            | 184.00 MB/s | 12.234 ms | 10.92x  |
| **core:json** (Wado) | 134.96 MB/s | 16.679 ms | 14.89x  |

Deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| serde_cbor (Rust)    | 926.50 MB/s |  2.430 ms | 1.00x   |
| JSON (JS)            | 345.45 MB/s |  6.516 ms | 2.68x   |
| serde_json (Rust)    | 336.07 MB/s |  6.698 ms | 2.76x   |
| **core:cbor** (Wado) | 256.79 MB/s |  8.766 ms | 3.61x   |
| **core:json** (Wado) | 157.13 MB/s | 14.325 ms | 5.90x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| serde_json (Rust)    |   3.77 GB/s | 0.459 ms | 1.00x   |
| serde_cbor (Rust)    |   3.00 GB/s | 0.576 ms | 1.26x   |
| **core:cbor** (Wado) |   1.12 GB/s | 1.537 ms | 3.37x   |
| JSON (JS)            | 868.95 MB/s | 1.988 ms | 4.34x   |
| **core:json** (Wado) | 565.61 MB/s | 3.053 ms | 6.67x   |

Deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| serde_cbor (Rust)    |   2.07 GB/s | 0.836 ms | 1.00x   |
| serde_json (Rust)    | 895.01 MB/s | 1.930 ms | 2.31x   |
| JSON (JS)            | 642.97 MB/s | 2.686 ms | 3.22x   |
| **core:cbor** (Wado) | 446.17 MB/s | 3.871 ms | 4.64x   |
| **core:json** (Wado) | 195.62 MB/s | 8.829 ms | 10.58x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         |  Throughput |   ms/iter | vs best |
| ---------------------- | ----------: | --------: | ------- |
| Rust (zlib-rs)         | 234.22 MB/s |  2.696 ms | 1.00x   |
| JavaScript (node:zlib) | 162.72 MB/s |  3.881 ms | 1.44x   |
| **Wado** (core:zlib)   |  47.32 MB/s | 13.345 ms | 4.95x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   1.96 GB/s | 0.322 ms | 1.00x   |
| JavaScript (node:zlib) |   1.04 GB/s | 0.604 ms | 1.88x   |
| **Wado** (core:zlib)   | 263.27 MB/s | 2.398 ms | 7.44x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) |  8.34 MB/s |   1.602 ms | 1.00x   |
| **Wado** (Gale)     |  6.98 MB/s |   1.914 ms | 1.19x   |
| Java (ANTLR4)       |  0.08 MB/s | 176.911 ms | 104.25x |

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

| Implementation               |  Throughput |   ms/iter | vs best |
| ---------------------------- | ----------: | --------: | ------- |
| JavaScript (Prism)           |  10.74 MB/s |  1.244 ms | 1.00x   |
| **Wado** (Gale)              |   5.24 MB/s |  2.552 ms | 2.05x   |
| JavaScript (Lezer)           |   3.48 MB/s |  3.839 ms | 3.09x   |
| Rust (tree-sitter)           |   2.92 MB/s |  4.572 ms | 3.68x   |
| JavaScript (web-tree-sitter) |   1.94 MB/s |  6.903 ms | 5.54x   |
| JavaScript (Shiki)           | 748.81 KB/s | 17.850 ms | 14.34x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| **Wado** (Gale) | 173.45 KB/s | 198.268 ms | 1.00x   |
| Java (ANTLR4)   |  39.96 KB/s | 860.677 ms | 4.34x   |

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
(`SLICE=4 ROUNDS=5 CONNECTIONS=50 mise run benchmark-http-routing`). The table
above carries over from the previous run: this one had no `oha`.

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
