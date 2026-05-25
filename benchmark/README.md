# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-05-25, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 4,858 ms |   1.00x |
| **Wado**          | 5,383 ms |   1.11x |
| JavaScript (Node) | 6,474 ms |   1.33x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| **Wado**          | 188 ms |   1.00x |
| JavaScript (Node) | 191 ms |   1.01x |
| C (gcc -O3)       | 193 ms |   1.02x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| C (gcc -O3)       |  48 ms |   1.00x |
| JavaScript (Node) |  71 ms |   1.48x |
| **Wado**          | 115 ms |   2.40x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 34 ms |   1.00x |
| Rust (native)  | 53 ms |   1.56x |
| **Wado**       | 83 ms |   2.44x |
| C (gcc -O3)    | 87 ms |   2.56x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    43 ms |       5 ms |  48 ms |   1.00x |
| **Wado** core:zlib    |   282 ms |     168 ms | 449 ms |   9.39x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |  0.96 ms |   1.00x |
| JSON.parse (Node)        |  2.49 ms |   2.61x |
| **Wado** core:json       | 11.54 ms |  12.08x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |      Time | vs best |
| ------------------------ | --------: | ------: |
| serde_json (Rust native) |  11.40 ms |   1.00x |
| JSON.parse (Node)        |  16.27 ms |   1.43x |
| **Wado** core:json       | 149.10 ms |  13.08x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| serde_json (Rust native)   |  2.99 ms |   1.00x |
| JSON.parse (Node)          |  6.00 ms |   2.01x |
| **Wado** v2 (hand-rolled¹) | 18.44 ms |   6.17x |
| **Wado** core:json         | 56.93 ms |  19.05x |

¹ `json_catalog/json_catalog_v2.wado` is a hand-rolled CitmCatalog parser
PoC (no `core:json` / `core:serde`). Kept as a marker of the upper bound
that's currently reachable without changes to `core:json`'s
sub-access-struct architecture. See its source for design notes.

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

Best of three runs per implementation:

| Implementation             |   Time | vs best |
| -------------------------- | -----: | ------: |
| sqlparser-rs (Rust native) | 200 ms |   1.00x |
| **Wado** (Gale)            | 831 ms |   4.15x |

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
| Prism.js                  |   229 ms |   1.00x |
| tree-sitter (Rust native) |   641 ms |   2.80x |
| Lezer (CodeMirror)        |   647 ms |   2.83x |
| tree-sitter (JS / WASM)   | 1,065 ms |   4.65x |
| **Wado** (Gale)           | 1,370 ms |   5.98x |
| Shiki (JS engine)         | 2,524 ms |  11.02x |

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
[Hono](https://hono.dev/) server on Node.js and on Bun, vs an
equivalent [Axum](https://github.com/tokio-rs/axum) server in native
Rust. The route and request set is Hono's own official router benchmark
(`honojs/hono`, `benchmarks/routers/`), driven over HTTP with `oha`.
Servers and the load generator run on disjoint pinned cores; each
request is measured in rotating slices and the fastest is kept.

Throughput (requests/sec, higher is better):

| Request                         | `wado serve` | Hono (Node) | Hono (Bun) | Axum (native) |
| ------------------------------- | -----------: | ----------: | ---------: | ------------: |
| `GET /user`                     |       54,106 |      40,486 |     71,401 |        93,103 |
| `GET /user/lookup/username/hey` |       51,752 |      42,260 |     64,678 |        95,552 |
| `GET /event/abcd1234/comments`  |       50,771 |      42,425 |     66,314 |        92,374 |
| `POST /event/abcd1234/comment`  |       50,780 |      33,452 |     66,137 |        92,109 |
| `GET /static/index.html`        |       50,553 |      41,972 |     66,981 |        94,257 |

`wado serve` leads Hono on Node on every request (~50k–54k vs ~33k–45k
req/s) but trails Hono on Bun (~65k–73k) — Bun's HTTP server is
markedly faster than Node's. Native-Rust Axum is the ceiling; its
figure here is load-generator-limited (`oha` saturates before Axum
does). `wado serve` runs a `wasi:http/service` component on wasmtime,
dispatching through `core:router`, with pooled instance reuse +
periodic recycling. A cross-runtime comparison (Wasm component on
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
