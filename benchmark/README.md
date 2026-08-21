# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-21, wasmtime 47.0.3, gcc 11.4.0, wasi-sdk 33.0,
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
| C              | 11.20 M nums/s | 89.315 ms | 1.00x   |
| **Wado**       | 10.92 M nums/s | 91.581 ms | 1.03x   |
| JavaScript     | 10.63 M nums/s | 94.051 ms | 1.05x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| **Wado**       | 7.57 M px/s | 103.938 ms | 1.00x   |
| JavaScript     | 7.56 M px/s | 104.067 ms | 1.00x   |
| C              | 7.51 M px/s | 104.721 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              |   1.03 G nums/s | 1.933 ms | 1.00x   |
| JavaScript     | 538.71 M nums/s | 3.713 ms | 1.92x   |
| **Wado**       | 370.73 M nums/s | 5.394 ms | 2.79x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| Rust (core::fmt) | 19.37 M conv/s |  51.635 ms | 1.00x   |
| **Wado**         | 19.18 M conv/s |  52.124 ms | 1.01x   |
| C (printf)       |  8.75 M conv/s | 114.336 ms | 2.21x   |

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
| Rust (serde_json)    |   2.03 GB/s | 0.311 ms | 1.00x   |
| JavaScript (JSON)    |   1.41 GB/s | 0.449 ms | 1.44x   |
| **Wado** (core:json) | 612.11 MB/s | 1.031 ms | 3.32x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 544.97 MB/s | 1.159 ms | 1.00x   |
| Rust (serde_json)    | 533.34 MB/s | 1.184 ms | 1.02x   |
| **Wado** (core:json) | 155.21 MB/s | 4.068 ms | 3.51x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.37 GB/s | 0.267 ms | 1.00x   |
| **Wado** (core:cbor) | 688.24 MB/s | 0.917 ms | 3.43x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 804.46 MB/s | 0.785 ms | 1.00x   |
| **Wado** (core:cbor) | 202.07 MB/s | 3.125 ms | 3.98x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| Rust (serde_json)    | 913.79 MB/s |  2.463 ms | 1.00x   |
| JavaScript (JSON)    | 546.55 MB/s |  4.119 ms | 1.67x   |
| **Wado** (core:json) | 177.45 MB/s | 12.685 ms | 5.15x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 352.44 MB/s |  6.387 ms | 1.00x   |
| Rust (serde_json)    | 334.11 MB/s |  6.738 ms | 1.05x   |
| **Wado** (core:json) | 162.15 MB/s | 13.882 ms | 2.17x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.46 GB/s | 0.914 ms | 1.00x   |
| **Wado** (core:cbor) | 531.04 MB/s | 4.238 ms | 4.64x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.04 GB/s | 2.167 ms | 1.00x   |
| **Wado** (core:cbor) | 305.86 MB/s | 7.359 ms | 3.40x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  3.99 GB/s | 0.433 ms | 1.00x   |
| JavaScript (JSON)    |  1.37 GB/s | 1.257 ms | 2.90x   |
| **Wado** (core:json) |  1.03 GB/s | 1.674 ms | 3.87x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.00 GB/s | 1.725 ms | 1.00x   |
| JavaScript (JSON)     | 728.40 MB/s | 2.371 ms | 1.37x   |
| **Wado** (PoC parser) | 378.74 MB/s | 4.560 ms | 2.64x   |
| **Wado** (core:json)  | 249.82 MB/s | 6.913 ms | 4.01x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.45 GB/s | 0.501 ms | 1.00x   |
| **Wado** (core:cbor) |  1.23 GB/s | 1.399 ms | 2.79x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.26 GB/s | 0.764 ms | 1.00x   |
| **Wado** (core:cbor) | 594.87 MB/s | 2.903 ms | 3.80x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 312.21 MB/s | 2.023 ms | 1.00x   |
| JavaScript (node:zlib) | 191.96 MB/s | 3.290 ms | 1.63x   |
| C (zlib 1.3.1, Wasm)   | 126.89 MB/s | 4.977 ms | 2.46x   |
| **Wado** (core:zlib)   |  69.00 MB/s | 9.152 ms | 4.52x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.06 GB/s | 0.206 ms | 1.00x   |
| JavaScript (node:zlib) |   1.75 GB/s | 0.361 ms | 1.75x   |
| C (zlib 1.3.1, Wasm)   | 795.18 MB/s | 0.794 ms | 3.85x   |
| **Wado** (core:zlib)   | 350.41 MB/s | 1.802 ms | 8.75x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 11.24 MB/s |   1.185 ms | 1.00x   |
| **Wado** (Gale)     | 10.03 MB/s |   1.328 ms | 1.12x   |
| Java (ANTLR4)       |  0.09 MB/s | 140.234 ms | 118.34x |

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
| Prism.js                      | 11.02 MB/s |  1.208 ms | 1.00x   |
| **Gale** (Wado)               |  7.65 MB/s |  1.741 ms | 1.44x   |
| Lezer (CodeMirror)            |  4.50 MB/s |  2.962 ms | 2.45x   |
| tree-sitter (Rust native)     |  4.22 MB/s |  3.158 ms | 2.61x   |
| tree-sitter (web-tree-sitter) |  2.62 MB/s |  5.093 ms | 4.22x   |
| Shiki (JS engine)             |  0.96 MB/s | 13.549 ms | 11.22x  |

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

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| Java (ANTLR4)   | 588.02 KB/s |  58.485 ms | 1.00x   |
| **Wado** (Gale) | 236.78 KB/s | 145.238 ms | 2.48x   |

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
| `GET /user`                     |      42,971 |                   43,519 |                16,803 |                    16,616 |
| `GET /user/lookup/username/hey` |      41,283 |                   39,842 |                16,299 |                    16,476 |
| `POST /event/abcd1234/comment`  |      42,649 |                   37,053 |                16,298 |                    15,228 |
| `GET /static/index.html`        |      40,564 |                   41,992 |                16,559 |                    16,279 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     374,969 |                  264,426 |                91,820 |                    77,430 |
| `GET /user/lookup/username/hey` |     367,134 |                  224,187 |                85,520 |                    73,675 |
| `POST /event/abcd1234/comment`  |     373,798 |                  227,640 |                84,187 |                    64,281 |
| `GET /static/index.html`        |     373,769 |                  228,782 |                86,381 |                    72,744 |

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
