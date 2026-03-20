# Wasmtime Profiler Characteristics Research

Date: 2026-03-20

## Overview

Wasmtime provides 3 profiling modes. This document summarizes the characteristics
of each based on running `zlib` (compress+decompress) and `json-twitter` (JSON parsing)
benchmarks in the Wado project (wasmtime 42, component-model-async enabled).

## Benchmark Results

### zlib (100KB x 10 iterations, compress + decompress)

| Mode     | Run 1 | Run 2 | Run 3 | Run 4 | Run 5 | Median | Overhead |
|----------|-------|-------|-------|-------|-------|--------|----------|
| none     | 360   | 362   | 363   | 361   | 387   | 362 ms | baseline |
| guest    | 380   | 405   | 418   | 391   | 386   | 391 ms | +8.0%    |
| jitdump  | 367   | 363   | 396   | 363   | 389   | 367 ms | +1.4%    |
| perfmap  | 356   | 355   | 356   | 392   | 365   | 356 ms | -1.7%*  |

\* Within measurement noise — effectively zero overhead.

### json-twitter (631KB JSON, 100 statuses)

| Mode     | Run 1  | Run 2  | Run 3  | Run 4  | Run 5  | Median  | Overhead |
|----------|--------|--------|--------|--------|--------|---------|----------|
| none     | 41.5   | 41.4   | 41.4   | 45.3   | 43.1   | 41.5 ms | baseline |
| guest    | 47.9   | 43.4   | 44.3   | 42.4   | 42.6   | 43.4 ms | +4.6%    |
| jitdump  | 41.8   | 41.8   | 44.6   | 41.5   | 41.5   | 41.8 ms | +0.7%    |
| perfmap  | 42.7   | 41.1   | 41.4   | 42.7   | 44.6   | 42.7 ms | +2.9%    |

## Profiler Characteristics

### 1. Guest Profiler (`--profile guest`)

**Mechanism:** Epoch-based sampling profiler built into wasmtime. A background thread
increments the engine's epoch counter at a configurable interval (default: 10ms).
When the epoch deadline is reached, a callback invokes `GuestProfiler::sample()` to
capture the Wasm call stack.

**Output:** Firefox Profiler JSON format (`profile.json`). Can be viewed at
`https://profiler.firefox.com/`.

**Strengths:**
- Cross-platform (works on Linux, macOS, Windows)
- Self-contained — no external tools required
- Configurable sampling interval (down to 1ms)
- Output includes call stacks with function names

**Weaknesses:**
- **Does not work with component-model-async (CM-async):** In the current Wado
  runtime (wasmtime 42 with `component_model_async`, `component_model_async_builtins`,
  `component_model_async_stackful` enabled), the guest profiler consistently produces
  **0 samples**. The epoch deadline callback (`store.epoch_deadline_callback`) appears
  to conflict with the concurrent execution model used by CM-async's `call_async` path,
  which uses a different task scheduling mechanism than the traditional fiber-based async.
- Highest runtime overhead (~5-8%) due to epoch interruption instrumentation
- Sampling resolution limited to function entry points and loop headers
- Function names may appear as `wasm function N` without debug info

**Output files:** ~5-7 KB (empty due to 0 samples in CM-async mode)

**Verdict:** Currently **non-functional** for Wado's CM-async runtime. Would need
wasmtime fixes or a different integration approach to work with CM-async.

### 2. JitDump Profiler (`--profile jitdump`)

**Mechanism:** Writes a JIT dump file (`jit-<pid>.dump`) containing JIT-compiled code
regions with their memory addresses, sizes, and symbol names. Designed to integrate
with Linux `perf` via `perf inject --jit`.

**Output:** Binary `jit-<pid>.dump` file in the current working directory.

**Usage workflow:**
```sh
perf record -k mono wado run --profile jitdump prog.wado
perf inject --jit --input perf.data --output perf.jit.data
perf report --input perf.jit.data
```

