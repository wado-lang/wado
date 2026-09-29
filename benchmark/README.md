# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-29, wasmtime 49.0.0, gcc 13.3.0, wasi-sdk 33.0,
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
| Rust           | 2.72 k tokens/s |  83.572 ms | 1.00x   |
| **Wado**       | 1.29 k tokens/s | 175.409 ms | 2.10x   |
| JavaScript     | 1.19 k tokens/s | 190.711 ms | 2.28x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| Rust           | 4.20 k tokens/s |  91.340 ms | 1.00x   |
| **Wado**       | 3.69 k tokens/s | 104.098 ms | 1.14x   |
| JavaScript     | 3.26 k tokens/s | 117.866 ms | 1.29x   |

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
| JavaScript     | 7.72 M px/s | 101.836 ms | 1.00x   |
| **Wado**       | 7.60 M px/s | 103.496 ms | 1.02x   |
| C              | 7.56 M px/s | 104.087 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 764.18 M nums/s | 2.617 ms | 1.00x   |
| JavaScript     | 558.59 M nums/s | 3.580 ms | 1.37x   |
| **Wado**       | 355.77 M nums/s | 5.621 ms | 2.15x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 26.74 M conv/s | 37.390 ms | 1.00x   |
| Rust (core::fmt) | 20.08 M conv/s | 49.803 ms | 1.33x   |
| C (printf)       | 11.13 M conv/s | 89.861 ms | 2.40x   |

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
| Rust (serde_json)    |   1.88 GB/s | 0.337 ms | 1.00x   |
| JavaScript (JSON)    |   1.54 GB/s | 0.410 ms | 1.22x   |
| **Wado** (core:json) | 957.64 MB/s | 0.659 ms | 1.96x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 570.95 MB/s | 1.106 ms | 1.00x   |
| Rust (serde_json)    | 563.97 MB/s | 1.120 ms | 1.01x   |
| **Wado** (core:json) | 323.46 MB/s | 1.952 ms | 1.77x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.31 GB/s | 0.273 ms | 1.00x   |
| **Wado** (core:cbor) |  1.57 GB/s | 0.401 ms | 1.47x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 850.30 MB/s | 0.743 ms | 1.00x   |
| **Wado** (core:cbor) | 533.40 MB/s | 1.183 ms | 1.59x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 924.57 MB/s | 2.435 ms | 1.00x   |
| JavaScript (JSON)    | 568.57 MB/s | 3.959 ms | 1.63x   |
| **Wado** (core:json) | 351.49 MB/s | 6.404 ms | 2.63x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 351.07 MB/s | 6.412 ms | 1.00x   |
| Rust (serde_json)    | 343.71 MB/s | 6.549 ms | 1.02x   |
| **Wado** (core:json) | 245.66 MB/s | 9.163 ms | 1.43x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.50 GB/s | 0.900 ms | 1.00x   |
| **Wado** (core:cbor) | 892.33 MB/s | 2.522 ms | 2.80x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.14 GB/s | 1.972 ms | 1.00x   |
| **Wado** (core:cbor) | 487.90 MB/s | 4.613 ms | 2.34x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.19 GB/s | 0.412 ms | 1.00x   |
| **Wado** (core:json) |  2.17 GB/s | 0.794 ms | 1.93x   |
| JavaScript (JSON)    |  1.42 GB/s | 1.216 ms | 2.95x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   1.02 GB/s | 1.701 ms | 1.00x   |
| JavaScript (JSON)    | 741.03 MB/s | 2.331 ms | 1.37x   |
| **Wado** (core:json) | 463.51 MB/s | 3.726 ms | 2.19x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.64 GB/s | 0.475 ms | 1.00x   |
| **Wado** (core:cbor) |  2.67 GB/s | 0.646 ms | 1.36x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.56 GB/s | 0.674 ms | 1.00x   |
| **Wado** (core:cbor) | 934.82 MB/s | 1.847 ms | 2.74x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 326.67 MB/s | 1.933 ms | 1.00x   |
| JavaScript (node:zlib) | 198.95 MB/s | 3.174 ms | 1.64x   |
| C (zlib 1.3.1, Wasm)   | 128.96 MB/s | 4.897 ms | 2.53x   |
| **Wado** (core:zlib)   | 117.06 MB/s | 5.394 ms | 2.79x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.13 GB/s | 0.202 ms | 1.00x   |
| JavaScript (node:zlib) |   1.82 GB/s | 0.347 ms | 1.72x   |
| C (zlib 1.3.1, Wasm)   | 812.22 MB/s | 0.778 ms | 3.85x   |
| **Wado** (core:zlib)   | 648.64 MB/s | 0.973 ms | 4.82x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 14.43 MB/s |   0.923 ms | 1.00x   |
| Rust (sqlparser-rs) | 11.73 MB/s |   1.135 ms | 1.23x   |
| Java (ANTLR4)       |  0.10 MB/s | 136.439 ms | 147.82x |

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
| Prism.js                      | 11.91 MB/s |  1.118 ms | 1.00x   |
| **Gale** (Wado)               | 11.14 MB/s |  1.195 ms | 1.07x   |
| Lezer (CodeMirror)            |  4.71 MB/s |  2.827 ms | 2.53x   |
| tree-sitter (Rust native)     |  4.65 MB/s |  2.865 ms | 2.56x   |
| tree-sitter (web-tree-sitter) |  2.72 MB/s |  4.896 ms | 4.38x   |
| Shiki (JS engine)             |  1.05 MB/s | 12.667 ms | 11.33x  |

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
| Java (ANTLR4)   | 752.40 KB/s |  56.028 ms | 1.00x   |
| **Wado** (Gale) | 336.76 KB/s | 125.177 ms | 2.23x   |

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
| `GET /user`                     |      43,165 |                   44,938 |                25,491 |                    16,705 |
| `GET /user/lookup/username/hey` |      43,623 |                   42,593 |                24,971 |                    16,452 |
| `POST /event/abcd1234/comment`  |      42,587 |                   38,929 |                24,901 |                    15,284 |
| `GET /static/index.html`        |      41,890 |                   36,062 |                25,114 |                    16,256 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     378,681 |                  253,270 |               126,658 |                    78,758 |
| `GET /user/lookup/username/hey` |     374,786 |                  196,949 |               119,509 |                    75,000 |
| `POST /event/abcd1234/comment`  |     373,421 |                  216,314 |               118,373 |                    65,795 |
| `GET /static/index.html`        |     376,357 |                  219,567 |               120,930 |                    74,725 |

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
