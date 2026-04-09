# Wado Benchmarks

Performance benchmarks comparing Wado (Wasm/wasmtime) against native implementations.

## Running

```bash
mise run benchmark-all              # run all benchmarks

mise run benchmark-count-prime      # integer arithmetic
mise run benchmark-mandelbrot       # float arithmetic
mise run benchmark-sieve            # array operations
mise run benchmark-zlib             # compression
mise run benchmark-fts              # float-to-string
mise run benchmark-json-twitter     # JSON parsing (631KB)
mise run benchmark-json-canada      # JSON parsing (2.3MB)
mise run benchmark-json-catalog     # JSON parsing (1.7MB)
mise run benchmark-sqlite-parse     # SQL parsing (Gale vs sqlparser-rs)
mise run benchmark-syntax-highlight # syntax highlighting (Gale vs tree-sitter)
```

Prerequisites: `cc`, `rustc`, `zig`, `node` (managed by `mise install`).

## Results

Environment: Wado 2026-04-09, wasmtime 42.0.1, gcc 13.3.0, rustc 1.94.1, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

### Compute

| Benchmark              | Baseline      | Baseline (ms) | Wado (ms) | Relative |
| ---------------------- | ------------- | ------------- | --------- | -------- |
| Mandelbrot (1024x768)  | C (gcc -O3)   | 131           | 139       | 1.06x    |
| Prime counting (10M)   | C (gcc -O3)   | 3,289         | 3,342     | 1.02x    |
| Sieve (10M)            | C (gcc -O3)   | 51            | 85        | 1.67x    |
| Float-to-string (500K) | Zig (RelFast) | 24            | 43        | 1.79x    |

### Compression (twitter.json 631KB x 10 iterations)

| Operation  | zlib-rs (native) | C zlib (Wasm) | Wado (pure) |
| ---------- | ---------------- | ------------- | ----------- |
| Compress   | 29 ms            | 73 ms         | 248 ms      |
| Decompress | 4 ms             | 12 ms         | 93 ms       |

### JSON Parsing (Wado `core:json` vs Rust `serde_json`)

| Dataset         | Rust (native) | Wado (Wasm) | Relative |
| --------------- | ------------- | ----------- | -------- |
| twitter (631KB) | 0.676 ms      | 16.415 ms   | 24.28x   |
| canada (2.3MB)  | 6.990 ms      | 79.570 ms   | 11.38x   |
| catalog (1.7MB) | 2.112 ms      | 36.138 ms   | 17.11x   |

### Parser & Highlighter (13KB SQL, 81 statements x 100 iterations)

| Benchmark        | Baseline (native)   | Baseline (ms) | Wado (ms) | Relative |
| ---------------- | ------------------- | ------------- | --------- | -------- |
| SQLite parse     | sqlparser-rs (Rust) | 160           | 2,892     | 18.08x   |
| Syntax highlight | tree-sitter (Rust)  | 446           | 8,795     | 19.72x   |

Syntax highlight Wasm-to-Wasm comparison (both on wasmtime):

| Runtime                        | Time (ms) | Relative |
| ------------------------------ | --------- | -------- |
| tree-sitter (Wasm/wasmtime)    | 575       | 1.00x    |
| **Wado** (Gale, Wasm/wasmtime) | 8,795     | 15.30x   |

## Profiling

```sh
# Guest profiling (all platforms) — view at https://profiler.firefox.com/
wado run --profile guest benchmark/count_prime/count_prime.wado
wado run --profile guest,output.json,5 benchmark/count_prime/count_prime.wado  # custom path, 5ms interval

# Linux perf (jitdump — detailed, with JIT symbol resolution)
perf record -k mono wado run --profile jitdump benchmark/count_prime/count_prime.wado
perf inject --jit --input perf.data --output perf.jit.data
perf report --input perf.jit.data

# Linux perf (perfmap — simpler, no inject step)
perf record -k mono wado run --profile perfmap benchmark/mandelbrot/mandelbrot.wado
perf report

# samply (Linux/macOS)
samply record wado run --profile perfmap benchmark/count_prime/count_prime.wado
```

| Mode      | Platform | Scope                 | Viewer                        |
| --------- | -------- | --------------------- | ----------------------------- |
| `guest`   | All      | Wasm guest only       | https://profiler.firefox.com/ |
| `jitdump` | Linux    | Guest + host + kernel | `perf report`                 |
| `perfmap` | Linux    | Guest + host + kernel | `perf report` / samply        |
