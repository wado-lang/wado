# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-10-22, wasmtime 50.0.0, gcc 15.2.0, wasi-sdk 34.0,
rustc 1.98.1, Node.js v26.10.0, Bun 1.4.2, Ubuntu 26.04 x86_64 (Linux 7.0).

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
| Rust           | 2.45 k tokens/s |  92.591 ms | 1.00x   |
| **Wado**       | 1.90 k tokens/s | 119.327 ms | 1.29x   |
| JavaScript     | 1.27 k tokens/s | 178.117 ms | 1.92x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| **Wado**       | 5.48 k tokens/s |  70.058 ms | 1.00x   |
| Rust           | 2.99 k tokens/s | 128.462 ms | 1.83x   |
| JavaScript     | 2.46 k tokens/s | 156.029 ms | 2.23x   |

Training spends most of its time in the backward pass, which
sorts the whole graph topologically and then walks every edge again. That is
pointer chasing over GC objects, where Rust's flat `Vec` of indices is at its
strongest. Inference never builds that traversal.

Each sample also runs the full 16-position attention window, deeper than any
training step reaches on this corpus: the longest name gives 11 positions and
the mean is 7, and attention cost grows with the square of the position count.

Rust makes a node a `usize` index into a `Vec<Value>`, because a `&mut` into a
growing `Vec` is what the borrow checker forbids. Wado's `Graph::value` returns
that `&mut Value` and a node holds those handles as its children, which is the
shape the Python original has. The training gap is what that costs.

All three arms print the same final loss and generate the same sample, so they
are the same computation.

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| JavaScript     | 7.68 M px/s | 102.466 ms | 1.00x   |
| C              | 7.56 M px/s | 104.013 ms | 1.02x   |
| **Wado**       | 7.52 M px/s | 104.615 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              |   1.04 G nums/s | 1.918 ms | 1.00x   |
| JavaScript     | 553.89 M nums/s | 3.611 ms | 1.88x   |
| **Wado**       | 333.42 M nums/s | 5.998 ms | 3.13x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 26.63 M conv/s | 37.554 ms | 1.00x   |
| Rust (core::fmt) | 20.39 M conv/s | 49.052 ms | 1.31x   |
| C (printf)       | 11.59 M conv/s | 86.309 ms | 2.30x   |

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

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  1.84 GB/s | 0.343 ms | 1.00x   |
| JavaScript (JSON)    |  1.58 GB/s | 0.399 ms | 1.16x   |
| **Wado** (core:json) |  1.12 GB/s | 0.562 ms | 1.64x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 592.64 MB/s | 1.066 ms | 1.00x   |
| Rust (serde_json)    | 588.42 MB/s | 1.073 ms | 1.01x   |
| **Wado** (core:json) | 340.11 MB/s | 1.856 ms | 1.74x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.33 GB/s | 0.271 ms | 1.00x   |
| **Wado** (core:cbor) |  1.71 GB/s | 0.368 ms | 1.36x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 874.19 MB/s | 0.722 ms | 1.00x   |
| **Wado** (core:cbor) | 667.94 MB/s | 0.945 ms | 1.31x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 934.28 MB/s | 2.409 ms | 1.00x   |
| JavaScript (JSON)    | 568.79 MB/s | 3.958 ms | 1.64x   |
| **Wado** (core:json) | 359.22 MB/s | 6.266 ms | 2.60x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 489.79 MB/s | 4.596 ms | 1.00x   |
| JavaScript (JSON)    | 349.73 MB/s | 6.436 ms | 1.40x   |
| **Wado** (core:json) | 254.94 MB/s | 8.829 ms | 1.92x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.45 GB/s | 0.919 ms | 1.00x   |
| **Wado** (core:cbor) | 898.02 MB/s | 2.506 ms | 2.73x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.23 GB/s | 1.825 ms | 1.00x   |
| **Wado** (core:cbor) | 479.29 MB/s | 4.696 ms | 2.57x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.15 GB/s | 0.416 ms | 1.00x   |
| **Wado** (core:json) |  2.42 GB/s | 0.713 ms | 1.71x   |
| JavaScript (JSON)    |  1.45 GB/s | 1.193 ms | 2.87x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 991.03 MB/s | 1.743 ms | 1.00x   |
| JavaScript (JSON)    | 766.36 MB/s | 2.254 ms | 1.29x   |
| **Wado** (core:json) | 505.93 MB/s | 3.413 ms | 1.96x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.62 GB/s | 0.477 ms | 1.00x   |
| **Wado** (core:cbor) |  2.71 GB/s | 0.638 ms | 1.34x   |

