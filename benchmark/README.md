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
| Rust           | 2.70 k tokens/s |  83.935 ms | 1.00x   |
| **Wado**       | 1.43 k tokens/s | 159.154 ms | 1.90x   |
| JavaScript     | 1.27 k tokens/s | 179.425 ms | 2.14x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| Rust           | 4.18 k tokens/s |  91.852 ms | 1.00x   |
| JavaScript     | 3.71 k tokens/s | 103.504 ms | 1.13x   |
| **Wado**       | 3.68 k tokens/s | 104.362 ms | 1.14x   |

Wado beats JavaScript on training and ties it on inference. It trails Rust on
both, and the gap is wider on training. Training spends most of its time in the
backward pass, which sorts the whole graph topologically and then walks every
edge again. That is pointer chasing over GC objects, where Rust's flat `Vec` of
indices is at its strongest. Inference never builds that traversal.

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

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.87 M px/s |  99.937 ms | 1.00x   |
| **Wado**       | 7.74 M px/s | 101.580 ms | 1.02x   |
| C              | 7.73 M px/s | 101.698 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 773.70 M nums/s | 2.585 ms | 1.00x   |
| JavaScript     | 546.64 M nums/s | 3.659 ms | 1.42x   |
| **Wado**       | 335.30 M nums/s | 5.964 ms | 2.31x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 28.33 M conv/s | 35.294 ms | 1.00x   |
| Rust (core::fmt) | 21.13 M conv/s | 47.324 ms | 1.34x   |
| C (printf)       | 11.38 M conv/s | 87.844 ms | 2.49x   |

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
| Rust (serde_json)    |   1.96 GB/s | 0.322 ms | 1.00x   |
| JavaScript (JSON)    |   1.65 GB/s | 0.382 ms | 1.19x   |
| **Wado** (core:json) | 758.59 MB/s | 0.832 ms | 2.58x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 609.11 MB/s | 1.037 ms | 1.00x   |
| Rust (serde_json)    | 572.37 MB/s | 1.103 ms | 1.06x   |
| **Wado** (core:json) | 277.67 MB/s | 2.274 ms | 2.19x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.36 GB/s | 0.268 ms | 1.00x   |
| **Wado** (core:cbor) |  1.29 GB/s | 0.487 ms | 1.82x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 854.95 MB/s | 0.739 ms | 1.00x   |
| **Wado** (core:cbor) | 420.96 MB/s | 1.500 ms | 2.03x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 949.58 MB/s | 2.371 ms | 1.00x   |
| JavaScript (JSON)    | 583.10 MB/s | 3.860 ms | 1.63x   |
| **Wado** (core:json) | 277.08 MB/s | 8.124 ms | 3.43x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 359.75 MB/s | 6.257 ms | 1.00x   |
| Rust (serde_json)    | 350.66 MB/s | 6.419 ms | 1.03x   |
| **Wado** (core:json) | 266.83 MB/s | 8.436 ms | 1.35x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.53 GB/s | 0.889 ms | 1.00x   |
| **Wado** (core:cbor) | 620.71 MB/s | 3.626 ms | 4.08x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.18 GB/s | 1.910 ms | 1.00x   |
| **Wado** (core:cbor) | 443.92 MB/s | 5.070 ms | 2.65x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.42 GB/s | 0.391 ms | 1.00x   |
| **Wado** (core:json) |  1.71 GB/s | 1.012 ms | 2.59x   |
| JavaScript (JSON)    |  1.50 GB/s | 1.155 ms | 2.95x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.07 GB/s | 1.618 ms | 1.00x   |
| JavaScript (JSON)     | 756.56 MB/s | 2.283 ms | 1.41x   |
| **Wado** (PoC parser) | 430.23 MB/s | 4.014 ms | 2.48x   |
| **Wado** (core:json)  | 379.13 MB/s | 4.555 ms | 2.82x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It was the mark `core:json` had to reach, and
the row above shows how close `core:json` comes while decoding any schema.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.68 GB/s | 0.470 ms | 1.00x   |
| **Wado** (core:cbor) |  2.19 GB/s | 0.788 ms | 1.68x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.60 GB/s | 0.665 ms | 1.00x   |
| **Wado** (core:cbor) | 692.09 MB/s | 2.495 ms | 3.75x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 322.06 MB/s | 1.961 ms | 1.00x   |
| JavaScript (node:zlib) | 211.57 MB/s | 2.985 ms | 1.52x   |
| C (zlib 1.3.1, Wasm)   | 132.37 MB/s | 4.771 ms | 2.43x   |
| **Wado** (core:zlib)   | 115.10 MB/s | 5.486 ms | 2.80x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.20 GB/s | 0.197 ms | 1.00x   |
| JavaScript (node:zlib) |   1.79 GB/s | 0.353 ms | 1.79x   |
| C (zlib 1.3.1, Wasm)   | 872.95 MB/s | 0.723 ms | 3.67x   |
| **Wado** (core:zlib)   | 499.72 MB/s | 1.263 ms | 6.41x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 13.38 MB/s |   0.995 ms | 1.00x   |
| Rust (sqlparser-rs) | 11.84 MB/s |   1.125 ms | 1.13x   |
| Java (ANTLR4)       |  0.11 MB/s | 124.240 ms | 124.86x |

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
| Prism.js                      | 11.92 MB/s |  1.118 ms | 1.00x   |
| **Gale** (Wado)               |  9.62 MB/s |  1.384 ms | 1.24x   |
| Lezer (CodeMirror)            |  4.82 MB/s |  2.763 ms | 2.47x   |
| tree-sitter (Rust native)     |  4.62 MB/s |  2.882 ms | 2.58x   |
| tree-sitter (web-tree-sitter) |  2.78 MB/s |  4.791 ms | 4.29x   |
| Shiki (JS engine)             |  1.08 MB/s | 12.369 ms | 11.06x  |

