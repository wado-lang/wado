---
name: profiling-wado-compiler
description: Profile the native Rust `wado` binary (compile/serve/run) for host-side bottlenecks — CPU with a sampling profiler, memory with the span trace's RSS and valgrind DHAT. Use for native CPU or memory profiling, not guest wasm (see wado-performance for that).
---

# Profiling the Native `wado` Binary

Host side only: the compiler, `serve`, `run`, wasmtime. The guest program is
`wado-performance`'s.

## Build

- `cargo build --profile profiling --bin wado` — release codegen with debug
  info, for what users run.
- `cargo build --bin wado` — dev, for iteration speed. Absolute numbers run
  high; ratios hold.

## CPU

Linux needs `perf_event_paranoid <= 1` (`echo 1 | sudo tee
/proc/sys/kernel/perf_event_paranoid`) and `addr2line` (binutils).

```sh
A=.claude/skills/profiling-wado-compiler/scripts
samply record --save-only --rate 1000 -o /tmp/prof.json -- \
  target/debug/wado test package-gale/tests/driver_rust_test.wado
node $A/analyze_native_profile.ts /tmp/prof.json   # --top 60, --binary wado-lsp, --under 'fn'
```

For a server, record it in the background, drive load, then SIGTERM the child,
not samply: `kill -TERM "$(pgrep -P "$SAMPLY_PID" | head -1)"; wait
"$SAMPLY_PID"`. `samply load` opens the call tree in a browser.

The analyzer weights by CPU, not wall-clock, and reports CPU by library, the
top self and inclusive frames (all, and `wado` only), syscall and allocator cost
credited to the nearest Rust caller, and allocation's share of total CPU by
requesting caller.

- Symbols resolve against the binary at the recorded path. Rebuilding there
  re-symbolicates old profiles to garbage that looks normal, so give each A/B
  arm its own copy.
- Identical monomorphizations fold to one address under whichever name sorts
  first, so one pass can appear to own the whole compiler's cost. A span that
  contradicts the profile is the tell; `nm target/debug/wado | grep <symbol>`
  counts the names sharing the address.
- On macOS, `atos` names kernel syscalls wrongly; read the nearest-Rust-caller
  view instead.
- Choose a target from a fresh profile, never a WEP's percentages, and profile
  again after fixing the top item. Estimate a change from where the time goes,
  not from what it removes: `-O3` is the inliner and the NIR fixed point.
- Validate with the profile, which is reproducible; throughput on a busy machine
  is not.

## Memory

Peak RSS first, then per phase, then per allocation site.

`--log-level debug` prints each span's current/peak RSS in MiB and the change
over the span (Linux). Read peak jumps too, since a span that frees what it
allocated nets near zero. RSS is process-wide, so use `wado test -p 1`.

DHAT sees only the system allocator, so build without `mimalloc`:

```sh
cargo build -p wado-cli --bin wado --no-default-features
cp target/debug/wado /tmp/wado-sysalloc
valgrind --tool=dhat --num-callers=40 --dhat-out-file=/tmp/dhat.json \
  /tmp/wado-sysalloc compile -O2 hello.wado -o /tmp/out.wasm
node $A/analyze_dhat.ts /tmp/dhat.json   # --where RE, --not RE, --stacks N
```

It reports what was live at the heap's peak, by site and by `wado` function.
