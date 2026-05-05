# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-05-05, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| C (gcc -O3)       | 3,571 ms |   1.00x |
| **Wado**          | 3,682 ms |   1.03x |
| JavaScript (Node) | 4,276 ms |   1.20x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| C (gcc -O3)       | 162 ms |   1.00x |
| JavaScript (Node) | 172 ms |   1.06x |
| **Wado**          | 182 ms |   1.12x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 39 ms |   1.00x |
| JavaScript (Node) | 64 ms |   1.64x |
| **Wado**          | 75 ms |   1.92x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 28 ms |   1.00x |
| Rust (native)  | 39 ms |   1.39x |
| **Wado**       | 48 ms |   1.71x |
| C (gcc -O3)    | 61 ms |   2.18x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    31 ms |       4 ms |  35 ms |   1.00x |
| C zlib (Wasm)         |    75 ms |      11 ms |  86 ms |   2.46x |
| **Wado** core:zlib    |   210 ms |     106 ms | 317 ms |   9.06x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |  0.78 ms |   1.00x |
| JSON.parse (Node)        |  2.60 ms |   3.33x |
| **Wado** core:json       | 13.02 ms |  16.69x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |      Time | vs best |
| ------------------------ | --------: | ------: |
| serde_json (Rust native) |   8.39 ms |   1.00x |
| JSON.parse (Node)        |  11.83 ms |   1.41x |
| **Wado** core:json       | 113.67 ms |  13.55x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation              |     Time | vs best |
| --------------------------- | -------: | ------: |
| serde_json (Rust native)    |  2.45 ms |   1.00x |
| JSON.parse (Node)           |  4.27 ms |   1.74x |
| **Wado** v2 (hand-rolled¹) | 17.95 ms |   7.33x |
| **Wado** core:json          | 40.48 ms |  16.52x |

¹ `json_catalog/json_catalog_v2.wado` is a hand-rolled CitmCatalog parser
PoC (no `core:json` / `core:serde`). Kept as a marker of the upper bound
that's currently reachable without changes to `core:json`'s
sub-access-struct architecture. See its source for design notes.

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

| Implementation             |    Time | vs best |
| -------------------------- | ------: | ------: |
| sqlparser-rs (Rust native) |  174 ms |   1.00x |
| **Wado** (Gale)            | 1022 ms |   5.86x |

## Syntax Highlight

Highlight 81 SQL statements (13 KB) x 100 iterations. Gale-generated highlighter vs tree-sitter.

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| tree-sitter (Rust native)  |   498 ms |   1.00x |
| tree-sitter (Wasm)         |   677 ms |   1.36x |
| **Wado** (Gale)            | 4,672 ms |   9.38x |

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
