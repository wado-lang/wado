# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-04-24, wasmtime 44.0.0, gcc 13.3.0, rustc 1.95.0, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Prime Counting

Count primes up to 10M (integer arithmetic).

| Implementation    |     Time | vs best |
| ----------------- | -------: | ------: |
| **Wado**          | 3,769 ms |   1.00x |
| C (gcc -O3)       | 3,847 ms |   1.02x |
| JavaScript (Node) | 4,390 ms |   1.16x |

## Mandelbrot

1024x768 fractal, max 256 iterations (float arithmetic).

| Implementation    |   Time | vs best |
| ----------------- | -----: | ------: |
| C (gcc -O3)       | 168 ms |   1.00x |
| JavaScript (Node) | 172 ms |   1.03x |
| **Wado**          | 188 ms |   1.12x |

## Sieve

Sieve of Eratosthenes up to 10M (array operations).

| Implementation    |  Time | vs best |
| ----------------- | ----: | ------: |
| C (gcc -O3)       | 44 ms |   1.00x |
| JavaScript (Node) | 66 ms |   1.50x |
| **Wado**          | 80 ms |   1.82x |

## Float-to-String

500K f64 conversions to fixed-point string.

| Implementation |  Time | vs best |
| -------------- | ----: | ------: |
| Zig (RelFast)  | 31 ms |   1.00x |
| Rust (native)  | 45 ms |   1.45x |
| **Wado**       | 48 ms |   1.55x |
| C (gcc -O3)    | 65 ms |   2.10x |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation        | Compress | Decompress |  Total | vs best |
| --------------------- | -------: | ---------: | -----: | ------: |
| zlib-rs (Rust native) |    33 ms |       4 ms |  38 ms |   1.00x |
| C zlib (Wasm)         |    78 ms |      11 ms |  89 ms |   2.35x |
| **Wado** core:zlib    |   215 ms |     105 ms | 321 ms |   8.49x |

## JSON: twitter

Deserialize twitter.json (631 KB).

| Implementation           |    Time | vs best |
| ------------------------ | ------: | ------: |
| serde_json (Rust native) | 0.74 ms |   1.00x |
| JSON.parse (Node)        | 1.72 ms |   2.32x |
| **Wado** core:json       | 14.9 ms |  20.11x |

## JSON: canada

Deserialize canada.json (2.3 MB, geographic coordinates).

| Implementation           |     Time | vs best |
| ------------------------ | -------: | ------: |
| serde_json (Rust native) |   8.6 ms |   1.00x |
| JSON.parse (Node)        |  11.7 ms |   1.37x |
| **Wado** core:json       | 128.9 ms |  15.07x |

## JSON: catalog

Deserialize citm_catalog.json (1.7 MB, event catalog).

| Implementation           |    Time | vs best |
| ------------------------ | ------: | ------: |
| serde_json (Rust native) | 2.46 ms |   1.00x |
| JSON.parse (Node)        | 4.42 ms |   1.80x |
| **Wado** core:json       | 48.2 ms |  19.62x |

## SQL Parse

Parse 81 SQL statements (13 KB) x 100 iterations. Gale-generated parser vs sqlparser-rs.

| Implementation             |     Time | vs best |
| -------------------------- | -------: | ------: |
| sqlparser-rs (Rust native) |   179 ms |   1.00x |
| **Wado** (Gale)            | 1,161 ms |   6.49x |

## Syntax Highlight

Highlight 81 SQL statements (13 KB) x 100 iterations. Gale-generated highlighter vs tree-sitter.

| Implementation              |     Time | vs best |
| --------------------------- | -------: | ------: |
| tree-sitter (Rust native)   |   514 ms |   1.00x |
| tree-sitter (Wasm/wasmtime) |   668 ms |   1.30x |
| **Wado** (Gale)             | 4,619 ms |   8.98x |

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
