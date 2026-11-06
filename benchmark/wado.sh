# The compiler a benchmark task runs, sourced by each task in `mise.toml`:
# `WADO_BIN`'s prebuilt one — an A/B arm from `mise run benchmark-baseline` —
# or this tree's.
wado() {
    if [ -n "${WADO_BIN:-}" ]; then
        "$WADO_BIN" "$@"
    else
        cargo run --release --manifest-path ../wado-cli/Cargo.toml --quiet -- "$@"
    fi
}

# Every row runs at the CLI's 256 MiB default. These two benchmarks run again at
# a heap of their own, as a second row.

# gale_gen: how far its time falls once collections are rare.
GALE_GEN_BIG_HEAP="--gc-heap-initial 1g"

# microgpt: the size where train and infer cost least together. A collection
# copies the sample's graph so far, so the best size moves with what a sample
# allocates: sweep it again when that changes.
MICROGPT_TUNED_HEAP="--gc-heap-initial 320m"
