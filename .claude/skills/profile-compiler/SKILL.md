---
name: profile-compiler
description: Profile the native Rust `wado` binary (compile/serve/run) with a sampling profiler to find host-side bottlenecks. Use for native CPU profiling, not guest wasm (see profiling-wado for that).
---

# Profiling the native `wado` binary

Host-side Rust profiling (the compiler, `wado serve`, `wado run`, …
including wasmtime/cranelift). For the **guest** wasm program, use
`profiling-wado` instead.

## Pick a build profile

Choose based on **what you're optimising for**:

| Profile | Cargo flag | Use when | Trade-off |
| --- | --- | --- | --- |
| `profiling` | `cargo build --profile profiling --bin wado` | Improving **benchmark scores** — release-equivalent codegen with debug info kept. Inherits `release` (thin LTO, `codegen-units=1`) and adds `debug = 2`, `strip = false`. | Slow build (LTO), but the CPU profile reflects what users actually run. |
| `dev` | `cargo build --bin wado` | Improving **developer-iteration time** — making `cargo run -- compile/test/...` faster for compiler-hackers. Uses the in-workspace `[profile.dev.package.wado-compiler] opt-level = 1`, so the compiler itself isn't molasses while everything else stays unoptimised. | Fast build, but the absolute numbers are larger than a release run; ratios between hot paths are still actionable. |

If unsure: pick `profiling` for "users complain it's slow", pick `dev`
for "rebuild → run → tweak feels slow during development." The
analyzer script and recording flow below are identical for both.

## Workflow

```sh
# 1. Build with the chosen profile (see table above)
cargo build --profile profiling --bin wado    # benchmark-oriented
# or
cargo build --bin wado                        # dev-iteration-oriented

# 2. Record under load with samply (cargo install samply)
samply record --save-only --rate 1000 -o /tmp/prof.json -- \
  target/profiling/wado serve --addr 127.0.0.1:8080 app.wado &
SAMPLY_PID=$!
# ... drive load (e.g. oha against benchmark/http_routing) ...

# 3. Stop: SIGTERM the CHILD, not samply. samply finalizes on child exit;
#    signalling samply leaves the child running and the recording hangs.
kill -TERM "$(pgrep -P "$SAMPLY_PID" | head -1)"; wait "$SAMPLY_PID"

# 4. Analyze
python3 .claude/skills/profile-compiler/scripts/analyze_native_profile.py /tmp/prof.json
```

For one-shot commands (`wado compile foo.wado`, `wado test foo.wado`)
there is nothing to drive — samply records until the child exits, so
just invoke it directly:

```sh
samply record --save-only --rate 1000 -o /tmp/prof.json -- \
  target/debug/wado test package-gale/tests/driver_rust_test.wado
```

Interactive call tree (and correct kernel symbols):
`samply load /tmp/prof.json` opens a browser-based call-tree UI. The
CLI analyzer below is for grep-able, transcript-friendly summaries.

## Linux setup

```sh
# samply needs perf_event_paranoid <= 1 for a non-root user
echo '1' | sudo tee /proc/sys/kernel/perf_event_paranoid

# `addr2line` is part of binutils — usually already installed
addr2line --version >/dev/null || sudo apt-get install -y binutils
```

## How `analyze_native_profile.py` works

samply's `--save-only` profile is **unsymbolicated**: `funcTable.name`
holds the hex relative-virtual-address (RVA), keyed by `(lib_index,
rva)` so the same hex address in two different libs is never merged.
The script:

1. **Auto-detects the symbolicator** (`--symbolicator auto`):
   - macOS → `atos -o <path> -arch <arch> -l <base> <addrs>`; the main
     executable's `__TEXT` base is `0x100000000`, shared dylibs use base 0.
   - Linux → `addr2line -fC -e <path>`; PIE binaries store RVAs directly
     in the profile (no base offset to add). The script reshapes the
     output to `<func> (in <lib>) (<file:line>)` so the
     `(in <binary>)` filter works on both platforms.
2. **Weights samples by `threadCPUDelta`** (real CPU), not wall-clock
   `weight` — otherwise parked tokio/rayon worker threads bury everything.
3. **Reports** four views:
   - **CPU by library (self)** — where the leaf frames land. A high
     `libc.so.6 / libsystem_*` ratio means your hot path is in
     syscalls/`memcpy`, not Rust code.
   - **Top SELF — all** — flat hot list with foreign code mixed in.
     Useful to spot allocator / hashing / `memcpy` pressure.
   - **Top SELF / INCLUSIVE — `wado` only** — the Rust-only view.
     INCLUSIVE is deduped per sample so recursive frames don't push
     percentages above 100%.
   - **Syscall/alloc CPU attributed to nearest Rust caller** — walks up
     each non-`wado` leaf stack until it finds a `wado` frame and
     credits the cost there. This is how you find which Rust function
     is responsible for the `__memcpy` / `mmap` / mimalloc hot spots.

Common invocations:

```sh
# Default: top 30, auto-symbolicator
python3 .claude/skills/profile-compiler/scripts/analyze_native_profile.py /tmp/prof.json

# Wider view; force Linux symbolicator even on macOS
python3 .claude/skills/profile-compiler/scripts/analyze_native_profile.py \
  /tmp/prof.json --top 60 --symbolicator addr2line

# Profile a different binary
python3 .claude/skills/profile-compiler/scripts/analyze_native_profile.py \
  /tmp/prof.json --binary wado-lsp
```

## Non-obvious points

- **Read CPU, not wall-clock.** The script weights by `threadCPUDelta`;
  otherwise parked tokio/rayon worker threads bury everything.
- **Kernel syscall names from `atos` are wrong** (shared-cache base
  offset). Read syscall cost via the script's "nearest Rust caller"
  attribution, not the syscall name.
- **`addr2line` outermost-frame only.** `-i` (inlined frames) is
  intentionally omitted because addr2line does not emit a per-input
  separator with `-i`, so the output cannot be reliably split back to
  addresses. The outermost frame matches the self-CPU bucket — which
  is what you want. If you need inlined frames, use `samply load`.
- **Symbolication needs the matching binary.** A saved profile holds only
  addresses; the script resolves them against the binary at the recorded
  path, so rebuilding `target/profiling/wado` (or `target/debug/wado`)
  makes earlier profiles re-symbolicate to garbage. Analyze before
  rebuilding, or keep the matching binary.
- **Validate with the profile, not req/s or wall time.** The CPU
  breakdown is reproducible run to run; throughput on a busy dev
  machine swings by tens of percent. Use the profile to confirm a
  change landed (e.g. a hot function shrank); measure absolute
  throughput on a quiet/target host.
- **Linux profile includes the ld-linux frames.** Unwinder occasionally
  attributes a frame to `ld-linux-x86-64.so.2` when the RVA happens to
  collide between the main binary and the dynamic linker mapping. The
  library breakdown's self-CPU view (which uses the leaf frame's lib)
  is the trustworthy ratio — incl-by-lib can over-count those frames.
