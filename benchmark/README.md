# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-11-24, wasmtime 50.0.1, gcc 15.2.0, wasi-sdk 34.0,
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
| **Wado**          | 2.51 k tokens/s |  90.502 ms | 1.00x   |
| Rust              | 2.46 k tokens/s |  92.234 ms | 1.02x   |
| **Wado** (no-pgo) | 2.40 k tokens/s |  94.590 ms | 1.05x   |
| JavaScript        | 1.71 k tokens/s | 132.745 ms | 1.47x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation    |      Throughput |    ms/iter | vs best |
| ----------------- | --------------: | ---------: | ------- |
| **Wado** (no-pgo) | 4.97 k tokens/s |  77.339 ms | 1.00x   |
| **Wado**          | 4.85 k tokens/s |  79.213 ms | 1.02x   |
| JavaScript        | 3.80 k tokens/s | 100.948 ms | 1.31x   |
| Rust              | 3.01 k tokens/s | 127.538 ms | 1.65x   |

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
| JavaScript        | 7.68 M px/s | 102.442 ms | 1.00x   |
| **Wado**          | 7.61 M px/s | 103.365 ms | 1.01x   |
| **Wado** (no-pgo) | 7.61 M px/s | 103.393 ms | 1.01x   |
| C                 | 7.56 M px/s | 104.021 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation    |      Throughput |  ms/iter | vs best |
| ----------------- | --------------: | -------: | ------- |
| C                 |   1.04 G nums/s | 1.922 ms | 1.00x   |
| JavaScript        | 554.55 M nums/s | 3.607 ms | 1.88x   |
| **Wado** (no-pgo) | 434.62 M nums/s | 4.601 ms | 2.39x   |
| **Wado**          | 433.34 M nums/s | 4.615 ms | 2.40x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation    |     Throughput |   ms/iter | vs best |
| ----------------- | -------------: | --------: | ------- |
| **Wado**          | 28.38 M conv/s | 35.230 ms | 1.00x   |
| **Wado** (no-pgo) | 28.30 M conv/s | 35.331 ms | 1.00x   |
| Rust (core::fmt)  | 19.61 M conv/s | 51.001 ms | 1.45x   |
| C (printf)        | 11.70 M conv/s | 85.478 ms | 2.43x   |

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
| Rust (serde_json)            |  2.26 GB/s | 0.280 ms | 1.00x   |
| JavaScript (JSON)            |  1.60 GB/s | 0.395 ms | 1.41x   |
| **Wado** (core:json)         |  1.27 GB/s | 0.497 ms | 1.78x   |
| **Wado** (core:json, no-pgo) |  1.06 GB/s | 0.594 ms | 2.12x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 605.98 MB/s | 1.042 ms | 1.00x   |
| JavaScript (JSON)            | 581.33 MB/s | 1.086 ms | 1.04x   |
| **Wado** (core:json)         | 339.29 MB/s | 1.861 ms | 1.79x   |
| **Wado** (core:json, no-pgo) | 328.46 MB/s | 1.922 ms | 1.84x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.54 GB/s | 0.249 ms | 1.00x   |
| **Wado** (core:cbor)         |  2.36 GB/s | 0.267 ms | 1.07x   |
| **Wado** (core:cbor, no-pgo) |  1.95 GB/s | 0.323 ms | 1.30x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            | 885.78 MB/s | 0.713 ms | 1.00x   |
| **Wado** (core:cbor)         | 635.83 MB/s | 0.993 ms | 1.39x   |
| **Wado** (core:cbor, no-pgo) | 607.93 MB/s | 1.038 ms | 1.46x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 936.73 MB/s | 2.403 ms | 1.00x   |
| JavaScript (JSON)            | 570.97 MB/s | 3.943 ms | 1.64x   |
| **Wado** (core:json)         | 394.65 MB/s | 5.703 ms | 2.37x   |
| **Wado** (core:json, no-pgo) | 378.58 MB/s | 5.946 ms | 2.47x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 485.24 MB/s | 4.639 ms | 1.00x   |
| JavaScript (JSON)            | 342.21 MB/s | 6.578 ms | 1.42x   |
| **Wado** (core:json)         | 296.99 MB/s | 7.579 ms | 1.63x   |
| **Wado** (core:json, no-pgo) | 268.23 MB/s | 8.392 ms | 1.81x   |

CBOR serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   2.45 GB/s | 0.919 ms | 1.00x   |
| **Wado** (core:cbor)         |   1.21 GB/s | 1.866 ms | 2.03x   |
| **Wado** (core:cbor, no-pgo) | 944.49 MB/s | 2.383 ms | 2.59x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   1.24 GB/s | 1.812 ms | 1.00x   |
| **Wado** (core:cbor)         | 785.43 MB/s | 2.866 ms | 1.58x   |
| **Wado** (core:cbor, no-pgo) | 573.45 MB/s | 3.925 ms | 2.17x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_json)            |  4.16 GB/s | 0.415 ms | 1.00x   |
| **Wado** (core:json)         |  2.99 GB/s | 0.578 ms | 1.39x   |
| **Wado** (core:json, no-pgo) |  2.66 GB/s | 0.649 ms | 1.56x   |
| JavaScript (JSON)            |  1.44 GB/s | 1.199 ms | 2.89x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 982.56 MB/s | 1.758 ms | 1.00x   |
| JavaScript (JSON)            | 759.43 MB/s | 2.274 ms | 1.29x   |
| **Wado** (core:json)         | 534.95 MB/s | 3.228 ms | 1.84x   |
| **Wado** (core:json, no-pgo) | 507.76 MB/s | 3.401 ms | 1.93x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  4.28 GB/s | 0.403 ms | 1.00x   |
| Rust (serde_cbor)            |  3.75 GB/s | 0.461 ms | 1.14x   |
| **Wado** (core:cbor, no-pgo) |  3.30 GB/s | 0.523 ms | 1.30x   |

