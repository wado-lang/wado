# benchmark

Performance benchmarks comparing Wado against C and JavaScript.

## Setup

```sh
mise install  # node, bun
```

C compiler (`cc`) and Rust (`cargo`) are expected from the system.

## Tasks

```sh
mise run count-prime   # integer arithmetic (count primes to 1M)
mise run mandelbrot    # float arithmetic (1024x768 fractal)
mise run sieve         # array operations (sieve of Eratosthenes to 10M)
mise run zlib          # compression (zlib-rs native vs Wado)
mise run fts           # float-to-string conversion
mise run json-twitter  # JSON deserialization (twitter.json)
mise run json-canada   # JSON deserialization (canada.json)
mise run json-catalog       # JSON deserialization (citm_catalog.json)
mise run sqlite-parse       # SQLite parsing (Gale vs sqlparser-rs)
mise run syntax-highlight   # syntax highlighting (Gale vs tree-sitter)
mise run clean              # remove build artifacts
```

## Profiling

```sh
wado run --profile guest prog.wado                  # cross-platform, writes profile.json
wado run --profile guest,out.json,5 prog.wado       # custom path, 5ms interval
perf record -k mono wado run --profile jitdump prog.wado  # Linux perf (detailed)
perf record -k mono wado run --profile perfmap prog.wado  # Linux perf (simple)
```

View guest profiles at https://profiler.firefox.com/. See `README.md` for full documentation.

## Updating Results

After running benchmarks, update `README.md` with the new results. Use the `/benchmark` skill or run `mise run benchmark-all` and `mise run report-wasm-size`, then update the tables in `README.md` accordingly.

## Structure

Each benchmark has its own directory with implementations in all languages side by side. The `zlib/` directory also contains a Rust (`Cargo.toml` + `zlib_rs.rs`) native reference. The `json_*` directories contain JSON parsing benchmarks with Rust `serde_json` as the native reference.
