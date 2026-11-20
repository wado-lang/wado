# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-11-20, wasmtime 50.0.1, gcc 15.2.0, wasi-sdk 34.0,
rustc 1.100.0, Node.js v26.13.0, Bun 1.4.4, Ubuntu 26.04 x86_64 (Linux 7.0).

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
| **Wado**          | 2.53 k tokens/s |  89.813 ms | 1.00x   |
| Rust              | 2.43 k tokens/s |  93.396 ms | 1.04x   |
| **Wado** (no-pgo) | 2.42 k tokens/s |  93.757 ms | 1.04x   |
| JavaScript        | 1.69 k tokens/s | 134.617 ms | 1.50x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation    |      Throughput |    ms/iter | vs best |
| ----------------- | --------------: | ---------: | ------- |
| **Wado** (no-pgo) | 4.94 k tokens/s |  77.790 ms | 1.00x   |
| **Wado**          | 4.83 k tokens/s |  79.550 ms | 1.02x   |
| JavaScript        | 3.90 k tokens/s |  98.549 ms | 1.27x   |
| Rust              | 2.96 k tokens/s | 129.949 ms | 1.67x   |

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
| JavaScript        | 7.67 M px/s | 102.467 ms | 1.00x   |
| **Wado**          | 7.60 M px/s | 103.414 ms | 1.01x   |
| **Wado** (no-pgo) | 7.60 M px/s | 103.426 ms | 1.01x   |
| C                 | 7.56 M px/s | 104.018 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation    |      Throughput |  ms/iter | vs best |
| ----------------- | --------------: | -------: | ------- |
| C                 |   1.04 G nums/s | 1.918 ms | 1.00x   |
| JavaScript        | 554.72 M nums/s | 3.605 ms | 1.88x   |
| **Wado**          | 433.42 M nums/s | 4.614 ms | 2.41x   |
| **Wado** (no-pgo) | 432.54 M nums/s | 4.623 ms | 2.41x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation    |     Throughput |   ms/iter | vs best |
| ----------------- | -------------: | --------: | ------- |
| **Wado** (no-pgo) | 28.63 M conv/s | 34.924 ms | 1.00x   |
| **Wado**          | 28.36 M conv/s | 35.260 ms | 1.01x   |
| Rust (core::fmt)  | 19.07 M conv/s | 52.440 ms | 1.50x   |
| C (printf)        | 11.69 M conv/s | 85.523 ms | 2.45x   |

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
| Rust (serde_json)            |  2.25 GB/s | 0.280 ms | 1.00x   |
| JavaScript (JSON)            |  1.60 GB/s | 0.395 ms | 1.41x   |
| **Wado** (core:json)         |  1.19 GB/s | 0.528 ms | 1.89x   |
| **Wado** (core:json, no-pgo) |  1.05 GB/s | 0.599 ms | 2.14x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 609.52 MB/s | 1.036 ms | 1.00x   |
| JavaScript (JSON)            | 583.53 MB/s | 1.082 ms | 1.04x   |
| **Wado** (core:json)         | 338.50 MB/s | 1.865 ms | 1.80x   |
| **Wado** (core:json, no-pgo) | 327.04 MB/s | 1.930 ms | 1.86x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.53 GB/s | 0.249 ms | 1.00x   |
| **Wado** (core:cbor)         |  2.36 GB/s | 0.267 ms | 1.07x   |
| **Wado** (core:cbor, no-pgo) |  1.96 GB/s | 0.322 ms | 1.29x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            | 882.72 MB/s | 0.715 ms | 1.00x   |
| **Wado** (core:cbor)         | 647.43 MB/s | 0.975 ms | 1.36x   |
| **Wado** (core:cbor, no-pgo) | 619.11 MB/s | 1.020 ms | 1.43x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 937.76 MB/s | 2.400 ms | 1.00x   |
| JavaScript (JSON)            | 571.31 MB/s | 3.940 ms | 1.64x   |
| **Wado** (core:json)         | 392.82 MB/s | 5.730 ms | 2.39x   |
| **Wado** (core:json, no-pgo) | 376.03 MB/s | 5.986 ms | 2.49x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 492.38 MB/s | 4.572 ms | 1.00x   |
| JavaScript (JSON)            | 373.73 MB/s | 6.023 ms | 1.32x   |
| **Wado** (core:json)         | 294.71 MB/s | 7.638 ms | 1.67x   |
| **Wado** (core:json, no-pgo) | 271.49 MB/s | 8.291 ms | 1.81x   |

CBOR serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   2.45 GB/s | 0.919 ms | 1.00x   |
| **Wado** (core:cbor)         |   1.20 GB/s | 1.882 ms | 2.05x   |
| **Wado** (core:cbor, no-pgo) | 935.13 MB/s | 2.407 ms | 2.62x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   1.24 GB/s | 1.817 ms | 1.00x   |
| **Wado** (core:cbor)         | 780.16 MB/s | 2.885 ms | 1.59x   |
| **Wado** (core:cbor, no-pgo) | 566.47 MB/s | 3.973 ms | 2.19x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_json)            |  4.15 GB/s | 0.416 ms | 1.00x   |
| **Wado** (core:json)         |  2.93 GB/s | 0.588 ms | 1.41x   |
| **Wado** (core:json, no-pgo) |  2.66 GB/s | 0.649 ms | 1.56x   |
| JavaScript (JSON)            |  1.45 GB/s | 1.188 ms | 2.86x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 975.18 MB/s | 1.771 ms | 1.00x   |
| JavaScript (JSON)            | 769.28 MB/s | 2.245 ms | 1.27x   |
| **Wado** (core:json)         | 556.89 MB/s | 3.101 ms | 1.75x   |
| **Wado** (core:json, no-pgo) | 512.46 MB/s | 3.370 ms | 1.90x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  4.26 GB/s | 0.405 ms | 1.00x   |
| Rust (serde_cbor)            |  3.74 GB/s | 0.462 ms | 1.14x   |
| **Wado** (core:cbor, no-pgo) |  3.31 GB/s | 0.521 ms | 1.29x   |

