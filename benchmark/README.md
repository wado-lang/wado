# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-11-10, wasmtime 50.0.1, gcc 15.2.0, wasi-sdk 34.0,
rustc 1.99.1, Node.js v26.13.0, Bun 1.4.4, Ubuntu 26.04 x86_64 (Linux 7.0).

Throughput is work per second (higher is better), with per-iteration time in
parentheses. Native rows are optimized builds (C `gcc -O3`, Rust release, Wado
`-O2` with the benchmark's recorded [profile](#profile-guided-optimization));
JavaScript runs on Node.js. A Wado row marked `no-pgo` is the same program built
with `-f no-pgo`. No C or Rust row uses a profile, so that row is the one to
compare with them on equal terms. `vs best` is the fastest row's throughput over
this row's (1.00x = fastest). Absolute throughput is machine-dependent, so compare
by `vs best`. Each figure is the best of three runs.

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

| Implementation    |      Throughput |    ms/iter | vs best |
| ----------------- | --------------: | ---------: | ------- |
| Rust              | 2.45 k tokens/s |  92.593 ms | 1.00x   |
| **Wado**          | 2.44 k tokens/s |  92.892 ms | 1.00x   |
| **Wado** (no-pgo) | 2.37 k tokens/s |  95.624 ms | 1.03x   |
| JavaScript        | 1.70 k tokens/s | 133.208 ms | 1.44x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation    |      Throughput |    ms/iter | vs best |
| ----------------- | --------------: | ---------: | ------- |
| **Wado**          | 6.30 k tokens/s |  60.969 ms | 1.00x   |
| **Wado** (no-pgo) | 5.44 k tokens/s |  70.535 ms | 1.16x   |
| JavaScript        | 3.88 k tokens/s |  98.997 ms | 1.62x   |
| Rust              | 3.00 k tokens/s | 128.152 ms | 2.10x   |

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

The JavaScript rows run at the best semi-space size found, as
[GC heap size](#gc-heap-size) shows.

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |  Throughput |    ms/iter | vs best |
| ----------------- | ----------: | ---------: | ------- |
| JavaScript        | 7.68 M px/s | 102.452 ms | 1.00x   |
| **Wado** (no-pgo) | 7.61 M px/s | 103.396 ms | 1.01x   |
| **Wado**          | 7.60 M px/s | 103.431 ms | 1.01x   |
| C                 | 7.56 M px/s | 104.031 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation    |      Throughput |  ms/iter | vs best |
| ----------------- | --------------: | -------: | ------- |
| C                 |   1.04 G nums/s | 1.924 ms | 1.00x   |
| **Wado**          | 567.76 M nums/s | 3.522 ms | 1.83x   |
| JavaScript        | 553.95 M nums/s | 3.610 ms | 1.88x   |
| **Wado** (no-pgo) | 433.24 M nums/s | 4.616 ms | 2.40x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation    |     Throughput |   ms/iter | vs best |
| ----------------- | -------------: | --------: | ------- |
| **Wado**          | 28.19 M conv/s | 35.472 ms | 1.00x   |
| **Wado** (no-pgo) | 28.01 M conv/s | 35.705 ms | 1.01x   |
| Rust (core::fmt)  | 19.58 M conv/s | 51.079 ms | 1.44x   |
| C (printf)        | 11.57 M conv/s | 86.403 ms | 2.44x   |

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

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_json)            |  2.26 GB/s | 0.279 ms | 1.00x   |
| JavaScript (JSON)            |  1.60 GB/s | 0.395 ms | 1.42x   |
| **Wado** (core:json)         |  1.19 GB/s | 0.529 ms | 1.90x   |
| **Wado** (core:json, no-pgo) |  1.15 GB/s | 0.551 ms | 1.97x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 592.99 MB/s | 1.065 ms | 1.00x   |
| JavaScript (JSON)            | 582.83 MB/s | 1.084 ms | 1.02x   |
| **Wado** (core:json)         | 340.00 MB/s | 1.857 ms | 1.74x   |
| **Wado** (core:json, no-pgo) | 329.96 MB/s | 1.913 ms | 1.80x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  2.32 GB/s | 0.271 ms | 1.00x   |
| Rust (serde_cbor)            |  2.32 GB/s | 0.272 ms | 1.00x   |
| **Wado** (core:cbor, no-pgo) |  1.78 GB/s | 0.355 ms | 1.31x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            | 901.48 MB/s | 0.701 ms | 1.00x   |
| **Wado** (core:cbor)         | 660.59 MB/s | 0.955 ms | 1.36x   |
| **Wado** (core:cbor, no-pgo) | 608.27 MB/s | 1.038 ms | 1.48x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 942.14 MB/s | 2.389 ms | 1.00x   |
| JavaScript (JSON)            | 569.48 MB/s | 3.953 ms | 1.65x   |
| **Wado** (core:json, no-pgo) | 373.62 MB/s | 6.024 ms | 2.52x   |
| **Wado** (core:json)         | 359.51 MB/s | 6.261 ms | 2.62x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 495.08 MB/s | 4.547 ms | 1.00x   |
| JavaScript (JSON)            | 353.35 MB/s | 6.371 ms | 1.40x   |
| **Wado** (core:json)         | 298.62 MB/s | 7.538 ms | 1.66x   |
| **Wado** (core:json, no-pgo) | 258.23 MB/s | 8.717 ms | 1.92x   |

CBOR serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   2.59 GB/s | 0.870 ms | 1.00x   |
| **Wado** (core:cbor)         |   1.17 GB/s | 1.921 ms | 2.21x   |
| **Wado** (core:cbor, no-pgo) | 936.71 MB/s | 2.403 ms | 2.76x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   1.24 GB/s | 1.812 ms | 1.00x   |
| **Wado** (core:cbor)         | 751.39 MB/s | 2.995 ms | 1.65x   |
| **Wado** (core:cbor, no-pgo) | 567.37 MB/s | 3.967 ms | 2.19x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_json)            |  4.12 GB/s | 0.419 ms | 1.00x   |
| **Wado** (core:json)         |  3.05 GB/s | 0.566 ms | 1.35x   |
| **Wado** (core:json, no-pgo) |  2.34 GB/s | 0.736 ms | 1.76x   |
| JavaScript (JSON)            |  1.42 GB/s | 1.213 ms | 2.89x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 986.85 MB/s | 1.750 ms | 1.00x   |
| JavaScript (JSON)            | 766.92 MB/s | 2.252 ms | 1.29x   |
| **Wado** (core:json)         | 531.62 MB/s | 3.248 ms | 1.86x   |
| **Wado** (core:json, no-pgo) | 507.83 MB/s | 3.401 ms | 1.94x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  4.25 GB/s | 0.406 ms | 1.00x   |
| Rust (serde_cbor)            |  3.75 GB/s | 0.461 ms | 1.14x   |
| **Wado** (core:cbor, no-pgo) |  2.70 GB/s | 0.638 ms | 1.57x   |

