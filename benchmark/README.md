# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-10-15, wasmtime 49.0.2, gcc 15.2.0, wasi-sdk 34.0,
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
| Rust           | 2.44 k tokens/s |  93.099 ms | 1.00x   |
| **Wado**       | 1.92 k tokens/s | 118.088 ms | 1.27x   |
| JavaScript     | 1.26 k tokens/s | 180.635 ms | 1.94x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| **Wado**       | 5.54 k tokens/s |  69.258 ms | 1.00x   |
| JavaScript     | 3.16 k tokens/s | 121.373 ms | 1.75x   |
| Rust           | 2.98 k tokens/s | 128.980 ms | 1.86x   |

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
| JavaScript     | 7.68 M px/s | 102.444 ms | 1.00x   |
| C              | 7.56 M px/s | 103.992 ms | 1.02x   |
| **Wado**       | 7.50 M px/s | 104.913 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              |   1.05 G nums/s | 1.913 ms | 1.00x   |
| JavaScript     | 554.55 M nums/s | 3.607 ms | 1.89x   |
| **Wado**       | 328.73 M nums/s | 6.084 ms | 3.18x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 25.94 M conv/s | 38.546 ms | 1.00x   |
| Rust (core::fmt) | 20.36 M conv/s | 49.112 ms | 1.27x   |
| C (printf)       | 11.68 M conv/s | 85.617 ms | 2.22x   |

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
| JavaScript (JSON)    |  1.61 GB/s | 0.392 ms | 1.15x   |
| **Wado** (core:json) |  1.16 GB/s | 0.546 ms | 1.60x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 591.56 MB/s | 1.068 ms | 1.00x   |
| Rust (serde_json)    | 590.08 MB/s | 1.070 ms | 1.00x   |
| **Wado** (core:json) | 346.04 MB/s | 1.824 ms | 1.71x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.31 GB/s | 0.273 ms | 1.00x   |
| **Wado** (core:cbor) |  1.72 GB/s | 0.366 ms | 1.34x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 900.46 MB/s | 0.701 ms | 1.00x   |
| **Wado** (core:cbor) | 661.97 MB/s | 0.953 ms | 1.36x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 935.35 MB/s | 2.407 ms | 1.00x   |
| JavaScript (JSON)    | 568.90 MB/s | 3.957 ms | 1.64x   |
| **Wado** (core:json) | 347.27 MB/s | 6.482 ms | 2.69x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 490.29 MB/s | 4.591 ms | 1.00x   |
| JavaScript (JSON)    | 357.64 MB/s | 6.294 ms | 1.37x   |
| **Wado** (core:json) | 259.52 MB/s | 8.673 ms | 1.89x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.45 GB/s | 0.920 ms | 1.00x   |
| **Wado** (core:cbor) | 907.04 MB/s | 2.481 ms | 2.70x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.24 GB/s | 1.821 ms | 1.00x   |
| **Wado** (core:cbor) | 488.02 MB/s | 4.612 ms | 2.53x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.17 GB/s | 0.414 ms | 1.00x   |
| **Wado** (core:json) |  2.17 GB/s | 0.796 ms | 1.92x   |
| JavaScript (JSON)    |  1.46 GB/s | 1.184 ms | 2.86x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 996.40 MB/s | 1.733 ms | 1.00x   |
| JavaScript (JSON)    | 753.51 MB/s | 2.292 ms | 1.32x   |
| **Wado** (core:json) | 476.80 MB/s | 3.622 ms | 2.09x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.62 GB/s | 0.477 ms | 1.00x   |
| **Wado** (core:cbor) |  2.71 GB/s | 0.636 ms | 1.33x   |

CBOR deserialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.59 GB/s | 0.666 ms | 1.00x   |
| **Wado** (core:cbor) |  1.03 GB/s | 1.682 ms | 2.53x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 323.50 MB/s | 1.952 ms | 1.00x   |
| JavaScript (node:zlib) | 200.51 MB/s | 3.150 ms | 1.61x   |
| C (zlib 1.3.1, Wasm)   | 129.52 MB/s | 4.876 ms | 2.50x   |
| **Wado** (core:zlib)   | 114.19 MB/s | 5.530 ms | 2.83x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.15 GB/s | 0.200 ms | 1.00x   |
| JavaScript (node:zlib) |   2.00 GB/s | 0.315 ms | 1.58x   |
| C (zlib 1.3.1, Wasm)   | 844.93 MB/s | 0.747 ms | 3.74x   |
| **Wado** (core:zlib)   | 705.02 MB/s | 0.895 ms | 4.48x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 18.69 MB/s |   0.712 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.33 MB/s |   1.080 ms | 1.52x   |
| Java (ANTLR4)       |  0.10 MB/s | 126.877 ms | 178.20x |

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
| **Gale** (Wado)               | 13.36 MB/s | 0.996 ms | 1.00x   |
| Prism.js                      | 12.04 MB/s | 1.107 ms | 1.11x   |
| Lezer (CodeMirror)            |  4.76 MB/s | 2.796 ms | 2.81x   |
| tree-sitter (Rust native)     |  4.75 MB/s | 2.804 ms | 2.82x   |
| tree-sitter (web-tree-sitter) |  2.81 MB/s | 4.737 ms | 4.76x   |

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
| Java (ANTLR4)                    | 973.89 KB/s | 43.285 ms | 1.00x   |
| **Wado** (Gale, 1 GiB heap)      | 527.01 KB/s | 79.988 ms | 1.85x   |
| **Wado** (Gale, 256 MiB default) | 496.29 KB/s | 84.939 ms | 1.96x   |

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
| `GET /user`                     |     144,392 |                  125,837 |                    61,297 |                55,998 |
| `GET /user/lookup/username/hey` |     138,906 |                  120,299 |                    59,572 |                53,270 |
| `POST /event/abcd1234/comment`  |     139,842 |                  121,175 |                    49,759 |                53,684 |
| `GET /static/index.html`        |     139,744 |                  119,492 |                    57,663 |                53,917 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     481,983 |                  446,061 |                   239,089 |               193,162 |
| `GET /user/lookup/username/hey` |     482,587 |                  420,318 |                   220,927 |               183,165 |
| `POST /event/abcd1234/comment`  |     493,920 |                  422,195 |                   190,293 |               184,690 |
| `GET /static/index.html`        |     481,413 |                  421,129 |                   220,344 |               186,270 |

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
