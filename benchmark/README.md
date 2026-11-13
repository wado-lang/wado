# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-11-13, wasmtime 50.0.1, gcc 15.2.0, wasi-sdk 34.0,
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
| Rust              | 2.45 k tokens/s |  92.732 ms | 1.00x   |
| **Wado**          | 2.43 k tokens/s |  93.380 ms | 1.01x   |
| **Wado** (no-pgo) | 2.38 k tokens/s |  95.508 ms | 1.03x   |
| JavaScript        | 1.69 k tokens/s | 133.953 ms | 1.44x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation    |      Throughput |    ms/iter | vs best |
| ----------------- | --------------: | ---------: | ------- |
| **Wado**          | 6.28 k tokens/s |  61.106 ms | 1.00x   |
| **Wado** (no-pgo) | 5.44 k tokens/s |  70.547 ms | 1.15x   |
| JavaScript        | 3.83 k tokens/s | 100.310 ms | 1.64x   |
| Rust              | 3.00 k tokens/s | 128.124 ms | 2.10x   |

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
| JavaScript        | 7.66 M px/s | 102.627 ms | 1.00x   |
| **Wado** (no-pgo) | 7.60 M px/s | 103.411 ms | 1.01x   |
| **Wado**          | 7.60 M px/s | 103.414 ms | 1.01x   |
| C                 | 7.56 M px/s | 104.062 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation    |      Throughput |  ms/iter | vs best |
| ----------------- | --------------: | -------: | ------- |
| C                 |   1.04 G nums/s | 1.918 ms | 1.00x   |
| **Wado**          | 567.25 M nums/s | 3.525 ms | 1.84x   |
| JavaScript        | 553.60 M nums/s | 3.613 ms | 1.88x   |
| **Wado** (no-pgo) | 433.96 M nums/s | 4.608 ms | 2.40x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation    |     Throughput |   ms/iter | vs best |
| ----------------- | -------------: | --------: | ------- |
| **Wado**          | 28.27 M conv/s | 35.374 ms | 1.00x   |
| **Wado** (no-pgo) | 28.06 M conv/s | 35.632 ms | 1.01x   |
| Rust (core::fmt)  | 19.59 M conv/s | 51.049 ms | 1.44x   |
| C (printf)        | 11.65 M conv/s | 85.832 ms | 2.43x   |

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
| Rust (serde_json)            |  2.25 GB/s | 0.281 ms | 1.00x   |
| JavaScript (JSON)            |  1.64 GB/s | 0.386 ms | 1.37x   |
| **Wado** (core:json)         |  1.19 GB/s | 0.532 ms | 1.89x   |
| **Wado** (core:json, no-pgo) |  1.15 GB/s | 0.550 ms | 1.96x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 609.13 MB/s | 1.037 ms | 1.00x   |
| JavaScript (JSON)            | 583.58 MB/s | 1.082 ms | 1.04x   |
| **Wado** (core:json)         | 343.89 MB/s | 1.836 ms | 1.77x   |
| **Wado** (core:json, no-pgo) | 329.38 MB/s | 1.917 ms | 1.85x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.53 GB/s | 0.249 ms | 1.00x   |
| **Wado** (core:cbor)         |  2.33 GB/s | 0.271 ms | 1.09x   |
| **Wado** (core:cbor, no-pgo) |  1.78 GB/s | 0.355 ms | 1.43x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            | 889.86 MB/s | 0.710 ms | 1.00x   |
| **Wado** (core:cbor)         | 657.35 MB/s | 0.960 ms | 1.35x   |
| **Wado** (core:cbor, no-pgo) | 610.47 MB/s | 1.034 ms | 1.46x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 936.17 MB/s | 2.405 ms | 1.00x   |
| JavaScript (JSON)            | 570.53 MB/s | 3.946 ms | 1.64x   |
| **Wado** (core:json, no-pgo) | 373.67 MB/s | 6.024 ms | 2.50x   |
| **Wado** (core:json)         | 372.94 MB/s | 6.035 ms | 2.51x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 492.92 MB/s | 4.567 ms | 1.00x   |
| JavaScript (JSON)            | 347.46 MB/s | 6.479 ms | 1.42x   |
| **Wado** (core:json)         | 298.24 MB/s | 7.547 ms | 1.65x   |
| **Wado** (core:json, no-pgo) | 261.73 MB/s | 8.600 ms | 1.88x   |

CBOR serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   2.45 GB/s | 0.919 ms | 1.00x   |
| **Wado** (core:cbor)         |   1.17 GB/s | 1.921 ms | 2.09x   |
| **Wado** (core:cbor, no-pgo) | 925.72 MB/s | 2.431 ms | 2.65x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   1.24 GB/s | 1.821 ms | 1.00x   |
| **Wado** (core:cbor)         | 740.54 MB/s | 3.039 ms | 1.67x   |
| **Wado** (core:cbor, no-pgo) | 558.91 MB/s | 4.027 ms | 2.21x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_json)            |  4.12 GB/s | 0.419 ms | 1.00x   |
| **Wado** (core:json)         |  3.04 GB/s | 0.568 ms | 1.36x   |
| **Wado** (core:json, no-pgo) |  2.43 GB/s | 0.710 ms | 1.69x   |
| JavaScript (JSON)            |  1.44 GB/s | 1.197 ms | 2.86x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 976.54 MB/s | 1.769 ms | 1.00x   |
| JavaScript (JSON)            | 768.47 MB/s | 2.248 ms | 1.27x   |
| **Wado** (core:json)         | 534.78 MB/s | 3.229 ms | 1.83x   |
| **Wado** (core:json, no-pgo) | 506.11 MB/s | 3.412 ms | 1.93x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  4.25 GB/s | 0.406 ms | 1.00x   |
| Rust (serde_cbor)            |  3.75 GB/s | 0.460 ms | 1.13x   |
| **Wado** (core:cbor, no-pgo) |  2.63 GB/s | 0.655 ms | 1.61x   |

