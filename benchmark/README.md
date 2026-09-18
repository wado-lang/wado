# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-18, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
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
| Rust           | 2.72 k tokens/s |  83.497 ms | 1.00x   |
| JavaScript     | 1.22 k tokens/s | 186.400 ms | 2.23x   |
| **Wado**       | 985.76 tokens/s | 230.280 ms | 2.76x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| Rust           | 4.24 k tokens/s |  90.635 ms | 1.00x   |
| JavaScript     | 3.52 k tokens/s | 109.002 ms | 1.20x   |
| **Wado**       | 2.05 k tokens/s | 187.720 ms | 2.07x   |

Wado passes JavaScript on inference and stays behind it on training. Training
spends most of its time in the backward pass, which sorts the whole graph
topologically and then walks every edge again. That is pointer chasing over GC
objects, where Rust's flat `Vec` of indices is at its strongest. Inference
never builds that traversal.

Each sample also runs the full 16-position attention window, deeper than any
training step reaches on this corpus: the longest name gives 11 positions and
the mean is 7, and attention cost grows with the square of the position count.

Rust makes a node a `usize` index into a `Vec<Value>`, because a `&mut` into a
growing `Vec` is what the borrow checker forbids. Wado's `Graph::value` returns
that `&mut Value` and a node holds those handles as its children, which is the
shape the Python original has. The gap between the two rows is what that costs.

All three arms print the same final loss and generate the same sample, so they
are the same computation.

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| **Wado**       | 7.82 M px/s | 100.568 ms | 1.00x   |
| JavaScript     | 7.80 M px/s | 100.786 ms | 1.00x   |
| C              | 7.72 M px/s | 101.892 ms | 1.01x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 795.05 M nums/s | 2.516 ms | 1.00x   |
| JavaScript     | 556.20 M nums/s | 3.596 ms | 1.43x   |
| **Wado**       | 331.21 M nums/s | 6.038 ms | 2.40x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.17 M conv/s | 36.801 ms | 1.00x   |
| Rust (core::fmt) | 20.99 M conv/s | 47.633 ms | 1.29x   |
| C (printf)       | 11.08 M conv/s | 90.240 ms | 2.45x   |

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
| Rust (serde_json)    |   1.87 GB/s | 0.339 ms | 1.00x   |
| JavaScript (JSON)    |   1.53 GB/s | 0.413 ms | 1.22x   |
| **Wado** (core:json) | 787.94 MB/s | 0.801 ms | 2.37x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 577.01 MB/s | 1.094 ms | 1.00x   |
| Rust (serde_json)    | 571.46 MB/s | 1.105 ms | 1.01x   |
| **Wado** (core:json) | 279.96 MB/s | 2.255 ms | 2.06x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.38 GB/s | 0.265 ms | 1.00x   |
| **Wado** (core:cbor) |  1.27 GB/s | 0.498 ms | 1.88x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 875.98 MB/s | 0.721 ms | 1.00x   |
| **Wado** (core:cbor) | 422.32 MB/s | 1.495 ms | 2.07x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 963.10 MB/s | 2.337 ms | 1.00x   |
| JavaScript (JSON)    | 582.21 MB/s | 3.866 ms | 1.65x   |
| **Wado** (core:json) | 278.57 MB/s | 8.080 ms | 3.46x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 363.52 MB/s | 6.192 ms | 1.00x   |
| Rust (serde_json)    | 357.94 MB/s | 6.289 ms | 1.02x   |
| **Wado** (core:json) | 261.29 MB/s | 8.615 ms | 1.39x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.52 GB/s | 0.894 ms | 1.00x   |
| **Wado** (core:cbor) | 606.14 MB/s | 3.713 ms | 4.15x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.17 GB/s | 1.929 ms | 1.00x   |
| **Wado** (core:cbor) | 429.94 MB/s | 5.235 ms | 2.72x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.27 GB/s | 0.405 ms | 1.00x   |
| **Wado** (core:json) |  1.73 GB/s | 1.000 ms | 2.47x   |
| JavaScript (JSON)    |  1.43 GB/s | 1.209 ms | 2.99x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.02 GB/s | 1.691 ms | 1.00x   |
| JavaScript (JSON)     | 775.44 MB/s | 2.227 ms | 1.35x   |
| **Wado** (PoC parser) | 439.17 MB/s | 3.932 ms | 2.38x   |
| **Wado** (core:json)  | 376.20 MB/s | 4.591 ms | 2.78x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It was the mark `core:json` had to reach, and
`core:json` comes within 15% of it while decoding any schema at all.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.66 GB/s | 0.472 ms | 1.00x   |
| **Wado** (core:cbor) |  2.21 GB/s | 0.782 ms | 1.66x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.59 GB/s | 0.668 ms | 1.00x   |
| **Wado** (core:cbor) | 688.16 MB/s | 2.509 ms | 3.76x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 325.56 MB/s | 1.940 ms | 1.00x   |
| JavaScript (node:zlib) | 200.82 MB/s | 3.145 ms | 1.62x   |
| C (zlib 1.3.1, Wasm)   | 132.35 MB/s | 4.771 ms | 2.46x   |
| **Wado** (core:zlib)   | 115.60 MB/s | 5.462 ms | 2.82x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.21 GB/s | 0.197 ms | 1.00x   |
| JavaScript (node:zlib) |   1.83 GB/s | 0.344 ms | 1.75x   |
| C (zlib 1.3.1, Wasm)   | 822.65 MB/s | 0.768 ms | 3.90x   |
| **Wado** (core:zlib)   | 504.54 MB/s | 1.251 ms | 6.36x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 13.32 MB/s |   1.000 ms | 1.00x   |
| Rust (sqlparser-rs) | 11.91 MB/s |   1.118 ms | 1.12x   |
| Java (ANTLR4)       |  0.10 MB/s | 132.970 ms | 133.20x |

