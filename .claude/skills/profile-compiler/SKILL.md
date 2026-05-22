---
name: profile-compiler
description: Profile the native Rust `wado` binary (compile/serve/run) with a sampling profiler to find host-side bottlenecks. Use for native CPU profiling, not guest wasm (see profiling-wado for that).
---

# Profiling the native `wado` binary

Host-side Rust profiling (the compiler, `wado serve`, `wado run`, …
including wasmtime/cranelift). For the **guest** wasm program, use
`profiling-wado` instead.

## Workflow

```sh
# 1. Build with symbols (release is stripped; `profiling` inherits it + keeps DWARF)
cargo build --profile profiling --bin wado

# 2. Record under load with samply (brew install samply)
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

Interactive call tree (and correct kernel symbols): `samply load /tmp/prof.json`.

## Non-obvious points

- **Read CPU, not wall-clock.** The script weights by `threadCPUDelta`;
  otherwise parked tokio/rayon worker threads bury everything.
- **Kernel syscall names from `atos` are wrong** (shared-cache base
  offset). Read syscall cost via the script's "nearest Rust caller"
  attribution, not the syscall name.
- **macOS only** (`atos`). On Linux use `perf` or `samply load`; the
  weighting/attribution logic ports, the `atos` symbolication does not.
