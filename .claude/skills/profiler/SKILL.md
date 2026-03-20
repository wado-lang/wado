---
name: profiler
description: Profile Wado programs using wasmtime's jitdump profiler and Linux perf.
---

# Profiling Wado Programs

Profile Wado programs with `--profile jitdump` and Linux `perf` to identify hot functions and instructions.

## Why jitdump?

Wado aggressively inlines functions, so hot functions are often large inlined blobs. Function-level profiling ("run is hot") is not actionable. JitDump enables instruction-level annotation via `perf annotate`, revealing which inlined callee is the actual bottleneck.

The guest profiler (`--profile guest`) does not work with the current CM-async runtime (produces 0 samples).

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

### 3. Report (function-level)

```sh
perf report -i perf.jit.data
```

This shows which functions consume the most samples. Look for `wasm[1]::function[N]::name` entries — these are user Wasm functions compiled from Wado source.

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

## Symbol naming

Symbols include full monomorphization detail:

```
wasm[1]::function[78]::Status^Deserialize::deserialize<JsonDeserializer>
wasm[1]::function[48]::deflate_with_level
```

Each function has both a long form (`wasm[1]::function[N]::name`) and a short alias (`name`).

## Cleanup

JitDump creates `jit-<pid>.dump` files (500-600 KB each) in the current working directory. Remove after analysis:

```sh
rm -f jit-*.dump perf.data perf.jit.data
```

## Notes

- Runtime overhead is ~1% — measurements are not significantly distorted.
- `perf record` requires Linux. There is no macOS equivalent for jitdump.
- For background, see `docs/research/wasmtime-profiler-characteristics.md`.
