# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-09-19, wasmtime 47.0.3, gcc 13.3.0, wasi-sdk 33.0,
rustc 1.98.0, Node.js v26.7.0, Bun 1.3.14, Linux x86_64.

Throughput is work per second (higher is better), with per-iteration time in
parentheses. Native rows are optimized builds (C `gcc -O3`, Rust release, Wado
`-O2`); JavaScript runs on Node.js. `vs best` is the fastest row's throughput
over this row's (1.00x = fastest). Absolute throughput is machine-dependent, so
compare by `vs best`. Each figure is the best of three runs, except MicroGPT's,
which says below why it takes more.

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
| Rust           | 2.91 k tokens/s |  78.023 ms | 1.00x   |
| **Wado**       | 1.49 k tokens/s | 152.109 ms | 1.95x   |
| JavaScript     | 1.32 k tokens/s | 171.599 ms | 2.20x   |

Infer — 24 samples of the forward path alone, no gradients:

| Implementation |      Throughput |    ms/iter | vs best |
| -------------- | --------------: | ---------: | ------- |
| Rust           | 4.47 k tokens/s |  85.901 ms | 1.00x   |
| **Wado**       | 4.01 k tokens/s |  95.655 ms | 1.11x   |
| JavaScript     | 3.56 k tokens/s | 107.769 ms | 1.25x   |

These rows are best of eight rather than the best of three the rest of the file
uses. The JavaScript arm needs it: its inference time swung by more than a
factor of two across those eight, where Rust and Wado each stayed near their own
best. Three iterations per run is too few for the JIT to settle on that phase.

Wado beats JavaScript on both phases. It trails Rust on both, and the gap is
wider on training. Training spends most of its time in the backward pass, which
sorts the whole graph topologically and then walks every edge again. That is
pointer chasing over GC objects, where Rust's flat `Vec` of indices is at its
strongest. Inference never builds that traversal.

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
| JavaScript     | 8.00 M px/s |  98.352 ms | 1.00x   |
| **Wado**       | 7.84 M px/s | 100.330 ms | 1.02x   |
| C              | 7.80 M px/s | 100.797 ms | 1.02x   |

### Sieve

Sieve of Eratosthenes up to 2M (array operations).

| Implementation |      Throughput |  ms/iter | vs best |
| -------------- | --------------: | -------: | ------- |
| C              | 783.00 M nums/s | 2.554 ms | 1.00x   |
| JavaScript     | 565.39 M nums/s | 3.537 ms | 1.38x   |
| **Wado**       | 344.13 M nums/s | 5.811 ms | 2.28x   |

The 2 MB buffer stays within the L2 TLB's 4K-page reach. A larger one makes the
row turn on whether a runtime's allocator got transparent huge pages.

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |   ms/iter | vs best |
| ---------------- | -------------: | --------: | ------- |
| **Wado**         | 27.95 M conv/s | 35.780 ms | 1.00x   |
| Rust (core::fmt) | 21.84 M conv/s | 45.778 ms | 1.28x   |
| C (printf)       | 11.53 M conv/s | 86.706 ms | 2.42x   |

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
| JavaScript (JSON)    |   1.54 GB/s | 0.410 ms | 1.27x   |
| **Wado** (core:json) | 850.12 MB/s | 0.742 ms | 2.29x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 597.43 MB/s | 1.057 ms | 1.00x   |
| Rust (serde_json)    | 587.02 MB/s | 1.076 ms | 1.02x   |
| **Wado** (core:json) | 281.43 MB/s | 2.243 ms | 2.12x   |

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  2.38 GB/s | 0.265 ms | 1.00x   |
| **Wado** (core:cbor) |  1.62 GB/s | 0.389 ms | 1.47x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    | 860.39 MB/s | 0.734 ms | 1.00x   |
| **Wado** (core:cbor) | 426.30 MB/s | 1.481 ms | 2.02x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

JSON serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_json)    | 959.01 MB/s | 2.347 ms | 1.00x   |
| JavaScript (JSON)    | 584.59 MB/s | 3.851 ms | 1.64x   |
| **Wado** (core:json) | 352.61 MB/s | 6.383 ms | 2.72x   |

JSON deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| JavaScript (JSON)    | 381.66 MB/s | 5.898 ms | 1.00x   |
| Rust (serde_json)    | 356.92 MB/s | 6.307 ms | 1.07x   |
| **Wado** (core:json) | 279.06 MB/s | 8.066 ms | 1.37x   |

CBOR serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.55 GB/s | 0.883 ms | 1.00x   |
| **Wado** (core:cbor) | 859.47 MB/s | 2.619 ms | 2.97x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   1.18 GB/s | 1.903 ms | 1.00x   |
| **Wado** (core:cbor) | 480.62 MB/s | 4.683 ms | 2.46x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

JSON serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_json)    |  4.32 GB/s | 0.400 ms | 1.00x   |
| **Wado** (core:json) |  2.08 GB/s | 0.828 ms | 2.07x   |
| JavaScript (JSON)    |  1.47 GB/s | 1.173 ms | 2.93x   |

