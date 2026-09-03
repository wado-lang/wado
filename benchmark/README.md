# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-03, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| C              | 11.30 M nums/s | 88.517 ms | 1.00x   |
| **Wado**       | 11.12 M nums/s | 89.941 ms | 1.02x   |
| JavaScript     | 10.91 M nums/s | 91.687 ms | 1.04x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.66 M px/s | 102.690 ms | 1.00x   |
| **Wado**       | 7.61 M px/s | 103.338 ms | 1.01x   |
| C              | 7.55 M px/s | 104.159 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 755.65 M nums/s | 2.647 ms | 1.00x   |
| JavaScript     | 531.62 M nums/s | 3.762 ms | 1.42x   |
| **Wado**       | 353.58 M nums/s | 5.656 ms | 2.14x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 26.15 M conv/s | 38.237 ms | 1.00x   |
| Rust (core::fmt) | 20.53 M conv/s | 48.708 ms | 1.27x   |
| C (printf)       | 11.01 M conv/s | 90.840 ms | 2.38x   |

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
| Rust (serde_json)    |   1.84 GB/s | 0.344 ms | 1.00x   |
| JavaScript (JSON)    |   1.50 GB/s | 0.422 ms | 1.23x   |
| **Wado** (core:json) | 722.36 MB/s | 0.874 ms | 2.54x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 561.29 MB/s | 1.125 ms | 1.00x   |
| Rust (serde_json)    | 556.14 MB/s | 1.136 ms | 1.01x   |
| **Wado** (core:json) | 246.50 MB/s | 2.561 ms | 2.28x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.31 GB/s | 0.274 ms | 1.00x   |
| **Wado** (core:cbor) |  1.34 GB/s | 0.471 ms | 1.72x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 842.02 MB/s | 0.750 ms | 1.00x   |
| **Wado** (core:cbor) | 405.27 MB/s | 1.558 ms | 2.08x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 927.16 MB/s | 2.428 ms | 1.00x   |
| JavaScript (JSON)    | 564.41 MB/s | 3.988 ms | 1.64x   |
| **Wado** (core:json) | 274.25 MB/s | 8.207 ms | 3.38x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 363.87 MB/s |  6.186 ms | 1.00x   |
| Rust (serde_json)    | 342.71 MB/s |  6.568 ms | 1.06x   |
| **Wado** (core:json) | 206.09 MB/s | 10.922 ms | 1.77x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.48 GB/s | 0.909 ms | 1.00x   |
| **Wado** (core:cbor) | 682.29 MB/s | 3.299 ms | 3.63x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.14 GB/s | 1.967 ms | 1.00x   |
| **Wado** (core:cbor) | 346.08 MB/s | 6.504 ms | 3.31x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.11 GB/s | 0.420 ms | 1.00x   |
| **Wado** (core:json) |  1.54 GB/s | 1.125 ms | 2.68x   |
| JavaScript (JSON)    |  1.42 GB/s | 1.220 ms | 2.90x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     | 997.53 MB/s | 1.731 ms | 1.00x   |
| JavaScript (JSON)     | 802.94 MB/s | 2.151 ms | 1.24x   |
| **Wado** (PoC parser) | 439.28 MB/s | 3.931 ms | 2.27x   |
| **Wado** (core:json)  | 285.02 MB/s | 6.059 ms | 3.50x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.62 GB/s | 0.477 ms | 1.00x   |
| **Wado** (core:cbor) |  2.38 GB/s | 0.726 ms | 1.52x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.54 GB/s | 0.680 ms | 1.00x   |
| **Wado** (core:cbor) | 660.26 MB/s | 2.615 ms | 3.85x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 327.71 MB/s | 1.927 ms | 1.00x   |
| JavaScript (node:zlib) | 205.94 MB/s | 3.067 ms | 1.59x   |
| C (zlib 1.3.1, Wasm)   | 126.41 MB/s | 4.996 ms | 2.59x   |
| **Wado** (core:zlib)   | 116.98 MB/s | 5.398 ms | 2.80x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.24 GB/s | 0.195 ms | 1.00x   |
| JavaScript (node:zlib) |   1.79 GB/s | 0.354 ms | 1.82x   |
| C (zlib 1.3.1, Wasm)   | 840.05 MB/s | 0.752 ms | 3.86x   |
| **Wado** (core:zlib)   | 509.36 MB/s | 1.239 ms | 6.35x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 11.97 MB/s |   1.113 ms | 1.00x   |
| **Wado** (Gale)     | 11.93 MB/s |   1.116 ms | 1.00x   |
| Java (ANTLR4)       |  0.10 MB/s | 127.805 ms | 114.83x |

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
| Prism.js                      | 11.82 MB/s |  1.127 ms | 1.00x   |
| **Gale** (Wado)               |  9.42 MB/s |  1.414 ms | 1.25x   |
| Lezer (CodeMirror)            |  4.80 MB/s |  2.776 ms | 2.46x   |
| tree-sitter (Rust native)     |  4.52 MB/s |  2.948 ms | 2.62x   |
| tree-sitter (web-tree-sitter) |  2.70 MB/s |  4.935 ms | 4.38x   |
| Shiki (JS engine)             |  1.06 MB/s | 12.619 ms | 11.20x  |

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
| Java (ANTLR4)   | 622.63 KB/s | 55.234 ms | 1.00x   |
| **Wado** (Gale) | 354.28 KB/s | 97.069 ms | 1.76x   |

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
