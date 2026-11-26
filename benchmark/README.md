# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-11-26, wasmtime 50.0.1, gcc 15.2.0, wasi-sdk 34.0,
rustc 1.100.0, Node.js v26.13.0, Bun 1.4.5, Ubuntu 26.04 x86_64 (Linux 7.0).

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
| **Wado**          | 2.52 k tokens/s |  89.926 ms | 1.00x   |
| Rust              | 2.46 k tokens/s |  92.192 ms | 1.03x   |
| **Wado** (no-pgo) | 2.39 k tokens/s |  94.814 ms | 1.05x   |
| JavaScript        | 1.70 k tokens/s | 133.421 ms | 1.48x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation    |      Throughput |    ms/iter | vs best |
| ----------------- | --------------: | ---------: | ------- |
| **Wado** (no-pgo) | 4.87 k tokens/s |  78.803 ms | 1.00x   |
| **Wado**          | 4.65 k tokens/s |  82.553 ms | 1.05x   |
| JavaScript        | 3.85 k tokens/s |  99.843 ms | 1.27x   |
| Rust              | 3.01 k tokens/s | 127.534 ms | 1.62x   |

Training spends most of its time in the backward pass, which
sorts the whole graph topologically and then walks every edge again: pointer
chasing over GC objects. Inference never builds that traversal.

Each sample also runs the full 16-position attention window, deeper than any
training step reaches on this corpus: the longest name gives 11 positions and
the mean is 7, and attention cost grows with the square of the position count.

Rust makes a node a `usize` index into a `Vec<Value>`, because a `&mut` into a
growing `Vec` is what the borrow checker forbids. Wado's `Graph::value` returns
that `&mut Value` and a node holds those handles as its children, which is the
shape the Python original has.

All three arms print the same final loss and generate the same sample, so they
are the same computation.

The JavaScript rows run at the best semi-space size found, as
[GC heap size](#gc-heap-size) shows.

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |  Throughput |    ms/iter | vs best |
| ----------------- | ----------: | ---------: | ------- |
| JavaScript        | 7.68 M px/s | 102.462 ms | 1.00x   |
| **Wado**          | 7.60 M px/s | 103.418 ms | 1.01x   |
| **Wado** (no-pgo) | 7.60 M px/s | 103.419 ms | 1.01x   |
| C                 | 7.56 M px/s | 104.019 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation    |      Throughput |  ms/iter | vs best |
| ----------------- | --------------: | -------: | ------- |
| C                 |   1.05 G nums/s | 1.912 ms | 1.00x   |
| JavaScript        | 554.19 M nums/s | 3.609 ms | 1.89x   |
| **Wado**          | 433.30 M nums/s | 4.615 ms | 2.41x   |
| **Wado** (no-pgo) | 432.72 M nums/s | 4.621 ms | 2.42x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation    |     Throughput |   ms/iter | vs best |
| ----------------- | -------------: | --------: | ------- |
| **Wado**          | 27.95 M conv/s | 35.781 ms | 1.00x   |
| **Wado** (no-pgo) | 27.39 M conv/s | 36.505 ms | 1.02x   |
| Rust (core::fmt)  | 19.53 M conv/s | 51.194 ms | 1.43x   |
| C (printf)        | 11.70 M conv/s | 85.496 ms | 2.39x   |

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
| Rust (serde_json)            |  2.27 GB/s | 0.278 ms | 1.00x   |
| JavaScript (JSON)            |  1.58 GB/s | 0.399 ms | 1.44x   |
| **Wado** (core:json)         |  1.24 GB/s | 0.508 ms | 1.83x   |
| **Wado** (core:json, no-pgo) |  1.06 GB/s | 0.597 ms | 2.15x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 612.02 MB/s | 1.032 ms | 1.00x   |
| JavaScript (JSON)            | 580.96 MB/s | 1.087 ms | 1.05x   |
| **Wado** (core:json)         | 342.25 MB/s | 1.845 ms | 1.79x   |
| **Wado** (core:json, no-pgo) | 336.21 MB/s | 1.878 ms | 1.82x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.50 GB/s | 0.253 ms | 1.00x   |
| **Wado** (core:cbor)         |  2.35 GB/s | 0.269 ms | 1.06x   |
| **Wado** (core:cbor, no-pgo) |  1.89 GB/s | 0.334 ms | 1.32x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            | 894.83 MB/s | 0.706 ms | 1.00x   |
| **Wado** (core:cbor)         | 630.37 MB/s | 1.001 ms | 1.42x   |
| **Wado** (core:cbor, no-pgo) | 610.00 MB/s | 1.035 ms | 1.47x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 936.22 MB/s | 2.404 ms | 1.00x   |
| JavaScript (JSON)            | 570.01 MB/s | 3.949 ms | 1.64x   |
| **Wado** (core:json)         | 394.54 MB/s | 5.705 ms | 2.37x   |
| **Wado** (core:json, no-pgo) | 377.31 MB/s | 5.966 ms | 2.48x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 487.78 MB/s | 4.615 ms | 1.00x   |
| JavaScript (JSON)            | 348.43 MB/s | 6.461 ms | 1.40x   |
| **Wado** (core:json)         | 293.76 MB/s | 7.663 ms | 1.66x   |
| **Wado** (core:json, no-pgo) | 272.53 MB/s | 8.259 ms | 1.79x   |

CBOR serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   2.45 GB/s | 0.918 ms | 1.00x   |
| **Wado** (core:cbor)         |   1.19 GB/s | 1.889 ms | 2.06x   |
| **Wado** (core:cbor, no-pgo) | 929.82 MB/s | 2.420 ms | 2.64x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   1.24 GB/s | 1.815 ms | 1.00x   |
| **Wado** (core:cbor)         | 779.74 MB/s | 2.886 ms | 1.59x   |
| **Wado** (core:cbor, no-pgo) | 576.24 MB/s | 3.906 ms | 2.15x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_json)            |  4.18 GB/s | 0.414 ms | 1.00x   |
| **Wado** (core:json)         |  2.98 GB/s | 0.580 ms | 1.40x   |
| **Wado** (core:json, no-pgo) |  2.65 GB/s | 0.651 ms | 1.57x   |
| JavaScript (JSON)            |  1.44 GB/s | 1.203 ms | 2.91x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 978.27 MB/s | 1.766 ms | 1.00x   |
| JavaScript (JSON)            | 763.81 MB/s | 2.261 ms | 1.28x   |
| **Wado** (core:json)         | 552.87 MB/s | 3.124 ms | 1.77x   |
| **Wado** (core:json, no-pgo) | 513.53 MB/s | 3.363 ms | 1.90x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  4.28 GB/s | 0.403 ms | 1.00x   |
| Rust (serde_cbor)            |  3.74 GB/s | 0.462 ms | 1.15x   |
| **Wado** (core:cbor, no-pgo) |  3.28 GB/s | 0.526 ms | 1.31x   |

