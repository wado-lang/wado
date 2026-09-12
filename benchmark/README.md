# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-12, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| C              | 11.61 M nums/s | 86.110 ms | 1.00x   |
| **Wado**       | 11.28 M nums/s | 88.666 ms | 1.03x   |
| JavaScript     | 11.14 M nums/s | 89.761 ms | 1.04x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.84 M px/s | 100.320 ms | 1.00x   |
| C              | 7.73 M px/s | 101.783 ms | 1.01x   |
| **Wado**       | 7.62 M px/s | 103.203 ms | 1.03x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 758.10 M nums/s | 2.638 ms | 1.00x   |
| JavaScript     | 550.15 M nums/s | 3.635 ms | 1.38x   |
| **Wado**       | 357.64 M nums/s | 5.592 ms | 2.12x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.29 M conv/s | 36.648 ms | 1.00x   |
| Rust (core::fmt) | 20.84 M conv/s | 47.988 ms | 1.31x   |
| C (printf)       | 11.37 M conv/s | 87.928 ms | 2.40x   |

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
| Rust (serde_json)    |   1.85 GB/s | 0.342 ms | 1.00x   |
| JavaScript (JSON)    |   1.56 GB/s | 0.405 ms | 1.18x   |
| **Wado** (core:json) | 787.10 MB/s | 0.802 ms | 2.35x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 574.30 MB/s | 1.100 ms | 1.00x   |
| Rust (serde_json)    | 564.47 MB/s | 1.119 ms | 1.02x   |
| **Wado** (core:json) | 275.14 MB/s | 2.295 ms | 2.09x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.32 GB/s | 0.272 ms | 1.00x   |
| **Wado** (core:cbor) |  1.31 GB/s | 0.482 ms | 1.77x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 839.35 MB/s | 0.752 ms | 1.00x   |
| **Wado** (core:cbor) | 414.53 MB/s | 1.523 ms | 2.03x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 938.16 MB/s | 2.399 ms | 1.00x   |
| JavaScript (JSON)    | 570.13 MB/s | 3.948 ms | 1.65x   |
| **Wado** (core:json) | 299.19 MB/s | 7.523 ms | 3.14x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 375.72 MB/s | 5.991 ms | 1.00x   |
| Rust (serde_json)    | 346.01 MB/s | 6.506 ms | 1.09x   |
| **Wado** (core:json) | 230.28 MB/s | 9.775 ms | 1.63x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.49 GB/s | 0.902 ms | 1.00x   |
| **Wado** (core:cbor) | 696.83 MB/s | 3.230 ms | 3.58x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.17 GB/s | 1.918 ms | 1.00x   |
| **Wado** (core:cbor) | 443.27 MB/s | 5.078 ms | 2.65x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.23 GB/s | 0.408 ms | 1.00x   |
| **Wado** (core:json) |  1.92 GB/s | 0.900 ms | 2.21x   |
| JavaScript (JSON)    |  1.44 GB/s | 1.199 ms | 2.94x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.03 GB/s | 1.670 ms | 1.00x   |
| JavaScript (JSON)     | 764.77 MB/s | 2.258 ms | 1.35x   |
| **Wado** (PoC parser) | 442.83 MB/s | 3.900 ms | 2.34x   |
| **Wado** (core:json)  | 440.47 MB/s | 3.921 ms | 2.35x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It was the mark `core:json` had to reach, and the
two now measure within half a percent of each other.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.69 GB/s | 0.468 ms | 1.00x   |
| **Wado** (core:cbor) |  2.48 GB/s | 0.697 ms | 1.49x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.65 GB/s | 0.653 ms | 1.00x   |
| **Wado** (core:cbor) | 808.30 MB/s | 2.136 ms | 3.27x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 316.06 MB/s | 1.998 ms | 1.00x   |
| JavaScript (node:zlib) | 205.30 MB/s | 3.076 ms | 1.54x   |
| C (zlib 1.3.1, Wasm)   | 130.41 MB/s | 4.842 ms | 2.42x   |
| **Wado** (core:zlib)   | 116.46 MB/s | 5.422 ms | 2.71x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.15 GB/s | 0.200 ms | 1.00x   |
| JavaScript (node:zlib) |   1.87 GB/s | 0.338 ms | 1.69x   |
| C (zlib 1.3.1, Wasm)   | 813.98 MB/s | 0.776 ms | 3.88x   |
| **Wado** (core:zlib)   | 501.42 MB/s | 1.259 ms | 6.29x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 13.61 MB/s |   0.978 ms | 1.00x   |
| Rust (sqlparser-rs) | 11.78 MB/s |   1.131 ms | 1.16x   |
| Java (ANTLR4)       |  0.10 MB/s | 133.501 ms | 136.50x |

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
| Prism.js                      | 11.89 MB/s |  1.120 ms | 1.00x   |
| **Gale** (Wado)               |  9.87 MB/s |  1.349 ms | 1.20x   |
| Lezer (CodeMirror)            |  4.87 MB/s |  2.737 ms | 2.44x   |
| tree-sitter (Rust native)     |  4.71 MB/s |  2.826 ms | 2.52x   |
| tree-sitter (web-tree-sitter) |  2.75 MB/s |  4.839 ms | 4.32x   |
| Shiki (JS engine)             |  1.05 MB/s | 12.694 ms | 11.33x  |

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
| Java (ANTLR4)   | 756.67 KB/s |  55.334 ms | 1.00x   |
| **Wado** (Gale) | 296.29 KB/s | 141.316 ms | 2.55x   |

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
| `GET /user`                     |      48,071 |                   46,731 |                23,600 |                    16,995 |
| `GET /user/lookup/username/hey` |      41,695 |                   44,565 |                22,854 |                    16,473 |
| `POST /event/abcd1234/comment`  |      43,208 |                   37,778 |                22,687 |                    15,304 |
| `GET /static/index.html`        |      42,383 |                   48,666 |                23,395 |                    16,478 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     383,226 |                  266,893 |               118,474 |                    82,476 |
| `GET /user/lookup/username/hey` |     376,264 |                  230,963 |               114,417 |                    80,297 |
| `POST /event/abcd1234/comment`  |     376,620 |                  228,302 |               112,750 |                    68,555 |
| `GET /static/index.html`        |     375,041 |                  226,962 |               112,889 |                    77,409 |

`wado serve` places third at both shapes, about 40% ahead of Hono on Node. What
separates it from Axum is the component-model boundary, not the compiled code.
Lifting the arguments of a `wasi:http` call and lowering its result costs
several times the guest code that call wraps.

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
