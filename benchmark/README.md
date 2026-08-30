# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-30, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| C              | 12.26 M nums/s | 81.555 ms | 1.00x   |
| **Wado**       | 11.99 M nums/s | 83.426 ms | 1.02x   |
| JavaScript     | 11.71 M nums/s | 85.370 ms | 1.05x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |   ms/iter | vs best |
| -------------- | ----------: | --------: | ------- |
| C              | 8.15 M px/s | 96.465 ms | 1.00x   |
| JavaScript     | 8.12 M px/s | 96.830 ms | 1.00x   |
| **Wado**       | 8.07 M px/s | 97.394 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 802.50 M nums/s | 2.492 ms | 1.00x   |
| JavaScript     | 571.95 M nums/s | 3.497 ms | 1.40x   |
| **Wado**       | 368.48 M nums/s | 5.427 ms | 2.18x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 21.88 M conv/s | 45.700 ms | 1.00x   |
| Rust (core::fmt) | 21.41 M conv/s | 46.716 ms | 1.02x   |
| C (printf)       | 11.46 M conv/s | 87.239 ms | 1.91x   |

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
| Rust (serde_json)    |   1.89 GB/s | 0.335 ms | 1.00x   |
| JavaScript (JSON)    |   1.57 GB/s | 0.403 ms | 1.20x   |
| **Wado** (core:json) | 771.02 MB/s | 0.819 ms | 2.44x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 613.45 MB/s | 1.029 ms | 1.00x   |
| Rust (serde_json)    | 579.59 MB/s | 1.090 ms | 1.06x   |
| **Wado** (core:json) | 164.66 MB/s | 3.835 ms | 3.73x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.39 GB/s | 0.264 ms | 1.00x   |
| **Wado** (core:cbor) |  1.40 GB/s | 0.452 ms | 1.71x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 866.85 MB/s | 0.729 ms | 1.00x   |
| **Wado** (core:cbor) | 216.13 MB/s | 2.921 ms | 4.01x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 962.83 MB/s | 2.338 ms | 1.00x   |
| JavaScript (JSON)    | 588.53 MB/s | 3.825 ms | 1.64x   |
| **Wado** (core:json) | 280.04 MB/s | 8.038 ms | 3.44x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 366.50 MB/s |  6.142 ms | 1.00x   |
| Rust (serde_json)    | 356.61 MB/s |  6.312 ms | 1.03x   |
| **Wado** (core:json) | 186.15 MB/s | 12.092 ms | 1.97x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.59 GB/s | 0.868 ms | 1.00x   |
| **Wado** (core:cbor) | 721.80 MB/s | 3.118 ms | 3.59x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.20 GB/s | 1.881 ms | 1.00x   |
| **Wado** (core:cbor) | 350.91 MB/s | 6.414 ms | 3.41x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.32 GB/s | 0.400 ms | 1.00x   |
| **Wado** (core:json) |  1.67 GB/s | 1.036 ms | 2.59x   |
| JavaScript (JSON)    |  1.47 GB/s | 1.176 ms | 2.94x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.05 GB/s | 1.644 ms | 1.00x   |
| JavaScript (JSON)     | 769.28 MB/s | 2.245 ms | 1.37x   |
| **Wado** (PoC parser) | 442.02 MB/s | 3.907 ms | 2.38x   |
| **Wado** (core:json)  | 294.71 MB/s | 5.860 ms | 3.56x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.69 GB/s | 0.469 ms | 1.00x   |
| **Wado** (core:cbor) |  2.50 GB/s | 0.692 ms | 1.48x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.65 GB/s | 0.653 ms | 1.00x   |
| **Wado** (core:cbor) | 659.84 MB/s | 2.617 ms | 4.01x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 327.38 MB/s | 1.929 ms | 1.00x   |
| JavaScript (node:zlib) | 205.08 MB/s | 3.079 ms | 1.60x   |
| C (zlib 1.3.1, Wasm)   | 134.26 MB/s | 4.704 ms | 2.44x   |
| **Wado** (core:zlib)   |  78.92 MB/s | 8.002 ms | 4.15x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.25 GB/s | 0.194 ms | 1.00x   |
| JavaScript (node:zlib) |   1.84 GB/s | 0.344 ms | 1.77x   |
| C (zlib 1.3.1, Wasm)   | 846.56 MB/s | 0.746 ms | 3.84x   |
| **Wado** (core:zlib)   | 395.14 MB/s | 1.598 ms | 8.24x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 11.95 MB/s |   1.114 ms | 1.00x   |
| **Wado** (Gale)     | 11.33 MB/s |   1.175 ms | 1.05x   |
| Java (ANTLR4)       |  0.11 MB/s | 126.450 ms | 113.51x |

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
| Prism.js                      | 12.05 MB/s |  1.105 ms | 1.00x   |
| **Gale** (Wado)               |  8.76 MB/s |  1.521 ms | 1.38x   |
| Lezer (CodeMirror)            |  4.89 MB/s |  2.721 ms | 2.46x   |
| tree-sitter (Rust native)     |  4.68 MB/s |  2.847 ms | 2.58x   |
| tree-sitter (web-tree-sitter) |  2.81 MB/s |  4.733 ms | 4.29x   |
| Shiki (JS engine)             |  1.10 MB/s | 12.145 ms | 10.95x  |

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
| Java (ANTLR4)   | 673.75 KB/s | 51.043 ms | 1.00x   |
| **Wado** (Gale) | 367.80 KB/s | 93.502 ms | 1.83x   |

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
| `GET /user`                     |      41,385 |                   49,308 |                17,218 |                    17,016 |
| `GET /user/lookup/username/hey` |      39,774 |                   52,998 |                16,533 |                    16,891 |
| `POST /event/abcd1234/comment`  |      41,161 |                   35,702 |                16,612 |                    15,250 |
| `GET /static/index.html`        |      41,357 |                   36,222 |                16,795 |                    16,164 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     368,607 |                  258,493 |                94,949 |                    77,850 |
| `GET /user/lookup/username/hey` |     367,154 |                  217,003 |                87,058 |                    78,130 |
| `POST /event/abcd1234/comment`  |     364,166 |                  231,570 |                87,044 |                    65,547 |
| `GET /static/index.html`        |     366,822 |                  223,427 |                87,477 |                    73,312 |

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
