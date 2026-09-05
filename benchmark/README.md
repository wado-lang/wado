# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-05, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| C              | 11.65 M nums/s | 85.832 ms | 1.00x   |
| **Wado**       | 11.35 M nums/s | 88.082 ms | 1.03x   |
| JavaScript     | 11.12 M nums/s | 89.964 ms | 1.05x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |   ms/iter | vs best |
| -------------- | ----------: | --------: | ------- |
| JavaScript     | 8.12 M px/s | 96.865 ms | 1.00x   |
| C              | 7.99 M px/s | 98.385 ms | 1.02x   |
| **Wado**       | 7.95 M px/s | 98.979 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 776.54 M nums/s | 2.576 ms | 1.00x   |
| JavaScript     | 562.03 M nums/s | 3.559 ms | 1.38x   |
| **Wado**       | 374.64 M nums/s | 5.338 ms | 2.07x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 28.70 M conv/s | 34.848 ms | 1.00x   |
| Rust (core::fmt) | 22.41 M conv/s | 44.625 ms | 1.28x   |
| C (printf)       | 11.95 M conv/s | 83.669 ms | 2.40x   |

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
| Rust (serde_json)    |   1.98 GB/s | 0.319 ms | 1.00x   |
| JavaScript (JSON)    |   1.57 GB/s | 0.403 ms | 1.26x   |
| **Wado** (core:json) | 767.02 MB/s | 0.823 ms | 2.58x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 578.83 MB/s | 1.091 ms | 1.00x   |
| Rust (serde_json)    | 568.97 MB/s | 1.110 ms | 1.02x   |
| **Wado** (core:json) | 268.44 MB/s | 2.352 ms | 2.16x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.43 GB/s | 0.260 ms | 1.00x   |
| **Wado** (core:cbor) |  1.39 GB/s | 0.454 ms | 1.75x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 863.23 MB/s | 0.732 ms | 1.00x   |
| **Wado** (core:cbor) | 431.95 MB/s | 1.462 ms | 2.00x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 946.77 MB/s | 2.378 ms | 1.00x   |
| JavaScript (JSON)    | 574.88 MB/s | 3.916 ms | 1.65x   |
| **Wado** (core:json) | 314.96 MB/s | 7.147 ms | 3.01x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 358.97 MB/s | 6.271 ms | 1.00x   |
| Rust (serde_json)    | 351.04 MB/s | 6.412 ms | 1.02x   |
| **Wado** (core:json) | 229.57 MB/s | 9.805 ms | 1.56x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.53 GB/s | 0.888 ms | 1.00x   |
| **Wado** (core:cbor) | 753.93 MB/s | 2.985 ms | 3.36x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.18 GB/s | 1.904 ms | 1.00x   |
| **Wado** (core:cbor) | 467.46 MB/s | 4.815 ms | 2.53x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.34 GB/s | 0.398 ms | 1.00x   |
| **Wado** (core:json) |  1.79 GB/s | 0.964 ms | 2.42x   |
| JavaScript (JSON)    |  1.55 GB/s | 1.114 ms | 2.80x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.05 GB/s | 1.647 ms | 1.00x   |
| JavaScript (JSON)     | 816.66 MB/s | 2.115 ms | 1.28x   |
| **Wado** (PoC parser) | 455.74 MB/s | 3.789 ms | 2.30x   |
| **Wado** (core:json)  | 324.74 MB/s | 5.318 ms | 3.23x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.96 GB/s | 0.436 ms | 1.00x   |
| **Wado** (core:cbor) |  2.51 GB/s | 0.688 ms | 1.58x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.70 GB/s | 0.640 ms | 1.00x   |
| **Wado** (core:cbor) | 808.12 MB/s | 2.137 ms | 3.34x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 322.97 MB/s | 1.955 ms | 1.00x   |
| JavaScript (node:zlib) | 208.72 MB/s | 3.026 ms | 1.55x   |
| C (zlib 1.3.1, Wasm)   | 131.84 MB/s | 4.790 ms | 2.45x   |
| **Wado** (core:zlib)   | 115.76 MB/s | 5.455 ms | 2.79x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.19 GB/s | 0.198 ms | 1.00x   |
| JavaScript (node:zlib) |   1.82 GB/s | 0.348 ms | 1.76x   |
| C (zlib 1.3.1, Wasm)   | 859.03 MB/s | 0.735 ms | 3.71x   |
| **Wado** (core:zlib)   | 503.94 MB/s | 1.253 ms | 6.33x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 13.67 MB/s |   0.974 ms | 1.00x   |
| Rust (sqlparser-rs) | 11.96 MB/s |   1.114 ms | 1.14x   |
| Java (ANTLR4)       |  0.11 MB/s | 125.591 ms | 128.94x |

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
| Prism.js                      | 12.04 MB/s |  1.106 ms | 1.00x   |
| **Gale** (Wado)               | 10.57 MB/s |  1.260 ms | 1.14x   |
| Lezer (CodeMirror)            |  4.87 MB/s |  2.737 ms | 2.47x   |
| tree-sitter (Rust native)     |  4.57 MB/s |  2.916 ms | 2.64x   |
| tree-sitter (web-tree-sitter) |  2.82 MB/s |  4.731 ms | 4.28x   |
| Shiki (JS engine)             |  1.07 MB/s | 12.436 ms | 11.24x  |

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
| Java (ANTLR4)   | 657.50 KB/s | 52.304 ms | 1.00x   |
| **Wado** (Gale) | 373.46 KB/s | 92.084 ms | 1.76x   |

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
| `GET /user`                     |      44,854 |                   45,982 |                16,594 |                    17,404 |
| `GET /user/lookup/username/hey` |      43,193 |                   42,929 |                16,355 |                    16,732 |
| `POST /event/abcd1234/comment`  |      44,715 |                   34,628 |                16,237 |                    15,622 |
| `GET /static/index.html`        |      45,428 |                   50,184 |                16,094 |                    16,634 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     370,720 |                  263,507 |                88,567 |                    79,780 |
| `GET /user/lookup/username/hey` |     366,934 |                  229,973 |                84,744 |                    76,790 |
| `POST /event/abcd1234/comment`  |     368,792 |                  228,596 |               120,269 |                    65,164 |
| `GET /static/index.html`        |     369,692 |                  230,423 |               123,896 |                    75,584 |

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
