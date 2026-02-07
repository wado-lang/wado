# Profile: `update-golden-fixtures` Performance

## Summary

`make update-golden-fixtures` takes **2m21s** for 588 fixture files.
The dominant bottleneck is **redundant `build_from_stdlib()` calls** that re-parse WASI stdlib modules repeatedly.

## Measurement Setup

- Command: `cargo run --bin wado -- dump --optimize --unparse -O2 -o ... wado-compiler/tests/fixtures/*.wado`
- 588 fixture files, batch mode (single process invocation)

## Per-File Pipeline Breakdown

Each file takes ~250ms regardless of file size (the cost is dominated by stdlib processing):

| Phase | Time | Share | Notes |
|-------|------|-------|-------|
| Lexer | ~0.1ms | <0.1% | |
| Parser | ~0.1ms | <0.1% | |
| Bind | ~0.1ms | <0.1% | |
| Desugar | ~0.1ms | <0.1% | |
| **Load** | **~19ms** | **8%** | Re-parses all stdlib modules from scratch |
| Analyze | ~0.5ms | 0.2% | |
| **Resolve** | **~125ms** | **50%** | Rebuilds registries 9 times per file |
| Monomorphize | ~9ms | 4% | |
| Lower | ~1.5ms | 0.6% | |
| **Optimize** | **~93ms** | **37%** | Rebuilds registries 3 more times |
| **Total** | **~250ms** | **100%** | |

## Root Cause: `build_from_stdlib()` Called 12 Times Per File

`WasiRegistry::build_from_stdlib()` parses 6 WASI modules (2,702 lines) every call.
`BuiltinRegistry::build_from_stdlib()` parses `core:builtin` (504 lines) every call.

Per-file call count (measured with atomic counters):

| Location | WasiRegistry | BuiltinRegistry | Cost per call |
|----------|--------------|-----------------|---------------|
| `resolver.rs:1152` (inside per-module loop) | 9x | 9x | ~8ms / ~1.4ms |
| `optimize_dce.rs:54` (`analyze_project`) | 1x | 0x | ~8ms |
| `optimize_dce.rs:213` | 0x | 1x | ~1.4ms |
| `optimize_dce.rs:366-374` (`populate_all_features`) | 1x | 1x | ~8ms / ~1.4ms |
| `lib.rs:567-572` (dump_with_host Phase 10) | 1x | 1x | ~8ms / ~1.4ms |
| **Total per file** | **12x** | **11x** | |

### Cost Calculation

- WasiRegistry: 12 calls × ~8ms = **~96ms** (39% of pipeline)
- BuiltinRegistry: 11 calls × ~1.4ms = **~15ms** (6% of pipeline)
- **Combined: ~111ms per file (45% of total pipeline)**
- **For 588 files: ~65 seconds wasted on redundant registry rebuilds**

## Additional Findings

### No Cross-File Caching

Each fixture file runs the full compilation pipeline from scratch:

- stdlib is re-loaded, re-parsed, re-analyzed, re-resolved, and re-optimized for every file
- No state is shared between files in batch mode (`run_bulk` in `dump.rs`)

### Stdlib Dominates All Phases

For a minimal `fn run() {}` file, the pipeline takes the same ~250ms as for complex files because:

- Load phase processes 14 stdlib files (4,751 lines total)
- Resolve phase processes ~9 modules (mostly stdlib)
- Optimize phase runs 10 iterations over all modules including stdlib

## Recommendations (by impact)

### 1. Hoist `build_from_stdlib` out of per-module loop in resolver (estimated: -70ms/file, -41s total)

In `resolver.rs:1069-1195`, move lines 1152-1153 before the `for module_source in &sorted_sources` loop.
Build once, pass references to each `Resolver` instance.

### 2. Cache registries in `Project` struct (estimated: -30ms/file, -18s total)

`Project::new()` already receives `wasi_registry`, `world_registry`, `builtin_registry`.
But `optimize_dce.rs` rebuilds them from scratch instead of using the cached copies.
Pass `&project.wasi_registry` to `analyze_project()` and `populate_all_features()`.

### 3. Cross-file stdlib caching in bulk mode (estimated: -100ms/file, -59s total)

For `dump --optimize ... *.wado`, load and compile stdlib once, then for each file only process the entry module's delta. This requires refactoring `dump_with_host` to accept pre-compiled stdlib state.

### 4. Skip stdlib in optimization iterations (estimated: -20ms/file, -12s total)

Stdlib functions are stable across files. The 10 optimization iterations (inlining, copy propagation, constant folding, LICM, ref elimination) process all modules including stdlib. Consider skipping stdlib modules in optimization passes, only running DCE analysis on the full call graph.

### Total Potential Improvement

Applying recommendations 1-3 could reduce `update-golden-fixtures` from **~141s to ~30-40s** (3-4x speedup).

## Call Sites Reference

```
wado-compiler/src/resolver.rs:1152-1153      (in per-module loop - 9 calls)
wado-compiler/src/resolver.rs:579-581         (in Resolver::new - separate path)
wado-compiler/src/resolver.rs:10043-10048     (in resolve_to_project)
wado-compiler/src/optimize_dce.rs:54          (in analyze_project)
wado-compiler/src/optimize_dce.rs:213         (in analyze_project)
wado-compiler/src/optimize_dce.rs:366-374     (in populate_all_features)
wado-compiler/src/lib.rs:567-572              (in dump_with_host)
wado-compiler/src/lower.rs:6314               (in lower phase)
```
