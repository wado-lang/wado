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

# Every row runs at the CLI's 256 MiB default. gale_gen runs again with
# collections rare, as a second row, to show how far its time falls.
GALE_GEN_BIG_HEAP="--gc-heap-initial 1g"
