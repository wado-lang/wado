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
# source path. Only microgpt beats the CLI's 256 MiB default: it holds a whole
# autograd graph live, so it pays for every collection and wants room to
# allocate between them. Every other row is flat at 512 MiB or slower, so it
# takes the default. gale_gen runs at the default and again at
# `GALE_GEN_BIG_HEAP` as a row of its own.
gc_heap_flags() {
    case "$1" in
        *microgpt*) echo "--gc-heap-initial 512m" ;;
    esac
}

# gale_gen's second row: how far its time falls once collections are rare.
GALE_GEN_BIG_HEAP="--gc-heap-initial 1g"
