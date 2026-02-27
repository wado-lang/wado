# Wado Benchmarks

This directory contains performance benchmarks comparing Wado against C, Rust, Zig, JavaScript, Python, and Ruby.

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
- **Comparison**: Wado (`core:zlib`, pure Wado implementation) vs zlib-rs (native Rust)

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

## Prerequisites

To run all benchmarks, ensure you have the following tools installed:

- `cc` (C compiler, e.g., clang or gcc)
- `rustc` (Rust compiler — for fts benchmark)
- `zig` (Zig compiler — for fts benchmark)
- `node` (Node.js)
- `python3` (Python 3)
- `ruby` (Ruby)

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
```

## Recent Results

### Environment

| Component  | Version                  |
| ---------- | ------------------------ |
| Wado       | commit `9b39202`         |
| wasmtime   | 41.0.4                   |
| Node.js    | v24.14.0                 |
| Python     | 3.14.3 (CPython, no JIT) |
| Ruby       | 4.0.1 (CRuby)            |
| C compiler | gcc 13.3.0               |
| Rust       | rustc 1.93.1             |
| Zig        | 0.15.2                   |
| Platform   | Linux x86_64             |

### Mandelbrot (1024x768, max_iter=256)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| C (gcc -O3) | 130       | 1.00x    |
| **Wado**    | 139       | 1.07x    |
| JavaScript  | 201       | 1.55x    |
| Python      | 3,371     | 25.93x   |
| Ruby        | 4,240     | 32.62x   |

All implementations produce the same result: 47,407,790 total iterations.

### Prime Counting (limit=10,000,000)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| C (gcc -O3) | 3,190     | 1.00x    |
| **Wado**    | 3,276     | 1.03x    |
| JavaScript  | 3,384     | 1.06x    |
| Ruby        | 41,941    | 13.15x   |
| Python      | 69,825    | 21.89x   |

All implementations produce the same result: 664,579 primes.

### Sieve of Eratosthenes (limit=10,000,000)

| Runtime     | Time (ms) | Relative |
| ----------- | --------- | -------- |
| C (gcc -O3) | 49        | 1.00x    |
| JavaScript  | 74        | 1.51x    |
| **Wado**    | 164       | 3.35x    |
| Python      | 727       | 14.84x   |
| Ruby        | 1,113     | 22.71x   |

All implementations produce the same result: 664,579 primes.

### zlib Compression (100KB x 10 iterations)

| Runtime               | Compress (ms) | Decompress (ms) | Total (ms) |
| --------------------- | ------------- | --------------- | ---------- |
| zlib-rs (native Rust) | 2             | 0.2             | 2          |
| **Wado** (pure Wado)  | 519           | 148,482         | 149,002    |

Wado's `core:zlib` is a pure Wado implementation compiled to Wasm, so significant overhead is expected compared to native. The decompression path is especially slow due to byte-at-a-time array operations.

### Float-to-String (500,000 conversions, 6 decimal places)

| Runtime             | Time (ms) | Relative |
| ------------------- | --------- | -------- |
| Zig (-OReleaseFast) | 27        | 1.00x    |
| Rust (rustc -O)     | 39        | 1.44x    |
| C (gcc -O3)         | 66        | 2.44x    |
| **Wado**            | 3,043     | 112.70x  |

All implementations produce: Total bytes: 4,000,000. Byte sums are nearly identical (Wado's fts has minor last-digit rounding differences in some values).

The overhead in Wado is primarily from GC struct/array allocation per conversion. Template string copy elision avoids deep-copying the formatted String, but each iteration still allocates a String struct + backing byte array + Formatter. String operations use `array.copy` for bulk byte transfers and short constant appends are decomposed into `append_char` calls.

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
- Python uses CPython (no JIT)
- Ruby uses CRuby
- Times include program initialization overhead
- Wado CLI is built with `--release` for fair comparison with natively-compiled competitors
- Wado benchmarks use `MonotonicClock::now()` from `core:clocks` for timing
- zlib benchmark compares Wado's pure Wado zlib against native zlib-rs (Rust)
- fts benchmark compares Wado, C (`snprintf`), Rust (`write!`), and Zig (`std.fmt`)
- Rust benchmarks use `rustc -O` (release optimization)
- Zig benchmarks use `-OReleaseFast`

## File Structure

```
benchmark/
├── README.md
├── count_prime/count_prime.{wado,c,js,py,rb}
├── fts/fts.{wado,c,rs,zig}
├── mandelbrot/mandelbrot.{wado,c,js,py,rb}
├── sieve/sieve.{wado,c,js,py,rb}
└── zlib/{zlib_bench.wado,zlib_rs.rs}
```
