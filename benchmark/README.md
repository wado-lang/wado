# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-08-05, wasmtime 47.0.2, gcc 13.3.0, rustc 1.97.1,
Node.js v26.3.1, Bun 1.3.11, Linux x86_64.

Throughput is work per second (higher is better), with per-iteration time in
parentheses. Native rows are optimized builds (C `gcc -O3`, Rust release, Wado
`-O2`); JavaScript runs on Node.js. `vs best` is the fastest row's throughput
over this row's (1.00x = fastest). Absolute throughput is machine-dependent, so
compare by `vs best`. Each figure is the best of three runs.

Benchmarks are grouped into four sections: pure computation, serialization &
compression, parsing, and application server.

## Pure Computation

### Prime Counting

Count primes up to 1M (integer arithmetic, trial division).

| Implementation |    Throughput |    ms/iter | vs best |
| -------------- | ------------: | ---------: | ------- |
| C              | 7.68 M nums/s | 130.237 ms | 1.00x   |
| **Wado**       |  7.5 M nums/s | 133.244 ms | 1.02x   |
| JavaScript     | 7.21 M nums/s | 138.648 ms | 1.07x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation |  Throughput |    ms/iter | vs best |
| -------------- | ----------: | ---------: | ------- |
| C              | 5.99 M px/s | 131.376 ms | 1.00x   |
| **Wado**       | 5.63 M px/s | 139.726 ms | 1.06x   |
| JavaScript     | 5.57 M px/s | 141.185 ms | 1.08x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation |      Throughput |   ms/iter | vs best |
| -------------- | --------------: | --------: | ------- |
| C              | 207.29 M nums/s | 48.243 ms | 1.00x   |
| **Wado**       | 163.44 M nums/s | 61.184 ms | 1.27x   |
| JavaScript     |  161.9 M nums/s | 61.767 ms | 1.28x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   |     Throughput |    ms/iter | vs best |
| ---------------- | -------------: | ---------: | ------- |
| Rust (core::fmt) | 14.21 M conv/s |  70.380 ms | 1.00x   |
| C (printf)       |  8.74 M conv/s | 114.370 ms | 1.63x   |
| **Wado** (fpfmt) |  5.11 M conv/s | 195.575 ms | 2.78x   |

## Serialization & Compression

Each dataset is measured under two codecs — JSON (`core:json`) and CBOR
(`core:cbor`) — over the same Wado data types, so serialization and
deserialization compare both across languages and across codecs. `serde_json` /
`serde_cbor` (Rust) and `JSON.stringify` / `JSON.parse` (JS) are the references.
Throughput for both phases is reported over the JSON source size (the shared
denominator across codecs).

### twitter

`twitter.json` (631514 bytes): a Twitter API search response with 100 statuses.

Serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| serde_cbor (Rust)    |   2.07 GB/s | 0.305 ms | 1.00x   |
| serde_json (Rust)    |   1.85 GB/s | 0.342 ms | 1.12x   |
| **core:cbor** (Wado) | 635.06 MB/s | 0.994 ms | 3.26x   |
| JSON (JS)            | 315.51 MB/s | 2.002 ms | 6.56x   |
| **core:json** (Wado) | 287.85 MB/s | 2.193 ms | 7.19x   |

Deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| serde_json (Rust)    | 790.53 MB/s | 0.799 ms | 1.00x   |
| serde_cbor (Rust)    | 736.55 MB/s | 0.857 ms | 1.07x   |
| JSON (JS)            | 595.62 MB/s | 1.060 ms | 1.33x   |
| **core:cbor** (Wado) | 127.43 MB/s | 4.955 ms | 6.20x   |
| **core:json** (Wado) | 114.12 MB/s | 5.533 ms | 6.93x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| serde_cbor (Rust)    |   2.05 GB/s |  1.099 ms | 1.00x   |
| serde_json (Rust)    | 676.07 MB/s |  3.330 ms | 3.03x   |
| **core:cbor** (Wado) | 299.48 MB/s |  7.516 ms | 6.85x   |
| JSON (JS)            | 185.38 MB/s | 12.143 ms | 11.06x  |
| **core:json** (Wado) | 131.09 MB/s | 17.171 ms | 15.64x  |

Deserialize:

| Implementation       |  Throughput |   ms/iter | vs best |
| -------------------- | ----------: | --------: | ------- |
| serde_cbor (Rust)    | 919.76 MB/s |  2.447 ms | 1.00x   |
| JSON (JS)            |  349.9 MB/s |  6.433 ms | 2.63x   |
| serde_json (Rust)    |    326 MB/s |  6.905 ms | 2.82x   |
| **core:cbor** (Wado) |  200.1 MB/s | 11.249 ms | 4.60x   |
| **core:json** (Wado) | 127.66 MB/s | 17.632 ms | 7.20x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| serde_json (Rust)    |   3.63 GB/s | 0.476 ms | 1.00x   |
| serde_cbor (Rust)    |   3.02 GB/s | 0.572 ms | 1.20x   |
| **core:cbor** (Wado) |   1.16 GB/s | 1.487 ms | 3.13x   |
| JSON (JS)            | 850.66 MB/s | 2.030 ms | 4.27x   |
| **core:json** (Wado) | 547.13 MB/s | 3.156 ms | 6.63x   |

Deserialize:

