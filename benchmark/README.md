# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-17, wasmtime 47.0.3, gcc 11.4.0, wasi-sdk 33.0,
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
| C              | 11.67 M nums/s | 85.678 ms | 1.00x   |
| **Wado**       | 11.37 M nums/s | 87.949 ms | 1.03x   |
| JavaScript     | 10.98 M nums/s | 91.038 ms | 1.06x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.87 M px/s |  99.894 ms | 1.00x   |
| **Wado**       | 7.84 M px/s | 100.261 ms | 1.00x   |
| C              | 7.77 M px/s | 101.228 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              |   1.07 G nums/s | 1.862 ms | 1.00x   |
| JavaScript     | 553.17 M nums/s | 3.615 ms | 1.94x   |
| **Wado**       | 378.61 M nums/s | 5.282 ms | 2.84x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| Rust (core::fmt) | 20.41 M conv/s |  48.985 ms | 1.00x   |
| **Wado**         | 20.11 M conv/s |  49.737 ms | 1.02x   |
| C (printf)       |  9.28 M conv/s | 107.785 ms | 2.20x   |

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
| Rust (serde_json)    |   2.05 GB/s | 0.307 ms | 1.00x   |
| JavaScript (JSON)    |   1.44 GB/s | 0.438 ms | 1.43x   |
| **Wado** (core:json) | 618.01 MB/s | 1.021 ms | 3.33x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 557.12 MB/s | 1.134 ms | 1.00x   |
| Rust (serde_json)    | 544.87 MB/s | 1.159 ms | 1.02x   |
| **Wado** (core:json) | 158.42 MB/s | 3.986 ms | 3.51x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.48 GB/s | 0.254 ms | 1.00x   |
| **Wado** (core:cbor) | 731.65 MB/s | 0.863 ms | 3.40x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 854.32 MB/s | 0.739 ms | 1.00x   |
| **Wado** (core:cbor) | 208.00 MB/s | 3.036 ms | 4.11x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| Rust (serde_json)    | 915.57 MB/s |  2.459 ms | 1.00x   |
| JavaScript (JSON)    | 556.16 MB/s |  4.048 ms | 1.65x   |
| **Wado** (core:json) | 166.94 MB/s | 13.484 ms | 5.48x   |

JSON deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| JavaScript (JSON)    | 348.82 MB/s |  6.453 ms | 1.00x   |
| Rust (serde_json)    | 332.99 MB/s |  6.760 ms | 1.05x   |
| **Wado** (core:json) | 173.79 MB/s | 12.952 ms | 2.01x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.54 GB/s | 0.887 ms | 1.00x   |
| **Wado** (core:cbor) | 509.39 MB/s | 4.419 ms | 4.98x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.08 GB/s | 2.087 ms | 1.00x   |
| **Wado** (core:cbor) | 317.10 MB/s | 7.098 ms | 3.40x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  3.96 GB/s | 0.437 ms | 1.00x   |
| JavaScript (JSON)    |  1.40 GB/s | 1.231 ms | 2.82x   |
| **Wado** (core:json) |  1.02 GB/s | 1.685 ms | 3.86x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.00 GB/s | 1.726 ms | 1.00x   |
| JavaScript (JSON)     | 735.33 MB/s | 2.349 ms | 1.36x   |
| **Wado** (PoC parser) | 394.42 MB/s | 4.379 ms | 2.54x   |
| **Wado** (core:json)  | 260.99 MB/s | 6.617 ms | 3.83x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It is the mark `core:json` should reach first.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.54 GB/s | 0.489 ms | 1.00x   |
| **Wado** (core:cbor) |  1.28 GB/s | 1.354 ms | 2.77x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.57 GB/s | 0.673 ms | 1.00x   |
| **Wado** (core:cbor) | 614.52 MB/s | 2.810 ms | 4.18x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 324.72 MB/s | 1.945 ms | 1.00x   |
| JavaScript (node:zlib) | 201.83 MB/s | 3.129 ms | 1.61x   |
| C (zlib 1.3.1, Wasm)   | 131.51 MB/s | 4.802 ms | 2.47x   |
| **Wado** (core:zlib)   |  73.66 MB/s | 8.573 ms | 4.41x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.20 GB/s | 0.198 ms | 1.00x   |
| JavaScript (node:zlib) |   1.76 GB/s | 0.359 ms | 1.81x   |
| C (zlib 1.3.1, Wasm)   | 824.44 MB/s | 0.766 ms | 3.87x   |
| **Wado** (core:zlib)   | 380.76 MB/s | 1.658 ms | 8.37x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) | 11.35 MB/s |   1.174 ms | 1.00x   |
| **Wado** (Gale)     |  9.69 MB/s |   1.374 ms | 1.17x   |
| Java (ANTLR4)       |  0.10 MB/s | 138.706 ms | 118.15x |

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

| Implementation                |  Throughput |   ms/iter | vs best |
| ----------------------------- | ----------: | --------: | ------- |
| Prism.js                      |  11.18 MB/s |  1.191 ms | 1.00x   |
| **Gale** (Wado)               |   7.46 MB/s |  1.785 ms | 1.50x   |
| Lezer (CodeMirror)            |   4.52 MB/s |  2.950 ms | 2.48x   |
| tree-sitter (Rust native)     |   4.25 MB/s |  3.135 ms | 2.63x   |
| tree-sitter (web-tree-sitter) |   2.62 MB/s |  5.087 ms | 4.27x   |
| Shiki (JS engine)             | 976.63 KB/s | 13.640 ms | 11.45x  |

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
| Java (ANTLR4)   | 611.67 KB/s |  56.223 ms | 1.00x   |
| **Wado** (Gale) | 222.64 KB/s | 154.467 ms | 2.75x   |

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
