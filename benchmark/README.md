# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-17, wasmtime 47.0.3, gcc 11.4.0, wasi-sdk 33.0,
rustc 1.97.1, Node.js v26.7.0, Bun 1.3.14, Linux x86_64.

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

| Implementation |     Throughput |   ms/iter | vs best |
| -------------- | -------------: | --------: | ------- |
| C              | 11.67 M nums/s | 85.678 ms | 1.00x   |
| **Wado**       | 11.37 M nums/s | 87.949 ms | 1.03x   |
| JavaScript     | 10.98 M nums/s | 91.038 ms | 1.06x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.87 M px/s |  99.894 ms | 1.00x   |
| **Wado**       | 7.84 M px/s | 100.261 ms | 1.00x   |
| C              | 7.77 M px/s | 101.228 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              |   1.07 G nums/s | 1.862 ms | 1.00x   |
| JavaScript     | 553.17 M nums/s | 3.615 ms | 1.94x   |
| **Wado**       | 378.61 M nums/s | 5.282 ms | 2.84x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach: at 32M the same code
swings 38% on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| Rust (core::fmt) | 20.41 M conv/s |  48.985 ms | 1.00x   |
| **Wado**         | 20.11 M conv/s |  49.737 ms | 1.02x   |
| C (printf)       |  9.28 M conv/s | 107.785 ms | 2.20x   |

## Serialization & Compression

Each dataset is measured under two codecs, JSON and CBOR, over the same Wado
data types. Each codec is a comparison of its own: JSON puts `core:json` (Wado)
against `serde_json` (Rust) and `JSON.stringify` / `JSON.parse` (JS), CBOR puts
`core:cbor` (Wado) against `serde_cbor` (Rust). Throughput is reported over the
JSON source size in both, so the CBOR figures stay readable next to the JSON
ones; `vs best` ranks within one codec.

### twitter

`twitter.json` (631514 bytes): a Twitter API search response with 100 statuses.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   2.12 GB/s | 0.298 ms | 1.00x   |
| JavaScript (JSON)    |   1.91 GB/s | 0.330 ms | 1.11x   |
| **Wado** (core:json) | 643.16 MB/s | 0.981 ms | 3.29x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 896.53 MB/s | 0.704 ms | 1.00x   |
| JavaScript (JSON)    | 658.11 MB/s | 0.960 ms | 1.36x   |
| **Wado** (core:json) | 164.71 MB/s | 3.834 ms | 5.45x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.48 GB/s | 0.254 ms | 1.00x   |
| **Wado** (core:cbor) | 731.65 MB/s | 0.863 ms | 3.40x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 854.32 MB/s | 0.739 ms | 1.00x   |
| **Wado** (core:cbor) | 208.00 MB/s | 3.036 ms | 4.11x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| Rust (serde_json)    | 961.14 MB/s |  2.342 ms | 1.00x   |
| JavaScript (JSON)    | 594.45 MB/s |  3.787 ms | 1.62x   |
| **Wado** (core:json) | 173.44 MB/s | 12.978 ms | 5.54x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 432.44 MB/s |  5.206 ms | 1.00x   |
| Rust (serde_json)    | 355.59 MB/s |  6.330 ms | 1.22x   |
| **Wado** (core:json) | 175.29 MB/s | 12.841 ms | 2.47x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.54 GB/s | 0.887 ms | 1.00x   |
| **Wado** (core:cbor) | 509.39 MB/s | 4.419 ms | 4.98x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.08 GB/s | 2.087 ms | 1.00x   |
| **Wado** (core:cbor) | 317.10 MB/s | 7.098 ms | 3.40x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.06 GB/s | 0.425 ms | 1.00x   |
| JavaScript (JSON)    |  1.54 GB/s | 1.124 ms | 2.64x   |
| **Wado** (core:json) |  1.08 GB/s | 1.606 ms | 3.78x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.18 GB/s | 1.469 ms | 1.00x   |
| JavaScript (JSON)     | 912.85 MB/s | 1.892 ms | 1.29x   |
| **Wado** (PoC parser) | 405.15 MB/s | 4.263 ms | 2.90x   |
| **Wado** (core:json)  | 270.31 MB/s | 6.389 ms | 4.35x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It is the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.54 GB/s | 0.489 ms | 1.00x   |
| **Wado** (core:cbor) |  1.28 GB/s | 1.354 ms | 2.77x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.57 GB/s | 0.673 ms | 1.00x   |
| **Wado** (core:cbor) | 614.52 MB/s | 2.810 ms | 4.18x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6; the
compressed sizes still differ marginally between implementations.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 324.72 MB/s | 1.945 ms | 1.00x   |
| JavaScript (node:zlib) | 201.83 MB/s | 3.129 ms | 1.61x   |
| C (zlib 1.3.1, Wasm)   | 131.51 MB/s | 4.802 ms | 2.47x   |
| **Wado** (core:zlib)   |  73.66 MB/s | 8.573 ms | 4.41x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.20 GB/s | 0.198 ms | 1.00x   |
| JavaScript (node:zlib) |   1.76 GB/s | 0.359 ms | 1.81x   |
| C (zlib 1.3.1, Wasm)   | 824.44 MB/s | 0.766 ms | 3.87x   |
| **Wado** (core:zlib)   | 380.76 MB/s | 1.658 ms | 8.37x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 11.87 MB/s |   1.126 ms | 1.00x   |
| **Wado** (Gale)     | 10.27 MB/s |   1.301 ms | 1.16x   |
| Java (ANTLR4)       |  0.10 MB/s | 132.364 ms | 117.55x |

