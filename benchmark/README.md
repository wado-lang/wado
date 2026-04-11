# Wado Benchmarks

Performance comparison of Wado (Wasm/wasmtime) against native compilers.

Environment: Wado 2026-04-11, wasmtime 42.0.1, gcc 13.3.0, rustc 1.94.1, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

## Compute

Pure computation — integer, float, array, and string formatting.

| Benchmark              | C (gcc -O3) | JS (Node) | Zig        | Rust   | **Wado** | vs best |
| ---------------------- | ----------: | --------: | ---------: | -----: | -------: | ------: |
| Prime counting (10M)   |    3,282 ms |  3,337 ms |          — |      — | 3,336 ms |  1.02x  |
| Mandelbrot (1024x768)  |      121 ms |    140 ms |          — |      — |   139 ms |  1.15x  |
| Sieve (10M)            |       50 ms |     82 ms |          — |      — |    82 ms |  1.64x  |
| Float-to-string (500K) |       56 ms |         — |      24 ms |  33 ms |    47 ms |  1.96x  |

## Compression

zlib compress/decompress of twitter.json (631 KB) x 10 iterations.

| Implementation    | Runtime      | Compress | Decompress |  Total | vs best |
| ----------------- | ------------ | -------: | ---------: | -----: | ------: |
| zlib-rs           | Rust native  |    29 ms |       4 ms |  33 ms |  1.00x  |
| C zlib 1.3.1      | Wasm/wasmtime |    70 ms |      11 ms |  81 ms |  2.45x  |
| **Wado** core:zlib | Wasm/wasmtime |   243 ms |      90 ms | 333 ms | 10.09x  |

## JSON Parsing

Single-iteration deserialization. Wado `core:json` vs Rust `serde_json` (native).

| Dataset         | serde_json | **Wado** | Relative |
| --------------- | ---------: | -------: | -------: |
| twitter (631 KB) |   0.63 ms |  16.3 ms |  25.87x  |
| canada (2.3 MB)  |   7.51 ms |  82.5 ms |  10.99x  |
| catalog (1.7 MB) |   2.24 ms |  38.5 ms |  17.19x  |

## Parsing & Highlighting

SQL parsing and syntax highlighting (13 KB, 81 statements x 100 iterations).
Gale-generated parser/highlighter (Wado) vs Rust crates (native).

| Benchmark        | Rust native         | **Wado** (Gale) | Relative |
| ---------------- | ------------------: | --------------: | -------: |
| SQL parse        |    173 ms (sqlparser-rs) |       2,137 ms |  12.35x  |
| Syntax highlight |    466 ms (tree-sitter)  |       8,166 ms |  17.53x  |

Wasm-to-Wasm comparison (both on wasmtime):

| Implementation              |    Time | Relative |
| --------------------------- | ------: | -------: |
| tree-sitter (Wasm/wasmtime) |  577 ms |   1.00x  |
| **Wado** (Gale)             | 8,166 ms |  14.15x  |

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