CBOR deserialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.70 GB/s | 0.639 ms | 1.00x   |
| **Wado** (core:cbor)         |  1.31 GB/s | 1.313 ms | 2.05x   |
| **Wado** (core:cbor, no-pgo) |  1.14 GB/s | 1.509 ms | 2.36x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               | 324.37 MB/s | 1.947 ms | 1.00x   |
| JavaScript (node:zlib)       | 200.04 MB/s | 3.157 ms | 1.62x   |
| C (zlib 1.3.1, Wasm)         | 129.72 MB/s | 4.868 ms | 2.50x   |
| **Wado** (core:zlib)         | 129.05 MB/s | 4.893 ms | 2.51x   |
| **Wado** (core:zlib, no-pgo) | 122.01 MB/s | 5.175 ms | 2.66x   |

Decompress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               |   3.03 GB/s | 0.208 ms | 1.00x   |
| JavaScript (node:zlib)       |   2.01 GB/s | 0.314 ms | 1.51x   |
| **Wado** (core:zlib, no-pgo) | 900.03 MB/s | 0.701 ms | 3.37x   |
| **Wado** (core:zlib)         | 894.65 MB/s | 0.705 ms | 3.39x   |
| C (zlib 1.3.1, Wasm)         | 844.04 MB/s | 0.748 ms | 3.60x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation          | Throughput |    ms/iter | vs best |
| ----------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)         | 20.23 MB/s |   0.658 ms | 1.00x   |
| **Wado** (Gale, no-pgo) | 19.85 MB/s |   0.670 ms | 1.02x   |
| Rust (sqlparser-rs)     | 12.56 MB/s |   1.061 ms | 1.61x   |
| Java (ANTLR4)           |  0.10 MB/s | 127.339 ms | 193.52x |

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
| **Gale** (Wado, no-pgo)       | 14.59 MB/s | 0.912 ms | 1.00x   |
| **Gale** (Wado)               | 14.58 MB/s | 0.913 ms | 1.00x   |
| Prism.js                      | 11.93 MB/s | 1.117 ms | 1.22x   |
| Lezer (CodeMirror)            |  4.87 MB/s | 2.734 ms | 3.00x   |
| tree-sitter (Rust native)     |  4.68 MB/s | 2.844 ms | 3.12x   |
| tree-sitter (web-tree-sitter) |  2.82 MB/s | 4.726 ms | 5.18x   |

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
| Java (ANTLR4)           | 977.18 KB/s | 44.021 ms | 1.00x   |
| **Wado** (Gale)         | 453.06 KB/s | 94.944 ms | 2.16x   |
| **Wado** (Gale, no-pgo) | 439.63 KB/s | 97.846 ms | 2.22x   |

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
| `GET /user`                     |     141,942 |                  135,851 |                    68,213 |                54,734 |
| `GET /user/lookup/username/hey` |     136,528 |                  129,460 |                    61,770 |                52,027 |
| `POST /event/abcd1234/comment`  |     137,246 |                  130,482 |                    52,785 |                52,206 |
| `GET /static/index.html`        |     137,478 |                  128,997 |                    63,097 |                52,539 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     413,446 |                  370,945 |                   229,096 |               182,779 |
| `GET /user/lookup/username/hey` |     389,897 |                  420,070 |                   218,959 |               172,682 |
| `POST /event/abcd1234/comment`  |     388,360 |                  425,676 |                   182,379 |               173,992 |
| `GET /static/index.html`        |     391,150 |                  419,641 |                   211,629 |               182,436 |

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
| microgpt / Wado / train        |           +4.5% |
| microgpt / Wado / infer        |           -2.4% |
| mandelbrot / Wado              |           noise |
| sieve / Wado                   |           noise |
| fts / Wado                     |           +0.3% |
| json_twitter / Wado / ser      |          +19.5% |
| json_twitter / Wado / de       |           +3.3% |
| cbor_twitter / Wado / ser      |          +21.0% |
| cbor_twitter / Wado / de       |           +4.5% |
| cbor_canada / Wado / ser       |          +27.7% |
| cbor_canada / Wado / de        |          +37.0% |
| cbor_catalog / Wado / ser      |          +29.8% |
| cbor_catalog / Wado / de       |          +14.9% |
| json_canada / Wado / ser       |           +4.3% |
| json_canada / Wado / de        |          +10.7% |
| json_catalog / Wado / ser      |          +12.3% |
| json_catalog / Wado / de       |           +5.4% |
| zlib_bench / Wado / compress   |           +5.8% |
| zlib_bench / Wado / decompress |           noise |
| sqlite_parse / Wado            |           +1.8% |
| syntax_highlight / Wado        |           noise |
| gale_gen / Wado                |           +3.1% |

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
