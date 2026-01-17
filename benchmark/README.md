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
├── mandelbrot.{wado,c,js,py,rb}
├── count_prime.{wado,c,js,py,rb}
└── sieve.{wado,c,js,py,rb}
```
