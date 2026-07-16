# benchmark

Performance benchmarks comparing Wado against C and JavaScript.

## Setup

```sh
mise install  # node, bun
```

C compiler (`cc`) and Rust (`cargo`) are expected from the system.

## Tasks

Benchmarks are grouped into four sections: pure computation, serde &
compression, parsing, and application server.

```sh
# pure computation
mise run count-prime   # integer arithmetic (count primes to 1M)
mise run mandelbrot    # float arithmetic (1024x768 fractal)
mise run sieve         # array operations (sieve of Eratosthenes to 10M)
mise run fts           # float-to-string conversion

# serde & compression
mise run json-twitter  # JSON ser/de (twitter.json)
mise run json-canada   # JSON ser/de (canada.json)
mise run json-catalog  # JSON ser/de (citm_catalog.json)
mise run cbor          # CBOR ser/de (twitter/canada/catalog, schemas shared with json-*)
mise run zlib          # compression (zlib-rs native vs Wado)

# parsing
mise run sqlite-parse       # SQLite parsing (Gale vs sqlparser-rs vs ANTLR4 Java, same SQLite.g4)
mise run syntax-highlight   # syntax highlighting (Gale vs tree-sitter)
mise run gale-gen           # Gale generator vs ANTLR4 over the same .g4

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

Run the suite three times, capturing each to a log, then use `pick.ts` to select the best of the runs per row (best of three absorbs cloud-VM noise):

```sh
for i in 1 2 3; do mise run all > run$i.log 2>&1; done
node pick.ts run1.log run2.log run3.log
```

`pick.ts` keys each row by (task, implementation, phase) and picks the run with the lowest ms/iter — the true best throughput — so a rounding tie between runs can't select the wrong ms/iter.

## Structure

Each benchmark has its own directory with implementations in all languages side by side. The `gale_gen/` benchmark measures parser-generator throughput over a Rust grammar. The Gale generator runs in-process (grammar assembly + codegen over `package-gale/tests/grammars/RustLexer.g4` + `RustParser.g4`, embedded via `#include_str`); its head-to-head reference is ANTLR4 over the _same_ `.g4` (`antlr4_gen.mjs`; needs `java`, fetches the jar to `~/.cache/gale`, skipped if java is absent). The `sqlite_parse/` directory adds, next to the Rust `sqlparser-rs` reference, an ANTLR4 Java parser generated at run time from the _same_ `SQLite.g4` the Gale row uses (`Antlr4SqliteBench.java` + `antlr4_java_bench.mjs`; needs `java`/`javac`, shares the jar cache with gale-gen, skipped if absent) — the runtime counterpart to gale-gen's generate-time ANTLR4 comparison. The `zlib/` directory also contains a Rust (`Cargo.toml` + `zlib_rs.rs`) native reference. The `json_*` directories contain JSON ser/de benchmarks with Rust `serde_json` as the native reference; the `cbor/` directory holds the CBOR ser/de benchmarks (twitter, canada, catalog) with `serde_cbor` (Rust) as the reference. Each `json_*` directory defines a shared schema module (`twitter_schema.wado`, `canada_schema.wado`, `catalog_schema.wado`) imported by both the JSON and CBOR benchmarks, so the two codecs are compared over identical data types.
