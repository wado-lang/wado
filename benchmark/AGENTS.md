# benchmark

Performance benchmarks comparing Wado against C, Rust, JavaScript and Java.
The `benchmark` skill runs the suite and updates `README.md`; the
`wado-performance` skill measures a change, and its `dead-ends.md` records what
was measured and dropped.

## Setup

`mise install` (node, bun); `cc` and `cargo` come from the system.

## Tasks

```sh
# pure computation
mise run microgpt      # autograd object graph (GPT training + inference)
mise run mandelbrot    # float arithmetic (1024x768 fractal)
mise run sieve         # array operations (sieve of Eratosthenes to 2M)
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

`pick.ts` takes the best run per row and `ab.ts` compares two compilers; both
parse through `logs.ts`.

## Structure

Each benchmark directory holds every language's implementation side by side.

- `microgpt/` is one autograd program in Wado, JavaScript and Rust (plain
  `rustc`, no crates), timing `train` and `infer`. The arms print the same loss
  and sample, so a divergence is a bug.
- `gale_gen/` runs the Gale generator in-process over the Rust grammar, against
  ANTLR4's `org.antlr.v4.Tool` over the same `.g4`. `sqlite_parse/` adds an
  ANTLR4 Java parser generated from the same `SQLite.g4` Gale uses. Both need
  `java`/`javac`, cache the jar in `~/.cache/gale`, and skip without them.
- The `json_*` directories each define a schema module that `cbor/` imports too,
  so the two codecs run over the same types; `serde_json` and `serde_cbor` are
  the references.
