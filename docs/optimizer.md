# Wado Optimizer

The optimizer rewrites the Normalized IR (NIR; see [WEP: NIR Layer](./wep-2026-05-11-nir.md)) in place before lowering to WIR, then runs a smaller set of WIR-level passes before Wasm emission. Pass span names used by `WADO_LIST_PASSES` / `WADO_SKIP_PASS` / `WADO_DUMP_PASS_*` carry a `nir/` prefix.

The module-level doc in `src/optimize.rs` is the authoritative pass index and ordering; per-pass design detail lives in each pass's source. This document covers the architecture and gives a one-line summary per pass.

## Philosophy

When WebAssembly provides a native instruction for a feature, prefer it over a complex compiler transformation — it keeps the compiler small, leverages the runtime JIT, and produces smaller output (e.g. `select` for branchless conditionals, `array.copy`/`array.fill` for bulk ops, `br_table` for dense matches).

## Optimization levels

All levels run DCE on functions, types, and globals.

| Flag            | Iterations | Inline threshold | Notes                                             |
| --------------- | ---------- | ---------------- | ------------------------------------------------- |
| `-O0`           | 0          | N/A              | DCE only + `match_to_switch` + post-loop rewrites |
| `-O1`           | 2          | 4                |                                                   |
| `-O2` (default) | 10         | 13               |                                                   |
| `-O3`           | 30         | 32               |                                                   |
| `-Os`           | 10         | 13               | strips the Wasm name section                      |

The fixed-point loop exits early on convergence. The post-loop rewrites the Wasm backend depends on (`select_lowering`, `multi_value_return`) and `match_to_switch` run at every level, including `-O0`.

## Architecture

### Worklist rewrite engine

Genuinely-local NIR rewrites run as [`Rule`]s on a worklist engine (`nir_engine.rs`) over one function's arena `Body`: a node is revisited only when an edit might have made it reducible, rather than via repeated whole-tree sweeps. The engine owns the session state (parent map, local use index, worklist) and a mutating edit API (`replace_expr_kind`, `set_block_stmts`, `become_expr`, `alloc_*`, `clone_expr`) that keeps that state coherent. Flow-sensitive passes that need per-block dataflow (`field_scalarize`, `licm`, `tmpl_hoist`, `value_copy_demote`, `store_load_forward`, the flow-sensitive half of `const_folding`) keep their own walkers. See [WEP: NIR Rewrite Engine](./wep-2026-06-05-nir-rewrite-engine-design.md).

### Unified peephole session

`optimize/peephole.rs` runs the position-flexible local rules — `string_push`, `array_literal`, `elide_local`, the environment-free subset of `const_folding` (literal arithmetic + pure CTFE), and `const_branch_prune` — together over one engine session per function, interleaved on a single worklist. It is invoked twice per iteration: before `inline` (so `string_push` sees the `push_str` `MethodCall`) and after (so `array_literal` sees the exposed `array_new + push` window).

### Per-function dirty-set gating

A `FunctionGate` (`optimize/gate.rs`) lets every loop pass skip functions that have not changed since it last ran. Each function has a monotonic revision and each pass a per-function watermark; a per-function pass processes a function only when `revision > watermark` (`run_gated`), and any pass marks the functions it changed dirty (`mark_changed`), which also bumps 1-hop call-graph neighbours. Interprocedural passes (`inline`, `dae`, `drve`, `sroa_param`, `value_copy_demote`) scan all functions but report exactly the ones they touched. `FunctionId` is a function's index in `NirPackage::functions` (stable within one optimizer run).

Gating affects only which functions a pass visits, never the IR a visit produces; since every loop pass is an optimization, an imprecise gate can cost only optimization quality (a missed rewrite), never correctness. The call graph is built once and not refreshed when a pass shifts a function's call edges, since stale edges only reduce propagation precision.

## Pipeline

`optimize.rs` orchestrates the NIR stages; `wir_optimize.rs` runs the WIR stages.

