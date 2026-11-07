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

# The semi-space size, in MiB, that the README's sweep found best for microgpt's
# JavaScript row. V8 left to resize it makes that row swing between runs.
MICROGPT_NODE_SEMI_SPACE=128

# One benchmark's two Wado rows: `$1` with its recorded profile, and again
# without one, which is how the C and Rust rows beside it are built.
wado_rows() {
    echo "=== $1 ==="
    wado run -O2 $WADO_BENCH_FLAGS "$2"
    echo "=== $1 (no-pgo) ==="
    wado run -O2 $WADO_BENCH_FLAGS -f no-pgo "$2"
}
