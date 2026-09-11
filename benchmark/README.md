# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-10, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| C              | 12.27 M nums/s | 81.468 ms | 1.00x   |
| **Wado**       | 12.01 M nums/s | 83.275 ms | 1.02x   |
| JavaScript     | 11.72 M nums/s | 85.346 ms | 1.05x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.96 M px/s |  98.814 ms | 1.00x   |
| **Wado**       | 7.84 M px/s | 100.269 ms | 1.01x   |
| C              | 7.82 M px/s | 100.517 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 783.87 M nums/s | 2.551 ms | 1.00x   |
| JavaScript     | 565.10 M nums/s | 3.539 ms | 1.39x   |
| **Wado**       | 366.00 M nums/s | 5.464 ms | 2.14x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.23 M conv/s | 36.726 ms | 1.00x   |
| Rust (core::fmt) | 21.00 M conv/s | 47.629 ms | 1.30x   |
| C (printf)       | 11.50 M conv/s | 86.930 ms | 2.37x   |

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
| JavaScript (JSON)    |   1.59 GB/s | 0.398 ms | 1.20x   |
| **Wado** (core:json) | 776.37 MB/s | 0.813 ms | 2.46x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 593.02 MB/s | 1.065 ms | 1.00x   |
| Rust (serde_json)    | 590.58 MB/s | 1.069 ms | 1.00x   |
| **Wado** (core:json) | 281.01 MB/s | 2.247 ms | 2.11x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.41 GB/s | 0.262 ms | 1.00x   |
| **Wado** (core:cbor) |  1.39 GB/s | 0.455 ms | 1.74x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 861.58 MB/s | 0.733 ms | 1.00x   |
| **Wado** (core:cbor) | 437.28 MB/s | 1.444 ms | 1.97x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 959.81 MB/s | 2.345 ms | 1.00x   |
| JavaScript (JSON)    | 585.47 MB/s | 3.845 ms | 1.64x   |
| **Wado** (core:json) | 311.99 MB/s | 7.215 ms | 3.08x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 364.56 MB/s | 6.175 ms | 1.00x   |
| Rust (serde_json)    | 356.36 MB/s | 6.317 ms | 1.02x   |
| **Wado** (core:json) | 239.30 MB/s | 9.406 ms | 1.52x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.56 GB/s | 0.880 ms | 1.00x   |
| **Wado** (core:cbor) | 721.72 MB/s | 3.119 ms | 3.54x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.19 GB/s | 1.892 ms | 1.00x   |
| **Wado** (core:cbor) | 452.99 MB/s | 4.969 ms | 2.63x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.27 GB/s | 0.404 ms | 1.00x   |
| **Wado** (core:json) |  1.84 GB/s | 0.940 ms | 2.33x   |
| JavaScript (JSON)    |  1.48 GB/s | 1.165 ms | 2.88x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.06 GB/s | 1.636 ms | 1.00x   |
| JavaScript (JSON)     | 763.32 MB/s | 2.263 ms | 1.38x   |
| **Wado** (PoC parser) | 458.84 MB/s | 3.764 ms | 2.30x   |
| **Wado** (core:json)  | 367.30 MB/s | 4.702 ms | 2.87x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder — the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.85 GB/s | 0.448 ms | 1.00x   |
| **Wado** (core:cbor) |  2.54 GB/s | 0.679 ms | 1.52x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.72 GB/s | 0.634 ms | 1.00x   |
| **Wado** (core:cbor) | 830.73 MB/s | 2.079 ms | 3.28x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 326.51 MB/s | 1.934 ms | 1.00x   |
| JavaScript (node:zlib) | 205.81 MB/s | 3.068 ms | 1.59x   |
| C (zlib 1.3.1, Wasm)   | 134.04 MB/s | 4.712 ms | 2.44x   |
| **Wado** (core:zlib)   | 116.57 MB/s | 5.417 ms | 2.80x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.24 GB/s | 0.195 ms | 1.00x   |
| JavaScript (node:zlib) |   1.86 GB/s | 0.340 ms | 1.74x   |
| C (zlib 1.3.1, Wasm)   | 839.56 MB/s | 0.752 ms | 3.86x   |
| **Wado** (core:zlib)   | 525.45 MB/s | 1.201 ms | 6.16x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 13.45 MB/s |   0.990 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.32 MB/s |   1.081 ms | 1.09x   |
| Java (ANTLR4)       |  0.10 MB/s | 131.318 ms | 132.65x |

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
| Prism.js                      | 12.15 MB/s |  1.096 ms | 1.00x   |
| **Gale** (Wado)               | 10.53 MB/s |  1.264 ms | 1.15x   |
| Lezer (CodeMirror)            |  4.91 MB/s |  2.716 ms | 2.48x   |
| tree-sitter (Rust native)     |  4.65 MB/s |  2.866 ms | 2.61x   |
| tree-sitter (web-tree-sitter) |  2.78 MB/s |  4.790 ms | 4.37x   |
| Shiki (JS engine)             |  1.08 MB/s | 12.286 ms | 11.21x  |

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
| Java (ANTLR4)   | 760.98 KB/s |  55.021 ms | 1.00x   |
| **Wado** (Gale) | 315.45 KB/s | 132.730 ms | 2.41x   |

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
| `GET /user`                     |      44,832 |                   46,515 |                24,820 |                    17,853 |
| `GET /user/lookup/username/hey` |      43,519 |                   34,787 |                24,926 |                    17,070 |
| `POST /event/abcd1234/comment`  |      43,437 |                   39,243 |                24,782 |                    15,860 |
| `GET /static/index.html`        |      43,295 |                   45,886 |                24,615 |                    17,080 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     394,919 |                  269,124 |               121,932 |                    83,610 |
| `GET /user/lookup/username/hey` |     400,348 |                  221,685 |               117,014 |                    78,097 |
| `POST /event/abcd1234/comment`  |     409,076 |                  231,646 |               116,147 |                    68,374 |
| `GET /static/index.html`        |     405,229 |                  232,922 |               116,161 |                    79,577 |

`wado serve` places third at both shapes, about half again Hono on Node. What
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
