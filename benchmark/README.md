# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-02, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
rustc 1.98.0, Node.js v26.7.0, Bun 1.3.14, Linux x86_64.

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
| C              | 11.35 M nums/s | 88.069 ms | 1.00x   |
| **Wado**       | 11.16 M nums/s | 89.625 ms | 1.02x   |
| JavaScript     | 10.94 M nums/s | 91.438 ms | 1.04x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.79 M px/s | 101.009 ms | 1.00x   |
| **Wado**       | 7.76 M px/s | 101.370 ms | 1.00x   |
| C              | 7.64 M px/s | 102.953 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 768.47 M nums/s | 2.603 ms | 1.00x   |
| JavaScript     | 555.71 M nums/s | 3.599 ms | 1.38x   |
| **Wado**       | 358.92 M nums/s | 5.572 ms | 2.14x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.14 M conv/s | 36.843 ms | 1.00x   |
| Rust (core::fmt) | 21.42 M conv/s | 46.676 ms | 1.27x   |
| C (printf)       | 11.68 M conv/s | 85.616 ms | 2.32x   |

## Serialization & Compression

Each dataset is measured under two codecs, JSON and CBOR, over the same Wado
data types. Each codec is a comparison of its own: JSON puts `core:json` (Wado)
against `serde_json` (Rust) and `JSON.stringify` / `JSON.parse` (JS), CBOR puts
`core:cbor` (Wado) against `serde_cbor` (Rust). Throughput is reported over the
JSON source size in both, so the CBOR figures stay readable next to the JSON
ones; `vs best` ranks within one codec.

Every row starts and ends at UTF-8 bytes; the JS ones go through `TextEncoder`
and `TextDecoder` to get there. What they build still differs — Rust and Wado a
typed struct tree, `JSON.parse` an untyped object graph with no type checking.

### twitter

`twitter.json` (631514 bytes): a Twitter API search response with 100 statuses.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   1.84 GB/s | 0.343 ms | 1.00x   |
| JavaScript (JSON)    |   1.55 GB/s | 0.406 ms | 1.18x   |
| **Wado** (core:json) | 739.75 MB/s | 0.853 ms | 2.49x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 570.26 MB/s | 1.107 ms | 1.00x   |
| JavaScript (JSON)    | 567.94 MB/s | 1.112 ms | 1.00x   |
| **Wado** (core:json) | 249.56 MB/s | 2.530 ms | 2.29x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.33 GB/s | 0.271 ms | 1.00x   |
| **Wado** (core:cbor) |  1.35 GB/s | 0.466 ms | 1.72x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 852.38 MB/s | 0.741 ms | 1.00x   |
| **Wado** (core:cbor) | 409.02 MB/s | 1.543 ms | 2.08x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 933.56 MB/s | 2.411 ms | 1.00x   |
| JavaScript (JSON)    | 568.75 MB/s | 3.958 ms | 1.64x   |
| **Wado** (core:json) | 267.18 MB/s | 8.425 ms | 3.49x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 358.13 MB/s |  6.286 ms | 1.00x   |
| Rust (serde_json)    | 345.40 MB/s |  6.517 ms | 1.04x   |
| **Wado** (core:json) | 185.28 MB/s | 12.149 ms | 1.93x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.50 GB/s | 0.900 ms | 1.00x   |
| **Wado** (core:cbor) | 696.88 MB/s | 3.230 ms | 3.59x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.17 GB/s | 1.925 ms | 1.00x   |
| **Wado** (core:cbor) | 347.79 MB/s | 6.472 ms | 3.36x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.20 GB/s | 0.411 ms | 1.00x   |
| **Wado** (core:json) |  1.55 GB/s | 1.115 ms | 2.71x   |
| JavaScript (JSON)    |  1.43 GB/s | 1.205 ms | 2.93x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.02 GB/s | 1.686 ms | 1.00x   |
| JavaScript (JSON)     | 756.58 MB/s | 2.283 ms | 1.35x   |
| **Wado** (PoC parser) | 440.50 MB/s | 3.921 ms | 2.33x   |
| **Wado** (core:json)  | 288.02 MB/s | 5.996 ms | 3.56x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.68 GB/s | 0.470 ms | 1.00x   |
| **Wado** (core:cbor) |  2.40 GB/s | 0.718 ms | 1.53x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.55 GB/s | 0.678 ms | 1.00x   |
| **Wado** (core:cbor) | 664.82 MB/s | 2.598 ms | 3.83x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 316.63 MB/s | 1.994 ms | 1.00x   |
| JavaScript (node:zlib) | 200.28 MB/s | 3.153 ms | 1.58x   |
| C (zlib 1.3.1, Wasm)   | 131.29 MB/s | 4.810 ms | 2.41x   |
| **Wado** (core:zlib)   |  76.40 MB/s | 8.265 ms | 4.15x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.15 GB/s | 0.201 ms | 1.00x   |
| JavaScript (node:zlib) |   1.78 GB/s | 0.355 ms | 1.77x   |
| C (zlib 1.3.1, Wasm)   | 823.68 MB/s | 0.767 ms | 3.82x   |
| **Wado** (core:zlib)   | 383.07 MB/s | 1.648 ms | 8.20x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 11.64 MB/s |   1.145 ms | 1.00x   |
| **Wado** (Gale)     | 11.18 MB/s |   1.191 ms | 1.04x   |
| Java (ANTLR4)       |  0.10 MB/s | 131.340 ms | 114.71x |

