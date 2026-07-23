# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-23, wasmtime 47.0.1, gcc 13.3.0, rustc 1.97.1,
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

| Implementation | Throughput    | ms/iter    | vs best |
| -------------- | ------------- | ---------- | ------- |
| C              | 6.76 M nums/s | 147.963 ms | 1.00x   |
| **Wado**       | 6.64 M nums/s | 150.489 ms | 1.02x   |
| JavaScript     | 6.13 M nums/s | 163.206 ms | 1.10x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| C              | 4.65 M px/s | 169.213 ms | 1.00x   |
| JavaScript     | 4.48 M px/s | 175.361 ms | 1.04x   |
| **Wado**       | 4.19 M px/s | 187.868 ms | 1.11x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 231.45 M nums/s | 43.206 ms | 1.00x   |
| JavaScript     | 175.40 M nums/s | 57.013 ms | 1.32x   |
| **Wado**       | 154.39 M nums/s | 64.772 ms | 1.50x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 12.08 M conv/s | 82.750 ms  | 1.00x   |
| **Wado** (fpfmt) | 11.75 M conv/s | 85.118 ms  | 1.03x   |
| C (printf)       | 7.36 M conv/s  | 135.959 ms | 1.64x   |

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

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 1.66 GB/s   | 0.380 ms | 1.00x   |
| serde_json (Rust)    | 1.66 GB/s   | 0.380 ms | 1.00x   |
| **core:cbor** (Wado) | 569.49 MB/s | 1.108 ms | 2.91x   |
| JSON (JS)            | 289.51 MB/s | 2.181 ms | 5.73x   |
| **core:json** (Wado) | 243.27 MB/s | 2.595 ms | 6.82x   |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 651.92 MB/s | 0.969 ms | 1.00x   |
| serde_cbor (Rust)    | 581.12 MB/s | 1.087 ms | 1.12x   |
| JSON (JS)            | 494.29 MB/s | 1.278 ms | 1.32x   |
| **core:cbor** (Wado) | 97.78 MB/s  | 6.458 ms | 6.67x   |
| **core:json** (Wado) | 87.19 MB/s  | 7.243 ms | 7.48x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.63 GB/s   | 1.380 ms  | 1.00x   |
| serde_json (Rust)    | 635.47 MB/s | 3.542 ms  | 2.57x   |
| **core:cbor** (Wado) | 208.34 MB/s | 10.804 ms | 7.82x   |
| JSON (JS)            | 162.29 MB/s | 13.871 ms | 10.04x  |
| **core:json** (Wado) | 102.15 MB/s | 22.037 ms | 15.96x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 813.18 MB/s | 2.768 ms  | 1.00x   |
| serde_json (Rust)    | 284.98 MB/s | 7.899 ms  | 2.85x   |
| JSON (JS)            | 272.51 MB/s | 8.260 ms  | 2.98x   |
| **core:cbor** (Wado) | 130.05 MB/s | 17.308 ms | 6.25x   |
| **core:json** (Wado) | 91.92 MB/s  | 24.488 ms | 8.85x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 2.93 GB/s   | 0.589 ms | 1.00x   |
| serde_cbor (Rust)    | 2.38 GB/s   | 0.725 ms | 1.23x   |
| **core:cbor** (Wado) | 896.11 MB/s | 1.927 ms | 3.27x   |
| JSON (JS)            | 775.67 MB/s | 2.227 ms | 3.78x   |
| **core:json** (Wado) | 413.79 MB/s | 4.174 ms | 7.08x   |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.69 GB/s   | 1.020 ms  | 1.00x   |
| serde_json (Rust)    | 832.32 MB/s | 2.075 ms  | 2.03x   |
| JSON (JS)            | 556.39 MB/s | 3.104 ms  | 3.04x   |
| **core:cbor** (Wado) | 357.62 MB/s | 4.829 ms  | 4.73x   |
| **core:json** (Wado) | 166.77 MB/s | 10.356 ms | 10.13x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 201.85 MB/s | 3.129 ms  | 1.00x   |
| JavaScript (node:zlib) | 138.46 MB/s | 4.561 ms  | 1.46x   |
| **Wado** (core:zlib)   | 42.15 MB/s  | 14.982 ms | 4.79x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.70 GB/s   | 0.372 ms | 1.00x   |
| JavaScript (node:zlib) | 908.16 MB/s | 0.695 ms | 1.87x   |
| **Wado** (core:zlib)   | 211.61 MB/s | 2.984 ms | 8.03x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput | ms/iter    | vs best |
| ------------------- | ---------- | ---------- | ------- |
| Rust (sqlparser-rs) | 7.02 MB/s  | 1.904 ms   | 1.00x   |
| **Wado** (Gale)     | 5.27 MB/s  | 2.535 ms   | 1.33x   |
| Java (ANTLR4)       | 0.06 MB/s  | 216.991 ms | 117.00x |

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

| Implementation               | Throughput  | ms/iter   | vs best |
| ---------------------------- | ----------- | --------- | ------- |
| JavaScript (Prism)           | 8.88 MB/s   | 1.505 ms  | 1.00x   |
| **Wado** (Gale)              | 3.61 MB/s   | 3.697 ms  | 2.46x   |
| JavaScript (Lezer)           | 2.90 MB/s   | 4.608 ms  | 3.06x   |
| Rust (tree-sitter)           | 2.50 MB/s   | 5.353 ms  | 3.55x   |
| JavaScript (web-tree-sitter) | 1.64 MB/s   | 8.127 ms  | 5.41x   |
| JavaScript (Shiki)           | 649.06 KB/s | 20.593 ms | 13.68x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  | Throughput  | ms/iter     | vs best |
| --------------- | ----------- | ----------- | ------- |
| **Wado** (Gale) | 149.07 KB/s | 230.696 ms  | 1.00x   |
| Java (ANTLR4)   | 34.10 KB/s  | 1008.461 ms | 4.37x   |

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
`http_routing/README.md` for the full table and methodology.

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                     |       34,274 |      38,416 |     66,840 |        96,478 |
| `GET /user/lookup/username/hey` |       30,776 |      42,258 |     58,785 |        94,146 |
| `GET /event/abcd1234/comments`  |       31,083 |      41,833 |     60,003 |        95,467 |
| `POST /event/abcd1234/comment`  |       30,083 |      33,576 |     57,640 |        92,347 |
| `GET /static/index.html`        |       30,688 |      40,436 |     60,354 |        95,014 |

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