CBOR deserialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.71 GB/s | 0.637 ms | 1.00x   |
| **Wado** (core:cbor)         |  1.30 GB/s | 1.325 ms | 2.08x   |
| **Wado** (core:cbor, no-pgo) |  1.14 GB/s | 1.517 ms | 2.38x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               | 323.02 MB/s | 1.955 ms | 1.00x   |
| JavaScript (node:zlib)       | 201.06 MB/s | 3.141 ms | 1.61x   |
| **Wado** (core:zlib)         | 129.71 MB/s | 4.868 ms | 2.49x   |
| C (zlib 1.3.1, Wasm)         | 129.53 MB/s | 4.875 ms | 2.49x   |
| **Wado** (core:zlib, no-pgo) | 117.30 MB/s | 5.383 ms | 2.75x   |

Decompress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               |   3.04 GB/s | 0.207 ms | 1.00x   |
| JavaScript (node:zlib)       |   2.09 GB/s | 0.302 ms | 1.46x   |
| **Wado** (core:zlib, no-pgo) | 899.97 MB/s | 0.701 ms | 3.39x   |
| **Wado** (core:zlib)         | 890.38 MB/s | 0.709 ms | 3.43x   |
| C (zlib 1.3.1, Wasm)         | 844.30 MB/s | 0.748 ms | 3.61x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation          | Throughput |    ms/iter | vs best |
| ----------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)         | 20.54 MB/s |   0.648 ms | 1.00x   |
| **Wado** (Gale, no-pgo) | 20.52 MB/s |   0.649 ms | 1.00x   |
| Rust (sqlparser-rs)     | 12.56 MB/s |   1.060 ms | 1.64x   |
| Java (ANTLR4)           |  0.11 MB/s | 126.650 ms | 195.45x |

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
| **Gale** (Wado, no-pgo)       | 14.64 MB/s | 0.910 ms | 1.00x   |
| **Gale** (Wado)               | 14.54 MB/s | 0.916 ms | 1.01x   |
| Prism.js                      | 12.07 MB/s | 1.103 ms | 1.21x   |
| Lezer (CodeMirror)            |  4.79 MB/s | 2.782 ms | 3.06x   |
| tree-sitter (Rust native)     |  4.69 MB/s | 2.839 ms | 3.12x   |
| tree-sitter (web-tree-sitter) |  2.76 MB/s | 4.834 ms | 5.31x   |

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
| Java (ANTLR4)           | 988.42 KB/s | 43.520 ms | 1.00x   |
| **Wado** (Gale)         | 476.10 KB/s | 90.350 ms | 2.08x   |
| **Wado** (Gale, no-pgo) | 475.82 KB/s | 90.403 ms | 2.08x   |

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
| `GET /user`                     |     140,727 |                  132,034 |                    66,628 |                54,299 |
| `GET /user/lookup/username/hey` |     135,111 |                  127,728 |                    63,074 |                51,813 |
| `POST /event/abcd1234/comment`  |     135,468 |                  127,892 |                    53,070 |                51,996 |
| `GET /static/index.html`        |     135,347 |                  127,162 |                    62,631 |                52,282 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     477,459 |                  468,433 |                   249,112 |               191,192 |
| `GET /user/lookup/username/hey` |     474,864 |                  442,544 |                   233,017 |               181,892 |
| `POST /event/abcd1234/comment`  |     477,510 |                  445,028 |                   198,171 |               182,140 |
| `GET /static/index.html`        |     466,383 |                  441,812 |                   232,393 |               183,147 |

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
| microgpt / Wado / train        |           +2.3% |
| microgpt / Wado / infer        |          +15.5% |
| mandelbrot / Wado              |           noise |
| sieve / Wado                   |          +30.7% |
| fts / Wado                     |           +0.7% |
| json_twitter / Wado / ser      |           +3.4% |
| json_twitter / Wado / de       |           +4.4% |
| cbor_twitter / Wado / ser      |          +31.0% |
| cbor_twitter / Wado / de       |           +7.7% |
| cbor_canada / Wado / ser       |          +26.5% |
| cbor_canada / Wado / de        |          +32.5% |
| cbor_catalog / Wado / ser      |          +61.3% |
| cbor_catalog / Wado / de       |          +14.5% |
| json_canada / Wado / ser       |           -0.2% |
| json_canada / Wado / de        |          +14.0% |
| json_catalog / Wado / ser      |          +25.0% |
| json_catalog / Wado / de       |           +5.7% |
| zlib_bench / Wado / compress   |          +10.6% |
| zlib_bench / Wado / decompress |           -1.1% |
| sqlite_parse / Wado            |           noise |
| syntax_highlight / Wado        |           noise |
| gale_gen / Wado                |           noise |

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
