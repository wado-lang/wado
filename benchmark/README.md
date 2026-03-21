# Wado Benchmarks

This directory contains performance benchmarks comparing Wado against C, Rust, Zig, and JavaScript.

## Benchmarks

### Mandelbrot Set (`mandelbrot.*`)

Computes the Mandelbrot fractal by counting total iterations across a 1024x768 grid.

- **Use case**: Fractal rendering, floating-point performance
- **Operations**: Float arithmetic, nested loops, function calls
- **Grid**: 1024x768 pixels, max 256 iterations per pixel

```bash
mise run benchmark-mandelbrot
```

### Prime Counting (`count_prime.*`)

Counts prime numbers up to 10,000,000 using trial division.

- **Use case**: Integer arithmetic, branching performance
- **Operations**: Integer modulo, nested loops, branch prediction
- **Reference**: π(10,000,000) = 664,579 primes

```bash
mise run benchmark-count-prime
```

### Sieve of Eratosthenes (`sieve.*`)

Counts prime numbers up to 10,000,000 using the sieve algorithm.

- **Use case**: Array allocation, indexed access, memory performance
- **Operations**: Array creation via append, indexed read/write, iteration
- **Reference**: π(10,000,000) = 664,579 primes (same as count_prime)

```bash
mise run benchmark-sieve
```

### zlib Compression (`zlib/`)

Compresses and decompresses `twitter.json` (~631KB of real JSON data) for 10 iterations.

- **Use case**: Compression library performance, byte array throughput
- **Operations**: zlib compress/decompress, large byte array manipulation
- **Data**: `json_twitter/twitter.json` — realistic JSON from Twitter API (better than synthetic patterns)
- **Comparison**: Wado (`core:zlib`, pure Wado) vs C zlib-1.3.1 (Wasm/wasmtime) vs zlib-rs (native Rust)

```bash
mise run benchmark-zlib
```

### Float-to-String (`fts.*`)

Converts 500,000 random f64 values (0.0–1.0) to decimal strings with 6 decimal places. Uses a linear congruential generator (seed=42) for a deterministic float sequence, ensuring all implementations produce identical output.

- **Use case**: Float formatting, string allocation throughput
- **Operations**: Float-to-string conversion, byte iteration, string buffer management
- **Comparison**: C (`snprintf`), Rust (`write!`), Zig (`std.fmt`), Wado (pure Wado via template literal)

```bash
mise run benchmark-fts
```

### JSON Parsing (`json_twitter/`, `json_canada/`, `json_catalog/`)