CBOR deserialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.72 GB/s | 0.635 ms | 1.00x   |
| **Wado** (core:cbor)         |  1.33 GB/s | 1.298 ms | 2.04x   |
| **Wado** (core:cbor, no-pgo) |  1.18 GB/s | 1.460 ms | 2.30x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               | 322.96 MB/s | 1.955 ms | 1.00x   |
| JavaScript (node:zlib)       | 200.98 MB/s | 3.142 ms | 1.61x   |
| **Wado** (core:zlib)         | 131.63 MB/s | 4.797 ms | 2.45x   |
| C (zlib 1.3.1, Wasm)         | 129.62 MB/s | 4.872 ms | 2.49x   |
| **Wado** (core:zlib, no-pgo) | 122.41 MB/s | 5.159 ms | 2.64x   |

Decompress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               |   3.04 GB/s | 0.208 ms | 1.00x   |
| JavaScript (node:zlib)       |   2.11 GB/s | 0.300 ms | 1.44x   |
| **Wado** (core:zlib, no-pgo) | 919.05 MB/s | 0.687 ms | 3.30x   |
| **Wado** (core:zlib)         | 914.38 MB/s | 0.690 ms | 3.32x   |
| C (zlib 1.3.1, Wasm)         | 844.03 MB/s | 0.748 ms | 3.60x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation          | Throughput |    ms/iter | vs best |
| ----------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale, no-pgo) | 20.22 MB/s |   0.658 ms | 1.00x   |
| **Wado** (Gale)         | 20.19 MB/s |   0.659 ms | 1.00x   |
| Rust (sqlparser-rs)     | 12.52 MB/s |   1.064 ms | 1.62x   |
| Java (ANTLR4)           |  0.11 MB/s | 126.701 ms | 192.55x |

