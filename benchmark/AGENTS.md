# benchmark

Performance benchmarks comparing Wado against C, JavaScript, Python, and Ruby.

## Setup

```sh
mise install  # node, python, ruby
```

C compiler (`cc`) and Rust (`cargo`) are expected from the system.

## Tasks

```sh
mise run count-prime  # integer arithmetic (count primes to 10M)
mise run mandelbrot   # float arithmetic (1024x768 fractal)
mise run sieve        # array operations (sieve of Eratosthenes to 10M)
mise run zlib         # compression (zlib-rs native vs Wado)
mise run clean        # remove build artifacts
```

## Structure

Each benchmark has its own directory with implementations in all languages side by side. The `zlib/` directory also contains a Rust (`Cargo.toml` + `zlib_rs.rs`) native reference.
