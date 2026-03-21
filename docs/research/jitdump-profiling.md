# JitDump Profiling with Wasmtime

Date: 2026-03-21

## Overview

Wasmtime supports JitDump profiling (`--profile jitdump`), which integrates with Linux `perf` to provide instruction-level profiling of JIT-compiled WebAssembly code. This document describes the mechanism, workflow, and characteristics.

## Mechanism

When `--profile jitdump` is enabled, wasmtime writes a binary `jit-<pid>.dump` file containing JIT-compiled code regions with their memory addresses, sizes, and symbol names. This file follows the JitDump format recognized by Linux `perf`.

The dump file includes actual machine code bytes for every JIT-compiled function, enabling `perf annotate` to disassemble functions and attribute sample counts to individual instructions.

## Workflow

### 1. Record

```sh
perf record -k mono wado run --profile jitdump prog.wado
```

Or with cargo during development:

```sh
perf record -k mono cargo run --release --bin wado -- run --profile jitdump prog.wado
```

### 2. Inject JIT symbols

```sh
perf inject --jit -i perf.data -o perf.jit.data
```

This reads the `jit-<pid>.dump` file and creates a new perf data file with JIT symbol information embedded.

### 3. Report (function-level)

```sh
perf report -i perf.jit.data
```

Shows which functions consume the most samples. Look for `wasm[1]::function[N]::name` entries — these are user Wasm functions compiled from Wado source.

### 4. Annotate (instruction-level)

```sh
perf annotate -i perf.jit.data -s <function_name>
```

For example:

```sh
perf annotate -i perf.jit.data -s run
perf annotate -i perf.jit.data -s inflate_raw_ex
```

This disassembles the function and shows per-instruction sample percentages. Use this to identify hot loops within inlined code.

## Symbol Naming

Symbols include full monomorphization detail:

```
wasm[1]::function[78]::Status^Deserialize::deserialize<JsonDeserializer>
wasm[1]::function[48]::deflate_with_level
```

Each function has both a long form (`wasm[1]::function[N]::name`) and a short alias (`name`).

## Characteristics

- **Runtime overhead:** ~1% — measurements are not significantly distorted
- **Output size:** 550–615 KB per dump file (contains actual machine code bytes)
- **Platform:** Linux only (requires `perf`)
- **Granularity:** Instruction-level when combined with `perf annotate`
- **CM-async compatibility:** Works correctly

## When to Use

JitDump is most valuable when function-level profiling is insufficient. Wado's compiler aggressively inlines functions, so hot functions are often large inlined blobs. Function-level attribution would only tell you "run is hot" — not which inlined callee within `run` is the bottleneck. JitDump + `perf annotate` shows instruction-level sample attribution within the inlined function body.

## Cleanup

JitDump creates `jit-<pid>.dump` files in the current working directory. Remove after analysis:

```sh
rm -f jit-*.dump perf.data perf.jit.data
```

## Comparison with Other Wasmtime Profilers

See [wasmtime-profiler-characteristics.md](./wasmtime-profiler-characteristics.md) for a detailed comparison of all three wasmtime profiling modes (guest, jitdump, perfmap).
