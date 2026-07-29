# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-28, wasmtime 47.0.2, gcc 13.3.0, rustc 1.97.1,
Node.js v24.14.1, Bun 1.3.11, Linux x86_64.

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
| C              | 7.79 M nums/s | 128.377 ms | 1.00x   |
| **Wado**       | 7.74 M nums/s | 129.249 ms | 1.01x   |
| JavaScript     | 7.55 M nums/s | 132.392 ms | 1.03x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| JavaScript     | 4.03 M px/s | 195.241 ms | 1.00x   |
| **Wado**       | 3.99 M px/s | 196.973 ms | 1.01x   |
| C              | 3.92 M px/s | 200.488 ms | 1.03x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 265.35 M nums/s | 37.686 ms | 1.00x   |
| JavaScript     | 187.72 M nums/s | 53.271 ms | 1.41x   |
| **Wado**       | 171.33 M nums/s | 58.367 ms | 1.55x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 13.32 M conv/s | 75.051 ms  | 1.00x   |
| **Wado** (fpfmt) | 10.92 M conv/s | 91.588 ms  | 1.22x   |
| C (printf)       | 7.36 M conv/s  | 135.820 ms | 1.81x   |

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
| serde_cbor (Rust)    | 1.98 GB/s   | 0.319 ms | 1.00x   |
| serde_json (Rust)    | 1.50 GB/s   | 0.422 ms | 1.32x   |
| **core:cbor** (Wado) | 556.54 MB/s | 1.134 ms | 3.55x   |
| JSON (JS)            | 292.85 MB/s | 2.156 ms | 6.76x   |
| **core:json** (Wado) | 241.14 MB/s | 2.618 ms | 8.21x   |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 634.59 MB/s | 0.995 ms | 1.00x   |
| serde_cbor (Rust)    | 626.05 MB/s | 1.009 ms | 1.01x   |
| JSON (JS)            | 472.70 MB/s | 1.336 ms | 1.34x   |
| **core:cbor** (Wado) | 92.78 MB/s  | 6.806 ms | 6.84x   |
| **core:json** (Wado) | 87.96 MB/s  | 7.179 ms | 7.22x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.74 GB/s   | 1.294 ms  | 1.00x   |
| serde_json (Rust)    | 638.24 MB/s | 3.527 ms  | 2.73x   |
| **core:cbor** (Wado) | 170.00 MB/s | 13.241 ms | 10.23x  |
| JSON (JS)            | 133.10 MB/s | 16.912 ms | 13.07x  |
| **core:json** (Wado) | 85.11 MB/s  | 26.448 ms | 20.44x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 855.48 MB/s | 2.631 ms  | 1.00x   |
| serde_json (Rust)    | 287.88 MB/s | 7.819 ms  | 2.97x   |
| JSON (JS)            | 274.16 MB/s | 8.211 ms  | 3.12x   |
| **core:cbor** (Wado) | 90.35 MB/s  | 24.913 ms | 9.47x   |
| **core:json** (Wado) | 85.89 MB/s  | 26.207 ms | 9.96x   |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 3.16 GB/s   | 0.547 ms | 1.00x   |
| serde_cbor (Rust)    | 2.70 GB/s   | 0.640 ms | 1.17x   |
| **core:cbor** (Wado) | 755.33 MB/s | 2.286 ms | 4.18x   |
| JSON (JS)            | 751.85 MB/s | 2.297 ms | 4.20x   |
| **core:json** (Wado) | 369.49 MB/s | 4.674 ms | 8.55x   |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.82 GB/s   | 0.947 ms  | 1.00x   |
| serde_json (Rust)    | 783.32 MB/s | 2.205 ms  | 2.33x   |
| JSON (JS)            | 572.77 MB/s | 3.016 ms  | 3.19x   |
| **core:cbor** (Wado) | 275.93 MB/s | 6.259 ms  | 6.61x   |
| **core:json** (Wado) | 171.17 MB/s | 10.090 ms | 10.65x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 223.20 MB/s | 2.829 ms  | 1.00x   |
| JavaScript (node:zlib) | 157.55 MB/s | 4.008 ms  | 1.42x   |
| **Wado** (core:zlib)   | 42.48 MB/s  | 14.867 ms | 5.26x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.99 GB/s   | 0.317 ms | 1.00x   |
| JavaScript (node:zlib) | 959.09 MB/s | 0.658 ms | 2.08x   |
| **Wado** (core:zlib)   | 200.16 MB/s | 3.155 ms | 9.95x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput | ms/iter    | vs best |
| ------------------- | ---------- | ---------- | ------- |
| Rust (sqlparser-rs) | 7.52 MB/s  | 1.777 ms   | 1.00x   |
| **Wado** (Gale)     | 4.88 MB/s  | 2.737 ms   | 1.54x   |
| Java (ANTLR4)       | 0.06 MB/s  | 227.773 ms | 128.18x |

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
| JavaScript (Prism)           | 8.39 MB/s   | 1.593 ms  | 1.00x   |
| **Wado** (Gale)              | 3.71 MB/s   | 3.605 ms  | 2.26x   |
| JavaScript (Lezer)           | 2.66 MB/s   | 5.017 ms  | 3.15x   |
| Rust (tree-sitter)           | 2.61 MB/s   | 5.113 ms  | 3.21x   |
| JavaScript (web-tree-sitter) | 1.49 MB/s   | 8.956 ms  | 5.62x   |
| JavaScript (Shiki)           | 603.32 KB/s | 22.154 ms | 13.91x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  | Throughput  | ms/iter     | vs best |
| --------------- | ----------- | ----------- | ------- |
| **Wado** (Gale) | 118.54 KB/s | 290.106 ms  | 1.00x   |
| Java (ANTLR4)   | 32.63 KB/s  | 1054.027 ms | 3.63x   |

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
| `GET /user`                     |       37,425 |      32,759 |     52,659 |        96,248 |
| `GET /user/lookup/username/hey` |       34,444 |      30,461 |     47,575 |        99,099 |
| `GET /event/abcd1234/comments`  |       35,706 |      30,242 |     47,533 |        96,041 |
| `POST /event/abcd1234/comment`  |       36,063 |      23,025 |     46,154 |        95,255 |
| `GET /static/index.html`        |       34,241 |      27,967 |     47,449 |        99,631 |

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