Java (ANTLR4) is the head-to-head for Gale's generated parser, on the JVM and
JIT-warmed to steady state, so the gap is algorithmic rather than a warmup
artifact. The cost is full-context LL — this
grammar's ambiguities defeat the two-stage SLL fast path. Needs `java`; skipped
if absent.

### Syntax Highlight

Highlight 81 SQL statements (13321 bytes). Gale-generated highlighter vs five
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
| Prism.js                      | 11.73 MB/s |  1.136 ms | 1.00x   |
| **Gale** (Wado)               |  8.55 MB/s |  1.557 ms | 1.37x   |
| Lezer (CodeMirror)            |  4.79 MB/s |  2.784 ms | 2.45x   |
| tree-sitter (Rust native)     |  4.57 MB/s |  2.916 ms | 2.57x   |
| tree-sitter (web-tree-sitter) |  2.71 MB/s |  4.921 ms | 4.33x   |
| Shiki (JS engine)             |  1.07 MB/s | 12.502 ms | 11.01x  |

Every highlighter parses the corpus without errors: a highlighter that gives up
on a region skips the work of colouring it, so the constructs two of them
mishandled are written another way at the same token count.

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |   ms/iter | vs best |
| --------------- | ----------: | --------: | ------- |
| Java (ANTLR4)   | 646.22 KB/s | 53.217 ms | 1.00x   |
| **Wado** (Gale) | 353.10 KB/s | 97.393 ms | 1.83x   |

Both rows run in-process and warm, emitting a parser and no listeners: Gale a
Wado recursive-descent one from memory, ANTLR4 Java onto disk.

ANTLR4 also re-reads the grammars each iteration; both terms are small next to
generation. That row needs `java`/`javac` and is skipped without them.

## Application Server

### HTTP Routing

End-to-end HTTP throughput of `wado serve` vs [Hono](https://hono.dev/) on
Node.js and Bun, vs native-Rust [Axum](https://github.com/tokio-rs/axum), over
Hono's official router benchmark route set driven with `oha`. See
`http_routing/README.md` for the full route set and methodology.

Throughput (requests/sec, higher is better), all over HTTP/1.1. Every server
gets the same worker count and the same pinned cores.

One worker — a 1-core container scaled out horizontally:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |      43,360 |                   49,949 |                17,514 |                    17,896 |
| `GET /user/lookup/username/hey` |      42,225 |                   43,375 |                17,496 |                    17,153 |
| `POST /event/abcd1234/comment`  |      43,176 |                   39,379 |                17,937 |                    15,763 |
| `GET /static/index.html`        |      43,220 |                   39,372 |                18,288 |                    17,197 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     378,792 |                  268,175 |                98,322 |                    79,612 |
| `GET /user/lookup/username/hey` |     378,037 |                  222,830 |                92,334 |                    76,802 |
| `POST /event/abcd1234/comment`  |     372,773 |                  220,616 |                92,669 |                    66,981 |
| `GET /static/index.html`        |     372,651 |                  235,271 |                93,442 |                    76,202 |

`wado serve` places third: level with Hono on Node at one worker, clear of it at
four. The allocation behind its `content-length` header value costs it a few
percent of every request.

`SHAPES` names worker counts, not cores. Scaling past them is a question this
harness cannot answer: the generator-to-server thread ratio moves the result
more than the cores do, and it shifts with every point.

The Axum row at four workers is a floor — `oha` runs out of CPU there before the
server does, so the figure is the generator's limit rather than Axum's.

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