| Implementation       |  Throughput |  ms/iter | vs best |
| -------------------- | ----------: | -------: | ------- |
| serde_cbor (Rust)    |   2.03 GB/s | 0.850 ms | 1.00x   |
| serde_json (Rust)    | 889.12 MB/s | 1.943 ms | 2.28x   |
| JSON (JS)            |  654.1 MB/s | 2.641 ms | 3.10x   |
| **core:cbor** (Wado) | 406.52 MB/s | 4.248 ms | 4.99x   |
| **core:json** (Wado) | 181.17 MB/s | 9.533 ms | 11.20x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         |  Throughput |   ms/iter | vs best |
| ---------------------- | ----------: | --------: | ------- |
| Rust (zlib-rs)         | 234.59 MB/s |  2.692 ms | 1.00x   |
| JavaScript (node:zlib) | 165.92 MB/s |  3.806 ms | 1.41x   |
| **Wado** (core:zlib)   |  46.74 MB/s | 13.512 ms | 5.02x   |

Decompress:

| Implementation         |  Throughput |  ms/iter | vs best |
| ---------------------- | ----------: | -------: | ------- |
| Rust (zlib-rs)         |   1.91 GB/s | 0.330 ms | 1.00x   |
| JavaScript (node:zlib) | 956.16 MB/s | 0.660 ms | 2.00x   |
| **Wado** (core:zlib)   | 258.32 MB/s | 2.444 ms | 7.39x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput |    ms/iter | vs best |
| ------------------- | ---------: | ---------: | ------- |
| Rust (sqlparser-rs) |  8.38 MB/s |   1.596 ms | 1.00x   |
| **Wado** (Gale)     |  7.02 MB/s |   1.903 ms | 1.19x   |
| Java (ANTLR4)       |  0.08 MB/s | 171.850 ms | 104.75x |

ANTLR4 (Java) is the head-to-head for Gale's generated parser, on the JVM and
JIT-warmed to steady state (per-parse time flattens after ~50 parses, so the gap
is algorithmic, not a warmup artifact). The cost is full-context LL — this
grammar's ambiguities defeat the two-stage SLL fast path. Needs `java`; skipped
if absent.

### Syntax Highlight

Highlight 81 SQL statements (13366 bytes). Gale-generated highlighter vs four
reference SQL highlighters:

- **Prism.js** — regex-based, the speed reference (ultimate goal)
- **tree-sitter (Rust native)** — same `tree-sitter-sequel` grammar used by the
  JS row below, run as a Rust binary
- **Lezer (CodeMirror)** — `@codemirror/lang-sql` + `@lezer/highlight`, a
  pure-JS LR parser
- **tree-sitter (web-tree-sitter)** — official JS WASM binding, same
  `@derekstride/tree-sitter-sql` grammar as the Rust row
- **Shiki (JS engine)** — TextMate grammars, VSCode-quality output

| Implementation               |  Throughput |   ms/iter | vs best |
| ---------------------------- | ----------: | --------: | ------- |
| JavaScript (Prism)           |  10.41 MB/s |  1.284 ms | 1.00x   |
| **Wado** (Gale)              |   5.18 MB/s |  2.579 ms | 2.01x   |
| JavaScript (Lezer)           |   3.44 MB/s |  3.885 ms | 3.03x   |
| Rust (tree-sitter)           |    2.9 MB/s |  4.603 ms | 3.59x   |
| JavaScript (web-tree-sitter) |   1.88 MB/s |  7.113 ms | 5.54x   |
| JavaScript (Shiki)           | 750.46 KB/s | 17.810 ms | 13.87x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  |  Throughput |    ms/iter | vs best |
| --------------- | ----------: | ---------: | ------- |
| **Wado** (Gale) | 171.68 KB/s | 200.317 ms | 1.00x   |
| Java (ANTLR4)   |  39.34 KB/s | 874.213 ms | 4.36x   |

Gale is measured in-process (grammar assembly + code generation) and emits a
Wado recursive-descent parser; ANTLR4 runs its reference jar
(`java -jar antlr-4.13.2-complete.jar -Dlanguage=Java`, ~0.14 s of which is JVM
startup) over the same two files and emits Java. The ANTLR4 row needs `java` and
is skipped if it is absent.

## Application Server

### HTTP Routing

End-to-end HTTP throughput of `wado serve` vs [Hono](https://hono.dev/) on
Node.js and Bun, vs native-Rust [Axum](https://github.com/tokio-rs/axum), over
Hono's official router benchmark route set driven with `oha`. See
`http_routing/README.md` for the full route set and methodology.

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                     |       30,835 |      18,660 |     31,979 |        73,465 |
| `GET /user/lookup/username/hey` |       26,835 |      15,513 |     31,223 |        75,372 |
| `GET /event/abcd1234/comments`  |       24,982 |      15,931 |     27,405 |        72,029 |
| `POST /event/abcd1234/comment`  |       26,181 |      13,138 |     31,410 |        72,488 |
| `GET /static/index.html`        |       26,019 |      15,048 |     31,427 |        70,690 |

HTTP routing needs `oha` and Bun, and is measured separately
(`SLICE=4 ROUNDS=5 CONNECTIONS=50 mise run benchmark-http-routing`).

## Running

```sh
mise run benchmark-all              # run all

# pure computation
mise run benchmark-count-prime      # integer arithmetic
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
