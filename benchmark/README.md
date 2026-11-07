# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-11-06, wasmtime 50.0.1, gcc 15.2.0, wasi-sdk 34.0,
rustc 1.99.1, Node.js v26.13.0, Bun 1.4.4, Ubuntu 26.04 x86_64 (Linux 7.0).

Throughput is work per second (higher is better), with per-iteration time in
parentheses. Native rows are optimized builds (C `gcc -O3`, Rust release, Wado
`-O2` with the benchmark's recorded [profile](#profile-guided-optimization));
JavaScript runs on Node.js. `vs best` is the fastest row's throughput over this
row's (1.00x = fastest). Absolute throughput is machine-dependent, so compare by
`vs best`. Each figure is the best of three runs.

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
| Rust           | 2.45 k tokens/s |  92.518 ms | 1.00x   |
| **Wado**       | 2.40 k tokens/s |  94.628 ms | 1.02x   |
| JavaScript     | 1.27 k tokens/s | 178.169 ms | 1.93x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| **Wado**       | 6.19 k tokens/s |  62.056 ms | 1.00x   |
| JavaScript     | 4.33 k tokens/s |  88.755 ms | 1.43x   |
| Rust           | 2.99 k tokens/s | 128.618 ms | 2.07x   |

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
| JavaScript     | 7.68 M px/s | 102.459 ms | 1.00x   |
| **Wado**       | 7.60 M px/s | 103.413 ms | 1.01x   |
| C              | 7.56 M px/s | 104.025 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              |   1.03 G nums/s | 1.935 ms | 1.00x   |
| JavaScript     | 555.42 M nums/s | 3.601 ms | 1.86x   |
| **Wado**       | 531.57 M nums/s | 3.762 ms | 1.94x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 26.84 M conv/s | 37.254 ms | 1.00x   |
| Rust (core::fmt) | 19.15 M conv/s | 52.215 ms | 1.40x   |
| C (printf)       | 11.66 M conv/s | 85.774 ms | 2.30x   |

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
| Rust (serde_json)    |  2.26 GB/s | 0.280 ms | 1.00x   |
| JavaScript (JSON)    |  1.61 GB/s | 0.393 ms | 1.40x   |
| **Wado** (core:json) |  1.19 GB/s | 0.529 ms | 1.89x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 590.09 MB/s | 1.070 ms | 1.00x   |
| JavaScript (JSON)    | 583.91 MB/s | 1.082 ms | 1.01x   |
| **Wado** (core:json) | 343.40 MB/s | 1.838 ms | 1.72x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor) |  2.33 GB/s | 0.271 ms | 1.00x   |
| Rust (serde_cbor)    |  2.31 GB/s | 0.274 ms | 1.01x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 844.68 MB/s | 0.748 ms | 1.00x   |
| **Wado** (core:cbor) | 642.44 MB/s | 0.982 ms | 1.31x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 940.91 MB/s | 2.392 ms | 1.00x   |
| JavaScript (JSON)    | 569.85 MB/s | 3.950 ms | 1.65x   |
| **Wado** (core:json) | 369.51 MB/s | 6.091 ms | 2.55x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 489.53 MB/s | 4.598 ms | 1.00x   |
| JavaScript (JSON)    | 359.96 MB/s | 6.254 ms | 1.36x   |
| **Wado** (core:json) | 300.28 MB/s | 7.496 ms | 1.63x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.59 GB/s | 0.870 ms | 1.00x   |
| **Wado** (core:cbor) |  1.17 GB/s | 1.918 ms | 2.20x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.24 GB/s | 1.811 ms | 1.00x   |
| **Wado** (core:cbor) | 720.49 MB/s | 3.124 ms | 1.73x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.12 GB/s | 0.419 ms | 1.00x   |
| **Wado** (core:json) |  3.05 GB/s | 0.567 ms | 1.35x   |
| JavaScript (JSON)    |  1.45 GB/s | 1.192 ms | 2.84x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 955.90 MB/s | 1.807 ms | 1.00x   |
| JavaScript (JSON)    | 768.24 MB/s | 2.248 ms | 1.24x   |
| **Wado** (core:json) | 548.30 MB/s | 3.150 ms | 1.74x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor) |  4.20 GB/s | 0.410 ms | 1.00x   |
| Rust (serde_cbor)    |  3.74 GB/s | 0.462 ms | 1.13x   |

