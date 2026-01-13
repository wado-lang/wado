# Wado Benchmarks

This directory contains performance benchmarks comparing Wado against C, JavaScript, and Python.

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

## Running Benchmarks

```bash
# Run all benchmarks
make benchmark-mandelbrot
make benchmark-count-prime

# Or run them individually (see comments in each source file)
```

## Recent Results

### Environment

| Component  | Version                              |
| ---------- | ------------------------------------ |
| Wado       | commit `53e4f9d`                     |
| wasmtime   | 40.0.0 (0807b003e 2025-12-22)        |
| Node.js    | v24.11.0                             |
| Python     | 3.x                                  |
| C compiler | Apple clang 17.0.0                   |
| Platform   | macOS (Darwin 24.6.0), Apple Silicon |

### Mandelbrot (1024x768, max_iter=256)

| Runtime       | Time (ms) | Relative |
| ------------- | --------- | -------- |
| JavaScript    | 116       | 1.00x    |
| C (clang -O3) | 118       | 1.02x    |
| **Wado**      | 136       | 1.17x    |
| Python        | TBD       | TBD      |

All implementations produce the same result: 47,407,790 total iterations.

### Prime Counting (limit=10,000,000)

| Runtime       | Time (ms) | Relative |
| ------------- | --------- | -------- |
| **Wado**      | 1,296     | 1.00x    |
| C (clang -O3) | 1,350     | 1.04x    |
| JavaScript    | 1,992     | 1.54x    |
| Python        | TBD       | TBD      |

All implementations produce the same result: 664,579 primes.

## Notes

- C benchmarks use `-O3` optimization
- C mandelbrot uses `-ffp-contract=off` to disable FMA for IEEE 754 consistency
- Wado runs on wasmtime with WASI P3 and Wasm GC enabled
- JavaScript runs on Node.js
- Python uses CPython (no JIT)
- Times include program initialization overhead
- Wado benchmarks use `MonotonicClock::now()` from `core:clocks` for timing

## File Structure

```
benchmark/
├── README.md           # This file
├── mandelbrot.wado     # Wado implementation
├── mandelbrot.js       # JavaScript implementation
├── mandelbrot.c        # C implementation
├── mandelbrot.py       # Python implementation
├── count_prime.wado    # Wado implementation
├── count_prime.js      # JavaScript implementation
├── count_prime.c       # C implementation
└── count_prime.py      # Python implementation
```