CBOR deserialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.58 GB/s | 0.670 ms | 1.00x   |
| **Wado** (core:cbor) |  1.03 GB/s | 1.678 ms | 2.50x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 322.45 MB/s | 1.959 ms | 1.00x   |
| JavaScript (node:zlib) | 202.24 MB/s | 3.123 ms | 1.59x   |
| C (zlib 1.3.1, Wasm)   | 129.69 MB/s | 4.870 ms | 2.49x   |
| **Wado** (core:zlib)   | 116.69 MB/s | 5.412 ms | 2.76x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.15 GB/s | 0.200 ms | 1.00x   |
| JavaScript (node:zlib) |   1.94 GB/s | 0.326 ms | 1.63x   |
| C (zlib 1.3.1, Wasm)   | 843.71 MB/s | 0.748 ms | 3.74x   |
| **Wado** (core:zlib)   | 691.40 MB/s | 0.913 ms | 4.57x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 23.30 MB/s |   0.571 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.28 MB/s |   1.085 ms | 1.90x   |
| Java (ANTLR4)       |  0.10 MB/s | 127.047 ms | 222.50x |

The Gale parser is generated with `atn: "never"`: every decision is settled by
generated code, with no ATN simulator at run time.

Java (ANTLR4) is the head-to-head for Gale's generated parser, on the JVM and
JIT-warmed to steady state, so the gap is algorithmic rather than a warmup
artifact. The cost is full-context LL — this
grammar's ambiguities defeat the two-stage SLL fast path. Needs `java`; skipped
if absent.

### Syntax Highlight

Highlight 81 SQL statements (13321 bytes). Gale-generated highlighter vs four
reference SQL highlighters:

- **Prism.js** — regex-based, the speed reference
- **tree-sitter (Rust native)** — same `tree-sitter-sequel` grammar used by the
  JS row below, run as a Rust binary
- **Lezer (CodeMirror)** — `@codemirror/lang-sql` + `@lezer/highlight`, a
  pure-JS LR parser
- **tree-sitter (web-tree-sitter)** — official JS WASM binding, same
  `tree-sitter-sequel` grammar as the Rust row (upstream
  `@derekstride/tree-sitter-sql`)

Labels here name the highlighter rather than the language: this benchmark is
about what a browser would run.

| Implementation                | Throughput |  ms/iter | vs best |
| ----------------------------- | ---------: | -------: | ------- |
| **Gale** (Wado)               | 15.52 MB/s | 0.858 ms | 1.00x   |
| Prism.js                      | 11.96 MB/s | 1.114 ms | 1.30x   |
| Lezer (CodeMirror)            |  4.82 MB/s | 2.766 ms | 3.22x   |
| tree-sitter (Rust native)     |  4.73 MB/s | 2.814 ms | 3.28x   |
| tree-sitter (web-tree-sitter) |  2.80 MB/s | 4.754 ms | 5.54x   |

The Gale highlighter's parser is generated with `atn: "never"`, as in SQL Parse.

Every highlighter parses the corpus without errors: a highlighter that gives up
on a region skips the work of colouring it, so the constructs two of them
mishandled are written another way at the same token count.

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (42155 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation                   |  Throughput |   ms/iter | vs best |
| -------------------------------- | ----------: | --------: | ------- |
| Java (ANTLR4)                    | 966.93 KB/s | 43.597 ms | 1.00x   |
| **Wado** (Gale, 1 GiB heap)      | 482.72 KB/s | 87.328 ms | 2.00x   |
| **Wado** (Gale, 256 MiB default) | 468.16 KB/s | 90.044 ms | 2.07x   |

Both rows run in-process and warm, emitting a parser and no listeners: Gale a
Wado recursive-descent one from memory, ANTLR4 Java onto disk. Gale generates
with `atn: "never"`, so the parser it emits carries no ATN simulator.

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

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     142,820 |                  129,056 |                    59,550 |                54,056 |
| `GET /user/lookup/username/hey` |     138,923 |                  122,377 |                    56,640 |                51,503 |
| `POST /event/abcd1234/comment`  |     139,088 |                  122,456 |                    47,837 |                52,038 |
| `GET /static/index.html`        |     138,834 |                  120,522 |                    54,614 |                52,293 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     480,539 |                  450,434 |                   236,854 |               186,641 |
| `GET /user/lookup/username/hey` |     474,766 |                  427,887 |                   220,904 |               177,161 |
| `POST /event/abcd1234/comment`  |     472,551 |                  429,704 |                   191,834 |               178,509 |
| `GET /static/index.html`        |     462,191 |                  425,701 |                   214,341 |               179,604 |

What separates `wado serve` from Axum is the component-model boundary, not the
compiled code.
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

Prerequisites: `cc` and `cargo` (system); `node`, `bun` and `java` (managed by
`mise install`). The ANTLR4 reference rows (gale-gen, sqlite-parse) fetch the
jar to `~/.cache/gale`, and are skipped if `java` or `javac` is absent.

### GC heap size

`wado run` starts a guest with a 256 MiB GC heap, and `--gc-heap-initial`
changes it. The copying collector splits that into two semi-spaces, so a
program allocates through half of it between collections. A larger heap trades
resident memory for fewer of them.

Two benchmarks measure faster with a larger heap. microgpt holds a whole
autograd graph live, so it pays for every collection. It runs at 512 MiB, which
`gc_heap_flags` in `wado.sh` sets.

gale_gen allocates enough that its time depends on where its collections land,
and its ranking against another build can flip between two nearby heap sizes.
It runs at the default and again at 1 GiB (`GALE_GEN_BIG_HEAP`), as two rows:
the default is what a program gets, and 1 GiB shows the time once collections
are rare.

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
