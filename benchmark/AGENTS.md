# benchmark

Performance benchmarks comparing Wado against C and JavaScript.

## Setup

```sh
mise install  # node, bun
```

C compiler (`cc`) and Rust (`cargo`) are expected from the system.

## Tasks

Benchmarks are grouped into four sections: pure computation (純粋計算),
serde & compression (serde・圧縮), parser (パーサー), and application server.

```sh
# 純粋計算 (pure computation)
mise run count-prime   # integer arithmetic (count primes to 1M)
mise run mandelbrot    # float arithmetic (1024x768 fractal)
mise run sieve         # array operations (sieve of Eratosthenes to 10M)
mise run fts           # float-to-string conversion

# serde・圧縮 (serde & compression)
mise run json-twitter  # JSON ser/de (twitter.json)
mise run cbor          # CBOR ser/de (twitter.json, schema shared with json-twitter)
mise run json-canada   # JSON ser/de (canada.json)
mise run json-catalog  # JSON ser/de (citm_catalog.json)
mise run zlib          # compression (zlib-rs native vs Wado)

# パーサー (parser)
mise run sqlite-parse       # SQLite parsing (Gale vs sqlparser-rs)
mise run syntax-highlight   # syntax highlighting (Gale vs tree-sitter)

# application server
mise run http-routing       # HTTP routing (wado serve vs Hono vs Axum)

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

Each benchmark has its own directory with implementations in all languages side by side. The `zlib/` directory also contains a Rust (`Cargo.toml` + `zlib_rs.rs`) native reference. The `json_*` directories contain JSON ser/de benchmarks with Rust `serde_json` as the native reference; the `cbor/` directory holds the CBOR ser/de benchmark with `serde_cbor` (Rust) and `@msgpack/msgpack` (JS) as references. `json_twitter/twitter_schema.wado` defines the full twitter.json schema shared by both the JSON and CBOR benchmarks, so the two codecs are compared over identical data types.
