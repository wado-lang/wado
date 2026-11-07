# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-11-06, wasmtime 50.0.1, gcc 15.2.0, wasi-sdk 34.0,
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
| Rust              | 2.45 k tokens/s |  92.545 ms | 1.00x   |
| **Wado**          | 2.40 k tokens/s |  94.396 ms | 1.02x   |
| **Wado** (no-pgo) | 2.36 k tokens/s |  96.216 ms | 1.04x   |
| JavaScript        | 1.72 k tokens/s | 131.868 ms | 1.42x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation    |      Throughput |    ms/iter | vs best |
| ----------------- | --------------: | ---------: | ------- |
| **Wado**          | 6.23 k tokens/s |  61.628 ms | 1.00x   |
| **Wado** (no-pgo) | 5.45 k tokens/s |  70.413 ms | 1.14x   |
| JavaScript        | 3.85 k tokens/s |  99.794 ms | 1.62x   |
| Rust              | 2.98 k tokens/s | 128.937 ms | 2.09x   |

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
| JavaScript        | 7.68 M px/s | 102.449 ms | 1.00x   |
| **Wado**          | 7.61 M px/s | 103.391 ms | 1.01x   |
| **Wado** (no-pgo) | 7.60 M px/s | 103.523 ms | 1.01x   |
| C                 | 7.56 M px/s | 104.010 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation    |      Throughput |  ms/iter | vs best |
| ----------------- | --------------: | -------: | ------- |
| C                 |   1.04 G nums/s | 1.929 ms | 1.00x   |
| JavaScript        | 553.95 M nums/s | 3.610 ms | 1.87x   |
| **Wado**          | 530.88 M nums/s | 3.767 ms | 1.95x   |
| **Wado** (no-pgo) | 412.78 M nums/s | 4.845 ms | 2.51x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation    |     Throughput |   ms/iter | vs best |
| ----------------- | -------------: | --------: | ------- |
| **Wado** (no-pgo) | 27.29 M conv/s | 36.649 ms | 1.00x   |
| **Wado**          | 26.87 M conv/s | 37.223 ms | 1.02x   |
| Rust (core::fmt)  | 19.62 M conv/s | 50.959 ms | 1.39x   |
| C (printf)        | 11.68 M conv/s | 85.621 ms | 2.34x   |

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
| JavaScript (JSON)            |  1.61 GB/s | 0.392 ms | 1.41x   |
| **Wado** (core:json)         |  1.19 GB/s | 0.528 ms | 1.89x   |
| **Wado** (core:json, no-pgo) |  1.14 GB/s | 0.554 ms | 1.99x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 597.23 MB/s | 1.057 ms | 1.00x   |
| JavaScript (JSON)            | 586.44 MB/s | 1.077 ms | 1.02x   |
| **Wado** (core:json)         | 345.75 MB/s | 1.826 ms | 1.73x   |
| **Wado** (core:json, no-pgo) | 330.12 MB/s | 1.912 ms | 1.81x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  2.32 GB/s | 0.271 ms | 1.00x   |
| Rust (serde_cbor)            |  2.32 GB/s | 0.272 ms | 1.00x   |
| **Wado** (core:cbor, no-pgo) |  1.77 GB/s | 0.357 ms | 1.32x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            | 887.37 MB/s | 0.712 ms | 1.00x   |
| **Wado** (core:cbor)         | 645.40 MB/s | 0.978 ms | 1.37x   |
| **Wado** (core:cbor, no-pgo) | 616.92 MB/s | 1.023 ms | 1.44x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 938.89 MB/s | 2.398 ms | 1.00x   |
| JavaScript (JSON)            | 567.44 MB/s | 3.967 ms | 1.65x   |
| **Wado** (core:json, no-pgo) | 375.26 MB/s | 5.998 ms | 2.50x   |
| **Wado** (core:json)         | 369.60 MB/s | 6.090 ms | 2.54x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 495.11 MB/s | 4.547 ms | 1.00x   |
| JavaScript (JSON)            | 346.49 MB/s | 6.497 ms | 1.43x   |
| **Wado** (core:json)         | 300.87 MB/s | 7.481 ms | 1.65x   |
| **Wado** (core:json, no-pgo) | 254.77 MB/s | 8.835 ms | 1.94x   |

