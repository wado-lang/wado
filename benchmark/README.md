# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-10-02, wasmtime 49.0.0, gcc 13.3.0, wasi-sdk 33.0,
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
| Rust           | 2.81 k tokens/s |  80.782 ms | 1.00x   |
| **Wado**       | 1.67 k tokens/s | 135.822 ms | 1.68x   |
| JavaScript     | 1.27 k tokens/s | 179.413 ms | 2.22x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |   ms/iter | vs best |
| -------------- | --------------: | --------: | ------- |
| **Wado**       | 4.66 k tokens/s | 82.431 ms | 1.00x   |
| Rust           | 4.45 k tokens/s | 86.252 ms | 1.05x   |
| JavaScript     | 3.98 k tokens/s | 96.395 ms | 1.17x   |

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

| Implementation |  Throughput |   ms/iter | vs best |
| -------------- | ----------: | --------: | ------- |
| JavaScript     | 8.36 M px/s | 94.113 ms | 1.00x   |
| **Wado**       | 8.25 M px/s | 95.298 ms | 1.01x   |
| C              | 8.17 M px/s | 96.284 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 830.70 M nums/s | 2.408 ms | 1.00x   |
| JavaScript     | 581.55 M nums/s | 3.439 ms | 1.43x   |
| **Wado**       | 349.09 M nums/s | 5.729 ms | 2.38x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.89 M conv/s | 35.853 ms | 1.00x   |
| Rust (core::fmt) | 21.18 M conv/s | 47.214 ms | 1.32x   |
| C (printf)       | 11.62 M conv/s | 86.069 ms | 2.40x   |

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
| Rust (serde_json)    |   1.95 GB/s | 0.324 ms | 1.00x   |
| JavaScript (JSON)    |   1.58 GB/s | 0.400 ms | 1.23x   |
| **Wado** (core:json) | 999.14 MB/s | 0.632 ms | 1.95x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 606.36 MB/s | 1.041 ms | 1.00x   |
| Rust (serde_json)    | 602.49 MB/s | 1.048 ms | 1.01x   |
| **Wado** (core:json) | 340.86 MB/s | 1.852 ms | 1.78x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.46 GB/s | 0.257 ms | 1.00x   |
| **Wado** (core:cbor) |  1.67 GB/s | 0.377 ms | 1.47x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 893.02 MB/s | 0.707 ms | 1.00x   |
| **Wado** (core:cbor) | 598.53 MB/s | 1.055 ms | 1.49x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 992.67 MB/s | 2.268 ms | 1.00x   |
| JavaScript (JSON)    | 616.37 MB/s | 3.652 ms | 1.61x   |
| **Wado** (core:json) | 368.84 MB/s | 6.103 ms | 2.69x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 392.98 MB/s | 5.728 ms | 1.00x   |
| Rust (serde_json)    | 381.33 MB/s | 5.903 ms | 1.03x   |
| **Wado** (core:json) | 259.32 MB/s | 8.680 ms | 1.52x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.70 GB/s | 0.834 ms | 1.00x   |
| **Wado** (core:cbor) | 953.57 MB/s | 2.360 ms | 2.83x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.25 GB/s | 1.797 ms | 1.00x   |
| **Wado** (core:cbor) | 510.85 MB/s | 4.406 ms | 2.45x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.38 GB/s | 0.395 ms | 1.00x   |
| **Wado** (core:json) |  2.37 GB/s | 0.729 ms | 1.85x   |
| JavaScript (JSON)    |  1.50 GB/s | 1.151 ms | 2.91x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    |   1.07 GB/s | 1.614 ms | 1.00x   |
| JavaScript (JSON)    | 781.19 MB/s | 2.211 ms | 1.37x   |
| **Wado** (core:json) | 520.96 MB/s | 3.315 ms | 2.05x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.88 GB/s | 0.445 ms | 1.00x   |
| **Wado** (core:cbor) |  2.82 GB/s | 0.613 ms | 1.38x   |

CBOR deserialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.71 GB/s | 0.637 ms | 1.00x   |
| **Wado** (core:cbor) |  1.08 GB/s | 1.594 ms | 2.50x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 341.18 MB/s | 1.851 ms | 1.00x   |
| JavaScript (node:zlib) | 210.05 MB/s | 3.007 ms | 1.62x   |
| C (zlib 1.3.1, Wasm)   | 141.25 MB/s | 4.471 ms | 2.42x   |
| **Wado** (core:zlib)   | 119.20 MB/s | 5.297 ms | 2.86x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.40 GB/s | 0.186 ms | 1.00x   |
| JavaScript (node:zlib) |   1.89 GB/s | 0.335 ms | 1.80x   |
| C (zlib 1.3.1, Wasm)   | 863.06 MB/s | 0.732 ms | 3.94x   |
| **Wado** (core:zlib)   | 733.50 MB/s | 0.860 ms | 4.62x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 18.49 MB/s |   0.720 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.58 MB/s |   1.059 ms | 1.47x   |
| Java (ANTLR4)       |  0.11 MB/s | 123.976 ms | 172.19x |

Java (ANTLR4) is the head-to-head for Gale's generated parser, on the JVM and
JIT-warmed to steady state, so the gap is algorithmic rather than a warmup
artifact. The cost is full-context LL — this
grammar's ambiguities defeat the two-stage SLL fast path. Needs `java`; skipped
if absent.

### Syntax Highlight

Highlight 81 SQL statements (13321 bytes). Gale-generated highlighter vs five
reference SQL highlighters:

- **Prism.js** — regex-based, the speed reference
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
| **Gale** (Wado)               | 13.22 MB/s |  1.007 ms | 1.00x   |
| Prism.js                      | 12.25 MB/s |  1.088 ms | 1.08x   |
| Lezer (CodeMirror)            |  5.12 MB/s |  2.602 ms | 2.58x   |
| tree-sitter (Rust native)     |  4.75 MB/s |  2.804 ms | 2.78x   |
| tree-sitter (web-tree-sitter) |  2.96 MB/s |  4.505 ms | 4.47x   |
| Shiki (JS engine)             |  1.10 MB/s | 12.066 ms | 11.98x  |

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

| Implementation  |  Throughput |   ms/iter | vs best |
| --------------- | ----------: | --------: | ------- |
| Java (ANTLR4)   | 805.89 KB/s | 52.309 ms | 1.00x   |
| **Wado** (Gale) | 474.82 KB/s | 88.781 ms | 1.70x   |

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

| Request                         | JavaScript (Hono on Bun) | Rust (Axum) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | -----------------------: | ----------: | --------------------: | ------------------------: |
| `GET /user`                     |                   51,211 |      49,419 |                27,420 |                    18,354 |
| `GET /user/lookup/username/hey` |                   37,685 |      45,912 |                26,936 |                    18,165 |
| `POST /event/abcd1234/comment`  |                   45,512 |      45,063 |                27,042 |                    16,355 |
| `GET /static/index.html`        |                   52,982 |      48,359 |                27,088 |                    17,669 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     415,475 |                  295,125 |               137,347 |                    87,260 |
| `GET /user/lookup/username/hey` |     407,940 |                  243,625 |               127,189 |                    83,716 |
| `POST /event/abcd1234/comment`  |     408,389 |                  247,858 |               131,493 |                    74,865 |
| `GET /static/index.html`        |     413,977 |                  258,881 |               130,829 |                    82,791 |

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
