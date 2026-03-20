---
name: benchmark
description: Run performance benchmarks and wasm size reports, then update README files with new results.
---

# Benchmark

Run all benchmarks and wasm size comparison, then update the README files.

## Prerequisites

```sh
mise run on-task-started                                      # install mise and project tools
rustup target add wasm32-wasip1                           # for wasm-size Rust builds
curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash  # for wasm-size Moonbit builds
```

After installing Moonbit, run `moon update` in each wasm-size program directory to fetch dependencies:

```sh
export PATH="$HOME/.moon/bin:$PATH"
cd wasm-size/hello_world && moon update
cd ../pi_approx && moon update
```

If `mise trust` errors appear for `wasm-size/mise.toml`, run:

```sh
mise trust wasm-size/mise.toml
```

## Procedure

### 1. Run benchmarks (at least twice)

The first run may be slow due to cold compilation caches. Run at least twice and use the run with the most internally consistent numbers.

```sh
mise run benchmark-all   # 1st run (warmup)
mise run benchmark-all   # 2nd run (use these numbers)
```

Benchmarks compare Wado against C and JavaScript (plus Rust/Zig for fts, zlib-rs for zlib).

### 2. Extract results

Parse the output for each benchmark:

- **count-prime**: `Elapsed: NNN ms` for C, JavaScript, Wado
- **mandelbrot**: `Elapsed: NNN.NN ms` for C, JavaScript, Wado
- **sieve**: `Elapsed: NNN ms` for C, JavaScript, Wado
- **zlib**: `Compress: NNN ms`, `Decompress: NNN ms`, `Elapsed: NNN ms` for zlib-rs and Wado
- **fts**: `Elapsed: NNN ms` for Zig, Rust, C, Wado

### 3. Get tool versions

```sh
cd benchmark && mise exec -- node --version && mise exec -- zig version
wasmtime --version
rustc --version
cc --version | head -1
```

### 4. Update benchmark/README.md

Update the "Recent Results" section:

- **Environment table**: Update Wado date and any changed tool versions
- **Result tables**: Update times and relative ratios (baseline is always the fastest, 1.00x)
- Sort each table by time (fastest first)
- Format large numbers with commas (e.g., `3,140`)
- Round relative to 2 decimal places

### 5. Run wasm size report

```sh
mise run report-wasm-size
```

### 6. Update wasm-size/README.md

Update the size tables with new values for all languages.

## Notes

- Benchmarks use mise-managed tool versions, not system versions.
- Cloud VMs have noisy performance. Absolute times vary across runs but relative ratios are fairly stable. Choose the run with the most internally consistent numbers.
- `mise run benchmark-all` runs: count-prime, mandelbrot, sieve, zlib, fts serially (MISE_JOBS=1).
- `mise run report-wasm-size` builds all language targets with size-optimized flags and reports `.wasm` file sizes.