CBOR serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   2.59 GB/s | 0.870 ms | 1.00x   |
| **Wado** (core:cbor)         |   1.17 GB/s | 1.918 ms | 2.20x   |
| **Wado** (core:cbor, no-pgo) | 924.27 MB/s | 2.435 ms | 2.80x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   1.25 GB/s | 1.801 ms | 1.00x   |
| **Wado** (core:cbor)         | 717.48 MB/s | 3.137 ms | 1.74x   |
| **Wado** (core:cbor, no-pgo) | 540.41 MB/s | 4.165 ms | 2.31x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_json)            |  4.13 GB/s | 0.418 ms | 1.00x   |
| **Wado** (core:json)         |  3.04 GB/s | 0.567 ms | 1.36x   |
| **Wado** (core:json, no-pgo) |  2.46 GB/s | 0.703 ms | 1.68x   |
| JavaScript (JSON)            |  1.45 GB/s | 1.189 ms | 2.84x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 991.15 MB/s | 1.743 ms | 1.00x   |
| JavaScript (JSON)            | 766.68 MB/s | 2.253 ms | 1.29x   |
| **Wado** (core:json)         | 548.47 MB/s | 3.149 ms | 1.81x   |
| **Wado** (core:json, no-pgo) | 510.88 MB/s | 3.380 ms | 1.94x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  4.19 GB/s | 0.412 ms | 1.00x   |
| Rust (serde_cbor)            |  3.74 GB/s | 0.462 ms | 1.12x   |
| **Wado** (core:cbor, no-pgo) |  2.72 GB/s | 0.635 ms | 1.54x   |

CBOR deserialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.65 GB/s | 0.651 ms | 1.00x   |
| **Wado** (core:cbor)         |  1.30 GB/s | 1.332 ms | 2.05x   |
| **Wado** (core:cbor, no-pgo) |  1.10 GB/s | 1.570 ms | 2.41x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               | 322.01 MB/s | 1.961 ms | 1.00x   |
| JavaScript (node:zlib)       | 200.63 MB/s | 3.148 ms | 1.61x   |
| **Wado** (core:zlib)         | 130.09 MB/s | 4.854 ms | 2.48x   |
| C (zlib 1.3.1, Wasm)         | 129.63 MB/s | 4.872 ms | 2.48x   |
| **Wado** (core:zlib, no-pgo) | 126.51 MB/s | 4.991 ms | 2.55x   |

