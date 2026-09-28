# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-28, wasmtime 49.0.0, gcc 13.3.0, wasi-sdk 33.0,
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
| Rust           | 2.75 k tokens/s |  82.520 ms | 1.00x   |
| **Wado**       | 1.37 k tokens/s | 166.170 ms | 2.01x   |
| JavaScript     | 1.25 k tokens/s | 181.555 ms | 2.20x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| Rust           | 4.37 k tokens/s |  87.957 ms | 1.00x   |
| **Wado**       | 3.77 k tokens/s | 101.834 ms | 1.16x   |
| JavaScript     | 3.44 k tokens/s | 111.738 ms | 1.27x   |

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
| JavaScript     | 7.91 M px/s |  99.463 ms | 1.00x   |
| **Wado**       | 7.81 M px/s | 100.755 ms | 1.01x   |
| C              | 7.74 M px/s | 101.619 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 770.85 M nums/s | 2.595 ms | 1.00x   |
| JavaScript     | 553.33 M nums/s | 3.615 ms | 1.39x   |
| **Wado**       | 343.00 M nums/s | 5.830 ms | 2.25x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 28.37 M conv/s | 35.242 ms | 1.00x   |
| Rust (core::fmt) | 21.25 M conv/s | 47.061 ms | 1.34x   |
| C (printf)       | 11.88 M conv/s | 84.184 ms | 2.39x   |

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
| Rust (serde_json)    |   1.94 GB/s | 0.326 ms | 1.00x   |
| JavaScript (JSON)    |   1.62 GB/s | 0.390 ms | 1.20x   |
| **Wado** (core:json) | 986.87 MB/s | 0.639 ms | 1.96x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 595.45 MB/s | 1.061 ms | 1.00x   |
| JavaScript (JSON)    | 595.01 MB/s | 1.061 ms | 1.00x   |
| **Wado** (core:json) | 334.65 MB/s | 1.887 ms | 1.78x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.38 GB/s | 0.265 ms | 1.00x   |
| **Wado** (core:cbor) |  1.66 GB/s | 0.380 ms | 1.43x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 862.65 MB/s | 0.732 ms | 1.00x   |
| **Wado** (core:cbor) | 581.45 MB/s | 1.086 ms | 1.48x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 991.68 MB/s | 2.270 ms | 1.00x   |
| JavaScript (JSON)    | 606.39 MB/s | 3.712 ms | 1.64x   |
| **Wado** (core:json) | 366.41 MB/s | 6.143 ms | 2.71x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 367.53 MB/s | 6.125 ms | 1.00x   |
| JavaScript (JSON)    | 366.37 MB/s | 6.144 ms | 1.00x   |
| **Wado** (core:json) | 247.37 MB/s | 9.099 ms | 1.49x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.66 GB/s | 0.847 ms | 1.00x   |
| **Wado** (core:cbor) | 884.65 MB/s | 2.544 ms | 3.00x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.25 GB/s | 1.794 ms | 1.00x   |
| **Wado** (core:cbor) | 494.26 MB/s | 4.554 ms | 2.54x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.29 GB/s | 0.403 ms | 1.00x   |
| **Wado** (core:json) |  2.23 GB/s | 0.775 ms | 1.92x   |
| JavaScript (JSON)    |  1.45 GB/s | 1.188 ms | 2.95x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   1.04 GB/s | 1.659 ms | 1.00x   |
| JavaScript (JSON)    | 747.49 MB/s | 2.311 ms | 1.39x   |
| **Wado** (core:json) | 490.43 MB/s | 3.521 ms | 2.12x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.71 GB/s | 0.466 ms | 1.00x   |
| **Wado** (core:cbor) |  2.78 GB/s | 0.620 ms | 1.33x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.62 GB/s | 0.660 ms | 1.00x   |
| **Wado** (core:cbor) | 946.61 MB/s | 1.824 ms | 2.76x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 331.41 MB/s | 1.906 ms | 1.00x   |
| JavaScript (node:zlib) | 202.71 MB/s | 3.115 ms | 1.63x   |
| C (zlib 1.3.1, Wasm)   | 135.18 MB/s | 4.671 ms | 2.45x   |
| **Wado** (core:zlib)   | 118.57 MB/s | 5.326 ms | 2.79x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.27 GB/s | 0.193 ms | 1.00x   |
| JavaScript (node:zlib) |   1.85 GB/s | 0.340 ms | 1.76x   |
| C (zlib 1.3.1, Wasm)   | 834.90 MB/s | 0.756 ms | 3.92x   |
| **Wado** (core:zlib)   | 464.67 MB/s | 1.359 ms | 7.04x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 14.59 MB/s |   0.913 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.27 MB/s |   1.085 ms | 1.19x   |
| Java (ANTLR4)       |  0.10 MB/s | 128.858 ms | 141.14x |

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
| Prism.js                      | 11.79 MB/s |  1.130 ms | 1.00x   |
| **Gale** (Wado)               | 11.21 MB/s |  1.188 ms | 1.05x   |
| Lezer (CodeMirror)            |  4.95 MB/s |  2.691 ms | 2.38x   |
| tree-sitter (Rust native)     |  4.56 MB/s |  2.920 ms | 2.58x   |
| tree-sitter (web-tree-sitter) |  2.90 MB/s |  4.591 ms | 4.06x   |
| Shiki (JS engine)             |  1.09 MB/s | 12.212 ms | 10.81x  |

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
| Java (ANTLR4)   | 778.80 KB/s |  54.128 ms | 1.00x   |
| **Wado** (Gale) | 340.51 KB/s | 123.799 ms | 2.29x   |

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
| `GET /user`                     |      43,546 |                   37,197 |                26,402 |                    17,238 |
| `GET /user/lookup/username/hey` |      43,835 |                   57,505 |                25,751 |                    16,808 |
| `POST /event/abcd1234/comment`  |      42,223 |                   39,128 |                25,908 |                    15,491 |
| `GET /static/index.html`        |      42,771 |                   36,454 |                25,966 |                    16,608 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     388,989 |                  263,762 |               131,325 |                    83,150 |
| `GET /user/lookup/username/hey` |     380,324 |                  229,154 |               123,047 |                    78,891 |
| `POST /event/abcd1234/comment`  |     381,207 |                  229,901 |               123,795 |                    66,860 |
| `GET /static/index.html`        |     383,914 |                  226,436 |               124,833 |                    77,093 |

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
