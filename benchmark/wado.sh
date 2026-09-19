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

# The `wado run` flags a benchmark wants for the guest's GC heap, keyed by its
# source path. Only these two beat the CLI's 256 MiB default: microgpt holds a
# whole autograd graph live and gale_gen its grammar tables, so both pay for
# every collection and want room to allocate between them. Every other row is
# flat at 512 MiB or slower, so it takes the default.
gc_heap_flags() {
    case "$1" in
        *microgpt* | *gale_gen*) echo "--gc-heap-initial 512m" ;;
    esac
}
