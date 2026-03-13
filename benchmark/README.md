# Wado Benchmarks

This directory contains performance benchmarks comparing Wado against C, Rust, Zig, and JavaScript.

## Benchmarks

### Mandelbrot Set (`mandelbrot.*`)

Computes the Mandelbrot fractal by counting total iterations across a 1024x768 grid.

- **Use case**: Fractal rendering, floating-point performance
- **Operations**: Float arithmetic, nested loops, function calls
- **Grid**: 1024x768 pixels, max 256 iterations per pixel

```bash
make benchmark-mandelbrot
```

### Prime Counting (`count_prime.*`)

Counts prime numbers up to 10,000,000 using trial division.

- **Use case**: Integer arithmetic, branching performance
- **Operations**: Integer modulo, nested loops, branch prediction
- **Reference**: π(10,000,000) = 664,579 primes

```bash
make benchmark-count-prime
```

### Sieve of Eratosthenes (`sieve.*`)

Counts prime numbers up to 10,000,000 using the sieve algorithm.

- **Use case**: Array allocation, indexed access, memory performance
- **Operations**: Array creation via append, indexed read/write, iteration
- **Reference**: π(10,000,000) = 664,579 primes (same as count_prime)

```bash
make benchmark-sieve
```

### zlib Compression (`zlib/`)

Compresses and decompresses 100KB of patterned data (bytes `i % 256`) for 10 iterations.

- **Use case**: Compression library performance, byte array throughput
- **Operations**: zlib compress/decompress, large byte array manipulation
- **Comparison**: Wado (`core:zlib`, pure Wado) vs C zlib-1.3.1 (Wasm/wasmtime) vs zlib-rs (native Rust)

```bash
make benchmark-zlib
```

### Float-to-String (`fts.*`)

Converts 500,000 random f64 values (0.0–1.0) to decimal strings with 6 decimal places. Uses a linear congruential generator (seed=42) for a deterministic float sequence, ensuring all implementations produce identical output.

- **Use case**: Float formatting, string allocation throughput
- **Operations**: Float-to-string conversion, byte iteration, string buffer management
- **Comparison**: C (`snprintf`), Rust (`write!`), Zig (`std.fmt`), Wado (pure Wado via template literal)

```bash
make benchmark-fts
```

### JSON Parsing (`json_twitter/`, `json_canada/`, `json_catalog/`)

