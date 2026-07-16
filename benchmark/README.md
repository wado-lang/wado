# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-07-16, wasmtime 46.0.1, gcc 13.3.0, rustc 1.96.1,
Node.js v24.14.1, Linux x86_64.

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
| JavaScript     | 8.41 M nums/s | 118.857 ms | 1.00x   |
| **Wado**       | 7.81 M nums/s | 128.111 ms | 1.08x   |
| C              | 7.78 M nums/s | 128.481 ms | 1.08x   |

### Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation | Throughput  | ms/iter    | vs best |
| -------------- | ----------- | ---------- | ------- |
| C              | 5.98 M px/s | 131.405 ms | 1.00x   |
| **Wado**       | 5.63 M px/s | 139.575 ms | 1.06x   |
| JavaScript     | 5.63 M px/s | 139.738 ms | 1.06x   |

### Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation | Throughput      | ms/iter   | vs best |
| -------------- | --------------- | --------- | ------- |
| C              | 215.15 M nums/s | 46.479 ms | 1.00x   |
| **Wado**       | 169.67 M nums/s | 58.939 ms | 1.27x   |
| JavaScript     | 166.72 M nums/s | 59.979 ms | 1.29x   |

### Float-to-String

1M f64 conversions to fixed-point string (`%.6f`).

| Implementation   | Throughput     | ms/iter    | vs best |
| ---------------- | -------------- | ---------- | ------- |
| Rust (core::fmt) | 16.10 M conv/s | 62.106 ms  | 1.00x   |
| **Wado** (fpfmt) | 10.87 M conv/s | 91.959 ms  | 1.48x   |
| C (printf)       | 9.77 M conv/s  | 102.397 ms | 1.65x   |

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
| serde_cbor (Rust)    | 2.16 GB/s   | 0.292 ms | 1.00x   |
| serde_json (Rust)    | 1.89 GB/s   | 0.334 ms | 1.14x   |
| **core:cbor** (Wado) | 605.46 MB/s | 1.043 ms | 3.57x   |
| JSON (JS)            | 319.27 MB/s | 1.978 ms | 6.77x   |
| **core:json** (Wado) | 263.67 MB/s | 2.395 ms | 8.20x   |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 802.43 MB/s | 0.787 ms | 1.00x   |
| serde_cbor (Rust)    | 765.25 MB/s | 0.825 ms | 1.05x   |
| JSON (JS)            | 589.37 MB/s | 1.072 ms | 1.36x   |
| **core:json** (Wado) | 104.44 MB/s | 6.046 ms | 7.68x   |
| **core:cbor** (Wado) | 101.66 MB/s | 6.211 ms | 7.89x   |

### canada

`canada.json` (2251051 bytes): a GeoJSON FeatureCollection with 55,563
coordinate points.

Serialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.95 GB/s   | 1.157 ms  | 1.00x   |
| serde_json (Rust)    | 823.29 MB/s | 2.734 ms  | 2.36x   |
| **core:cbor** (Wado) | 232.37 MB/s | 9.687 ms  | 8.37x   |
| JSON (JS)            | 200.12 MB/s | 11.248 ms | 9.72x   |
| **core:json** (Wado) | 107.32 MB/s | 20.975 ms | 18.13x  |

Deserialize:

| Implementation       | Throughput  | ms/iter   | vs best |
| -------------------- | ----------- | --------- | ------- |
| serde_cbor (Rust)    | 1.01 GB/s   | 2.236 ms  | 1.00x   |
| serde_json (Rust)    | 350.40 MB/s | 6.424 ms  | 2.87x   |
| JSON (JS)            | 341.51 MB/s | 6.591 ms  | 2.95x   |
| **core:json** (Wado) | 100.39 MB/s | 22.422 ms | 10.03x  |
| **core:cbor** (Wado) | 96.46 MB/s  | 23.336 ms | 10.44x  |

### catalog

`citm_catalog.json` (1727204 bytes): a CITM event catalog with 184 events and
243 performances.

Serialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_json (Rust)    | 3.64 GB/s   | 0.475 ms | 1.00x   |
| serde_cbor (Rust)    | 3.03 GB/s   | 0.569 ms | 1.20x   |
| **core:cbor** (Wado) | 1.03 GB/s   | 1.683 ms | 3.54x   |
| JSON (JS)            | 833.73 MB/s | 2.072 ms | 4.36x   |
| **core:json** (Wado) | 435.80 MB/s | 3.963 ms | 8.34x   |

