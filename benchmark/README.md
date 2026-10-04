# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-10-04, wasmtime 49.0.0, gcc 15.2.0, wasi-sdk 34.0,
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
| Rust           | 2.45 k tokens/s |  92.680 ms | 1.00x   |
| **Wado**       | 1.54 k tokens/s | 147.242 ms | 1.59x   |
| JavaScript     | 1.31 k tokens/s | 173.691 ms | 1.87x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| **Wado**       | 4.27 k tokens/s |  89.939 ms | 1.00x   |
| JavaScript     | 3.45 k tokens/s | 111.362 ms | 1.24x   |
| Rust           | 3.00 k tokens/s | 127.871 ms | 1.42x   |

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
| JavaScript     | 7.67 M px/s | 102.467 ms | 1.00x   |
| C              | 7.56 M px/s | 104.020 ms | 1.02x   |
| **Wado**       | 7.50 M px/s | 104.878 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              |   1.04 G nums/s | 1.926 ms | 1.00x   |
| JavaScript     | 554.53 M nums/s | 3.607 ms | 1.87x   |
| **Wado**       | 328.64 M nums/s | 6.085 ms | 3.16x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 25.67 M conv/s | 38.958 ms | 1.00x   |
| Rust (core::fmt) | 20.33 M conv/s | 49.193 ms | 1.26x   |
| C (printf)       | 11.64 M conv/s | 85.937 ms | 2.21x   |

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
| Rust (serde_json)    |  1.85 GB/s | 0.341 ms | 1.00x   |
| JavaScript (JSON)    |  1.57 GB/s | 0.401 ms | 1.18x   |
| **Wado** (core:json) |  1.14 GB/s | 0.552 ms | 1.62x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 588.52 MB/s | 1.073 ms | 1.00x   |
| JavaScript (JSON)    | 586.51 MB/s | 1.077 ms | 1.00x   |
| **Wado** (core:json) | 330.97 MB/s | 1.908 ms | 1.78x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.31 GB/s | 0.273 ms | 1.00x   |
| **Wado** (core:cbor) |  1.67 GB/s | 0.378 ms | 1.38x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 882.95 MB/s | 0.715 ms | 1.00x   |
| **Wado** (core:cbor) | 555.47 MB/s | 1.136 ms | 1.59x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 934.02 MB/s | 2.410 ms | 1.00x   |
| JavaScript (JSON)    | 568.56 MB/s | 3.959 ms | 1.64x   |
| **Wado** (core:json) | 355.22 MB/s | 6.336 ms | 2.63x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 486.98 MB/s | 4.622 ms | 1.00x   |
| JavaScript (JSON)    | 349.30 MB/s | 6.445 ms | 1.39x   |
| **Wado** (core:json) | 241.42 MB/s | 9.324 ms | 2.02x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.45 GB/s | 0.918 ms | 1.00x   |
| **Wado** (core:cbor) | 908.63 MB/s | 2.477 ms | 2.70x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.23 GB/s | 1.829 ms | 1.00x   |
| **Wado** (core:cbor) | 472.97 MB/s | 4.759 ms | 2.60x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.15 GB/s | 0.416 ms | 1.00x   |
| **Wado** (core:json) |  2.37 GB/s | 0.728 ms | 1.75x   |
| JavaScript (JSON)    |  1.45 GB/s | 1.190 ms | 2.86x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 992.80 MB/s | 1.740 ms | 1.00x   |
| JavaScript (JSON)    | 763.03 MB/s | 2.264 ms | 1.30x   |
| **Wado** (core:json) | 465.98 MB/s | 3.706 ms | 2.13x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.61 GB/s | 0.478 ms | 1.00x   |
| **Wado** (core:cbor) |  2.69 GB/s | 0.642 ms | 1.34x   |

CBOR deserialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.59 GB/s | 0.668 ms | 1.00x   |
| **Wado** (core:cbor) |  1.03 GB/s | 1.682 ms | 2.52x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 318.81 MB/s | 1.981 ms | 1.00x   |
| JavaScript (node:zlib) | 200.29 MB/s | 3.153 ms | 1.59x   |
| C (zlib 1.3.1, Wasm)   | 129.65 MB/s | 4.871 ms | 2.46x   |
| **Wado** (core:zlib)   | 117.53 MB/s | 5.373 ms | 2.71x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.14 GB/s | 0.201 ms | 1.00x   |
| JavaScript (node:zlib) |   2.00 GB/s | 0.315 ms | 1.57x   |
| C (zlib 1.3.1, Wasm)   | 843.45 MB/s | 0.749 ms | 3.73x   |
| **Wado** (core:zlib)   | 691.57 MB/s | 0.913 ms | 4.54x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 18.47 MB/s |   0.721 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.26 MB/s |   1.087 ms | 1.51x   |
| Java (ANTLR4)       |  0.10 MB/s | 126.887 ms | 175.99x |

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
| **Gale** (Wado)               | 13.13 MB/s | 1.014 ms | 1.00x   |
| Prism.js                      | 12.03 MB/s | 1.108 ms | 1.09x   |
| Lezer (CodeMirror)            |  4.83 MB/s | 2.757 ms | 2.72x   |
| tree-sitter (Rust native)     |  4.77 MB/s | 2.792 ms | 2.75x   |
| tree-sitter (web-tree-sitter) |  2.78 MB/s | 4.793 ms | 4.73x   |

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

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| Java (ANTLR4)   | 974.52 KB/s |  43.257 ms | 1.00x   |
| **Wado** (Gale) | 405.19 KB/s | 104.036 ms | 2.41x   |

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

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     140,844 |                  127,485 |                    65,666 |                56,473 |
| `GET /user/lookup/username/hey` |     137,417 |                  122,219 |                    61,830 |                53,952 |
| `POST /event/abcd1234/comment`  |     137,376 |                  122,285 |                    51,802 |                53,978 |
| `GET /static/index.html`        |     137,826 |                  121,552 |                    60,789 |                54,839 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     478,844 |                  459,473 |                   242,547 |               195,529 |
| `GET /user/lookup/username/hey` |     475,432 |                  431,147 |                   226,837 |               185,146 |
| `POST /event/abcd1234/comment`  |     473,930 |                  435,130 |                   190,463 |               185,460 |
| `GET /static/index.html`        |     472,411 |                  431,122 |                   222,048 |               186,900 |

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
