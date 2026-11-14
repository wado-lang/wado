# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-11-14, wasmtime 50.0.1, gcc 15.2.0, wasi-sdk 34.0,
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
| Rust              | 2.45 k tokens/s |  92.487 ms | 1.00x   |
| **Wado**          | 2.44 k tokens/s |  93.203 ms | 1.01x   |
| **Wado** (no-pgo) | 2.38 k tokens/s |  95.535 ms | 1.03x   |
| JavaScript        | 1.71 k tokens/s | 132.594 ms | 1.43x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation    |      Throughput |    ms/iter | vs best |
| ----------------- | --------------: | ---------: | ------- |
| **Wado**          | 6.23 k tokens/s |  61.615 ms | 1.00x   |
| **Wado** (no-pgo) | 5.41 k tokens/s |  70.949 ms | 1.15x   |
| JavaScript        | 3.89 k tokens/s |  98.654 ms | 1.60x   |
| Rust              | 3.01 k tokens/s | 127.409 ms | 2.07x   |

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
| JavaScript        | 7.68 M px/s | 102.445 ms | 1.00x   |
| **Wado** (no-pgo) | 7.60 M px/s | 103.420 ms | 1.01x   |
| **Wado**          | 7.60 M px/s | 103.439 ms | 1.01x   |
| C                 | 7.56 M px/s | 103.997 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation    |      Throughput |  ms/iter | vs best |
| ----------------- | --------------: | -------: | ------- |
| C                 |   1.03 G nums/s | 1.934 ms | 1.00x   |
| **Wado**          | 566.41 M nums/s | 3.531 ms | 1.83x   |
| JavaScript        | 553.40 M nums/s | 3.614 ms | 1.87x   |
| **Wado** (no-pgo) | 433.41 M nums/s | 4.614 ms | 2.39x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation    |     Throughput |   ms/iter | vs best |
| ----------------- | -------------: | --------: | ------- |
| **Wado**          | 28.18 M conv/s | 35.490 ms | 1.00x   |
| **Wado** (no-pgo) | 28.09 M conv/s | 35.601 ms | 1.00x   |
| Rust (core::fmt)  | 19.62 M conv/s | 50.974 ms | 1.44x   |
| C (printf)        | 11.69 M conv/s | 85.520 ms | 2.41x   |

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
| JavaScript (JSON)            |  1.61 GB/s | 0.393 ms | 1.40x   |
| **Wado** (core:json)         |  1.19 GB/s | 0.529 ms | 1.88x   |
| **Wado** (core:json, no-pgo) |  1.14 GB/s | 0.554 ms | 1.97x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 613.34 MB/s | 1.030 ms | 1.00x   |
| JavaScript (JSON)            | 581.30 MB/s | 1.086 ms | 1.05x   |
| **Wado** (core:json)         | 343.95 MB/s | 1.836 ms | 1.78x   |
| **Wado** (core:json, no-pgo) | 330.96 MB/s | 1.908 ms | 1.85x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.53 GB/s | 0.250 ms | 1.00x   |
| **Wado** (core:cbor)         |  2.36 GB/s | 0.267 ms | 1.07x   |
| **Wado** (core:cbor, no-pgo) |  1.78 GB/s | 0.355 ms | 1.42x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            | 897.24 MB/s | 0.704 ms | 1.00x   |
| **Wado** (core:cbor)         | 656.55 MB/s | 0.961 ms | 1.37x   |
| **Wado** (core:cbor, no-pgo) | 611.13 MB/s | 1.033 ms | 1.47x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 935.77 MB/s | 2.406 ms | 1.00x   |
| JavaScript (JSON)            | 570.48 MB/s | 3.946 ms | 1.64x   |
| **Wado** (core:json, no-pgo) | 374.64 MB/s | 6.008 ms | 2.50x   |
| **Wado** (core:json)         | 372.29 MB/s | 6.046 ms | 2.51x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 489.80 MB/s | 4.596 ms | 1.00x   |
| JavaScript (JSON)            | 346.37 MB/s | 6.499 ms | 1.41x   |
| **Wado** (core:json)         | 299.71 MB/s | 7.510 ms | 1.63x   |
| **Wado** (core:json, no-pgo) | 263.84 MB/s | 8.532 ms | 1.86x   |

CBOR serialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   2.47 GB/s | 0.910 ms | 1.00x   |
| **Wado** (core:cbor)         |   1.18 GB/s | 1.908 ms | 2.10x   |
| **Wado** (core:cbor, no-pgo) | 927.20 MB/s | 2.427 ms | 2.67x   |

CBOR deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)            |   1.24 GB/s | 1.811 ms | 1.00x   |
| **Wado** (core:cbor)         | 742.91 MB/s | 3.030 ms | 1.67x   |
| **Wado** (core:cbor, no-pgo) | 558.04 MB/s | 4.033 ms | 2.23x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_json)            |  4.13 GB/s | 0.419 ms | 1.00x   |
| **Wado** (core:json)         |  3.05 GB/s | 0.566 ms | 1.35x   |
| **Wado** (core:json, no-pgo) |  2.48 GB/s | 0.695 ms | 1.66x   |
| JavaScript (JSON)            |  1.44 GB/s | 1.199 ms | 2.86x   |

