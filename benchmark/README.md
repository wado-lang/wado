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
| C              | 10.99 M nums/s | 90.955 ms | 1.00x   |
| **Wado**       | 10.64 M nums/s | 93.944 ms | 1.03x   |
| JavaScript     | 10.21 M nums/s | 97.959 ms | 1.08x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| **Wado**       | 7.45 M px/s | 105.564 ms | 1.00x   |
| C              | 7.37 M px/s | 106.748 ms | 1.01x   |
| JavaScript     | 7.36 M px/s | 106.818 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation |      Throughput |   ms/iter | vs best |
| -------------- | --------------: | --------: | ------- |
| C              | 776.01 M nums/s | 12.886 ms | 1.00x   |
| JavaScript     | 419.78 M nums/s | 23.822 ms | 1.85x   |
| **Wado**       | 309.35 M nums/s | 32.325 ms | 2.51x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| Rust (core::fmt) | 17.94 M conv/s |  55.754 ms | 1.00x   |
| **Wado**         | 15.89 M conv/s |  62.936 ms | 1.13x   |
| C (printf)       |  7.88 M conv/s | 126.972 ms | 2.28x   |

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
| Rust (serde_json)    |   1.95 GB/s | 0.324 ms | 1.00x   |
| JavaScript (JSON)    |   1.83 GB/s | 0.345 ms | 1.06x   |
| **Wado** (core:json) | 373.96 MB/s | 1.688 ms | 5.21x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 799.61 MB/s | 0.790 ms | 1.00x   |
| JavaScript (JSON)    | 595.99 MB/s | 1.060 ms | 1.34x   |
| **Wado** (core:json) | 131.34 MB/s | 4.808 ms | 6.09x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.21 GB/s | 0.286 ms | 1.00x   |
| **Wado** (core:cbor) | 866.63 MB/s | 0.728 ms | 2.55x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 770.49 MB/s | 0.820 ms | 1.00x   |
| **Wado** (core:cbor) | 161.63 MB/s | 3.907 ms | 4.76x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| Rust (serde_json)    | 886.73 MB/s |  2.539 ms | 1.00x   |
| JavaScript (JSON)    | 549.62 MB/s |  4.096 ms | 1.61x   |
| **Wado** (core:json) | 144.60 MB/s | 15.567 ms | 6.13x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 394.80 MB/s |  5.702 ms | 1.00x   |
| Rust (serde_json)    | 338.63 MB/s |  6.648 ms | 1.17x   |
| **Wado** (core:json) | 180.26 MB/s | 12.487 ms | 2.19x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.34 GB/s | 0.962 ms | 1.00x   |
| **Wado** (core:cbor) | 334.97 MB/s | 6.720 ms | 6.99x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.01 GB/s | 2.222 ms | 1.00x   |
| **Wado** (core:cbor) | 297.44 MB/s | 7.568 ms | 3.41x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   3.87 GB/s | 0.446 ms | 1.00x   |
| JavaScript (JSON)    |   1.41 GB/s | 1.224 ms | 2.74x   |
| **Wado** (core:json) | 648.68 MB/s | 2.662 ms | 5.97x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   1.09 GB/s | 1.578 ms | 1.00x   |
| JavaScript (JSON)    | 808.98 MB/s | 2.135 ms | 1.35x   |
| **Wado** (core:json) | 246.85 MB/s | 6.996 ms | 4.43x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.23 GB/s | 0.534 ms | 1.00x   |
| **Wado** (core:cbor) |  1.40 GB/s | 1.229 ms | 2.30x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.36 GB/s | 0.733 ms | 1.00x   |
| **Wado** (core:cbor) | 561.69 MB/s | 3.075 ms | 4.20x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6; the
compressed sizes still differ marginally between implementations.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 334.75 MB/s | 1.887 ms | 1.00x   |
| JavaScript (node:zlib) | 209.51 MB/s | 3.014 ms | 1.60x   |
| C (zlib 1.3.1, Wasm)   | 134.93 MB/s | 4.680 ms | 2.48x   |
| **Wado** (core:zlib)   |  75.65 MB/s | 8.347 ms | 4.42x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.27 GB/s | 0.193 ms | 1.00x   |
| JavaScript (node:zlib) |   1.91 GB/s | 0.331 ms | 1.71x   |
| C (zlib 1.3.1, Wasm)   | 850.90 MB/s | 0.742 ms | 3.84x   |
| **Wado** (core:zlib)   | 389.16 MB/s | 1.622 ms | 8.40x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 10.75 MB/s |   1.244 ms | 1.00x   |
| **Wado** (Gale)     |  8.54 MB/s |   1.565 ms | 1.26x   |
| Java (ANTLR4)       |  0.09 MB/s | 140.803 ms | 113.19x |

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

| Implementation                |  Throughput |   ms/iter | vs best |
| ----------------------------- | ----------: | --------: | ------- |
| Prism.js                      |  13.63 MB/s |  0.981 ms | 1.00x   |
| **Gale** (Wado)               |   6.44 MB/s |  2.074 ms | 2.11x   |
| Lezer (CodeMirror)            |   4.31 MB/s |  3.098 ms | 3.16x   |
| tree-sitter (Rust native)     |   3.57 MB/s |  3.739 ms | 3.81x   |
| tree-sitter (web-tree-sitter) |   2.27 MB/s |  5.887 ms | 6.00x   |
| Shiki (JS engine)             | 969.00 KB/s | 13.794 ms | 14.06x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| **Wado** (Gale) | 205.77 KB/s | 167.131 ms | 1.00x   |
| Java (ANTLR4)   |  35.62 KB/s | 965.397 ms | 5.78x   |

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
| `GET /user`                                 |                99,407 |                    16,628 |                   30,433 |     247,249 |
| `GET /user/comments`                        |               100,132 |                    16,510 |                   35,555 |     240,503 |
| `GET /user/lookup/username/hey`             |                94,165 |                    15,753 |                   32,841 |     244,809 |
| `GET /event/abcd1234/comments`              |                92,531 |                    15,845 |                   33,670 |     235,848 |
| `POST /event/abcd1234/comment`              |                91,651 |                    14,559 |                   38,207 |     240,567 |
| `GET /very/deeply/nested/route/hello/there` |                96,531 |                    16,305 |                   30,446 |     239,581 |
| `GET /static/index.html`                    |                94,458 |                    15,447 |                   32,816 |     247,693 |

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
