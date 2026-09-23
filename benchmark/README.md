# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-23, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
rustc 1.98.0, Node.js v26.7.0, Bun 1.3.14, Linux x86_64.

Throughput is work per second (higher is better), with per-iteration time in
parentheses. Native rows are optimized builds (C `gcc -O3`, Rust release, Wado
`-O2`); JavaScript runs on Node.js. `vs best` is the fastest row's throughput
over this row's (1.00x = fastest). Absolute throughput is machine-dependent, so
compare by `vs best`. Each figure is the best of three runs, except MicroGPT's,
which says below why it takes more.

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
| Rust           | 2.60 k tokens/s |  87.204 ms | 1.00x   |
| **Wado**       | 1.37 k tokens/s | 165.408 ms | 1.90x   |
| JavaScript     | 1.13 k tokens/s | 200.134 ms | 2.30x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| Rust           | 4.14 k tokens/s |  92.740 ms | 1.00x   |
| **Wado**       | 3.61 k tokens/s | 106.389 ms | 1.15x   |
| JavaScript     | 3.32 k tokens/s | 115.647 ms | 1.25x   |

These rows are best of eight rather than the best of three the rest of the file
uses. The JavaScript arm needs it: its inference time swung by more than a
factor of two across those eight, where Rust and Wado each stayed near their own
best. Three iterations per run is too few for the JIT to settle on that phase.