CBOR deserialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.66 GB/s | 0.650 ms | 1.00x   |
| **Wado** (core:cbor) |  1.29 GB/s | 1.339 ms | 2.06x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 321.84 MB/s | 1.962 ms | 1.00x   |
| JavaScript (node:zlib) | 202.06 MB/s | 3.125 ms | 1.59x   |
| C (zlib 1.3.1, Wasm)   | 129.60 MB/s | 4.873 ms | 2.48x   |
| **Wado** (core:zlib)   | 130.52 MB/s | 4.838 ms | 2.47x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.09 GB/s | 0.205 ms | 1.00x   |
| JavaScript (node:zlib) |   2.09 GB/s | 0.302 ms | 1.47x   |
| C (zlib 1.3.1, Wasm)   | 844.16 MB/s | 0.748 ms | 3.65x   |
| **Wado** (core:zlib)   | 690.73 MB/s | 0.914 ms | 4.46x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 22.87 MB/s |   0.582 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.77 MB/s |   1.043 ms | 1.79x   |
| Java (ANTLR4)       |  0.11 MB/s | 126.442 ms | 217.25x |

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
| **Gale** (Wado)               | 15.36 MB/s | 0.867 ms | 1.00x   |
| Prism.js                      | 12.02 MB/s | 1.108 ms | 1.28x   |
| Lezer (CodeMirror)            |  4.92 MB/s | 2.709 ms | 3.12x   |
| tree-sitter (Rust native)     |  4.74 MB/s | 2.812 ms | 3.24x   |
| tree-sitter (web-tree-sitter) |  2.79 MB/s | 4.776 ms | 5.51x   |

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

| Implementation                   |  Throughput |   ms/iter | vs best |
| -------------------------------- | ----------: | --------: | ------- |
| Java (ANTLR4)                    | 979.76 KB/s | 43.670 ms | 1.00x   |
| **Wado** (Gale, 1 GiB heap)      | 589.04 KB/s | 72.636 ms | 1.66x   |
| **Wado** (Gale, 256 MiB default) | 558.53 KB/s | 76.604 ms | 1.75x   |

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

Wall clock, ms/iter, best of three, with the builds interleaved in each round
(`WADO_BENCH_FLAGS="-f no-pgo" mise run -C benchmark all-wado`, and so on). The last column
reads `noise` where the two builds' ranges overlap, as `ab.ts` judges:

| Benchmark                      | `no-cold-paths` | `no-pgo` |     PGO | PGO vs `no-pgo` |
| ------------------------------ | --------------: | -------: | ------: | --------------: |
| microgpt / Wado / train        |          95.599 |   96.094 |  94.628 |           +1.5% |
| microgpt / Wado / infer        |          61.236 |   70.017 |  62.056 |          +11.4% |
| mandelbrot / Wado              |         103.472 |  103.393 | 103.413 |           noise |
| sieve / Wado                   |           3.770 |    4.845 |   3.762 |          +22.4% |
| fts / Wado                     |          37.289 |   36.641 |  37.254 |           -1.7% |
| json_twitter / Wado / ser      |           0.536 |    0.554 |   0.529 |           +4.5% |
| json_twitter / Wado / de       |           1.872 |    1.903 |   1.838 |           +3.4% |
| cbor_twitter / Wado / ser      |           0.293 |    0.358 |   0.271 |          +24.3% |
| cbor_twitter / Wado / de       |           0.987 |    1.028 |   0.982 |           +4.5% |
| cbor_canada / Wado / ser       |           1.931 |    2.439 |   1.918 |          +21.4% |
| cbor_canada / Wado / de        |           3.324 |    4.160 |   3.124 |          +24.9% |
| cbor_catalog / Wado / ser      |           0.438 |    0.635 |   0.410 |          +35.4% |
| cbor_catalog / Wado / de       |           1.333 |    1.572 |   1.339 |          +14.8% |
| json_canada / Wado / ser       |           6.494 |    5.993 |   6.091 |           -1.6% |
| json_canada / Wado / de        |           7.901 |    8.828 |   7.496 |          +15.1% |
| json_catalog / Wado / ser      |           0.638 |    0.703 |   0.567 |          +19.3% |
| json_catalog / Wado / de       |           3.159 |    3.378 |   3.150 |           +6.7% |
| zlib_bench / Wado / compress   |           4.702 |    5.000 |   4.838 |           +3.2% |
| zlib_bench / Wado / decompress |           0.910 |    0.991 |   0.914 |           +7.8% |
| sqlite_parse / Wado            |           0.701 |    0.583 |   0.582 |           noise |
| syntax_highlight / Wado        |           1.005 |    0.863 |   0.867 |           -0.5% |
| gale_gen / Wado                |          77.070 |   76.000 |  76.604 |           noise |
| gale_gen / Wado (1 GiB heap)   |          73.751 |   72.085 |  72.636 |           -0.8% |
| gale_html / Wado               |           9.179 |    8.203 |   8.291 |           -1.1% |

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
It runs again at 1 GiB (`GALE_GEN_BIG_HEAP`), as a second row, which shows the
time once collections are rare.

microgpt depends on the size more than any other row. A sample's graph stays
live until the sample ends, so a collection copies as much of that graph as
exists where it lands. A semi-space that holds a whole number of samples lands
every collection at the same point of a sample. The best size moves with what a
sample allocates, so a change that moves that needs a new sweep, and a size that
beats the default gets a second row. A much larger heap is no cure: the program
never leaves its first pass through the heap, and each new page costs a fault.

Every other row takes the default. At 512 MiB json-catalog, zlib and
sqlite-parse are all slower and the rest are flat, and `wado serve` measures
the same at either size.

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
