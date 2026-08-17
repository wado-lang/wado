# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-16, wasmtime 47.0.3, gcc 11.4.0, wasi-sdk 33.0,
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
| C              | 12.28 M nums/s | 81.431 ms | 1.00x   |
| **Wado**       | 12.04 M nums/s | 83.053 ms | 1.02x   |
| JavaScript     | 11.78 M nums/s | 84.856 ms | 1.04x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |   ms/iter | vs best |
| -------------- | ----------: | --------: | ------- |
| **Wado**       | 8.28 M px/s | 95.010 ms | 1.00x   |
| JavaScript     | 8.27 M px/s | 95.054 ms | 1.00x   |
| C              | 8.18 M px/s | 96.159 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation |      Throughput |   ms/iter | vs best |
| -------------- | --------------: | --------: | ------- |
| C              |   1.05 G nums/s |  9.564 ms | 1.00x   |
| JavaScript     | 469.96 M nums/s | 21.278 ms | 2.22x   |
| **Wado**       | 309.21 M nums/s | 32.340 ms | 3.38x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| **Wado**         | 20.89 M conv/s |  47.860 ms | 1.00x   |
| Rust (core::fmt) | 20.12 M conv/s |  49.692 ms | 1.04x   |
| C (printf)       |  9.17 M conv/s | 109.057 ms | 2.28x   |

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
| Rust (serde_json)    |   2.15 GB/s | 0.293 ms | 1.00x   |
| JavaScript (JSON)    |   1.98 GB/s | 0.319 ms | 1.09x   |
| **Wado** (core:json) | 643.52 MB/s | 0.981 ms | 3.35x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 911.54 MB/s | 0.693 ms | 1.00x   |
| JavaScript (JSON)    | 637.61 MB/s | 0.990 ms | 1.43x   |
| **Wado** (core:json) | 153.36 MB/s | 4.117 ms | 5.94x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.55 GB/s | 0.248 ms | 1.00x   |
| **Wado** (core:cbor) | 771.78 MB/s | 0.818 ms | 3.30x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 866.96 MB/s | 0.728 ms | 1.00x   |
| **Wado** (core:cbor) | 189.90 MB/s | 3.325 ms | 4.57x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| Rust (serde_json)    | 980.32 MB/s |  2.296 ms | 1.00x   |
| JavaScript (JSON)    | 627.87 MB/s |  3.585 ms | 1.56x   |
| **Wado** (core:json) | 173.18 MB/s | 12.998 ms | 5.66x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 459.12 MB/s |  4.903 ms | 1.00x   |
| Rust (serde_json)    | 383.73 MB/s |  5.866 ms | 1.20x   |
| **Wado** (core:json) | 179.14 MB/s | 12.565 ms | 2.56x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.72 GB/s | 0.827 ms | 1.00x   |
| **Wado** (core:cbor) | 514.50 MB/s | 4.375 ms | 5.29x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.08 GB/s | 2.079 ms | 1.00x   |
| **Wado** (core:cbor) | 332.51 MB/s | 6.769 ms | 3.26x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.15 GB/s | 0.416 ms | 1.00x   |
| JavaScript (JSON)    |  1.59 GB/s | 1.088 ms | 2.62x   |
| **Wado** (core:json) |  1.09 GB/s | 1.591 ms | 3.82x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   1.19 GB/s | 1.446 ms | 1.00x   |
| JavaScript (JSON)    | 895.68 MB/s | 1.928 ms | 1.33x   |
| **Wado** (core:json) | 280.19 MB/s | 6.164 ms | 4.26x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.63 GB/s | 0.476 ms | 1.00x   |
| **Wado** (core:cbor) |  1.38 GB/s | 1.252 ms | 2.63x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.57 GB/s | 0.672 ms | 1.00x   |
| **Wado** (core:cbor) | 631.33 MB/s | 2.735 ms | 4.07x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6; the
compressed sizes still differ marginally between implementations.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 339.47 MB/s | 1.860 ms | 1.00x   |
| JavaScript (node:zlib) | 218.15 MB/s | 2.895 ms | 1.56x   |
| C (zlib 1.3.1, Wasm)   | 133.65 MB/s | 4.725 ms | 2.54x   |
| **Wado** (core:zlib)   |  75.12 MB/s | 8.407 ms | 4.52x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.23 GB/s | 0.195 ms | 1.00x   |
| JavaScript (node:zlib) |   1.88 GB/s | 0.336 ms | 1.72x   |
| C (zlib 1.3.1, Wasm)   | 847.68 MB/s | 0.745 ms | 3.82x   |
| **Wado** (core:zlib)   | 387.47 MB/s | 1.629 ms | 8.35x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 11.96 MB/s |   1.117 ms | 1.00x   |
| **Wado** (Gale)     | 10.23 MB/s |   1.306 ms | 1.17x   |
| Java (ANTLR4)       |  0.11 MB/s | 122.650 ms | 109.80x |

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
| Prism.js                      | 14.97 MB/s |  0.893 ms | 1.00x   |
| **Gale** (Wado)               |  7.97 MB/s |  1.676 ms | 1.88x   |
| Lezer (CodeMirror)            |  4.82 MB/s |  2.772 ms | 3.10x   |
| tree-sitter (Rust native)     |  4.01 MB/s |  3.329 ms | 3.73x   |
| tree-sitter (web-tree-sitter) |  2.48 MB/s |  5.387 ms | 6.03x   |
| Shiki (JS engine)             |  1.05 MB/s | 12.688 ms | 14.21x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| **Wado** (Gale) | 241.11 KB/s | 142.629 ms | 1.00x   |
| Java (ANTLR4)   |  38.80 KB/s | 886.344 ms | 6.21x   |

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
| `GET /user`                                 |               116,296 |                    16,619 |                   38,976 |     278,888 |
| `GET /user/comments`                        |               112,731 |                    16,580 |                   41,841 |     279,610 |
| `GET /user/lookup/username/hey`             |               109,223 |                    16,291 |                   39,179 |     276,021 |
| `GET /event/abcd1234/comments`              |               109,086 |                    16,368 |                   44,083 |     275,601 |
| `POST /event/abcd1234/comment`              |               110,706 |                    15,039 |                   33,072 |     283,813 |
| `GET /very/deeply/nested/route/hello/there` |               115,340 |                    16,658 |                   42,398 |     278,252 |
| `GET /static/index.html`                    |               110,493 |                    16,172 |                   35,840 |     275,168 |

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
