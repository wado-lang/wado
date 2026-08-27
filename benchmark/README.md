# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-27, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| C              | 11.86 M nums/s | 84.308 ms | 1.00x   |
| **Wado**       | 11.60 M nums/s | 86.190 ms | 1.02x   |
| JavaScript     | 11.32 M nums/s | 88.335 ms | 1.05x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |   ms/iter | vs best |
| -------------- | ----------: | --------: | ------- |
| JavaScript     | 8.04 M px/s | 97.780 ms | 1.00x   |
| **Wado**       | 7.96 M px/s | 98.745 ms | 1.01x   |
| C              | 7.91 M px/s | 99.370 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 744.97 M nums/s | 2.685 ms | 1.00x   |
| JavaScript     | 536.21 M nums/s | 3.730 ms | 1.39x   |
| **Wado**       | 390.54 M nums/s | 5.121 ms | 1.91x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| Rust (core::fmt) | 21.51 M conv/s | 46.497 ms | 1.00x   |
| **Wado**         | 20.33 M conv/s | 49.185 ms | 1.06x   |
| C (printf)       | 11.41 M conv/s | 87.646 ms | 1.89x   |

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
| Rust (serde_json)    |   1.91 GB/s | 0.331 ms | 1.00x   |
| JavaScript (JSON)    |   1.50 GB/s | 0.421 ms | 1.27x   |
| **Wado** (core:json) | 656.42 MB/s | 0.962 ms | 2.91x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 578.34 MB/s | 1.092 ms | 1.00x   |
| Rust (serde_json)    | 554.13 MB/s | 1.140 ms | 1.04x   |
| **Wado** (core:json) | 158.85 MB/s | 3.975 ms | 3.64x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.30 GB/s | 0.274 ms | 1.00x   |
| **Wado** (core:cbor) |  1.19 GB/s | 0.529 ms | 1.93x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 877.21 MB/s | 0.720 ms | 1.00x   |
| **Wado** (core:cbor) | 211.66 MB/s | 2.983 ms | 4.14x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| Rust (serde_json)    | 961.33 MB/s |  2.342 ms | 1.00x   |
| JavaScript (JSON)    | 581.70 MB/s |  3.870 ms | 1.65x   |
| **Wado** (core:json) | 195.75 MB/s | 11.499 ms | 4.91x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 359.09 MB/s |  6.269 ms | 1.00x   |
| Rust (serde_json)    | 354.26 MB/s |  6.354 ms | 1.01x   |
| **Wado** (core:json) | 176.09 MB/s | 12.783 ms | 2.04x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.55 GB/s | 0.882 ms | 1.00x   |
| **Wado** (core:cbor) | 577.25 MB/s | 3.899 ms | 4.42x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.18 GB/s | 1.903 ms | 1.00x   |
| **Wado** (core:cbor) | 324.55 MB/s | 6.935 ms | 3.64x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.26 GB/s | 0.406 ms | 1.00x   |
| JavaScript (JSON)    |  1.46 GB/s | 1.179 ms | 2.90x   |
| **Wado** (core:json) |  1.09 GB/s | 1.588 ms | 3.91x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.05 GB/s | 1.651 ms | 1.00x   |
| JavaScript (JSON)     | 759.35 MB/s | 2.275 ms | 1.38x   |
| **Wado** (PoC parser) | 414.51 MB/s | 4.166 ms | 2.52x   |
| **Wado** (core:json)  | 275.52 MB/s | 6.268 ms | 3.80x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.76 GB/s | 0.459 ms | 1.00x   |
| **Wado** (core:cbor) |  1.93 GB/s | 0.895 ms | 1.95x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.62 GB/s | 0.659 ms | 1.00x   |
| **Wado** (core:cbor) | 649.48 MB/s | 2.659 ms | 4.03x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 324.78 MB/s | 1.944 ms | 1.00x   |
| JavaScript (node:zlib) | 204.56 MB/s | 3.087 ms | 1.59x   |
| C (zlib 1.3.1, Wasm)   | 129.42 MB/s | 4.880 ms | 2.51x   |
| **Wado** (core:zlib)   |  74.14 MB/s | 8.517 ms | 4.38x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.24 GB/s | 0.195 ms | 1.00x   |
| JavaScript (node:zlib) |   1.83 GB/s | 0.345 ms | 1.77x   |
| C (zlib 1.3.1, Wasm)   | 836.79 MB/s | 0.755 ms | 3.87x   |
| **Wado** (core:zlib)   | 385.51 MB/s | 1.638 ms | 8.40x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 12.11 MB/s |   1.100 ms | 1.00x   |
| **Wado** (Gale)     | 10.76 MB/s |   1.238 ms | 1.13x   |
| Java (ANTLR4)       |  0.10 MB/s | 130.224 ms | 118.39x |

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
| Prism.js                      | 11.74 MB/s |  1.134 ms | 1.00x   |
| **Gale** (Wado)               |  8.31 MB/s |  1.602 ms | 1.41x   |
| Lezer (CodeMirror)            |  4.80 MB/s |  2.775 ms | 2.45x   |
| tree-sitter (Rust native)     |  4.66 MB/s |  2.859 ms | 2.52x   |
| tree-sitter (web-tree-sitter) |  2.81 MB/s |  4.747 ms | 4.19x   |
| Shiki (JS engine)             |  1.06 MB/s | 12.527 ms | 11.05x  |

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
| Java (ANTLR4)   | 647.97 KB/s | 53.074 ms | 1.00x   |
| **Wado** (Gale) | 354.70 KB/s | 96.955 ms | 1.83x   |

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
| `GET /user`                     |      44,628 |                   48,420 |                17,463 |                    17,801 |
| `GET /user/lookup/username/hey` |      42,930 |                   33,800 |                16,942 |                    16,947 |
| `POST /event/abcd1234/comment`  |      44,255 |                   38,624 |                17,535 |                    15,553 |
| `GET /static/index.html`        |      44,649 |                   43,576 |                17,355 |                    17,538 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     395,801 |                  265,558 |                87,500 |                    84,351 |
| `GET /user/lookup/username/hey` |     386,658 |                  237,356 |                85,889 |                    76,265 |
| `POST /event/abcd1234/comment`  |     384,943 |                  233,816 |                87,180 |                    68,863 |
| `GET /static/index.html`        |     388,848 |                  238,215 |                85,958 |                    78,963 |

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
