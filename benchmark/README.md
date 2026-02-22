# Wado Benchmarks

This directory contains performance benchmarks comparing Wado against C, JavaScript, Python, and Ruby.

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

## Prerequisites

To run all benchmarks, ensure you have the following tools installed:

- `cc` (C compiler, e.g., clang or gcc)
- `node` (Node.js)
- `python3` (Python 3)
- `ruby` (Ruby)

## Running Benchmarks

```bash
# Run all benchmarks
make benchmark-mandelbrot
make benchmark-count-prime
make benchmark-sieve

# Or run them individually (see comments in each source file)
```

## Recent Results

### Environment

| Component  | Version                              |
| ---------- | ------------------------------------ |
| Wado       | commit `8f2537f`                     |
| wasmtime   | 40.0.0 (0807b003e 2025-12-22)        |
| Node.js    | v24.11.0                             |
| Python     | 3.14.2 (CPython, no JIT)             |
| Ruby       | 3.4.7 (CRuby)                        |
| C compiler | Apple clang 17.0.0                   |
| Platform   | macOS (Darwin 24.6.0), Apple Silicon |

### Mandelbrot (1024x768, max_iter=256)

| Runtime       | Time (ms) | Relative |
| ------------- | --------- | -------- |
| C (clang -O3) | 136       | 1.00x    |
| JavaScript    | 143       | 1.05x    |
| **Wado**      | 173       | 1.27x    |
| Python        | 4,137     | 30.42x   |

All implementations produce the same result: 47,407,790 total iterations.

### Prime Counting (limit=10,000,000)

| Runtime       | Time (ms) | Relative |
| ------------- | --------- | -------- |
| **Wado**      | 1,363     | 1.00x    |
| C (clang -O3) | 1,496     | 1.10x    |
| JavaScript    | 2,427     | 1.78x    |
| Python        | 74,360    | 54.56x   |

All implementations produce the same result: 664,579 primes.

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

| Mode      | Platform    | Precision | Scope                | Output               | Viewer                        |
| --------- | ----------- | --------- | -------------------- | -------------------- | ----------------------------- |
| `guest`   | All         | Moderate  | Wasm guest only      | Firefox Profiler JSON | https://profiler.firefox.com/ |
| `jitdump` | Linux       | High      | Guest + host + kernel | perf jitdump         | `perf report`                 |
| `perfmap` | Linux       | High      | Guest + host + kernel | `/tmp/perf-<pid>.map` | `perf report` / samply        |

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
- Wado benchmarks use `MonotonicClock::now()` from `core:clocks` for timing

## File Structure

```
benchmark/
├── README.md
├── count_prime/count_prime.{wado,c,js,py,rb}
├── mandelbrot/mandelbrot.{wado,c,js,py,rb}
├── sieve/sieve.{wado,c,js,py,rb}
└── zlib/
```