Wado beats JavaScript on both phases. It trails Rust on both, and the gap is
wider on training. Training spends most of its time in the backward pass, which
sorts the whole graph topologically and then walks every edge again. That is
pointer chasing over GC objects, where Rust's flat `Vec` of indices is at its
strongest. Inference never builds that traversal.

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
| JavaScript     | 7.62 M px/s | 103.152 ms | 1.00x   |
| **Wado**       | 7.50 M px/s | 104.869 ms | 1.02x   |
| C              | 7.48 M px/s | 105.201 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 741.28 M nums/s | 2.698 ms | 1.00x   |
| JavaScript     | 544.09 M nums/s | 3.676 ms | 1.36x   |
| **Wado**       | 346.49 M nums/s | 5.772 ms | 2.14x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 25.74 M conv/s | 38.850 ms | 1.00x   |
| Rust (core::fmt) | 19.92 M conv/s | 50.189 ms | 1.29x   |
| C (printf)       | 10.90 M conv/s | 91.725 ms | 2.36x   |

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
| Rust (serde_json)    |   1.82 GB/s | 0.347 ms | 1.00x   |
| JavaScript (JSON)    |   1.47 GB/s | 0.431 ms | 1.24x   |
| **Wado** (core:json) | 898.23 MB/s | 0.703 ms | 2.03x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 552.28 MB/s | 1.143 ms | 1.00x   |
| JavaScript (JSON)    | 549.06 MB/s | 1.150 ms | 1.01x   |
| **Wado** (core:json) | 293.67 MB/s | 2.150 ms | 1.88x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.26 GB/s | 0.279 ms | 1.00x   |
| **Wado** (core:cbor) |  1.40 GB/s | 0.450 ms | 1.61x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 826.78 MB/s | 0.764 ms | 1.00x   |
| **Wado** (core:cbor) | 470.55 MB/s | 1.342 ms | 1.76x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 915.23 MB/s | 2.460 ms | 1.00x   |
| JavaScript (JSON)    | 555.70 MB/s | 4.051 ms | 1.65x   |
| **Wado** (core:json) | 335.58 MB/s | 6.707 ms | 2.73x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 357.18 MB/s | 6.302 ms | 1.00x   |
| Rust (serde_json)    | 335.21 MB/s | 6.715 ms | 1.07x   |
| **Wado** (core:json) | 270.04 MB/s | 8.335 ms | 1.32x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.43 GB/s | 0.925 ms | 1.00x   |
| **Wado** (core:cbor) | 799.48 MB/s | 2.815 ms | 3.04x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.13 GB/s | 1.991 ms | 1.00x   |
| **Wado** (core:cbor) | 460.99 MB/s | 4.883 ms | 2.45x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.06 GB/s | 0.425 ms | 1.00x   |
| **Wado** (core:json) |  1.98 GB/s | 0.870 ms | 2.05x   |
| JavaScript (JSON)    |  1.39 GB/s | 1.238 ms | 2.91x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     | 996.86 MB/s | 1.733 ms | 1.00x   |
| JavaScript (JSON)     | 733.89 MB/s | 2.353 ms | 1.36x   |
| **Wado** (PoC parser) | 444.51 MB/s | 3.885 ms | 2.24x   |
| **Wado** (core:json)  | 424.28 MB/s | 4.070 ms | 2.35x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It is the mark `core:json` has to reach while
decoding any schema.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.60 GB/s | 0.480 ms | 1.00x   |
| **Wado** (core:cbor) |  2.40 GB/s | 0.719 ms | 1.50x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.51 GB/s | 0.688 ms | 1.00x   |
| **Wado** (core:cbor) | 779.56 MB/s | 2.215 ms | 3.22x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 309.56 MB/s | 2.040 ms | 1.00x   |
| JavaScript (node:zlib) | 194.77 MB/s | 3.242 ms | 1.59x   |
| C (zlib 1.3.1, Wasm)   | 127.63 MB/s | 4.948 ms | 2.43x   |
| **Wado** (core:zlib)   | 107.57 MB/s | 5.870 ms | 2.88x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.11 GB/s | 0.203 ms | 1.00x   |
| JavaScript (node:zlib) |   1.81 GB/s | 0.348 ms | 1.71x   |
| C (zlib 1.3.1, Wasm)   | 802.33 MB/s | 0.787 ms | 3.88x   |
| **Wado** (core:zlib)   | 431.33 MB/s | 1.464 ms | 7.21x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 12.79 MB/s |   1.041 ms | 1.00x   |
| Rust (sqlparser-rs) | 11.54 MB/s |   1.155 ms | 1.11x   |
| Java (ANTLR4)       |  0.10 MB/s | 133.250 ms | 128.00x |

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
| Prism.js                      | 11.29 MB/s |  1.180 ms | 1.00x   |
| **Gale** (Wado)               |  9.68 MB/s |  1.376 ms | 1.17x   |
| Lezer (CodeMirror)            |  4.61 MB/s |  2.892 ms | 2.45x   |
| tree-sitter (Rust native)     |  4.46 MB/s |  2.984 ms | 2.53x   |
| tree-sitter (web-tree-sitter) |  2.68 MB/s |  4.970 ms | 4.21x   |
| Shiki (JS engine)             |  1.04 MB/s | 12.784 ms | 10.83x  |

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

| Implementation  |  Throughput |   ms/iter | vs best |
| --------------- | ----------: | --------: | ------- |
| Java (ANTLR4)   | 724.16 KB/s | 57.819 ms | 1.00x   |
| **Wado** (Gale) | 539.12 KB/s | 77.663 ms | 1.34x   |

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
| `GET /user`                     |      44,854 |                   41,653 |                22,797 |                    16,452 |
| `GET /user/lookup/username/hey` |      42,424 |                   38,270 |                22,542 |                    16,039 |
| `POST /event/abcd1234/comment`  |      40,586 |                   36,303 |                22,618 |                    14,934 |
| `GET /static/index.html`        |      42,995 |                   37,327 |                22,487 |                    15,804 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     368,995 |                  248,387 |               117,294 |                    78,098 |
| `GET /user/lookup/username/hey` |     359,166 |                  212,656 |               111,620 |                    74,291 |
| `POST /event/abcd1234/comment`  |     360,680 |                  214,879 |               111,347 |                    63,672 |
| `GET /static/index.html`        |     366,119 |                  221,140 |               113,120 |                    73,472 |

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
