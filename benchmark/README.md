# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-05-19, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 3,221 ms |   1.00x |
| JavaScript (Node) | 3,317 ms |   1.03x |
| **Wado**          | 3,321 ms |   1.03x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| C (gcc -O3)       | 132 ms |   1.00x |
| JavaScript (Node) | 140 ms |   1.06x |
| **Wado**          | 141 ms |   1.07x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 50 ms |   1.00x |
| **Wado**          | 75 ms |   1.49x |
| JavaScript (Node) | 77 ms |   1.54x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 24 ms |   1.00x |
| Rust (native)  | 35 ms |   1.46x |
| **Wado**       | 43 ms |   1.80x |
| C (gcc -O3)    | 55 ms |   2.29x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    29 ms |       4 ms |  33 ms |   1.00x |
| C zlib (Wasm)²        |    75 ms |      11 ms |  86 ms |   2.46x |
| **Wado** core:zlib    |   195 ms |      95 ms | 290 ms |   8.82x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |    Time | vs best |
| ------------------------ | ------: | ------: |
| serde_json (Rust native) | 0.67 ms |   1.00x |
| JSON.parse (Node)        | 1.59 ms |   2.36x |
| **Wado** core:json       | 7.88 ms |  11.73x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |      Time | vs best |
| ------------------------ | --------: | ------: |
| serde_json (Rust native) |   8.60 ms |   1.00x |
| JSON.parse (Node)        |  12.16 ms |   1.41x |
| **Wado** core:json       | 129.03 ms |  15.00x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| serde_json (Rust native)   |  2.14 ms |   1.00x |
| JSON.parse (Node)          |  4.59 ms |   2.15x |
| **Wado** v2 (hand-rolled¹) | 10.31 ms |   4.82x |
| **Wado** core:json         | 33.56 ms |  15.68x |

¹ `json_catalog/json_catalog_v2.wado` is a hand-rolled CitmCatalog parser
PoC (no `core:json` / `core:serde`). Kept as a marker of the upper bound
that's currently reachable without changes to `core:json`'s
sub-access-struct architecture. See its source for design notes.

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

Best of three runs per implementation:

| Implementation             |   Time | vs best |
| -------------------------- | -----: | ------: |
| sqlparser-rs (Rust native) | 173 ms |   1.00x |
| **Wado** (Gale)            | 591 ms |   3.42x |

## Syntax Highlight

Highlight 81 SQL statements (13 KB) x 100 iterations. Gale-generated
highlighter vs four reference SQL highlighters:

- **Prism.js** — regex-based, the speed reference (ultimate goal)
- **tree-sitter (Rust native)** — same `tree-sitter-sequel` grammar
  used by the JS row below, run as a Rust binary
- **Lezer (CodeMirror)** — `@codemirror/lang-sql` + `@lezer/highlight`,
  pure-JS LR parser
- **tree-sitter (web-tree-sitter)** — official JS WASM binding, same
  `@derekstride/tree-sitter-sql` grammar as the Rust row
- **Shiki (JS engine)** — TextMate grammars, VSCode-quality output

Best of three runs per implementation:

| Implementation            |     Time | vs best |
| ------------------------- | -------: | ------: |
| Prism.js                  |   151 ms |   1.00x |
| Lezer (CodeMirror)        |   447 ms |   2.96x |
| tree-sitter (Rust native) |   482 ms |   3.19x |
| tree-sitter (JS / WASM)   |   740 ms |   4.90x |
| **Wado** (Gale)           |   976 ms |   6.46x |
| Shiki (JS engine)         | 1,922 ms |  12.73x |

Notes:

- Regex-based Prism.js wins on raw speed for a token-poor language
  like SQL.
- Pure-JS Lezer is essentially on par with tree-sitter's Rust native
  parser, and clearly beats the same tree-sitter grammar accessed
  through its JS WASM binding — V8 optimises plain JS more aggressively
  than the WASM↔JS boundary crossings cost.
- Shiki (JS engine) is the slowest but produces the richest output
  (identifier-level coloring, VSCode-quality themes). The Oniguruma
  (WASM) engine is omitted because it is ~2.5x slower than the JS
  engine on this input while producing byte-identical output.

## HTTP Routing

End-to-end HTTP throughput of `wado serve` vs an equivalent
[Hono](https://hono.dev/) server on Node.js. The route and request set
is Hono's own official router benchmark (`honojs/hono`,
`benchmarks/routers/`), driven over HTTP with `oha` (6s, 50
connections per request).

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) |
| ------------------------------- | -----------: | ----------: |
| `GET /user`                     |       30,778 |      20,728 |
| `GET /user/lookup/username/hey` |       28,326 |      20,728 |
| `GET /event/abcd1234/comments`  |       25,292 |      23,258 |
| `POST /event/abcd1234/comment`  |       25,068 |      17,668 |
| `GET /static/index.html`        |       29,091 |      22,595 |

`wado serve` leads on every request (~25k–31k vs ~17k–26k req/s). It
runs a `wasi:http/service` component on wasmtime, dispatching through
`core:router`, with pooled instance reuse + periodic recycling. This is
a cross-runtime comparison (Wasm component on wasmtime vs JS on
Node.js). See `http_routing/README.md` for the full table and
methodology.

## Running

```sh
mise run benchmark-all              # run all

mise run benchmark-count-prime      # integer arithmetic
mise run benchmark-mandelbrot       # float arithmetic
mise run benchmark-sieve            # array operations
mise run benchmark-fts              # float-to-string
mise run benchmark-zlib             # compression
mise run benchmark-json-twitter     # JSON (631 KB)
mise run benchmark-json-canada      # JSON (2.3 MB)
mise run benchmark-json-catalog     # JSON (1.7 MB)
mise run benchmark-sqlite-parse     # SQL parsing
mise run benchmark-syntax-highlight # syntax highlighting
mise run benchmark-http-routing     # HTTP routing (wado serve vs Hono)
```

Prerequisites: `cc`, `cargo`, `zig`, `node` (managed by `mise install`).

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
