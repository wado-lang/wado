# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-15, wasmtime 47.0.3, gcc 11.4.0, wasi-sdk 33.0,
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
| C              | 12.16 M nums/s | 82.268 ms | 1.00x   |
| **Wado**       | 11.82 M nums/s | 84.609 ms | 1.03x   |
| JavaScript     | 11.63 M nums/s | 86.010 ms | 1.05x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |   ms/iter | vs best |
| -------------- | ----------: | --------: | ------- |
| JavaScript     | 8.24 M px/s | 95.496 ms | 1.00x   |
| **Wado**       | 8.17 M px/s | 96.265 ms | 1.01x   |
| C              | 8.10 M px/s | 97.106 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation |      Throughput |   ms/iter | vs best |
| -------------- | --------------: | --------: | ------- |
| C              | 896.55 M nums/s | 11.154 ms | 1.00x   |
| JavaScript     | 462.82 M nums/s | 21.606 ms | 1.94x   |
| **Wado**       | 348.64 M nums/s | 28.683 ms | 2.57x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| Rust (core::fmt) | 21.63 M conv/s |  46.225 ms | 1.00x   |
| **Wado**         | 20.87 M conv/s |  47.910 ms | 1.04x   |
| C (printf)       |  9.76 M conv/s | 102.503 ms | 2.22x   |

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
| Rust (serde_json)    |   2.25 GB/s | 0.280 ms | 1.00x   |
| JavaScript (JSON)    |   2.08 GB/s | 0.304 ms | 1.09x   |
| **Wado** (core:json) | 472.03 MB/s | 1.337 ms | 4.77x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 959.19 MB/s | 0.658 ms | 1.00x   |
| JavaScript (JSON)    | 699.50 MB/s | 0.903 ms | 1.37x   |
| **Wado** (core:json) | 159.82 MB/s | 3.951 ms | 6.00x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.61 GB/s | 0.242 ms | 1.00x   |
| **Wado** (core:cbor) |  1.01 GB/s | 0.627 ms | 2.59x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 914.34 MB/s | 0.691 ms | 1.00x   |
| **Wado** (core:cbor) | 192.04 MB/s | 3.288 ms | 4.76x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| Rust (serde_json)    |   1.02 GB/s |  2.204 ms | 1.00x   |
| JavaScript (JSON)    | 628.18 MB/s |  3.583 ms | 1.63x   |
| **Wado** (core:json) | 168.57 MB/s | 13.354 ms | 6.06x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 460.57 MB/s |  4.888 ms | 1.00x   |
| Rust (serde_json)    | 381.69 MB/s |  5.898 ms | 1.21x   |
| **Wado** (core:json) | 195.68 MB/s | 11.503 ms | 2.35x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.71 GB/s | 0.829 ms | 1.00x   |
| **Wado** (core:cbor) | 403.52 MB/s | 5.578 ms | 6.73x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.14 GB/s | 1.970 ms | 1.00x   |
| **Wado** (core:cbor) | 359.09 MB/s | 6.268 ms | 3.18x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   4.28 GB/s | 0.404 ms | 1.00x   |
| JavaScript (JSON)    |   1.60 GB/s | 1.079 ms | 2.67x   |
| **Wado** (core:json) | 771.53 MB/s | 2.238 ms | 5.54x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   1.24 GB/s | 1.391 ms | 1.00x   |
| JavaScript (JSON)    | 966.07 MB/s | 1.788 ms | 1.29x   |
| **Wado** (core:json) | 294.60 MB/s | 5.862 ms | 4.21x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.80 GB/s | 0.454 ms | 1.00x   |
| **Wado** (core:cbor) |  1.56 GB/s | 1.108 ms | 2.44x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.77 GB/s | 0.623 ms | 1.00x   |
| **Wado** (core:cbor) | 633.34 MB/s | 2.727 ms | 4.38x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6; the
compressed sizes still differ marginally between implementations.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 347.86 MB/s | 1.815 ms | 1.00x   |
| JavaScript (node:zlib) | 215.47 MB/s | 2.931 ms | 1.61x   |
| C (zlib 1.3.1, Wasm)   | 140.88 MB/s | 4.483 ms | 2.47x   |
| **Wado** (core:zlib)   |  77.97 MB/s | 8.098 ms | 4.46x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.40 GB/s | 0.186 ms | 1.00x   |
| JavaScript (node:zlib) |   1.89 GB/s | 0.334 ms | 1.80x   |
| C (zlib 1.3.1, Wasm)   | 880.47 MB/s | 0.717 ms | 3.85x   |
| **Wado** (core:zlib)   | 413.89 MB/s | 1.525 ms | 8.20x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 12.63 MB/s |   1.058 ms | 1.00x   |
| **Wado** (Gale)     |  9.89 MB/s |   1.350 ms | 1.28x   |
| Java (ANTLR4)       |  0.11 MB/s | 122.647 ms | 115.92x |

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
| Prism.js                      | 15.92 MB/s |  0.840 ms | 1.00x   |
| **Gale** (Wado)               |  7.51 MB/s |  1.780 ms | 2.12x   |
| Lezer (CodeMirror)            |  5.17 MB/s |  2.587 ms | 3.08x   |
| tree-sitter (Rust native)     |  4.23 MB/s |  3.159 ms | 3.76x   |
| tree-sitter (web-tree-sitter) |  2.68 MB/s |  4.980 ms | 5.93x   |
| Shiki (JS engine)             |  1.11 MB/s | 12.029 ms | 14.32x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| **Wado** (Gale) | 245.66 KB/s | 139.990 ms | 1.00x   |
| Java (ANTLR4)   |  40.21 KB/s | 855.300 ms | 6.11x   |

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

Throughput (requests/sec, higher is better), all over HTTP/1.1:

| Request                                     | **Wado** (wado serve) | JavaScript (Hono on Node) | JavaScript (Hono on Bun) | Rust (Axum) |
| ------------------------------------------- | --------------------: | ------------------------: | -----------------------: | ----------: |
| `GET /user`                                 |               123,669 |                    18,047 |                   44,183 |     295,568 |
| `GET /user/comments`                        |               122,235 |                    18,023 |                   41,950 |     295,758 |
| `GET /user/lookup/username/hey`             |               119,140 |                    18,061 |                   36,965 |     293,992 |
| `GET /event/abcd1234/comments`              |               119,128 |                    17,975 |                   32,835 |     291,892 |
| `POST /event/abcd1234/comment`              |               117,017 |                    16,256 |                   41,057 |     294,394 |
| `GET /very/deeply/nested/route/hello/there` |               120,828 |                    18,550 |                   40,945 |     294,906 |
| `GET /static/index.html`                    |               118,335 |                    17,902 |                   42,200 |     291,419 |

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