CBOR deserialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.72 GB/s | 0.635 ms | 1.00x   |
| **Wado** (core:cbor)         |  1.26 GB/s | 1.366 ms | 2.15x   |
| **Wado** (core:cbor, no-pgo) |  1.17 GB/s | 1.479 ms | 2.33x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               | 322.63 MB/s | 1.957 ms | 1.00x   |
| JavaScript (node:zlib)       | 201.03 MB/s | 3.141 ms | 1.61x   |
| **Wado** (core:zlib)         | 131.23 MB/s | 4.812 ms | 2.46x   |
| C (zlib 1.3.1, Wasm)         | 129.90 MB/s | 4.861 ms | 2.48x   |
| **Wado** (core:zlib, no-pgo) | 121.38 MB/s | 5.202 ms | 2.66x   |

Decompress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               |   3.03 GB/s | 0.208 ms | 1.00x   |
| JavaScript (node:zlib)       |   2.08 GB/s | 0.303 ms | 1.46x   |
| **Wado** (core:zlib, no-pgo) | 898.35 MB/s | 0.702 ms | 3.38x   |
| **Wado** (core:zlib)         | 891.86 MB/s | 0.708 ms | 3.40x   |
| C (zlib 1.3.1, Wasm)         | 844.09 MB/s | 0.748 ms | 3.60x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation          | Throughput |    ms/iter | vs best |
| ----------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)         | 20.28 MB/s |   0.656 ms | 1.00x   |
| **Wado** (Gale, no-pgo) | 19.90 MB/s |   0.669 ms | 1.02x   |
| Rust (sqlparser-rs)     | 12.50 MB/s |   1.066 ms | 1.62x   |
| Java (ANTLR4)           |  0.11 MB/s | 126.712 ms | 193.16x |

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
| **Gale** (Wado, no-pgo)       | 14.56 MB/s | 0.915 ms | 1.00x   |
| **Gale** (Wado)               | 14.55 MB/s | 0.915 ms | 1.00x   |
| Prism.js                      | 11.99 MB/s | 1.111 ms | 1.21x   |
| Lezer (CodeMirror)            |  4.86 MB/s | 2.743 ms | 3.00x   |
| tree-sitter (Rust native)     |  4.73 MB/s | 2.817 ms | 3.08x   |
| tree-sitter (web-tree-sitter) |  2.80 MB/s | 4.765 ms | 5.21x   |

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
| Java (ANTLR4)           | 988.28 KB/s | 43.526 ms | 1.00x   |
| **Wado** (Gale)         | 458.18 KB/s | 93.884 ms | 2.16x   |
| **Wado** (Gale, no-pgo) | 450.08 KB/s | 95.574 ms | 2.20x   |

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
| `GET /user`                     |     140,009 |                  130,483 |                    63,541 |                55,151 |
| `GET /user/lookup/username/hey` |     135,948 |                  125,272 |                    59,310 |                52,802 |
| `POST /event/abcd1234/comment`  |     135,271 |                  126,298 |                    51,570 |                52,671 |
| `GET /static/index.html`        |     135,843 |                  125,946 |                    60,398 |                53,061 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     482,461 |                  468,564 |                   245,004 |               189,823 |
| `GET /user/lookup/username/hey` |     476,967 |                  438,834 |                   230,781 |               180,112 |
| `POST /event/abcd1234/comment`  |     470,678 |                  441,290 |                   193,355 |               179,398 |
| `GET /static/index.html`        |     471,703 |                  439,971 |                   225,556 |               181,216 |

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
| microgpt / Wado / train        |           +4.4% |
| microgpt / Wado / infer        |           -2.2% |
| mandelbrot / Wado              |           noise |
| sieve / Wado                   |           noise |
| fts / Wado                     |           -1.0% |
| json_twitter / Wado / ser      |          +13.4% |
| json_twitter / Wado / de       |           +3.5% |
| cbor_twitter / Wado / ser      |          +20.6% |
| cbor_twitter / Wado / de       |           +4.6% |
| cbor_canada / Wado / ser       |          +27.9% |
| cbor_canada / Wado / de        |          +37.7% |
| cbor_catalog / Wado / ser      |          +28.6% |
| cbor_catalog / Wado / de       |           +8.3% |
| json_canada / Wado / ser       |           +4.5% |
| json_canada / Wado / de        |           +8.5% |
| json_catalog / Wado / ser      |          +10.4% |
| json_catalog / Wado / de       |           +8.7% |
| zlib_bench / Wado / compress   |           +8.1% |
| zlib_bench / Wado / decompress |           -0.8% |
| sqlite_parse / Wado            |           +2.0% |
| syntax_highlight / Wado        |           noise |
| gale_gen / Wado                |           +1.8% |

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