1. Early DCE — remove unreachable functions/types/globals.
2. Fixed-point loop (skipped at `-O0`), in order: `container_sroa`, peephole (pre-inline; hosts `match_to_switch` and `value_copy_elide` as rules), `value_copy_demote`, `sroa_param`, `inline`, peephole (post-inline; hosts `ref_elim` and `elide_box_local` as rules), `labeled_block_fusion`, `sroa`, `copy_prop`, `dae`, `drve`, `cse`, `store_load_forward`, `const_folding`, `licm`, `condition_implication`, `tmpl_hoist`. (`match_to_switch` on global initializers runs once before the loop; `-O0` lowers everything via `match_to_switch_all`.)
3. Post-loop, once: `field_scalarize`; `branch_prune_final` (flatten `__tmpl:` wrappers); `const_object_globalization` + a final `const_folding`/`const_branch_prune` cleanup.
4. Final DCE.
5. Backend-required rewrites (all levels): `select_lowering`, `multi_value_return`.
6. WIR-level passes — see [WIR optimizations](#wir-optimizations).

## NIR passes

Allocation-elimination and aggregate passes:

- `inline` — replace small pure non-recursive non-generic reference-free calls with their body; `#[inline]` ×5 threshold, `#[inline(always)]`/`(never)` force/block.
- `sroa` — Scalar Replacement of Aggregates: decompose non-escaping (or reconstructible soft-escaping) struct/tuple locals into scalar locals. The highest-impact WasmGC pass.
- `container_sroa` — `List<Tuple<…>>` / `List<Struct>` → parallel `List<T_k>` (AoS → SoA) when every use matches the spine/index whitelist.
- `sroa_param` — rewrite an internal `&S`/`&mut S` single-field-struct parameter to take the inner scalar, and the call-site allocation to the value; skips aliasing-sibling reference params.
- `elide_box_local` — collapse `let x = Box{value: inner}; … x.value …` when `x` is bound once and read once, guarded by `mod_ref::can_move_past`.
- `array_literal` — materialize the `array_new(N) + N×push` builder window into `ArrayLiteral` (lowered to `array.new_fixed`).
- `value_copy_elide` — strip a `$value_copy$T(arg)` wrapper whose target is observably read-only.
- `value_copy_demote` — demote a deep `List<E>` value-copy to a shallow spine copy when elements are provably never mutated through the binding (element-immutability taint analysis).
- `labeled_block_fusion` — fuse the inlined-`Option<T>` `let __tmp = label:{… break Some(v)…}; if VariantTest(__tmp,Some)…` shape, deleting the variant allocation.
- `ref_elim` — drop reference bindings (`let r = &x`) read only via field access, rewriting reads to the original.

Scalar / dataflow passes:

- `copy_prop` — propagate trivial copies (`let x = y/42/&y`) and drop the binding. A source mutated only outside the target's scope is still propagated (scope-stability check), covering loop-counter copies.
- `dae` — drop parameters never read by the callee, and the pure argument at every call site (collapsing a dead-receiver `MethodCall` to a `Call`).
- `drve` — convert a function whose return value is dropped at every call site to void-returning.
- `cse` — hoist a pure binary expression repeated in a loop guard and body into one local.
- `store_load_forward` — forward a stored literal to a later unmodified load.
- `elide_local` — drop `let x = expr` where `x` is never read (keeping `expr` if impure).
- `const_folding` — partial evaluation via `niri`. The env-free subset (literal arithmetic, pure CTFE, short-circuit identities) runs in the peephole session; the flow-sensitive half (env-bound locals, forwarded struct fields, immutable-global reads, constant-branch collapse) runs as a standalone per-function walker.
- `const_branch_prune` — simplify trivial blocks: `{ expr }` → `expr`, empty blocks → `()`, tail-/single-break labeled blocks → their value, and dead statements after a terminator. `__tmpl:` blocks are preserved for `tmpl_hoist` until `branch_prune_final`.

Loop and field passes:

- `licm` — hoist loop-invariant field-access chains (one level per fixpoint round, with reference-field aliasing guards) and loop-invariant non-trapping arithmetic trees.
- `condition_implication` — eliminate conditions implied false by a dominating loop guard, `if`, short-circuit `||`, or early-exit guard (subsumes WIR bounds-check elimination).
- `tmpl_hoist` — hoist a template string's backing buffer out of a loop and reuse it, when the result does not escape the iteration.
- `field_scalarize` — Hot Field Scalarization: shadow hot GC fields in scalar locals across a loop, with dataflow-driven write-back/re-read sync. Runs once after the loop.

`niri` (`src/niri.rs`) is the partial evaluator backing `const_folding`; see [WEP: NIR Interpreter Evolution Plan](./wep-2026-04-27-nir-interpreter.md). Unit tests: `wado-compiler/tests/niri.rs`.

Whole-program / backend passes:

- `dce` — remove unreachable functions, types, string/bytes literals, and WASI imports by call-graph reachability; tracks feature usage. Runs around the loop.
- `match_to_switch` — dense integer/enum `match` → `Switch` (Wasm `br_table`). Runs first each iteration and at `-O0`.
- `select_lowering` — `if cond { a } else { b }` with leaf-pure arms → `builtin::select`. Post-loop, all levels.
- `multi_value_return` — mark tuple/struct-returning functions whose returns are fresh literals and call sites destructure, so WIR build emits the multi-value ABI. Post-loop, all levels. (The variant case is the WIR-level `variant_return_sroa`.)
- `const_object_globalization` — hoist constant read-only aggregate `let` bindings into shared immutable globals; see [WEP](./wep-2026-05-31-const-object-globalization.md).

`nir_visitor.rs` provides the shared pre/post-order `*MutVisitor`/`*OptVisitor` traits; `arena_query.rs` holds shared arena queries (break-target search, mutation/place-root checks).

## Lowering optimizations

NIR→WIR lowering (`wir_build/`) avoids redundant shapes in a few spots; these fire once during the build at all levels. Notably, exhaustive-match last-arm elision (`wir_build/pattern_match.rs`) treats the final arm of a fully-covering `match` as irrefutable, removing one pattern test and branch per `?` on the hot path.

## WIR optimizations

`wir_optimize.rs` mutates the `WirPackage` in place after WIR build; phases run in order and may iterate.

1. Type representation — nullable-ref representation; pre-SROA copy propagation; variant-return SROA (small variants → multi-value returns).
2. Single-field struct local elimination (round 1) — substitute `StructGet(LocalGet(x), f)` for `LocalSet(x, StructNew{[inner]})` when re-evaluation-safe.
3. Data flow — forward struct field constants (`stores`-aware) for constant-index bounds-check elimination.
4. Library rewrites — short-string append expansion; constant array data promotion (`array.new_fixed` → `array.new_data` for ≥16 primitive constants); large-literal splitting (>256 elements).
5. Peephole + multi-field struct elimination — Wasm instruction-selection rewrites with no NIR analogue (constant-comparison/dead-`If` folding, `eqz`/negated-comparison folding, branchless increment, byte-mask/sign-extension folding, redundant `ref.cast`/`ref.test` elimination, nullability relaxation, `local.tee` fusion); multi-field struct local elimination; trivial labeled-block copy propagation.
6. Write-only local elimination — for locals the WIR builder synthesises (`__match_scrut_N`, pair/multi-value temps) that no NIR pass can reach.
7. Global cleanup — constant global-initializer promotion (`const_global.rs`); trivial init-guard removal.
8. Branch hints (`branch_hint.rs`) — `br_if` selection: `if cond { br N }` with an empty else collapses to `br_if N-1`, carrying any branch hint on the condition (runs after `init_guard`, whose matcher keys on the `If { GlobalGet, [Br] }` shape). Then trap-based hint inference: an `if` arm that always reaches an `unreachable` trap is hinted cold, and a `br_if` whose fall-through always traps is hinted likely-taken. Divergence alone (`br` / `return`) never counts as cold, and explicit hints (from `builtin::cold_path()`) always win. Inference also runs at `-O0`, keeping hints independent of the optimization level like the build-time `apply_cold_path_hints`.
9. Final DCE + compaction — remove unreachable defined functions and unused GC types, then compact and reindex.

Branch hints are transparent annotations: a `BranchHint` wraps an `if`/`br_if` condition, and any pass that matches on a condition's shape must look through it via `WirInstr::peel_hint` (or the hint blocks the rewrite). A pass that eliminates the branch drops the hint with it (`take_branch_hint`); a pass that logically negates a hinted condition or swaps hinted arms must flip `likely`. The emitter records a `metadata.code.branch_hint` entry for hints on `if` and `br_if` conditions; wasmtime (with `Config::wasm_branch_hinting`, which `wado run` enables) lays out the cold side out of line. For benchmarking, `-f no-branch-hinting` disables the feature: `cold_path()` lowers to a no-op at WIR build (keeping the NIR inliner's cold-cost exclusion identical in both configurations) and the inference pass is skipped, so no hint section is emitted.