Deserialize:

| Implementation       | Throughput  | ms/iter  | vs best |
| -------------------- | ----------- | -------- | ------- |
| serde_cbor (Rust)    | 2.06 GB/s   | 0.840 ms | 1.00x   |
| serde_json (Rust)    | 808.67 MB/s | 2.136 ms | 2.54x   |
| JSON (JS)            | 598.56 MB/s | 2.886 ms | 3.44x   |
| **core:cbor** (Wado) | 302.71 MB/s | 5.705 ms | 6.79x   |
| **core:json** (Wado) | 194.71 MB/s | 8.870 ms | 10.56x  |

### Compression: zlib

zlib compression and decompression of `twitter.json` (631514 bytes).

Compress:

| Implementation         | Throughput  | ms/iter   | vs best |
| ---------------------- | ----------- | --------- | ------- |
| Rust (zlib-rs)         | 234.75 MB/s | 2.690 ms  | 1.00x   |
| JavaScript (node:zlib) | 175.36 MB/s | 3.601 ms  | 1.34x   |
| **Wado** (core:zlib)   | 51.43 MB/s  | 12.278 ms | 4.56x   |

Decompress:

| Implementation         | Throughput  | ms/iter  | vs best |
| ---------------------- | ----------- | -------- | ------- |
| Rust (zlib-rs)         | 1.86 GB/s   | 0.340 ms | 1.00x   |
| JavaScript (node:zlib) | 929.85 MB/s | 0.679 ms | 2.00x   |
| **Wado** (core:zlib)   | 221.28 MB/s | 2.853 ms | 8.39x   |

## Parsing

### SQL Parse

Parse 81 SQL statements (13366 bytes). Two parsers are generated from the same
`SQLite.g4` — the Gale one and ANTLR4's own (Java) — alongside the hand-written
`sqlparser-rs`.

| Implementation      | Throughput | ms/iter    | vs best |
| ------------------- | ---------- | ---------- | ------- |
| Rust (sqlparser-rs) | 8.32 MB/s  | 1.607 ms   | 1.00x   |
| **Wado** (Gale)     | 6.57 MB/s  | 2.035 ms   | 1.27x   |
| Java (ANTLR4)       | 0.08 MB/s  | 173.380 ms | 107.89x |

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
| JavaScript (Prism)           | 10.85 MB/s  | 1.232 ms  | 1.00x   |
| **Wado** (Gale)              | 4.45 MB/s   | 3.004 ms  | 2.44x   |
| JavaScript (Lezer)           | 3.59 MB/s   | 3.721 ms  | 3.02x   |
| Rust (tree-sitter)           | 3.05 MB/s   | 4.377 ms  | 3.55x   |
| JavaScript (web-tree-sitter) | 2.01 MB/s   | 6.649 ms  | 5.40x   |
| JavaScript (Shiki)           | 747.79 KB/s | 17.874 ms | 14.51x  |

### Grammar Generation

Generate a Rust parser from an ANTLR4 `.g4` grammar. Gale is an
ANTLR4-compatible generator, so the head-to-head comparison is against
[ANTLR4](https://www.antlr.org/) itself over the **identical grammar** —
`RustLexer.g4` + `RustParser.g4` (34390 bytes), same input, same ALL(\*)
algorithm family, both emitting a parser. Throughput is grammar bytes processed
per second (higher is better).

| Implementation  | Throughput  | ms/iter    | vs best |
| --------------- | ----------- | ---------- | ------- |
| **Wado** (Gale) | 140.11 KB/s | 245.441 ms | 1.00x   |
| Java (ANTLR4)   | 39.54 KB/s  | 869.667 ms | 3.54x   |

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
| `GET /user`                     |       50,313 |      27,612 |     55,221 |        98,360 |
| `GET /user/lookup/username/hey` |       46,511 |      29,981 |     52,317 |        99,693 |
| `GET /event/abcd1234/comments`  |       46,766 |      26,159 |     52,291 |        99,961 |
| `POST /event/abcd1234/comment`  |       47,376 |      21,810 |     48,651 |        98,581 |
| `GET /static/index.html`        |       45,725 |      23,629 |     48,842 |        96,481 |

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
