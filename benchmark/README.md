# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-29, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| C              | 11.25 M nums/s | 88.872 ms | 1.00x   |
| **Wado**       | 10.86 M nums/s | 92.123 ms | 1.04x   |
| JavaScript     | 10.33 M nums/s | 96.759 ms | 1.09x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.85 M px/s | 100.239 ms | 1.00x   |
| **Wado**       | 7.40 M px/s | 106.310 ms | 1.06x   |
| C              | 7.36 M px/s | 106.840 ms | 1.07x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 707.82 M nums/s | 2.826 ms | 1.00x   |
| JavaScript     | 504.64 M nums/s | 3.963 ms | 1.40x   |
| **Wado**       | 350.11 M nums/s | 5.712 ms | 2.02x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| Rust (core::fmt) | 19.81 M conv/s |  50.482 ms | 1.00x   |
| **Wado**         | 18.58 M conv/s |  53.831 ms | 1.07x   |
| C (printf)       |  9.97 M conv/s | 100.309 ms | 1.99x   |

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
| JavaScript (JSON)    |   1.54 GB/s | 0.410 ms | 1.19x   |
| **Wado** (core:json) | 756.08 MB/s | 0.835 ms | 2.43x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 568.12 MB/s | 1.112 ms | 1.00x   |
| Rust (serde_json)    | 563.52 MB/s | 1.121 ms | 1.01x   |
| **Wado** (core:json) | 157.85 MB/s | 4.000 ms | 3.60x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.34 GB/s | 0.270 ms | 1.00x   |
| **Wado** (core:cbor) |  1.19 GB/s | 0.529 ms | 1.97x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 852.32 MB/s | 0.741 ms | 1.00x   |
| **Wado** (core:cbor) | 207.44 MB/s | 3.044 ms | 4.11x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 931.40 MB/s | 2.417 ms | 1.00x   |
| JavaScript (JSON)    | 564.04 MB/s | 3.991 ms | 1.65x   |
| **Wado** (core:json) | 260.71 MB/s | 8.634 ms | 3.57x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 357.88 MB/s |  6.290 ms | 1.00x   |
| Rust (serde_json)    | 346.75 MB/s |  6.492 ms | 1.03x   |
| **Wado** (core:json) | 174.19 MB/s | 12.922 ms | 2.05x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.52 GB/s | 0.895 ms | 1.00x   |
| **Wado** (core:cbor) | 636.23 MB/s | 3.538 ms | 3.96x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.16 GB/s | 1.942 ms | 1.00x   |
| **Wado** (core:cbor) | 329.34 MB/s | 6.834 ms | 3.52x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.18 GB/s | 0.413 ms | 1.00x   |
| **Wado** (core:json) |  1.64 GB/s | 1.052 ms | 2.55x   |
| JavaScript (JSON)    |  1.44 GB/s | 1.202 ms | 2.90x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.02 GB/s | 1.695 ms | 1.00x   |
| JavaScript (JSON)     | 739.40 MB/s | 2.336 ms | 1.38x   |
| **Wado** (PoC parser) | 424.70 MB/s | 4.066 ms | 2.40x   |
| **Wado** (core:json)  | 279.72 MB/s | 6.174 ms | 3.65x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.67 GB/s | 0.470 ms | 1.00x   |
| **Wado** (core:cbor) |  2.12 GB/s | 0.813 ms | 1.73x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.60 GB/s | 0.664 ms | 1.00x   |
| **Wado** (core:cbor) | 633.08 MB/s | 2.728 ms | 4.11x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 292.99 MB/s | 2.155 ms | 1.00x   |
| JavaScript (node:zlib) | 190.49 MB/s | 3.315 ms | 1.54x   |
| C (zlib 1.3.1, Wasm)   | 117.69 MB/s | 5.366 ms | 2.49x   |
| **Wado** (core:zlib)   |  68.80 MB/s | 9.179 ms | 4.26x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   2.92 GB/s | 0.217 ms | 1.00x   |
| JavaScript (node:zlib) |   1.71 GB/s | 0.369 ms | 1.71x   |
| C (zlib 1.3.1, Wasm)   | 786.95 MB/s | 0.802 ms | 3.71x   |
| **Wado** (core:zlib)   | 337.15 MB/s | 1.873 ms | 8.66x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 10.81 MB/s |   1.233 ms | 1.00x   |
| **Wado** (Gale)     |  9.19 MB/s |   1.449 ms | 1.18x   |
| Java (ANTLR4)       |  0.09 MB/s | 145.098 ms | 120.11x |

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
| Prism.js                      | 11.14 MB/s |  1.195 ms | 1.00x   |
| **Gale** (Wado)               |  7.74 MB/s |  1.720 ms | 1.44x   |
| Lezer (CodeMirror)            |  4.38 MB/s |  3.044 ms | 2.54x   |
| tree-sitter (Rust native)     |  4.23 MB/s |  3.150 ms | 2.63x   |
| tree-sitter (web-tree-sitter) |  2.42 MB/s |  5.498 ms | 4.60x   |
| Shiki (JS engine)             |  1.00 MB/s | 13.305 ms | 11.14x  |

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
| Java (ANTLR4)   | 638.15 KB/s | 53.890 ms | 1.00x   |
| **Wado** (Gale) | 355.36 KB/s | 96.774 ms | 1.80x   |

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