Decompress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               |   3.09 GB/s | 0.204 ms | 1.00x   |
| JavaScript (node:zlib)       |   2.06 GB/s | 0.307 ms | 1.50x   |
| C (zlib 1.3.1, Wasm)         | 844.32 MB/s | 0.748 ms | 3.67x   |
| **Wado** (core:zlib)         | 690.07 MB/s | 0.915 ms | 4.49x   |
| **Wado** (core:zlib, no-pgo) | 636.76 MB/s | 0.991 ms | 4.86x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation          | Throughput |    ms/iter | vs best |
| ----------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale, no-pgo) | 22.86 MB/s |   0.582 ms | 1.00x   |
| **Wado** (Gale)         | 22.78 MB/s |   0.584 ms | 1.00x   |
| Rust (sqlparser-rs)     | 12.61 MB/s |   1.057 ms | 1.82x   |
| Java (ANTLR4)           |  0.10 MB/s | 127.067 ms | 218.33x |

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
| **Gale** (Wado, no-pgo)       | 15.54 MB/s | 0.857 ms | 1.00x   |
| **Gale** (Wado)               | 15.44 MB/s | 0.862 ms | 1.01x   |
| Prism.js                      | 11.74 MB/s | 1.134 ms | 1.32x   |
| Lezer (CodeMirror)            |  4.89 MB/s | 2.723 ms | 3.18x   |
| tree-sitter (Rust native)     |  4.73 MB/s | 2.817 ms | 3.29x   |
| tree-sitter (web-tree-sitter) |  2.78 MB/s | 4.793 ms | 5.59x   |

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
| Java (ANTLR4)           | 969.65 KB/s | 44.125 ms | 1.00x   |
| **Wado** (Gale, no-pgo) | 559.99 KB/s | 76.404 ms | 1.73x   |
| **Wado** (Gale)         | 559.25 KB/s | 76.505 ms | 1.73x   |

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
| `GET /user`                     |     141,936 |                  132,092 |                    68,094 |                55,108 |
| `GET /user/lookup/username/hey` |     137,618 |                  124,193 |                    63,705 |                52,608 |
| `POST /event/abcd1234/comment`  |     138,220 |                  125,611 |                    53,264 |                52,754 |
| `GET /static/index.html`        |     137,829 |                  123,806 |                    63,889 |                53,208 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     473,458 |                  466,767 |                   245,985 |               188,471 |
| `GET /user/lookup/username/hey` |     467,398 |                  439,041 |                   229,866 |               179,941 |
| `POST /event/abcd1234/comment`  |     465,534 |                  442,053 |                   192,498 |               180,473 |
| `GET /static/index.html`        |     468,286 |                  439,969 |                   223,994 |               181,710 |

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

Fuel at `-O2`, Wado 2026-11-06. Fuel does not depend on the host, so one run
per build settles it:

```sh
wado test -O2 --allocator freelist --report-fuel --test-name fuel benchmark                 # PGO
wado test -O2 --allocator freelist --report-fuel --test-name fuel -f no-pgo benchmark
wado test -O2 --allocator freelist --report-fuel --test-name fuel -f no-cold-paths benchmark
```

| Region                   | `no-cold-paths` |      `no-pgo` |           PGO | PGO vs `no-pgo` | PGO vs `no-cold-paths` |
| ------------------------ | --------------: | ------------: | ------------: | --------------: | ---------------------: |
| `codec/zlib/compress`    |     157,307,600 |   158,867,637 |   157,209,292 |           +1.1% |                  +0.1% |
| `codec/zlib/decompress`  |      29,761,779 |    30,750,750 |    29,668,784 |           +3.6% |                  +0.3% |
| `compute/fts`            |   1,446,789,394 | 1,507,789,480 | 1,450,789,589 |           +3.9% |                  -0.3% |
| `compute/mandelbrot`     |   3,905,984,584 | 3,905,984,584 | 3,905,984,584 |           +0.0% |                  +0.0% |
| `compute/sieve`          |     241,186,150 |   241,186,106 |   241,186,106 |           +0.0% |                  +0.0% |
| `gale/generate`          |   1,158,448,170 | 1,169,236,082 | 1,155,905,334 |           +1.2% |                  +0.2% |
| `gale/sqlite/highlight`  |      20,792,394 |    20,092,304 |    20,091,394 |           +0.0% |                  +3.5% |
| `gale/sqlite/parse`      |      13,642,359 |    13,025,278 |    13,024,368 |           +0.0% |                  +4.7% |
| `ml/microgpt/infer`      |   1,085,601,951 | 1,137,122,345 | 1,100,228,758 |           +3.4% |                  -1.3% |
| `ml/microgpt/train`      |   1,410,116,362 | 1,437,585,145 | 1,416,187,914 |           +1.5% |                  -0.4% |
| `serde/cbor/canada/de`   |     106,534,797 |   127,216,330 |   106,173,270 |          +19.8% |                  +0.3% |
| `serde/cbor/canada/ser`  |      84,118,672 |    87,639,967 |    83,672,107 |           +4.7% |                  +0.5% |
| `serde/cbor/catalog/de`  |      41,328,252 |    45,667,536 |    41,089,804 |          +11.1% |                  +0.6% |
| `serde/cbor/catalog/ser` |      13,884,832 |    16,730,776 |    13,656,265 |          +22.5% |                  +1.7% |
| `serde/cbor/twitter/de`  |      28,158,920 |    29,088,840 |    28,063,084 |           +3.7% |                  +0.3% |
| `serde/cbor/twitter/ser` |       7,273,990 |     8,584,629 |     7,274,442 |          +18.0% |                  -0.0% |
| `serde/json/canada/de`   |     281,176,456 |   320,144,492 |   282,491,920 |          +13.3% |                  -0.5% |
| `serde/json/canada/ser`  |     257,556,159 |   255,510,421 |   255,159,712 |           +0.1% |                  +0.9% |
| `serde/json/catalog/de`  |     118,269,856 |   121,359,772 |   118,465,413 |           +2.4% |                  -0.2% |
| `serde/json/catalog/ser` |      21,268,351 |    22,542,724 |    21,459,989 |           +5.0% |                  -0.9% |
| `serde/json/twitter/de`  |      69,577,133 |    70,405,445 |    69,709,403 |           +1.0% |                  -0.2% |
| `serde/json/twitter/ser` |      17,156,612 |    17,787,377 |    17,445,389 |           +2.0% |                  -1.7% |