The Gale parser is generated with `prediction: "longest"`: every decision is settled by
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
| **Gale** (Wado, no-pgo)       | 14.62 MB/s | 0.911 ms | 1.00x   |
| **Gale** (Wado)               | 14.57 MB/s | 0.914 ms | 1.00x   |
| Prism.js                      | 11.89 MB/s | 1.121 ms | 1.23x   |
| Lezer (CodeMirror)            |  4.79 MB/s | 2.780 ms | 3.05x   |
| tree-sitter (Rust native)     |  4.71 MB/s | 2.829 ms | 3.11x   |
| tree-sitter (web-tree-sitter) |  2.80 MB/s | 4.765 ms | 5.23x   |

The Gale highlighter's parser is generated with `prediction: "longest"`, as in SQL Parse.

Every highlighter parses the corpus without errors: a highlighter that gives up
on a region skips the work of colouring it, so the constructs two of them
mishandled are written another way at the same token count.

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (43016 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation          |  Throughput |   ms/iter | vs best |
| ----------------------- | ----------: | --------: | ------- |
| Java (ANTLR4)           | 986.26 KB/s | 43.615 ms | 1.00x   |
| **Wado** (Gale)         | 484.03 KB/s | 88.870 ms | 2.04x   |
| **Wado** (Gale, no-pgo) | 472.82 KB/s | 90.976 ms | 2.09x   |

Every row runs in-process and warm, emitting a parser and no listeners: Gale a
Wado recursive-descent one from memory, ANTLR4 Java onto disk. Gale generates
with `prediction: "longest"`, so the parser it emits carries no ATN simulator.

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
| `GET /user`                     |     143,152 |                  132,821 |                    65,731 |                55,050 |
| `GET /user/lookup/username/hey` |     139,490 |                  125,967 |                    61,954 |                52,674 |
| `POST /event/abcd1234/comment`  |     138,893 |                  126,877 |                    52,955 |                52,984 |
| `GET /static/index.html`        |     140,037 |                  126,528 |                    62,048 |                53,301 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     480,068 |                  464,187 |                   249,795 |               194,113 |
| `GET /user/lookup/username/hey` |     469,973 |                  437,734 |                   229,760 |               185,673 |
| `POST /event/abcd1234/comment`  |     470,438 |                  440,827 |                   190,344 |               185,685 |
| `GET /static/index.html`        |     468,184 |                  436,176 |                   224,323 |               186,527 |

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
its `pgo/`. The table below compares that build with `-f no-pgo`, which ignores
the profile and keeps the source's `cold_path()` markers. `+` means the PGO
build is faster.

The PGO build's fuel at `-O2` is in [`fuel.json`](./fuel.json), which
`mise run update-baselines` records. Fuel does not depend on the host, so one
run per build settles it, and a flag measures the other:

```sh
wado test -O2 --allocator freelist --report-fuel --test-name fuel benchmark            # PGO
wado test -O2 --allocator freelist --report-fuel --test-name fuel -f no-pgo benchmark
```

Wall clock, best of three: the PGO and `no-pgo` figures are the tables above,
from `mise run benchmark-all`. A comparison reads `noise` where the two builds'
ranges overlap, as `ab.ts` judges:

| Benchmark                      | PGO vs `no-pgo` |
| ------------------------------ | --------------: |
| microgpt / Wado / train        |           +5.4% |
| microgpt / Wado / infer        |           -4.5% |
| mandelbrot / Wado              |           noise |
| sieve / Wado                   |           noise |
| fts / Wado                     |           +2.0% |
| json_twitter / Wado / ser      |          +17.5% |
| json_twitter / Wado / de       |           +1.8% |
| cbor_twitter / Wado / ser      |          +24.2% |
| cbor_twitter / Wado / de       |           +3.4% |
| cbor_canada / Wado / ser       |          +28.1% |
| cbor_canada / Wado / de        |          +35.3% |
| cbor_catalog / Wado / ser      |          +30.5% |
| cbor_catalog / Wado / de       |          +12.5% |
| json_canada / Wado / ser       |           +4.6% |
| json_canada / Wado / de        |           +7.8% |
| json_catalog / Wado / ser      |          +12.2% |
| json_catalog / Wado / de       |           +7.7% |
| zlib_bench / Wado / compress   |           +7.5% |
| zlib_bench / Wado / decompress |           -0.4% |
| sqlite_parse / Wado            |           noise |
| syntax_highlight / Wado        |           -0.3% |
| gale_gen / Wado                |           +2.4% |

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
mise run benchmark-re               # regular expression search (Re vs Node's RegExp)

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