CBOR deserialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.64 GB/s | 0.654 ms | 1.00x   |
| **Wado** (core:cbor)         |  1.30 GB/s | 1.332 ms | 2.04x   |
| **Wado** (core:cbor, no-pgo) |  1.13 GB/s | 1.525 ms | 2.33x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               | 322.13 MB/s | 1.960 ms | 1.00x   |
| JavaScript (node:zlib)       | 201.31 MB/s | 3.137 ms | 1.60x   |
| **Wado** (core:zlib)         | 130.38 MB/s | 4.843 ms | 2.47x   |
| C (zlib 1.3.1, Wasm)         | 129.51 MB/s | 4.876 ms | 2.49x   |
| **Wado** (core:zlib, no-pgo) | 124.01 MB/s | 5.092 ms | 2.60x   |

Decompress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               |   3.08 GB/s | 0.205 ms | 1.00x   |
| JavaScript (node:zlib)       |   2.10 GB/s | 0.300 ms | 1.46x   |
| C (zlib 1.3.1, Wasm)         | 844.17 MB/s | 0.748 ms | 3.65x   |
| **Wado** (core:zlib)         | 692.34 MB/s | 0.912 ms | 4.45x   |
| **Wado** (core:zlib, no-pgo) | 646.11 MB/s | 0.977 ms | 4.77x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation          | Throughput |    ms/iter | vs best |
| ----------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)         | 22.24 MB/s |   0.599 ms | 1.00x   |
| **Wado** (Gale, no-pgo) | 22.02 MB/s |   0.604 ms | 1.01x   |
| Rust (sqlparser-rs)     | 12.72 MB/s |   1.048 ms | 1.75x   |
| Java (ANTLR4)           |  0.11 MB/s | 126.538 ms | 211.25x |

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
| **Gale** (Wado, no-pgo)       | 15.09 MB/s | 0.882 ms | 1.00x   |
| **Gale** (Wado)               | 15.05 MB/s | 0.884 ms | 1.00x   |
| Prism.js                      | 11.90 MB/s | 1.119 ms | 1.27x   |
| Lezer (CodeMirror)            |  4.81 MB/s | 2.769 ms | 3.14x   |
| tree-sitter (Rust native)     |  4.79 MB/s | 2.783 ms | 3.16x   |
| tree-sitter (web-tree-sitter) |  2.81 MB/s | 4.748 ms | 5.38x   |

