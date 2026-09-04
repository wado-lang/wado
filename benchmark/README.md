# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-04, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| C              | 11.83 M nums/s | 84.561 ms | 1.00x   |
| **Wado**       | 11.54 M nums/s | 86.650 ms | 1.02x   |
| JavaScript     | 11.28 M nums/s | 88.673 ms | 1.05x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.99 M px/s |  98.457 ms | 1.00x   |
| **Wado**       | 7.86 M px/s | 100.109 ms | 1.02x   |
| C              | 7.85 M px/s | 100.227 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 816.14 M nums/s | 2.451 ms | 1.00x   |
| JavaScript     | 576.90 M nums/s | 3.467 ms | 1.41x   |
| **Wado**       | 371.61 M nums/s | 5.381 ms | 2.20x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.89 M conv/s | 35.853 ms | 1.00x   |
| Rust (core::fmt) | 21.43 M conv/s | 46.671 ms | 1.30x   |
| C (printf)       | 11.74 M conv/s | 85.189 ms | 2.38x   |

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
| Rust (serde_json)    |   1.92 GB/s | 0.329 ms | 1.00x   |
| JavaScript (JSON)    |   1.51 GB/s | 0.419 ms | 1.27x   |
| **Wado** (core:json) | 763.48 MB/s | 0.827 ms | 2.51x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 579.84 MB/s | 1.089 ms | 1.00x   |
| JavaScript (JSON)    | 579.09 MB/s | 1.091 ms | 1.00x   |
| **Wado** (core:json) | 265.68 MB/s | 2.376 ms | 2.18x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.41 GB/s | 0.262 ms | 1.00x   |
| **Wado** (core:cbor) |  1.43 GB/s | 0.441 ms | 1.68x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 880.87 MB/s | 0.717 ms | 1.00x   |
| **Wado** (core:cbor) | 431.24 MB/s | 1.464 ms | 2.04x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 962.68 MB/s | 2.338 ms | 1.00x   |
| JavaScript (JSON)    | 584.35 MB/s | 3.852 ms | 1.65x   |
| **Wado** (core:json) | 317.51 MB/s | 7.089 ms | 3.03x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 382.70 MB/s | 5.882 ms | 1.00x   |
| Rust (serde_json)    | 357.60 MB/s | 6.295 ms | 1.07x   |
| **Wado** (core:json) | 227.97 MB/s | 9.874 ms | 1.68x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.61 GB/s | 0.862 ms | 1.00x   |
| **Wado** (core:cbor) | 715.16 MB/s | 3.147 ms | 3.65x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.21 GB/s | 1.855 ms | 1.00x   |
| **Wado** (core:cbor) | 451.36 MB/s | 4.987 ms | 2.69x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.41 GB/s | 0.392 ms | 1.00x   |
| **Wado** (core:json) |  1.82 GB/s | 0.950 ms | 2.42x   |
| JavaScript (JSON)    |  1.53 GB/s | 1.129 ms | 2.88x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.09 GB/s | 1.585 ms | 1.00x   |
| JavaScript (JSON)     | 792.10 MB/s | 2.181 ms | 1.38x   |
| **Wado** (PoC parser) | 480.87 MB/s | 3.591 ms | 2.27x   |
| **Wado** (core:json)  | 331.30 MB/s | 5.213 ms | 3.29x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.75 GB/s | 0.461 ms | 1.00x   |
| **Wado** (core:cbor) |  2.55 GB/s | 0.676 ms | 1.47x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.65 GB/s | 0.651 ms | 1.00x   |
| **Wado** (core:cbor) | 827.96 MB/s | 2.086 ms | 3.20x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 340.78 MB/s | 1.853 ms | 1.00x   |
| JavaScript (node:zlib) | 208.28 MB/s | 3.032 ms | 1.64x   |
| C (zlib 1.3.1, Wasm)   | 135.25 MB/s | 4.669 ms | 2.52x   |
| **Wado** (core:zlib)   | 123.71 MB/s | 5.104 ms | 2.75x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.41 GB/s | 0.185 ms | 1.00x   |
| JavaScript (node:zlib) |   1.90 GB/s | 0.332 ms | 1.79x   |
| C (zlib 1.3.1, Wasm)   | 861.31 MB/s | 0.733 ms | 3.96x   |
| **Wado** (core:zlib)   | 535.09 MB/s | 1.180 ms | 6.38x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 12.46 MB/s |   1.069 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.39 MB/s |   1.075 ms | 1.01x   |
| Java (ANTLR4)       |  0.11 MB/s | 123.101 ms | 115.16x |

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
| Prism.js                      | 12.65 MB/s |  1.053 ms | 1.00x   |
| **Gale** (Wado)               |  8.87 MB/s |  1.501 ms | 1.43x   |
| Lezer (CodeMirror)            |  5.07 MB/s |  2.626 ms | 2.49x   |
| tree-sitter (Rust native)     |  4.76 MB/s |  2.799 ms | 2.66x   |
| tree-sitter (web-tree-sitter) |  2.91 MB/s |  4.576 ms | 4.35x   |
| Shiki (JS engine)             |  1.12 MB/s | 11.943 ms | 11.34x  |

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
| Java (ANTLR4)   | 671.27 KB/s | 51.231 ms | 1.00x   |
| **Wado** (Gale) | 368.01 KB/s | 93.448 ms | 1.82x   |

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
| `GET /user`                     |      42,150 |                   39,872 |                16,369 |                    16,550 |
| `GET /user/lookup/username/hey` |      38,691 |                   46,579 |                15,954 |                    16,302 |
| `POST /event/abcd1234/comment`  |      40,586 |                   52,331 |                15,834 |                    15,148 |
| `GET /static/index.html`        |      40,713 |                   51,833 |                15,636 |                    16,104 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     380,680 |                  262,433 |                90,601 |                    79,221 |
| `GET /user/lookup/username/hey` |     367,005 |                  233,549 |                82,617 |                    75,821 |
| `POST /event/abcd1234/comment`  |     368,346 |                  235,189 |                99,962 |                    66,715 |
| `GET /static/index.html`        |     369,805 |                  235,773 |               116,791 |                    74,623 |

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