JSON deserialize:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (serde_json)            | 977.16 MB/s | 1.768 ms | 1.00x   |
| JavaScript (JSON)            | 774.34 MB/s | 2.231 ms | 1.26x   |
| **Wado** (core:json)         | 535.09 MB/s | 3.227 ms | 1.83x   |
| **Wado** (core:json, no-pgo) | 510.53 MB/s | 3.383 ms | 1.91x   |

CBOR serialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| **Wado** (core:cbor)         |  4.28 GB/s | 0.403 ms | 1.00x   |
| Rust (serde_cbor)            |  3.74 GB/s | 0.461 ms | 1.14x   |
| **Wado** (core:cbor, no-pgo) |  2.72 GB/s | 0.634 ms | 1.57x   |

CBOR deserialize:

| Implementation               | Throughput |  ms/iter | vs best |
| ---------------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)            |  2.70 GB/s | 0.640 ms | 1.00x   |
| **Wado** (core:cbor)         |  1.33 GB/s | 1.302 ms | 2.03x   |
| **Wado** (core:cbor, no-pgo) |  1.13 GB/s | 1.524 ms | 2.38x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               | 322.45 MB/s | 1.959 ms | 1.00x   |
| JavaScript (node:zlib)       | 203.08 MB/s | 3.110 ms | 1.59x   |
| C (zlib 1.3.1, Wasm)         | 129.61 MB/s | 4.872 ms | 2.49x   |
| **Wado** (core:zlib)         | 129.55 MB/s | 4.874 ms | 2.49x   |
| **Wado** (core:zlib, no-pgo) | 118.30 MB/s | 5.338 ms | 2.72x   |

Decompress:

| Implementation               |  Throughput |  ms/iter | vs best |
| ---------------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)               |   3.04 GB/s | 0.208 ms | 1.00x   |
| JavaScript (node:zlib)       |   2.02 GB/s | 0.312 ms | 1.50x   |
| **Wado** (core:zlib, no-pgo) | 897.78 MB/s | 0.703 ms | 3.38x   |
| **Wado** (core:zlib)         | 887.29 MB/s | 0.711 ms | 3.42x   |
| C (zlib 1.3.1, Wasm)         | 844.30 MB/s | 0.748 ms | 3.60x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation          | Throughput |    ms/iter | vs best |
| ----------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale, no-pgo) | 20.61 MB/s |   0.646 ms | 1.00x   |
| **Wado** (Gale)         | 20.54 MB/s |   0.648 ms | 1.00x   |
| Rust (sqlparser-rs)     | 12.54 MB/s |   1.062 ms | 1.64x   |
| Java (ANTLR4)           |  0.10 MB/s | 128.323 ms | 198.64x |

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
| **Gale** (Wado, no-pgo)       | 14.56 MB/s | 0.914 ms | 1.00x   |
| **Gale** (Wado)               | 14.51 MB/s | 0.917 ms | 1.00x   |
| Prism.js                      | 12.03 MB/s | 1.107 ms | 1.21x   |
| Lezer (CodeMirror)            |  4.80 MB/s | 2.773 ms | 3.03x   |
| tree-sitter (Rust native)     |  4.71 MB/s | 2.825 ms | 3.09x   |
| tree-sitter (web-tree-sitter) |  2.77 MB/s | 4.804 ms | 5.26x   |

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
| Java (ANTLR4)           | 986.77 KB/s | 43.593 ms | 1.00x   |
| **Wado** (Gale)         | 479.20 KB/s | 89.766 ms | 2.06x   |
| **Wado** (Gale, no-pgo) | 475.53 KB/s | 90.458 ms | 2.08x   |

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
| `GET /user`                     |     141,439 |                  132,808 |                    66,618 |                54,387 |
| `GET /user/lookup/username/hey` |     137,856 |                  125,729 |                    62,655 |                51,925 |
| `POST /event/abcd1234/comment`  |     137,703 |                  126,023 |                    51,938 |                51,832 |
| `GET /static/index.html`        |     138,617 |                  125,783 |                    57,975 |                52,468 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | JavaScript (Hono on Node) | **Wado** (wado serve) |
| ------------------------------- | ----------: | -----------------------: | ------------------------: | --------------------: |
| `GET /user`                     |     478,240 |                  467,178 |                   252,780 |               189,819 |
| `GET /user/lookup/username/hey` |     476,460 |                  450,115 |                   233,357 |               180,956 |
| `POST /event/abcd1234/comment`  |     476,236 |                  449,897 |                   197,759 |               181,035 |
| `GET /static/index.html`        |     472,838 |                  446,253 |                   228,295 |               182,893 |

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
| microgpt / Wado / train        |           +2.4% |
| microgpt / Wado / infer        |          +13.2% |
| mandelbrot / Wado              |           noise |
| sieve / Wado                   |          +23.5% |
| fts / Wado                     |           +0.3% |
| json_twitter / Wado / ser      |           +4.5% |
| json_twitter / Wado / de       |           +3.8% |
| cbor_twitter / Wado / ser      |          +24.8% |
| cbor_twitter / Wado / de       |           +7.0% |
| cbor_canada / Wado / ser       |           noise |
| cbor_canada / Wado / de        |          +24.9% |
| cbor_catalog / Wado / ser      |          +36.4% |
| cbor_catalog / Wado / de       |          +14.6% |
| json_canada / Wado / ser       |           -0.6% |
| json_canada / Wado / de        |          +12.0% |
| json_catalog / Wado / ser      |          +18.6% |
| json_catalog / Wado / de       |           +4.6% |
| zlib_bench / Wado / compress   |           +8.7% |
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
