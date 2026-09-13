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
| C              | 12.36 M nums/s | 80.930 ms | 1.00x   |
| JavaScript     | 11.81 M nums/s | 84.703 ms | 1.05x   |
| **Wado**       | 11.66 M nums/s | 85.794 ms | 1.06x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |   ms/iter | vs best |
| -------------- | ----------: | --------: | ------- |
| JavaScript     | 8.18 M px/s | 96.130 ms | 1.00x   |
| **Wado**       | 8.07 M px/s | 97.422 ms | 1.01x   |
| C              | 8.06 M px/s | 97.576 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 808.04 M nums/s | 2.475 ms | 1.00x   |
| JavaScript     | 585.14 M nums/s | 3.418 ms | 1.38x   |
| **Wado**       | 379.02 M nums/s | 5.276 ms | 2.13x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.21 M conv/s | 36.752 ms | 1.00x   |
| Rust (core::fmt) | 21.42 M conv/s | 46.680 ms | 1.27x   |
| C (printf)       | 11.43 M conv/s | 87.493 ms | 2.38x   |

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
| Rust (serde_json)    |   1.90 GB/s | 0.332 ms | 1.00x   |
| JavaScript (JSON)    |   1.56 GB/s | 0.405 ms | 1.22x   |
| **Wado** (core:json) | 809.56 MB/s | 0.780 ms | 2.35x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 603.28 MB/s | 1.047 ms | 1.00x   |
| Rust (serde_json)    | 584.04 MB/s | 1.081 ms | 1.03x   |
| **Wado** (core:json) | 294.30 MB/s | 2.145 ms | 2.05x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.49 GB/s | 0.254 ms | 1.00x   |
| **Wado** (core:cbor) |  1.39 GB/s | 0.453 ms | 1.78x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 923.62 MB/s | 0.684 ms | 1.00x   |
| **Wado** (core:cbor) | 426.80 MB/s | 1.479 ms | 2.16x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 967.89 MB/s | 2.326 ms | 1.00x   |
| JavaScript (JSON)    | 587.62 MB/s | 3.831 ms | 1.65x   |
| **Wado** (core:json) | 344.72 MB/s | 6.530 ms | 2.81x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 382.87 MB/s | 5.879 ms | 1.00x   |
| Rust (serde_json)    | 358.45 MB/s | 6.280 ms | 1.07x   |
| **Wado** (core:json) | 271.80 MB/s | 8.281 ms | 1.41x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.56 GB/s | 0.878 ms | 1.00x   |
| **Wado** (core:cbor) | 708.15 MB/s | 3.178 ms | 3.62x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.19 GB/s | 1.897 ms | 1.00x   |
| **Wado** (core:cbor) | 438.28 MB/s | 5.136 ms | 2.71x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.42 GB/s | 0.391 ms | 1.00x   |
| **Wado** (core:json) |  1.99 GB/s | 0.866 ms | 2.21x   |
| JavaScript (JSON)    |  1.49 GB/s | 1.159 ms | 2.96x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.07 GB/s | 1.611 ms | 1.00x   |
| JavaScript (JSON)     | 783.85 MB/s | 2.203 ms | 1.37x   |
| **Wado** (PoC parser) | 475.18 MB/s | 3.634 ms | 2.26x   |
| **Wado** (core:json)  | 471.09 MB/s | 3.666 ms | 2.28x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It was the mark `core:json` had to reach, and the
two rows have converged.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.85 GB/s | 0.448 ms | 1.00x   |
| **Wado** (core:cbor) |  2.65 GB/s | 0.651 ms | 1.45x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.72 GB/s | 0.635 ms | 1.00x   |
| **Wado** (core:cbor) | 876.41 MB/s | 1.970 ms | 3.10x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 343.19 MB/s | 1.840 ms | 1.00x   |
| JavaScript (node:zlib) | 205.99 MB/s | 3.066 ms | 1.67x   |
| C (zlib 1.3.1, Wasm)   | 141.07 MB/s | 4.477 ms | 2.43x   |
| **Wado** (core:zlib)   | 124.17 MB/s | 5.085 ms | 2.76x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.41 GB/s | 0.185 ms | 1.00x   |
| JavaScript (node:zlib) |   1.96 GB/s | 0.323 ms | 1.75x   |
| C (zlib 1.3.1, Wasm)   | 881.52 MB/s | 0.716 ms | 3.87x   |
| **Wado** (core:zlib)   | 538.86 MB/s | 1.171 ms | 6.33x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 14.10 MB/s |   0.944 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.04 MB/s |   1.106 ms | 1.17x   |
| Java (ANTLR4)       |  0.10 MB/s | 129.716 ms | 137.41x |

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
| Prism.js                      | 11.98 MB/s |  1.112 ms | 1.00x   |
| **Gale** (Wado)               | 10.65 MB/s |  1.250 ms | 1.12x   |
| Lezer (CodeMirror)            |  4.91 MB/s |  2.713 ms | 2.44x   |
| tree-sitter (Rust native)     |  4.82 MB/s |  2.765 ms | 2.49x   |
| tree-sitter (web-tree-sitter) |  2.85 MB/s |  4.673 ms | 4.20x   |
| Shiki (JS engine)             |  1.14 MB/s | 11.690 ms | 10.51x  |

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
| Java (ANTLR4)   | 758.22 KB/s |  55.221 ms | 1.00x   |
| **Wado** (Gale) | 307.26 KB/s | 136.269 ms | 2.47x   |

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
| `GET /user`                     |      44,173 |                   56,319 |                24,705 |                    17,760 |
| `GET /user/lookup/username/hey` |      42,887 |                   42,100 |                24,152 |                    17,483 |
| `POST /event/abcd1234/comment`  |      45,096 |                   42,253 |                23,704 |                    16,235 |
| `GET /static/index.html`        |      43,892 |                   41,788 |                24,171 |                    17,058 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     415,071 |                  286,238 |               129,929 |                    88,563 |
| `GET /user/lookup/username/hey` |     411,176 |                  250,088 |               119,471 |                    86,138 |
| `POST /event/abcd1234/comment`  |     401,233 |                  251,058 |               122,366 |                    74,405 |
| `GET /static/index.html`        |     390,833 |                  250,423 |               123,688 |                    83,766 |

`wado serve` places third at both shapes, ahead of Hono on Node. What separates
it from Axum is the component-model boundary, not the compiled code.
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