Java (ANTLR4) is the head-to-head for Gale's generated parser, on the JVM and
JIT-warmed to steady state (per-parse time flattens after ~50 parses, so the gap
is algorithmic, not a warmup artifact). The cost is full-context LL — this
grammar's ambiguities defeat the two-stage SLL fast path. Needs `java`; skipped
if absent.

### Syntax Highlight

Highlight 81 SQL statements (13366 bytes). Gale-generated highlighter vs five
reference SQL highlighters:

- **Prism.js** — regex-based, the speed reference (ultimate goal)
- **tree-sitter (Rust native)** — same `tree-sitter-sequel` grammar used by the
  JS row below, run as a Rust binary
- **Lezer (CodeMirror)** — `@codemirror/lang-sql` + `@lezer/highlight`, a
  pure-JS LR parser
- **tree-sitter (web-tree-sitter)** — official JS WASM binding, same
  `tree-sitter-sequel` grammar as the Rust row (upstream
  `@derekstride/tree-sitter-sql`)
- **Shiki (JS engine)** — TextMate grammars, VSCode-quality output

Labels here name the highlighter rather than the language: this benchmark is
about what a browser would run.

| Implementation                | Throughput |   ms/iter | vs best |
| ----------------------------- | ---------: | --------: | ------- |
| Prism.js                      | 14.99 MB/s |  0.892 ms | 1.00x   |
| **Gale** (Wado)               |  8.20 MB/s |  1.629 ms | 1.83x   |
| Lezer (CodeMirror)            |  4.74 MB/s |  2.819 ms | 3.16x   |
| tree-sitter (Rust native)     |  3.98 MB/s |  3.359 ms | 3.77x   |
| tree-sitter (web-tree-sitter) |  2.56 MB/s |  5.216 ms | 5.85x   |
| Shiki (JS engine)             |  1.09 MB/s | 12.265 ms | 13.75x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| **Wado** (Gale) | 236.58 KB/s | 145.362 ms | 1.00x   |
| Java (ANTLR4)   |  38.68 KB/s | 888.985 ms | 6.12x   |

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

Throughput (requests/sec, higher is better), all over HTTP/1.1. Every server
gets the same worker count and the same pinned cores, and each table ends with a
headroom check confirming `oha` was not the ceiling.

One worker — a 1-core container scaled out horizontally:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |      40,794 |                   43,160 |                    17,345 |                13,926 |
| `GET /user/lookup/username/hey` |      38,655 |                   35,139 |                    17,001 |                13,371 |
| `POST /event/abcd1234/comment`  |      39,073 |                   34,829 |                    16,038 |                12,607 |
| `GET /static/index.html`        |      40,318 |                   28,922 |                    16,789 |                13,202 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     175,858 |                  135,246 |                    74,512 |                51,086 |
| `GET /user/lookup/username/hey` |     165,198 |                  124,424 |                    72,921 |                47,110 |
| `POST /event/abcd1234/comment`  |     166,878 |                  123,569 |                    67,045 |                47,577 |
| `GET /static/index.html`        |     165,837 |                  131,001 |                    71,801 |                47,465 |

`wado serve` is the slowest of the four in both shapes, and stops gaining past
roughly eight workers (8 → 12 workers is +2.6%), so its ceiling on a larger host
is lower than the four-worker figures suggest.

HTTP routing needs `oha` and Bun, and is measured separately
(`SLICE=10 ROUNDS=3 SHAPES="1 4" mise run benchmark-http-routing`).

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