Parses real-world JSON datasets using typed deserialization (struct-based parsing, not DOM). Three datasets from [nativejson-benchmark](https://github.com/miloyip/nativejson-benchmark):

- **twitter.json** (631KB): Twitter search API response with nested objects (statuses, users, entities)
- **canada.json** (2.3MB): GeoJSON with deeply nested coordinate arrays (number-heavy)
- **citm_catalog.json** (1.7MB): Event catalog with mixed types (strings, arrays, nested objects, maps)

Each benchmark reads the JSON file once, then deserializes it into typed structs. Compares Wado (`core:json` + `core:serde`, pure Wado compiled to Wasm) against Rust (`serde_json`, native).

```bash
make benchmark-json-twitter
make benchmark-json-canada
make benchmark-json-catalog
```

## Prerequisites

To run all benchmarks, ensure you have the following tools installed:

- `cc` (C compiler, e.g., clang or gcc)
- `rustc` (Rust compiler — for fts benchmark)
- `zig` (Zig compiler — for fts benchmark)
- `node` (Node.js)

## Running Benchmarks

```bash
# Run all benchmarks at once
make benchmark-all

# Or run individually
make benchmark-mandelbrot
make benchmark-count-prime
make benchmark-sieve
make benchmark-zlib
make benchmark-fts
make benchmark-json-twitter
make benchmark-json-canada
make benchmark-json-catalog
```

## Recent Results

### Environment

| Component  | Version      |
| ---------- | ------------ |
| Wado       | 2026-03-11   |
| wasmtime   | 42.0.1       |
| Node.js    | v24.14.0     |
| C compiler | gcc 13.3.0   |
| Rust       | rustc 1.94.0 |
| Zig        | 0.15.2       |
| Platform   | Linux x86_64 |

### Mandelbrot (1024x768, max_iter=256)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| C (gcc -O3) | 130       | 1.00x    |
| JavaScript  | 137       | 1.05x    |
| **Wado**    | 139       | 1.07x    |

All implementations produce the same result: 47,407,790 total iterations.

### Prime Counting (limit=10,000,000)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| **Wado**    | 2,851     | 1.00x    |
| C (gcc -O3) | 3,177     | 1.11x    |
| JavaScript  | 3,311     | 1.16x    |

All implementations produce the same result: 664,579 primes.

### Sieve of Eratosthenes (limit=10,000,000)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| C (gcc -O3) | 49        | 1.00x    |
| JavaScript  | 70        | 1.43x    |
| **Wado**    | 129       | 2.63x    |

All implementations produce the same result: 664,579 primes.

### zlib Compress (100KB x 10 iterations)

| Runtime               | Time (ms) | Relative |
| --------------------- | --------- | -------- |
| zlib-rs (native Rust) | 1.0       | 1.00x    |
| C (Wasm/wasmtime)     | 5.3       | 5.1x     |
| **Wado** (pure Wado)  | 59        | 57x      |

### zlib Decompress (100KB x 10 iterations)

| Runtime               | Time (ms) | Relative  |
| --------------------- | --------- | --------- |
| zlib-rs (native Rust) | 0.18      | 1.00x     |
| C (Wasm/wasmtime)     | 0.97      | 5.4x      |
| **Wado** (pure Wado)  | 888       | 4,933x    |

zlib-rs runs natively; C zlib-1.3.1 and Wado are both compiled to Wasm and run on wasmtime. Wado's `core:zlib` is a pure Wado implementation, so significant overhead is expected.

### Float-to-String (500,000 conversions, 6 decimal places)

| Runtime             | Time (ms) | Relative |
| ------------------- | --------- | -------- |
| Zig (-OReleaseFast) | 24        | 1.00x    |
| Rust (rustc -O)     | 34        | 1.42x    |
| C (gcc -O3)         | 55        | 2.29x    |
| **Wado**            | 157       | 6.54x    |

All implementations produce: Total bytes: 4,000,000, byte sum: 204,501,007.

### JSON Parsing — Twitter (631KB)

| Runtime                    | Time (ms) | Relative |
| -------------------------- | --------- | -------- |
| Rust (serde_json, native)  | 0.69      | 1.00x    |
| **Wado** (core:json, Wasm) | 111       | 160x     |

Both implementations parse 100 statuses from Twitter search results.

### JSON Parsing — Canada (2.3MB)

| Runtime                    | Time (ms) | Relative |
| -------------------------- | --------- | -------- |
| Rust (serde_json, native)  | 6.9       | 1.00x    |
| **Wado** (core:json, Wasm) | 440       | 64x      |

Both implementations parse 55,563 coordinate points from GeoJSON.

### JSON Parsing — CITM Catalog (1.7MB)

| Runtime                    | Time (ms) | Relative |
| -------------------------- | --------- | -------- |
| Rust (serde_json, native)  | 2.15      | 1.00x    |
| **Wado** (core:json, Wasm) | 96        | 45x      |

Both implementations parse 184 events and 243 performances from CITM catalog data.

## Profiling Wado Programs

`wado run --profile <mode>` enables runtime profiling via wasmtime's profiling infrastructure.

### Guest Profiling (All Platforms)

Cross-platform in-process sampling profiler. Uses wasmtime's `GuestProfiler` with epoch-based interruption to collect stack samples at regular intervals. Output is Firefox Profiler JSON.

```sh
# Basic usage (writes profile.json with 10ms sampling interval)
wado run --profile guest benchmark/count_prime/count_prime.wado

# Custom output path
wado run --profile guest,my_profile.json benchmark/mandelbrot/mandelbrot.wado

# Custom output path and 5ms sampling interval
wado run --profile guest,my_profile.json,5 benchmark/count_prime/count_prime.wado
```

View the output at https://profiler.firefox.com/ by uploading the JSON file.

**How it works**: The engine is configured with epoch interruption. A background thread bumps the epoch at the specified interval. On each epoch tick, a callback calls `GuestProfiler::sample()` which captures the current Wasm call stack. After execution, the profile is serialized to the Firefox "processed profile format".

**Limitations**:

- Only measures time in WebAssembly guest code (not host or kernel)
- Sampling granularity is limited to function entry points and loop headers (epoch check points)
- Function names may show as `<wasm function N>` without DWARF debug info

### JitDump Profiling (Linux)

Detailed profiling using Linux `perf` with JIT dump integration. Wasmtime emits a jitdump file that `perf` can use for Wasm function name resolution.

```sh
# Record with perf (must use -k mono for clock synchronization)
perf record -k mono wado run --profile jitdump benchmark/count_prime/count_prime.wado

# Merge JIT symbols into perf data
perf inject --jit --input perf.data --output perf.jit.data

# View the profile
perf report --input perf.jit.data
```

**Advantages over guest profiling**:

- Measures time in guest code, host runtime, and kernel
- Hardware performance counter support
- Higher precision timing from CPU counters

### PerfMap Profiling (Linux)

Simpler alternative to jitdump. Wasmtime generates a `/tmp/perf-<pid>.map` file with function name mappings that `perf` reads automatically.

```sh
# Record
perf record -k mono wado run --profile perfmap benchmark/mandelbrot/mandelbrot.wado

# View (no inject step needed)
perf report --input perf.data
```

### Samply Profiling (Linux / macOS)

[samply](https://github.com/mstange/samply) is a third-party profiler that supports perfmap. It opens Firefox Profiler UI automatically.

```sh
samply record wado run --profile perfmap benchmark/count_prime/count_prime.wado
```

### Comparison

| Mode      | Platform | Precision | Scope                 | Output                | Viewer                        |
| --------- | -------- | --------- | --------------------- | --------------------- | ----------------------------- |
| `guest`   | All      | Moderate  | Wasm guest only       | Firefox Profiler JSON | https://profiler.firefox.com/ |
| `jitdump` | Linux    | High      | Guest + host + kernel | perf jitdump          | `perf report`                 |
| `perfmap` | Linux    | High      | Guest + host + kernel | `/tmp/perf-<pid>.map` | `perf report` / samply        |

### Tips

- For quick cross-platform analysis, use `--profile guest`.
- For production-level Linux profiling, use `--profile jitdump` with `perf`.
- Combine with optimization levels (`-O0` through `-O3`) to compare optimized vs unoptimized performance.
- The guest profiler adds a small overhead from epoch interruption (~10% slowdown).
- The jitdump/perfmap profilers add no measurable overhead to Wasm execution itself.

## Notes

- C benchmarks use `-O3` optimization
- C mandelbrot uses `-ffp-contract=off` to disable FMA for IEEE 754 consistency
- Wado runs on wasmtime with WASI P3 and Wasm GC enabled
- JavaScript runs on Node.js
- Times include program initialization overhead
- Wado CLI is built with `--release` for fair comparison with natively-compiled competitors
- Wado benchmarks use `MonotonicClock::now()` from `wasi:clocks` for timing
- zlib benchmark compares Wado's pure Wado zlib, C zlib-1.3.1 (Wasm/wasmtime), and native zlib-rs (Rust)
- fts benchmark compares Wado, C (`snprintf`), Rust (`write!`), and Zig (`std.fmt`)
- Rust benchmarks use `rustc -O` (release optimization)
- Zig benchmarks use `-OReleaseFast`
- JSON benchmarks compare Wado's `core:json` (pure Wado, Wasm) against Rust's `serde_json` (native)
- JSON test data from [nativejson-benchmark](https://github.com/miloyip/nativejson-benchmark)

## File Structure

```
benchmark/
├── README.md
├── count_prime/count_prime.{wado,c,js}
├── fts/fts.{wado,c,rs,zig}
├── json_canada/{json_canada.wado,serde_json.rs,canada.json}
├── json_catalog/{json_catalog.wado,serde_json.rs,citm_catalog.json}
├── json_twitter/{json_twitter.wado,serde_json.rs,twitter.json}
├── mandelbrot/mandelbrot.{wado,c,js}
├── sieve/sieve.{wado,c,js}
└── zlib/{zlib_bench.wado,zlib_rs.rs,zlib_c.c}
```