The Gale highlighter's parser is generated with `atn: "never"`, as in SQL Parse.

Every highlighter parses the corpus without errors: a highlighter that gives up
on a region skips the work of colouring it, so the constructs two of them
mishandled are written another way at the same token count.

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (42786 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation          |  Throughput |   ms/iter | vs best |
| ----------------------- | ----------: | --------: | ------- |
| Java (ANTLR4)           | 973.47 KB/s | 43.952 ms | 1.00x   |
| **Wado** (Gale)         | 460.66 KB/s | 92.879 ms | 2.11x   |
| **Wado** (Gale, no-pgo) | 459.66 KB/s | 93.080 ms | 2.12x   |

Every row runs in-process and warm, emitting a parser and no listeners: Gale a
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
gets the same worker count and the same pinned cores. The Wado column applies
the recorded profile, and these tables have no `no-pgo` column.

One worker — a 1-core container scaled out horizontally:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     143,933 |                  132,076 |                    63,257 |                54,753 |
| `GET /user/lookup/username/hey` |     139,986 |                  127,247 |                    59,431 |                52,258 |
| `POST /event/abcd1234/comment`  |     139,289 |                  127,838 |                    48,944 |                52,224 |
| `GET /static/index.html`        |     140,257 |                  125,760 |                    58,058 |                52,758 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     484,472 |                  470,291 |                   249,006 |               190,155 |
| `GET /user/lookup/username/hey` |     476,454 |                  444,923 |                   236,931 |               180,071 |
| `POST /event/abcd1234/comment`  |     477,407 |                  449,021 |                   197,642 |               180,412 |
| `GET /static/index.html`        |     478,171 |                  445,824 |                   227,792 |               182,297 |

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

## Profile-Guided Optimization

A Wado build applies the profile each benchmark's `#[workload]` recorded into
its `pgo/`. These tables compare that build with the two the flags give.
`-f no-pgo` ignores the profile and keeps the source's `cold_path()` markers;
`-f no-cold-paths` drops every marker but still prices each call site by the
profile. `+` means the PGO build is faster.

Fuel at `-O2`, Wado 2026-11-10. Fuel does not depend on the host, so one run
per build settles it:

```sh
wado test -O2 --allocator freelist --report-fuel --test-name fuel benchmark                 # PGO
wado test -O2 --allocator freelist --report-fuel --test-name fuel -f no-pgo benchmark
wado test -O2 --allocator freelist --report-fuel --test-name fuel -f no-cold-paths benchmark
```

| Region                   | `no-cold-paths` |      `no-pgo` |           PGO | PGO vs `no-pgo` | PGO vs `no-cold-paths` |
| ------------------------ | --------------: | ------------: | ------------: | --------------: | ---------------------: |
| `codec/zlib/compress`    |     157,215,589 |   158,770,338 |   157,111,993 |           +1.1% |                  +0.1% |
| `codec/zlib/decompress`  |      29,755,225 |    30,744,196 |    29,662,230 |           +3.6% |                  +0.3% |
| `compute/fts`            |   1,359,789,394 | 1,420,789,480 | 1,363,789,589 |           +4.2% |                  -0.3% |
| `compute/mandelbrot`     |   3,905,984,584 | 3,905,984,584 | 3,905,984,584 |           +0.0% |                  +0.0% |
| `compute/sieve`          |     147,908,811 |   147,908,767 |   147,908,767 |           +0.0% |                  +0.0% |
| `gale/generate`          |   1,304,286,141 | 1,319,605,797 | 1,299,444,940 |           +1.6% |                  +0.4% |
| `gale/sqlite/highlight`  |      21,036,981 |    20,372,459 |    20,371,549 |           +0.0% |                  +3.3% |
| `gale/sqlite/parse`      |      13,988,313 |    13,406,880 |    13,405,970 |           +0.0% |                  +4.3% |
| `ml/microgpt/infer`      |   1,085,324,444 | 1,136,843,797 | 1,099,950,210 |           +3.4% |                  -1.3% |
| `ml/microgpt/train`      |   1,354,492,067 | 1,381,959,745 | 1,360,562,514 |           +1.6% |                  -0.4% |
| `serde/cbor/canada/de`   |      96,750,655 |   117,544,275 |    96,448,840 |          +21.9% |                  +0.3% |
| `serde/cbor/canada/ser`  |      84,118,672 |    87,639,967 |    83,672,688 |           +4.7% |                  +0.5% |
| `serde/cbor/catalog/de`  |      40,409,797 |    44,761,155 |    40,167,563 |          +11.4% |                  +0.6% |
| `serde/cbor/catalog/ser` |      13,884,832 |    16,730,776 |    13,656,265 |          +22.5% |                  +1.7% |
| `serde/cbor/twitter/de`  |      28,049,468 |    29,031,202 |    27,953,020 |           +3.9% |                  +0.3% |
| `serde/cbor/twitter/ser` |       7,273,990 |     8,584,629 |     7,276,737 |          +18.0% |                  -0.0% |
| `serde/json/canada/de`   |     281,172,947 |   320,140,952 |   282,488,380 |          +13.3% |                  -0.5% |
| `serde/json/canada/ser`  |     257,556,205 |   255,510,421 |   255,159,698 |           +0.1% |                  +0.9% |
| `serde/json/catalog/de`  |     118,186,539 |   121,276,423 |   118,382,020 |           +2.4% |                  -0.2% |
| `serde/json/catalog/ser` |      21,268,351 |    22,542,724 |    21,459,975 |           +5.0% |                  -0.9% |
| `serde/json/twitter/de`  |      69,577,133 |    70,405,409 |    69,673,759 |           +1.1% |                  -0.1% |
| `serde/json/twitter/ser` |      17,156,612 |    17,787,377 |    17,445,375 |           +2.0% |                  -1.7% |

Wall clock, best of three, with the builds alternated between rounds. The PGO
and `no-pgo` figures are the tables above, from `mise run benchmark-all`.
`no-cold-paths` is in ms/iter, from
`WADO_BENCH_FLAGS="-f no-cold-paths" mise run -C benchmark all-wado`. `gale_html`
runs neither of those, so its figures come from
`mise run -C benchmark gale-highlight-html` under the same flags. A comparison
reads `noise` where the two builds' ranges overlap, as `ab.ts` judges:

| Benchmark                      | `no-cold-paths` | PGO vs `no-pgo` | PGO vs `no-cold-paths` |
| ------------------------------ | --------------: | --------------: | ---------------------: |
| microgpt / Wado / train        |          93.646 |           +2.9% |                  +0.8% |
| microgpt / Wado / infer        |          60.146 |          +13.6% |                  noise |
| mandelbrot / Wado              |         103.416 |           noise |                  noise |
| sieve / Wado                   |           3.553 |          +23.7% |                  +0.9% |
| fts / Wado                     |          35.129 |           +0.7% |                  -1.0% |
| json_twitter / Wado / ser      |           0.532 |           +4.0% |                  noise |
| json_twitter / Wado / de       |           1.936 |           +2.9% |                  +4.1% |
| cbor_twitter / Wado / ser      |           0.294 |          +23.7% |                  +7.8% |
| cbor_twitter / Wado / de       |           0.984 |           +8.0% |                  +2.9% |
| cbor_canada / Wado / ser       |           1.957 |          +20.1% |                  +1.8% |
| cbor_canada / Wado / de        |           3.108 |          +24.5% |                  +3.6% |
| cbor_catalog / Wado / ser      |           0.434 |          +36.4% |                  +6.5% |
| cbor_catalog / Wado / de       |           1.313 |          +12.7% |                  -1.4% |
| json_canada / Wado / ser       |           6.282 |           -3.9% |                  noise |
| json_canada / Wado / de        |           7.774 |          +13.5% |                  +3.0% |
| json_catalog / Wado / ser      |           0.644 |          +23.1% |                 +12.1% |
| json_catalog / Wado / de       |           3.125 |           +4.5% |                  -3.9% |
| zlib_bench / Wado / compress   |           4.719 |           +4.9% |                  -2.6% |
| zlib_bench / Wado / decompress |           0.904 |           +6.7% |                  -0.9% |
| sqlite_parse / Wado            |           0.730 |           noise |                 +17.9% |
| syntax_highlight / Wado        |           1.025 |           -0.2% |                 +13.8% |
| gale_gen / Wado                |          93.753 |           noise |                  +0.9% |
| gale_html / Wado               |          17.476 |           -0.6% |                  +7.6% |

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
mise run benchmark-gale-highlight-html # HTML with embedded CSS/JS
mise run benchmark-gale-gen         # Gale generator over the Rust grammar

# application server
mise run benchmark-http-routing     # HTTP routing (wado serve vs Hono vs Axum)
mise run -C benchmark http-routing-wado # wado serve alone, one worker, GET /user
```

Prerequisites: `cc` and `cargo` (system); `node`, `bun` and `java` (managed by
`mise install`). The ANTLR4 reference rows (gale-gen, sqlite-parse) fetch the
jar to `~/.cache/gale`, and are skipped if `java` or `javac` is absent.

Each benchmark has its own directory with every implementation side by side.
Each `json_*` directory defines a schema module (`twitter_schema.wado`,
`canada_schema.wado`, `catalog_schema.wado`) that the JSON and CBOR benchmarks
both import, so the two codecs are compared over identical data types.

### GC heap size

`wado run` starts a guest with a 256 MiB GC heap, and `--gc-heap-initial`
changes it. The copying collector splits that into two semi-spaces, so a
program allocates through half of it between collections. A larger heap trades
resident memory for fewer of them, and `wado run --gc-log` shows each one.

Every row runs at the default, which is what a program gets.

gale_gen allocates enough that its time depends on where its collections land,
and its ranking against another build can flip between two nearby heap sizes.

microgpt depends on the size more than any other row. A sample's graph stays
live until the sample ends, so a collection copies as much of that graph as
exists where it lands. A semi-space that holds a whole number of samples lands
every collection at the same point of a sample. The best size moves with what a
sample allocates, so a change that moves that needs a new sweep. A much larger
heap is no cure: the program never leaves its first pass through the heap, and
each new page costs a fault.

At 512 MiB json-catalog, zlib and sqlite-parse are all slower and the rest are
flat, and `wado serve` measures the same at either size.

Left to resize its new space, V8 swings microgpt's JavaScript rows between runs.
That benchmark fixes it with `--min-semi-space-size` and `--max-semi-space-size`
at `MICROGPT_NODE_SEMI_SPACE`, the size that does best across both rows. The
other JavaScript rows run at V8's default. Five runs per size, in ms/iter:

| Semi-space size | Train fastest | Train slowest | Infer fastest | Infer slowest |
| --------------- | ------------: | ------------: | ------------: | ------------: |
| V8's default    |         172.7 |         202.6 |         109.2 |         329.7 |
| 64 MiB          |         159.2 |         168.4 |          98.8 |         107.2 |
| 96 MiB          |         140.5 |         149.1 |          94.6 |         101.9 |
| 128 MiB         |         131.5 |         134.4 |          99.5 |         101.1 |
| 192 MiB         |         128.7 |         131.8 |         108.0 |         109.6 |
| 256 MiB         |         150.7 |         153.0 |         104.3 |         105.8 |

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
