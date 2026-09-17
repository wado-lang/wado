# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-18, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
rustc 1.98.0, Node.js v26.7.0, Bun 1.3.14, Linux x86_64.

Throughput is work per second (higher is better), with per-iteration time in
parentheses. Native rows are optimized builds (C `gcc -O3`, Rust release, Wado
`-O2`); JavaScript runs on Node.js. `vs best` is the fastest row's throughput
over this row's (1.00x = fastest). Absolute throughput is machine-dependent, so
compare by `vs best`. Each figure is the best of three runs.

Benchmarks are grouped into four sections: pure computation, serialization &
compression, parsing, and application server.

## Pure Computation

### MicroGPT

A character-level GPT on a scalar autograd object graph: one layer, 16 embedding
dimensions, 4 heads, 4,096 parameters. A port of
[Andrej Karpathy's microgpt](https://gist.github.com/karpathy/8627fe009c40f57531cb18360106ce95);
`example/microgpt.wado` is the full version.

The other rows in this section loop over flat arrays. This one walks a graph of
small heap objects: a training step builds 31k to 89k nodes, traverses them
depth-first, and accumulates gradients back through them.

Train — 32 steps, one per document, of forward, backward and Adam:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| Rust           | 2.77 k tokens/s |  82.070 ms | 1.00x   |
| JavaScript     | 1.25 k tokens/s | 181.133 ms | 2.21x   |
| **Wado**       | 1.02 k tokens/s | 223.606 ms | 2.72x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| Rust           | 4.35 k tokens/s |  88.190 ms | 1.00x   |
| **Wado**       | 2.14 k tokens/s | 179.081 ms | 2.03x   |
| JavaScript     | 2.04 k tokens/s | 188.056 ms | 2.13x   |

Wado passes JavaScript on inference and stays behind it on training. Training
spends most of its time in the backward pass, which sorts the whole graph
topologically and then walks every edge again. That is pointer chasing over GC
objects, where Rust's flat `Vec` of indices is at its strongest. Inference
never builds that traversal.

Each sample also runs the full 16-position attention window, deeper than any
training step reaches on this corpus: the longest name gives 11 positions and
the mean is 7, and attention cost grows with the square of the position count.

Rust makes a node a `usize` index into a `Vec<Value>`, because a `&mut` into a
growing `Vec` is what the borrow checker forbids. Wado's `Graph::value` returns
that `&mut Value` and a node holds those handles as its children, which is the
shape the Python original has. The gap between the two rows is what that costs.

All three arms print the same final loss and generate the same sample, so they
are the same computation.

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |   ms/iter | vs best |
| -------------- | ----------: | --------: | ------- |
| JavaScript     | 8.02 M px/s | 98.082 ms | 1.00x   |
| C              | 8.01 M px/s | 98.125 ms | 1.00x   |
| **Wado**       | 7.88 M px/s | 99.749 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 788.93 M nums/s | 2.535 ms | 1.00x   |
| JavaScript     | 587.93 M nums/s | 3.402 ms | 1.34x   |
| **Wado**       | 293.59 M nums/s | 6.812 ms | 2.69x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.63 M conv/s | 36.192 ms | 1.00x   |
| Rust (core::fmt) | 21.67 M conv/s | 46.152 ms | 1.28x   |
| C (printf)       | 12.05 M conv/s | 83.014 ms | 2.29x   |

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
| Rust (serde_json)    |   1.91 GB/s | 0.330 ms | 1.00x   |
| JavaScript (JSON)    |   1.60 GB/s | 0.396 ms | 1.20x   |
| **Wado** (core:json) | 783.30 MB/s | 0.806 ms | 2.44x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 595.34 MB/s | 1.061 ms | 1.00x   |
| Rust (serde_json)    | 590.13 MB/s | 1.070 ms | 1.01x   |
| **Wado** (core:json) | 285.06 MB/s | 2.215 ms | 2.09x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.43 GB/s | 0.260 ms | 1.00x   |
| **Wado** (core:cbor) |  1.33 GB/s | 0.475 ms | 1.83x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 885.70 MB/s | 0.713 ms | 1.00x   |
| **Wado** (core:cbor) | 438.12 MB/s | 1.441 ms | 2.02x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 973.50 MB/s | 2.312 ms | 1.00x   |
| JavaScript (JSON)    | 592.96 MB/s | 3.796 ms | 1.64x   |
| **Wado** (core:json) | 286.49 MB/s | 7.857 ms | 3.40x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 378.00 MB/s | 5.955 ms | 1.00x   |
| Rust (serde_json)    | 361.67 MB/s | 6.224 ms | 1.05x   |
| **Wado** (core:json) | 263.31 MB/s | 8.549 ms | 1.44x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.59 GB/s | 0.869 ms | 1.00x   |
| **Wado** (core:cbor) | 607.80 MB/s | 3.703 ms | 4.26x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.18 GB/s | 1.910 ms | 1.00x   |
| **Wado** (core:cbor) | 433.81 MB/s | 5.189 ms | 2.72x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.24 GB/s | 0.407 ms | 1.00x   |
| **Wado** (core:json) |  1.75 GB/s | 0.987 ms | 2.43x   |
| JavaScript (JSON)    |  1.47 GB/s | 1.179 ms | 2.90x   |

JSON deserialize:

| Implementation        |  Throughput |     ms/iter | vs best |
| --------------------- | ----------: | ----------: | ------- |
| Rust (serde_json)     |   1.05 GB/s |    1.645 ms | 1.00x   |
| JavaScript (JSON)     | 774.47 MB/s |    2.230 ms | 1.36x   |
| **Wado** (core:json)  | 384.51 MB/s |    4.491 ms | 2.73x   |
| **Wado** (PoC parser) |   1.21 MB/s | 1426.843 ms | 867.38x |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It was the mark `core:json` had to reach, and
`core:json` has long since passed it.

That row is also broken. It last measured 475 MB/s and now takes about 1.4
seconds an iteration, with 98.7% of the samples self in `P::parse_area`. The
`origin/main` compiler produces the same figure, so the cause predates any
current branch and is unfound.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.76 GB/s | 0.459 ms | 1.00x   |
| **Wado** (core:cbor) |  2.22 GB/s | 0.777 ms | 1.69x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.66 GB/s | 0.650 ms | 1.00x   |
| **Wado** (core:cbor) | 691.05 MB/s | 2.499 ms | 3.84x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 331.54 MB/s | 1.905 ms | 1.00x   |
| JavaScript (node:zlib) | 209.20 MB/s | 3.019 ms | 1.58x   |
| C (zlib 1.3.1, Wasm)   | 134.46 MB/s | 4.697 ms | 2.47x   |
| **Wado** (core:zlib)   | 117.16 MB/s | 5.390 ms | 2.83x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.31 GB/s | 0.191 ms | 1.00x   |
| JavaScript (node:zlib) |   1.93 GB/s | 0.328 ms | 1.72x   |
| C (zlib 1.3.1, Wasm)   | 839.67 MB/s | 0.752 ms | 3.94x   |
| **Wado** (core:zlib)   | 508.94 MB/s | 1.240 ms | 6.49x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 13.56 MB/s |   0.982 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.09 MB/s |   1.102 ms | 1.12x   |
| Java (ANTLR4)       |  0.10 MB/s | 131.079 ms | 133.48x |

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
| **Gale** (Wado)               |  9.86 MB/s |  1.351 ms | 1.21x   |
| Lezer (CodeMirror)            |  5.01 MB/s |  2.660 ms | 2.39x   |
| tree-sitter (Rust native)     |  4.68 MB/s |  2.848 ms | 2.56x   |
| tree-sitter (web-tree-sitter) |  2.82 MB/s |  4.726 ms | 4.25x   |
| Shiki (JS engine)             |  1.12 MB/s | 11.939 ms | 10.74x  |

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
| Java (ANTLR4)   | 814.25 KB/s |  51.422 ms | 1.00x   |
| **Wado** (Gale) | 303.22 KB/s | 138.084 ms | 2.69x   |

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
| `GET /user`                     |      45,138 |                   55,396 |                24,821 |                    17,116 |
| `GET /user/lookup/username/hey` |      44,153 |                   37,275 |                23,342 |                    16,907 |
| `POST /event/abcd1234/comment`  |      42,328 |                   47,163 |                23,226 |                    15,755 |
| `GET /static/index.html`        |      43,589 |                   42,356 |                23,565 |                    16,906 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     398,341 |                  279,015 |               121,502 |                    87,783 |
| `GET /user/lookup/username/hey` |     392,047 |                  240,014 |               115,989 |                    81,692 |
| `POST /event/abcd1234/comment`  |     396,029 |                  238,758 |               115,339 |                    73,001 |
| `GET /static/index.html`        |     393,595 |                  246,521 |               118,758 |                    80,776 |

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
mise run benchmark-microgpt         # autograd object graph
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
