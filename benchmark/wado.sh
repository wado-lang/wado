# The compiler a benchmark task runs. Sourced by each task in `mise.toml`.
#
# `WADO_BIN` names a prebuilt binary — the arm an A/B compares against, built by
# `mise run benchmark-baseline`. Unset, the task builds this tree's. Pointing the
# same harness at both binaries is what keeps a benchmark's own sources out of
# the comparison, so only the compiler differs.
wado() {
    if [ -n "${WADO_BIN:-}" ]; then
        "$WADO_BIN" "$@"
    else
        cargo run --release --manifest-path ../wado-cli/Cargo.toml --quiet -- "$@"
    fi
}
