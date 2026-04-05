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

Environment: Wado 2026-04-03, wasmtime 42.0.1, gcc 13.3.0, rustc 1.94.1, Zig 0.15.2, Node.js v24.14.1, Linux x86_64.

### Compute

| Benchmark              | Baseline        | Baseline (ms) | Wado (ms) | Relative |
| ---------------------- | --------------- | ------------- | --------- | -------- |
| Mandelbrot (1024x768)  | C (gcc -O3)     | 131           | 139       | 1.06x    |
| Prime counting (10M)   | C (gcc -O3)     | 3,284         | 3,307     | 1.01x    |
| Sieve (10M)            | C (gcc -O3)     | 53            | 81        | 1.53x    |
| Float-to-string (500K) | Zig (RelFast)   | 25            | 43        | 1.72x    |

### Compression (twitter.json 631KB x 10 iterations)

| Operation  | zlib-rs (native) | C zlib (Wasm) | Wado (pure) |
| ---------- | ---------------- | ------------- | ----------- |
| Compress   | 29 ms            | 72 ms         | 251 ms      |
| Decompress | 4 ms             | 10 ms         | 96 ms       |

### JSON Parsing (Wado `core:json` vs Rust `serde_json`)

| Dataset          | Rust (native) | Wado (Wasm) | Relative |
| ---------------- | ------------- | ----------- | -------- |
| twitter (631KB)  | 0.789 ms      | 16.542 ms   | 20.96x   |
| canada (2.3MB)   | 7.972 ms      | 68.244 ms   | 8.56x    |
| catalog (1.7MB)  | 2.347 ms      | 36.112 ms   | 15.38x   |

### Parser & Highlighter (13KB SQL, 81 statements x 100 iterations)

| Benchmark         | Baseline (native)    | Baseline (ms) | Wado (ms) | Relative |
| ----------------- | -------------------- | ------------- | --------- | -------- |
| SQLite parse      | sqlparser-rs (Rust)  | 167           | 3,910     | 23.42x   |
| Syntax highlight  | tree-sitter (Rust)   | 450           | 27,660    | 61.47x   |

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