Parses real-world JSON datasets using typed deserialization (struct-based parsing, not DOM). Three datasets from [nativejson-benchmark](https://github.com/miloyip/nativejson-benchmark):

- **twitter.json** (631KB): Twitter search API response with nested objects (statuses, users, entities)
- **canada.json** (2.3MB): GeoJSON with deeply nested coordinate arrays (number-heavy)
- **citm_catalog.json** (1.7MB): Event catalog with mixed types (strings, arrays, nested objects, maps)

Each benchmark reads the JSON file once, then deserializes it into typed structs. Compares Wado (`core:json` + `core:serde`, pure Wado compiled to Wasm) against Rust (`serde_json`, native).

```bash
mise run benchmark-json-twitter
mise run benchmark-json-canada
mise run benchmark-json-catalog
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
mise run benchmark-all

# Or run individually
mise run benchmark-mandelbrot
mise run benchmark-count-prime
mise run benchmark-sieve
mise run benchmark-zlib
mise run benchmark-fts
mise run benchmark-json-twitter
mise run benchmark-json-canada
mise run benchmark-json-catalog
```

## Recent Results

### Environment

| Component  | Version      |
| ---------- | ------------ |
| Wado       | 2026-03-21   |
| wasmtime   | 42.0.1       |
| Node.js    | v24.14.0     |
| C compiler | gcc 13.3.0   |
| Rust       | rustc 1.94.0 |
| Zig        | 0.15.2       |
| Platform   | Linux x86_64 |

### Mandelbrot (1024x768, max_iter=256)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| **Wado**    | 194       | 1.00x    |
| C (gcc -O3) | 198       | 1.02x    |
| JavaScript  | 200       | 1.03x    |

All implementations produce the same result: 47,407,790 total iterations.

### Prime Counting (limit=10,000,000)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| **Wado**    | 3,249     | 1.00x    |
| C (gcc -O3) | 3,258     | 1.00x    |
| JavaScript  | 3,313     | 1.02x    |

All implementations produce the same result: 664,579 primes.

### Sieve of Eratosthenes (limit=10,000,000)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| C (gcc -O3) | 40        | 1.00x    |
| JavaScript  | 62        | 1.55x    |
| **Wado**    | 81        | 2.03x    |

All implementations produce the same result: 664,579 primes.

### zlib Compress (twitter.json 631KB x 10 iterations)

| Runtime               | Time (ms) | Relative |
| --------------------- | --------- | -------- |
| zlib-rs (native Rust) | 28        | 1.00x    |
| C zlib (Wasm)         | 70        | 2.50x    |
| **Wado** (pure Wado)  | 476       | 17.00x   |

### zlib Decompress (twitter.json 631KB x 10 iterations)

| Runtime               | Time (ms) | Relative |
| --------------------- | --------- | -------- |
| zlib-rs (native Rust) | 4         | 1.00x    |
| C zlib (Wasm)         | 10        | 2.50x    |
| **Wado** (pure Wado)  | 97        | 24.25x   |

zlib-rs runs natively; C zlib and Wado are compiled to Wasm and run on wasmtime. Wado's `core:zlib` is a pure Wado implementation. Compression ratio: ~8–9% (631KB → ~52KB).

### Float-to-String (500,000 conversions, 6 decimal places)

| Runtime             | Time (ms) | Relative |
| ------------------- | --------- | -------- |
| Zig (-OReleaseFast) | 30        | 1.00x    |
| Rust (rustc -O)     | 37        | 1.23x    |
| **Wado**            | 61        | 2.03x    |
| C (gcc -O3)         | 66        | 2.20x    |

All implementations produce: Total bytes: 4,000,000, byte sum: 204,501,007.

### JSON Parsing — Twitter (631KB)

| Runtime                    | Time (ms) | Relative |
| -------------------------- | --------- | -------- |
| Rust (serde_json, native)  | 0.798     | 1.00x    |
| **Wado** (core:json, Wasm) | 21.273    | 26.66x   |

Both implementations parse 100 statuses from Twitter search results.

### JSON Parsing — Canada (2.3MB)

| Runtime                    | Time (ms) | Relative |
| -------------------------- | --------- | -------- |
| Rust (serde_json, native)  | 9.811     | 1.00x    |
| **Wado** (core:json, Wasm) | 168.290   | 17.15x   |

Both implementations parse 55,563 coordinate points from GeoJSON.

### JSON Parsing — CITM Catalog (1.7MB)

| Runtime                    | Time (ms) | Relative |
| -------------------------- | --------- | -------- |
| Rust (serde_json, native)  | 2.742     | 1.00x    |
| **Wado** (core:json, Wasm) | 62.863    | 22.92x   |

Both implementations parse 184 events and 243 performances from CITM catalog data. Rust uses `BTreeMap` (ordered map) to match Wado's `TreeMap`.

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
- zlib benchmark uses `twitter.json` (~631KB) as input; compares Wado's pure Wado zlib, C zlib-1.3.1 (Wasm/wasmtime), and native zlib-rs (Rust)
- fts benchmark compares Wado, C (`snprintf`), Rust (`write!`), and Zig (`std.fmt`)
- Rust benchmarks use `rustc -O` (release optimization)
- Zig benchmarks use `-OReleaseFast`
- JSON benchmarks compare Wado's `core:json` (pure Wado, Wasm) against Rust's `serde_json` (native). Rust uses `BTreeMap` for map fields to match Wado's `TreeMap`
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