Every highlighter parses the corpus without errors: a highlighter that gives up
on a region skips the work of colouring it, so the constructs two of them
mishandled are written another way at the same token count.

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (41870 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| Java (ANTLR4)   | 768.16 KB/s |  54.507 ms | 1.00x   |
| **Wado** (Gale) | 290.33 KB/s | 144.215 ms | 2.65x   |

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
| `GET /user`                     |      45,651 |                   47,609 |                25,190 |                    17,726 |
| `GET /user/lookup/username/hey` |      42,610 |                   37,343 |                23,792 |                    17,577 |
| `POST /event/abcd1234/comment`  |      44,025 |                   44,229 |                23,439 |                    16,060 |
| `GET /static/index.html`        |      42,405 |                   41,437 |                24,413 |                    17,522 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     403,343 |                  281,941 |               119,022 |                    85,419 |
| `GET /user/lookup/username/hey` |     395,039 |                  246,311 |               109,439 |                    80,801 |
| `POST /event/abcd1234/comment`  |     384,374 |                  249,356 |               111,358 |                    69,535 |
| `GET /static/index.html`        |     389,315 |                  245,524 |               112,794 |                    77,379 |

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

### GC heap size

`wado run` starts a guest with a 256 MiB GC heap, and `--gc-heap-initial`
changes it. The copying collector splits that into two semi-spaces, so a
program allocates through half of it between collections. A larger heap trades
resident memory for fewer of them.

Two rows measure faster with a larger heap. microgpt holds a whole autograd
graph live and gale_gen its grammar tables, so both pay for every collection.
They run at 512 MiB, which `gc_heap_flags` in `wado.sh` sets.

Every other row takes the default. At 512 MiB json-catalog, zlib and
sqlite-parse are all slower and the rest are flat, and `wado serve` measures
the same at either size. Below the default nothing improves: microgpt loses
2.1x at 128 MiB, where its live set no longer fits.

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