JSON deserialize:

| Implementation        |  Throughput |  ms/iter | vs best |
| --------------------- | ----------: | -------: | ------- |
| Rust (serde_json)     |   1.04 GB/s | 1.656 ms | 1.00x   |
| JavaScript (JSON)     | 773.43 MB/s | 2.233 ms | 1.35x   |
| **Wado** (core:json)  | 436.26 MB/s | 3.959 ms | 2.39x   |
| **Wado** (PoC parser) | 424.54 MB/s | 4.068 ms | 2.46x   |

The PoC row (`json_catalog_v2.wado`) is a hand-written parser for this one
schema, not a general decoder. It was the mark `core:json` had to reach, and
`core:json` now reaches it while decoding any schema.

CBOR serialize:

| Implementation       | Throughput |  ms/iter | vs best |
| -------------------- | ---------: | -------: | ------- |
| Rust (serde_cbor)    |  3.77 GB/s | 0.458 ms | 1.00x   |
| **Wado** (core:cbor) |  2.61 GB/s | 0.661 ms | 1.44x   |

CBOR deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| Rust (serde_cbor)    |   2.65 GB/s | 0.653 ms | 1.00x   |
| **Wado** (core:cbor) | 812.82 MB/s | 2.124 ms | 3.25x   |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes). The C row
is compiled to Wasm with wasi-sdk's clang `-O3` and run on wasmtime; the Rust
and JavaScript rows are native. Every row compresses at deflate level 6, but
each library's level table trades ratio for speed a little differently, so the
rows differ in output size and each decompresses the stream it produced.

Compress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         | 326.21 MB/s | 1.936 ms | 1.00x   |
| JavaScript (node:zlib) | 203.12 MB/s | 3.109 ms | 1.61x   |
| C (zlib 1.3.1, Wasm)   | 133.91 MB/s | 4.716 ms | 2.44x   |
| **Wado** (core:zlib)   | 117.76 MB/s | 5.362 ms | 2.77x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   3.24 GB/s | 0.195 ms | 1.00x   |
| JavaScript (node:zlib) |   1.88 GB/s | 0.336 ms | 1.72x   |
| C (zlib 1.3.1, Wasm)   | 839.70 MB/s | 0.752 ms | 3.85x   |
| **Wado** (core:zlib)   | 479.98 MB/s | 1.315 ms | 6.74x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13321 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| **Wado** (Gale)     | 13.49 MB/s |   0.987 ms | 1.00x   |
| Rust (sqlparser-rs) | 12.08 MB/s |   1.103 ms | 1.12x   |
| Java (ANTLR4)       |  0.10 MB/s | 129.121 ms | 130.82x |

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
| Prism.js                      | 12.17 MB/s |  1.095 ms | 1.00x   |
| **Gale** (Wado)               | 10.04 MB/s |  1.326 ms | 1.21x   |
| Lezer (CodeMirror)            |  4.95 MB/s |  2.689 ms | 2.46x   |
| tree-sitter (Rust native)     |  4.60 MB/s |  2.897 ms | 2.65x   |
| tree-sitter (web-tree-sitter) |  2.86 MB/s |  4.657 ms | 4.25x   |
| Shiki (JS engine)             |  1.09 MB/s | 12.216 ms | 11.16x  |

Every highlighter parses the corpus without errors: a highlighter that gives up
on a region skips the work of colouring it, so the constructs two of them
mishandled are written another way at the same token count.

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (41870 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |   ms/iter | vs best |
| --------------- | ----------: | --------: | ------- |
| Java (ANTLR4)   | 784.40 KB/s | 53.379 ms | 1.00x   |
| **Wado** (Gale) | 560.78 KB/s | 74.663 ms | 1.40x   |

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
| `GET /user`                     |      45,332 |                   53,608 |                24,852 |                    17,739 |
| `GET /user/lookup/username/hey` |      43,474 |                   41,359 |                23,888 |                    17,815 |
| `POST /event/abcd1234/comment`  |      46,559 |                   42,866 |                24,304 |                    16,127 |
| `GET /static/index.html`        |      45,819 |                   44,500 |                24,130 |                    17,213 |

Four workers — a small VM running one instance:

| Request                         | Rust (Axum) | JavaScript (Hono on Bun) | **Wado** (wado serve) | JavaScript (Hono on Node) |
| ------------------------------- | ----------: | -----------------------: | --------------------: | ------------------------: |
| `GET /user`                     |     404,289 |                  279,755 |               124,251 |                    84,352 |
| `GET /user/lookup/username/hey` |     398,827 |                  240,215 |               117,859 |                    79,343 |
| `POST /event/abcd1234/comment`  |     389,614 |                  238,002 |               118,459 |                    69,276 |
| `GET /static/index.html`        |     395,136 |                  245,350 |               119,392 |                    79,011 |

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