Wall clock, best of three, with the builds alternated between rounds. The PGO
and `no-pgo` figures are the tables above, from `mise run benchmark-all`.
`no-cold-paths` is in ms/iter, from
`WADO_BENCH_FLAGS="-f no-cold-paths" mise run -C benchmark all-wado`. `gale_html`
is `package-gale-highlight-html/benchmark/gale_html.wado`, run under each flag. A
comparison reads `noise` where the two builds' ranges overlap, as `ab.ts` judges:

| Benchmark                      | `no-cold-paths` | PGO vs `no-pgo` | PGO vs `no-cold-paths` |
| ------------------------------ | --------------: | --------------: | ---------------------: |
| microgpt / Wado / train        |          95.288 |           +1.9% |                  +0.9% |
| microgpt / Wado / infer        |          60.575 |          +14.3% |                  -1.7% |
| mandelbrot / Wado              |         103.458 |           +0.1% |                  +0.1% |
| sieve / Wado                   |           3.760 |          +28.6% |                  noise |
| fts / Wado                     |          37.275 |           -1.5% |                  noise |
| json_twitter / Wado / ser      |           0.537 |           +4.9% |                  +1.7% |
| json_twitter / Wado / de       |           1.888 |           +4.7% |                  +3.4% |
| cbor_twitter / Wado / ser      |           0.291 |          +31.7% |                  +7.4% |
| cbor_twitter / Wado / de       |           0.991 |           +4.6% |                  noise |
| cbor_canada / Wado / ser       |           1.931 |          +27.0% |                  +0.7% |
| cbor_canada / Wado / de        |           3.330 |          +32.8% |                  +6.2% |
| cbor_catalog / Wado / ser      |           0.436 |          +54.1% |                  +5.8% |
| cbor_catalog / Wado / de       |           1.333 |          +17.9% |                  noise |
| json_canada / Wado / ser       |           6.303 |           -1.5% |                  +3.5% |
| json_canada / Wado / de        |           7.983 |          +18.1% |                  +6.7% |
| json_catalog / Wado / ser      |           0.641 |          +24.0% |                 +13.1% |
| json_catalog / Wado / de       |           3.164 |           +7.3% |                  +0.5% |
| zlib_bench / Wado / compress   |           4.756 |           +2.8% |                  -2.0% |
| zlib_bench / Wado / decompress |           0.912 |           +8.3% |                  -0.3% |
| sqlite_parse / Wado            |           0.709 |           noise |                 +21.4% |
| syntax_highlight / Wado        |           1.006 |           noise |                 +16.7% |
| gale_gen / Wado                |          77.157 |           noise |                  +0.9% |
| gale_html / Wado               |           9.169 |           -0.6% |                 +10.6% |

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