**Strengths:**
- Extremely low runtime overhead (~1%) — only writes symbol info at JIT time
- Full system-level profiling (guest Wasm + host runtime + kernel)
- Instruction-level precision when combined with `perf`
- Rich symbol information: 69 functions for zlib, 148 for json-twitter
- Includes function names from Wado source (e.g., `inflate_raw_ex`, `_status_field_lookup`)
- Works correctly with CM-async

**Weaknesses:**
- Linux-only (requires `perf`)
- Requires post-processing with `perf inject --jit`
- Largest output files (~550-615 KB) because it includes JIT machine code
- Requires `perf record` wrapper — can't profile standalone

**Output files:** 553 KB (zlib), 615 KB (json-twitter)

**Verdict:** Best choice for detailed performance analysis on Linux. The `perf inject`
step adds complexity but provides the richest profiling data.

### 3. PerfMap Profiler (`--profile perfmap`)

**Mechanism:** Writes a `/tmp/perf-<pid>.map` text file mapping JIT-compiled code
addresses to symbol names. This is the simplest integration with Linux `perf` —
`perf report` can read these map files directly without the `perf inject` step.

**Output:** Text file `/tmp/perf-<pid>.map` with format: `<address> <size> <name>`.

**Usage workflow:**
```sh
perf record -k mono wado run --profile perfmap prog.wado
perf report --input perf.data
# Or with samply:
samply record wado run --profile perfmap prog.wado
```

**Strengths:**
- Effectively zero runtime overhead (within measurement noise)
- Simpler workflow than jitdump — no `perf inject` step needed
- Compatible with `samply` for Firefox Profiler visualization
- Works correctly with CM-async
- Small output files (~15-27 KB)
- Rich symbol information: 258 entries for zlib, 446 for json-twitter
  (more entries than jitdump because it includes component trampolines)

**Weaknesses:**
- Linux-only (requires `perf` or `samply`)
- Less precise than jitdump — no machine code embedded in the map file
- Map files accumulate in `/tmp` and are not auto-cleaned

**Output files:** 15 KB (zlib), 27 KB (json-twitter)

**Verdict:** Best choice for quick, low-overhead profiling on Linux. Simpler workflow
than jitdump with comparable symbol quality.

## Comparison Summary

| Feature              | guest            | jitdump           | perfmap          |
|----------------------|------------------|-------------------|------------------|
| Platform             | Cross-platform   | Linux only        | Linux only       |
| External tool needed | None             | `perf`            | `perf`/`samply`  |
| Runtime overhead     | ~5-8%            | ~1%               | ~0%              |
| CM-async compatible  | **No** (0 samps) | Yes               | Yes              |
| Output format        | JSON             | Binary dump       | Text map         |
| Output size (zlib)   | 5 KB (empty)     | 553 KB            | 15 KB            |
| Post-processing      | None             | `perf inject`     | None             |
| Profiling scope      | Guest only       | Guest + host + OS | Guest + host + OS|
| Symbol quality       | N/A (broken)     | Good (69/148 fns) | Good (258/446)   |
| Visualization        | Firefox Profiler | `perf report`     | `perf`/`samply`  |

## Key Finding: Guest Profiler Incompatibility with CM-async

The guest profiler is architecturally incompatible with wasmtime's component-model-async
execution mode used in Wado. The root cause:

1. The guest profiler uses `epoch_deadline_callback` to sample Wasm call stacks
2. With CM-async (`wasm_component_model_async + async_builtins + async_stackful`),
   `TypedFunc::call_async` enters the concurrent execution path (`concurrency_support()`)
3. This concurrent path manages tasks differently from the traditional async fiber model
4. The epoch callback either never fires or fires when no Wasm frames are on the stack

This means the guest profiler requires either:
- Wasmtime-side fixes to support profiling in CM-async mode
- A separate profiling approach that doesn't rely on epoch interruption

## Recommendations

1. **For production profiling:** Use `perfmap` — zero overhead, simple workflow,
   works with CM-async
2. **For deep analysis:** Use `jitdump` — negligible overhead, instruction-level
   precision with `perf`
3. **For cross-platform:** The guest profiler needs fixes before it can be used
   with Wado's CM-async runtime. Consider filing an issue upstream with wasmtime.
