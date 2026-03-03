---
name: benchmark
description: Run performance benchmarks and wasm size reports, then update README files with new results.
---

# Benchmark

Run all benchmarks and wasm size comparison, then update the README files.

## Prerequisites

```sh
make on-task-started  # install mise and project tools
```

The first run installs mise-managed tools (node, python, ruby, zig, tinygo, wasi-sdk, etc.) and compiles the Rust toolchain. This can take several minutes.

## Procedure

### 1. Run benchmarks (at least twice)

The first run may be slow due to cold compilation caches. Run at least twice and use the second run's numbers.

```sh
make benchmark-all   # 1st run (warmup)
make benchmark-all   # 2nd run (use these numbers)
```

### 2. Extract results

Parse the output for each benchmark:

- **count-prime**: `Elapsed: NNN ms` for C, JavaScript, Python, Ruby, Wado
- **mandelbrot**: `Elapsed: NNN ms` (or `NNN.NN ms`) for C, JavaScript, Python, Ruby, Wado
- **sieve**: `Elapsed: NNN ms` for C, JavaScript, Python, Ruby, Wado
- **zlib**: `Compress: NNN ms`, `Decompress: NNN ms`, `Elapsed: NNN ms` for zlib-rs and Wado
- **fts**: `Elapsed: NNN ms` for C, Rust, Zig, Wado

### 3. Get tool versions

```sh
cd benchmark && mise exec -- node --version && mise exec -- python3 --version && mise exec -- ruby --version && mise exec -- zig version
wasmtime --version
rustc --version
cc --version | head -1
```

### 4. Update benchmark/README.md

Update the "Recent Results" section:

- **Environment table**: Update Wado date and any changed tool versions
- **Result tables**: Update times and relative ratios (baseline is always the fastest, 1.00x)
- Sort each table by time (fastest first)
- Format large numbers with commas (e.g., `71,808`)
- Round relative to 2 decimal places

### 5. Run wasm size report

```sh
make report-wasm-size
```

Some languages may be skipped if tools are not installed (Rust wasm target, Moonbit). Keep previous values for skipped languages.

### 6. Update wasm-size/README.md

Update the size tables with new values. Only update entries that were actually built.

## Notes

- Benchmarks use mise-managed tool versions, not system versions. Always check with `mise exec`.
- The benchmark environment may have higher latency than bare metal (e.g., cloud VMs). Absolute times vary but relative ratios are stable.
- `make benchmark-all` runs: count-prime, mandelbrot, sieve, zlib, fts (in order).
- `make report-wasm-size` builds all language targets with size-optimized flags and reports `.wasm` file sizes.
- Rust and Moonbit in wasm-size require manual setup (`rustup target add wasm32-wasip1`, moonbit installer).
