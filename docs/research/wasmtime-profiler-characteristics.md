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
# Instruction-level annotation:
perf annotate --input perf.jit.data -s <function_name>
```

**Strengths:**
- Extremely low runtime overhead (~1%) — only writes symbol info at JIT time
- Full system-level profiling (guest Wasm + host runtime + kernel)
- Instruction-level precision when combined with `perf annotate`
- Rich symbol information with full monomorphized names
- Works correctly with CM-async

**Weaknesses:**
- Linux-only (requires `perf`)
- Requires post-processing with `perf inject --jit`
- Largest output files (~550-615 KB) because it includes JIT machine code
- `jit-<pid>.dump` files created in the current working directory (need manual cleanup)

**Output files:** 553 KB (zlib), 615 KB (json-twitter)

**Verdict:** Use when you need instruction-level hot-spot analysis within a specific
function.

### 3. PerfMap Profiler (`--profile perfmap`)

**Mechanism:** Writes a `/tmp/perf-<pid>.map` text file mapping JIT-compiled code
addresses to symbol names. This is the simplest integration with Linux `perf` —
`perf report` can read these map files directly without the `perf inject` step.

**Output:** Text file `/tmp/perf-<pid>.map` with format: `<address> <size> <name>`.

**Usage workflow:**
```sh
perf record -k mono wado run --profile perfmap prog.wado
perf report --input perf.data
# Or with samply (best UX — Firefox Profiler auto-opens):
samply record wado run --profile perfmap prog.wado
```

**Strengths:**
- Effectively zero runtime overhead (within measurement noise)
- Simpler workflow than jitdump — no `perf inject` step needed
- Compatible with `samply` for Firefox Profiler visualization
- Works correctly with CM-async
- Small output files (~15-27 KB)

**Weaknesses:**
- Linux-only (requires `perf` or `samply`)
- Function-level granularity only — no instruction-level annotation
- Map files accumulate in `/tmp` and are not auto-cleaned

**Output files:** 15 KB (zlib), 27 KB (json-twitter)

**Verdict:** Best default choice. Start here for any profiling task.

## Deep Comparison: PerfMap vs JitDump

Since the guest profiler is non-functional with CM-async, the practical choice is
between PerfMap and JitDump. Both produce **identical symbol sets** (same function
names, same monomorphization detail), but differ in what analysis they enable.

### Symbol Quality (Identical)

Both formats provide the same 258 symbols for zlib (129 unique functions × 2
because each function has both a `wasm[1]::function[N]::name` and a short alias).
Symbol names include full monomorphization detail:

```
wasm[1]::function[72]::TwitterResponse^Deserialize::deserialize<JsonDeserializer>
wasm[1]::function[78]::Status^Deserialize::deserialize<JsonDeserializer>
wasm[1]::function[84]::UserMention^Deserialize::deserialize<JsonDeserializer>
```

This means `perf report` can distinguish different monomorphized instances of
the same generic function (e.g., `deserialize` for `Status` vs `UserMention`).

### Data Granularity

| Capability                    | PerfMap              | JitDump                        |
|-------------------------------|----------------------|--------------------------------|
| Function-level profiling      | Yes                  | Yes                            |
| Which function is hot         | Yes                  | Yes                            |
| Call graph (flat)             | Yes                  | Yes                            |
| Call graph (with `--call-graph=dwarf`) | Host frames only | Host frames only       |
| Instruction-level annotation  | **No**               | **Yes** (`perf annotate`)      |
| Machine code disassembly      | **No**               | **Yes** (code embedded in dump)|
| Source-line mapping (DWARF)   | No                   | No (no DEBUG_INFO records)     |

Key difference: JitDump embeds the actual x86-64 machine code for every JIT-compiled
function. This enables `perf annotate` to disassemble the function and attribute
sample counts to individual instructions:

```
perf annotate --input perf.jit.data -s inflate_raw_ex
```

This tells you not just "inflate_raw_ex is hot" but *which loop or instruction
within inflate_raw_ex* is the bottleneck.

PerfMap only maps address ranges to function names. `perf report` shows which function
is hot, but cannot zoom in further.

Note: Neither format includes DWARF debug info (JitDump has CODE_LOAD records only,
no DEBUG_INFO records), so neither can map back to Wado source lines. Attribution
is at the machine-code instruction level (jitdump) or function level (perfmap).

### Function Coverage

For json-twitter (a real-world JSON parsing workload):

| Category                    | Count | Example                                             |
|-----------------------------|-------|------------------------------------------------------|
| User Wasm functions         | 124   | `Status^Deserialize::deserialize<JsonDeserializer>` |
| Internal Wasm (grow/realloc)| 2     | `grow_memory`, `realloc`                            |
| Trampolines                 | 176   | `wasm_to_array_trampoline`, `native_to_wasm`        |
| Wasmtime builtins           | 18    | `wasmtime_builtin_gc_alloc_raw`                     |
| Component trampolines       | 48    | `component-trampolines[N]-array-call-*`             |

Function size distribution (user Wasm, json-twitter):

```
  < 100 bytes:    17 functions
  100-500 bytes:  33 functions
  500-2KB:        35 functions
  2KB-10KB:       36 functions
  > 10KB:          3 functions (largest: 20.2 KB)