Java (ANTLR4) is the head-to-head for Gale's generated parser, on the JVM and
JIT-warmed to steady state, so the gap is algorithmic rather than a warmup
artifact. The cost is full-context LL — this
grammar's ambiguities defeat the two-stage SLL fast path. Needs `java`; skipped
if absent.

### Syntax Highlight

Highlight 81 SQL statements (13321 bytes). Gale-generated highlighter vs five
reference SQL highlighters:

- **Prism.js** — regex-based, the speed reference (ultimate goal)
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
| Prism.js                      | 11.71 MB/s |  1.138 ms | 1.00x   |
| **Gale** (Wado)               |  9.70 MB/s |  1.372 ms | 1.21x   |
| Lezer (CodeMirror)            |  4.79 MB/s |  2.784 ms | 2.44x   |
| tree-sitter (Rust native)     |  4.57 MB/s |  2.914 ms | 2.56x   |
| tree-sitter (web-tree-sitter) |  2.80 MB/s |  4.750 ms | 4.18x   |
| Shiki (JS engine)             |  1.08 MB/s | 12.382 ms | 10.84x  |

Every highlighter parses the corpus without errors: a highlighter that gives up
on a region skips the work of colouring it, so the constructs two of them
mishandled are written another way at the same token count.

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| Java (ANTLR4)   | 761.38 KB/s |  54.992 ms | 1.00x   |
| **Wado** (Gale) | 292.50 KB/s | 143.144 ms | 2.60x   |

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

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |      43,264 |                   42,249 |                24,364 |                    17,200 |
| `GET /user/lookup/username/hey` |      44,585 |                   40,925 |                23,082 |                    16,732 |
| `POST /event/abcd1234/comment`  |      44,231 |                   45,984 |                22,913 |                    15,681 |
| `GET /static/index.html`        |      43,010 |                   53,601 |                23,753 |                    16,902 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     403,100 |                  268,996 |               118,893 |                    82,755 |
| `GET /user/lookup/username/hey` |     386,964 |                  237,896 |               111,075 |                    81,345 |
| `POST /event/abcd1234/comment`  |     386,906 |                  237,042 |               112,326 |                    70,629 |
| `GET /static/index.html`        |     389,773 |                  239,822 |               113,132 |                    78,997 |

`wado serve` places third at both shapes, ahead of Hono on Node. What separates
it from Axum is the component-model boundary, not the compiled code.
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
