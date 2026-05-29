# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-05-29, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 3,208 ms |   1.00x |
| **Wado**          | 3,252 ms |   1.01x |
| JavaScript (Node) | 3,311 ms |   1.03x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| **Wado**          | 195 ms |   1.00x |
| JavaScript (Node) | 196 ms |   1.01x |
| C (gcc -O3)       | 197 ms |   1.01x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 37 ms |   1.00x |
| JavaScript (Node) | 61 ms |   1.65x |
| **Wado**          | 71 ms |   1.92x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 31 ms |   1.00x |
| Rust (native)  | 38 ms |   1.23x |
| **Wado**       | 57 ms |   1.84x |
| C (gcc -O3)    | 67 ms |   2.16x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    29 ms |       4 ms |  32 ms |   1.00x |
| **Wado** core:zlib    |   201 ms |     116 ms | 317 ms |   9.86x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |    Time | vs best |
| ------------------------ | ------: | ------: |
| serde_json (Rust native) | 0.90 ms |   1.00x |
| JSON.parse (Node)        | 1.77 ms |   1.97x |
| **Wado** core:json       | 4.46 ms |   4.97x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |      Time | vs best |
| ------------------------ | --------: | ------: |
| serde_json (Rust native) |   8.39 ms |   1.00x |
| JSON.parse (Node)        |  12.25 ms |   1.46x |
| **Wado** core:json       | 117.73 ms |  14.03x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| serde_json (Rust native)   |  2.39 ms |   1.00x |
| JSON.parse (Node)          |  4.50 ms |   1.88x |
| **Wado** v2 (hand-rolled¹) |  6.42 ms |   2.69x |
| **Wado** core:json         | 17.09 ms |   7.15x |

¹ `json_catalog/json_catalog_v2.wado` is a hand-rolled CitmCatalog parser
PoC (no `core:json` / `core:serde`). Kept as a marker of the upper bound
that's currently reachable without changes to `core:json`'s
sub-access-struct architecture. See its source for design notes.

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

Best of three runs per implementation:

| Implementation             |   Time | vs best |
| -------------------------- | -----: | ------: |
| sqlparser-rs (Rust native) | 175 ms |   1.00x |
| **Wado** (Gale)            | 190 ms |   1.08x |

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
| Prism.js                  |   171 ms |   1.00x |
| **Wado** (Gale)           |   410 ms |   2.40x |
| tree-sitter (Rust native) |   500 ms |   2.92x |
| Lezer (CodeMirror)        |   525 ms |   3.07x |
| tree-sitter (JS / WASM)   |   874 ms |   5.11x |
| Shiki (JS engine)         | 2,126 ms |  12.43x |

Notes:

- Regex-based Prism.js wins on raw speed for a token-poor language
  like SQL.
- The Wado (Gale) highlighter now lands second overall, ahead of the
  tree-sitter Rust native parser and Lezer — only the regex-based
  Prism.js is faster.
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
[Hono](https://hono.dev/) server on Node.js and on Bun, vs an
equivalent [Axum](https://github.com/tokio-rs/axum) server in native
Rust. The route and request set is Hono's own official router benchmark
(`honojs/hono`, `benchmarks/routers/`), driven over HTTP with `oha`.
Servers and the load generator run on disjoint pinned cores; each
request is measured in rotating slices and the fastest is kept.

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                     |       52,979 |      30,595 |     61,858 |        98,406 |
| `GET /user/lookup/username/hey` |       49,963 |      30,536 |     54,222 |       100,359 |
| `GET /event/abcd1234/comments`  |       51,269 |      30,646 |     54,752 |       100,362 |
| `POST /event/abcd1234/comment`  |       50,210 |      21,421 |     53,639 |        99,944 |
| `GET /static/index.html`        |       50,816 |      28,582 |     55,317 |        98,739 |

`wado serve` leads Hono on Node on every request (~50k–53k vs ~21k–31k
req/s) but trails Hono on Bun (~54k–62k) — Bun's HTTP server is
markedly faster than Node's. Native-Rust Axum is the ceiling; its
figure here is load-generator-limited (`oha` saturates before Axum
does, staying flat at ~98k–100k). `wado serve` runs a
`wasi:http/service` component on wasmtime, dispatching through
`core:router`, with pooled instance reuse + periodic recycling. A
cross-runtime comparison (Wasm component on
wasmtime vs JS on Node.js/Bun vs native Rust). See
`http_routing/README.md` for the full table and methodology.

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
mise run benchmark-http-routing     # HTTP routing (wado serve vs Hono vs Axum)
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