```

Both profilers cover all these categories equally.

### Workflow Complexity

| Step                        | PerfMap                                    | JitDump                                    |
|-----------------------------|--------------------------------------------|--------------------------------------------|
| 1. Record                   | `perf record -k mono wado run --profile perfmap ...` | `perf record -k mono wado run --profile jitdump ...` |
| 2. Post-process             | —                                          | `perf inject --jit -i perf.data -o perf.jit.data` |
| 3. Report                   | `perf report`                              | `perf report -i perf.jit.data`            |
| 4. Annotate (optional)      | N/A                                        | `perf annotate -i perf.jit.data -s func`  |
| Alternative                 | `samply record wado run --profile perfmap ...` | —                                     |
| Cleanup                     | `/tmp/perf-*.map` (small)                  | `jit-*.dump` in CWD (550-615 KB each)    |

### Output Size

| Benchmark     | PerfMap | JitDump | Ratio  |
|---------------|---------|---------|--------|
| zlib          | 15 KB   | 553 KB  | 37×    |
| json-twitter  | 27 KB   | 615 KB  | 23×    |

JitDump files are 23-37× larger because they contain actual machine code bytes.

## Decision Guide: Which Profiler to Use?

```
Is the guest profiler working? (CM-async disabled)
├── Yes → Use guest (cross-platform, self-contained, Firefox Profiler UI)
└── No (CM-async enabled, as in Wado) → Use jitdump
```

**Recommendation: Use `jitdump` as the default profiler for Wado.**

Wado's compiler aggressively inlines functions (`#[inline]`, `#[inline(always)]`,
and optimizer-driven inlining). This means the "hot function" in a profile is often
a large inlined blob (e.g., `run` at 7.6 KB containing dozens of inlined callees).
Function-level attribution (perfmap) would only tell you "run is hot" — not *which
inlined callee* within `run` is the actual bottleneck.

JitDump solves this because `perf annotate` shows instruction-level sample attribution
within the inlined function body. Even without DWARF debug info, you can identify
hot loops and correlate instruction patterns back to specific Wado source operations.

**When to use perfmap instead:**
- Quick sanity checks where you only need "is this function hot at all?"
- When using `samply` for Firefox Profiler visualization (samply reads perfmap but
  not jitdump)
- When output file size matters (perfmap is 23-37× smaller)

**Typical workflow:**
```sh
# 1. Record with jitdump
perf record -k mono wado run --profile jitdump prog.wado

# 2. Inject JIT symbols
perf inject --jit -i perf.data -o perf.jit.data

# 3. See which functions are hot
perf report -i perf.jit.data

# 4. Drill into a hot function's instructions
perf annotate -i perf.jit.data -s run
```

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