Shared facility: `optimize/mod_ref.rs` (`ModRef::of_expr`/`of_stmt`) returns a conservative mod/ref summary used by the move-safety predicates (`is_re_evaluation_safe`, `may_clobber`, `can_move_past`).

## Not yet implemented

- [ ] Sparse Conditional Constant Propagation (SCCP) and interprocedural SCCP.
- [ ] Global Value Numbering — generalized CSE with hash-consing (loop-level CSE exists).
- [ ] Instruction combining — algebraic simplification (`x + 0 → x`, `x * 2 → x << 1`).
- [ ] Dead store elimination.
- [ ] Strength reduction; reassociation; jump threading; SimplifyCFG.
- [ ] Cross-block copy propagation.
- [ ] Function specialization / argument promotion.
- [ ] Tail call optimization (`return_call`).
- [ ] Bounds-check elimination for chained sequential access (`arr[0]; arr[1]; arr[2]`).

## Tried and found ineffective

- Empty-array singleton for default `String` fields — no measurable gain; the GC allocator handles tiny zero-length arrays cheaply.
- `array.copy` for `List::grow` — several times slower than the element loop under current runtime JITs.

## Testing

- E2E correctness — `wado-compiler/tests/fixtures/*.wado` run across `-O0`/`-O2` (and `-O1`/`-O3`/`-Os` under `WADO_FULL_TEST=1`). Optimizer-specific fixtures use the `opt_*`, `array_bounds_elim_*`, `select_*`, `hfs_*`, `tmpl_hoist_*`, `value_copy_*`, and `wir_optimize_*` name prefixes.
- WIR pattern tests — `wir_expect:Ox` / `wir_not_expect:Ox` in fixture `__DATA__` blocks assert optimization effects at a given level.
- Golden fixtures — `tests/generated/fixtures/*.wir.wado`; regenerate with `mise run update-golden-fixtures`.
- Benchmarks — `mise run benchmark-all` (sieve, mandelbrot, count-prime, fts, zlib, syntax-highlight, …).

## References

- LICM: [CSC D70 LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf), [Cornell CS 6120 loop reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/).
- LLVM: [analysis & transform passes](https://llvm.org/docs/Passes.html), [how LLVM optimizes a function](https://blog.regehr.org/archives/1603), [frontend performance tips](https://llvm.org/docs/Frontend/PerformanceTips.html).
- WasmGC: [Wasm 3.0](https://webassembly.org/news/2025-09-17-wasm-3.0/), [GC proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md), [V8 WasmGC porting (incl. escape analysis)](https://v8.dev/blog/wasm-gc-porting), [Binaryen optimizer cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook).
- SROA: [scalar replacement of aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form).
