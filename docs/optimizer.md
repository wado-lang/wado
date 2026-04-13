# Wado Optimizer

This document describes the optimization passes implemented in the Wado compiler. Each pass description links to a representative E2E fixture under `wado-compiler/tests/fixtures/`.

## Philosophy

When WebAssembly provides a native instruction for a feature, prefer it over a complex compiler transformation. This keeps the compiler small, leverages runtime JIT optimizations, and produces smaller output.

Examples: `select` for branchless conditionals, `array.copy`/`array.fill` for bulk operations, `return_call` for tail calls (planned).

## Optimization Levels

All levels run DCE (Dead Code Elimination) on functions, types, and globals.

| Flag            | Iterations | Inline Threshold | Notes                    |
| --------------- | ---------- | ---------------- | ------------------------ |
| `-O0`           | 0          | N/A              | DCE only                 |
| `-O1`           | 2          | 5                |                          |
| `-O2` (default) | 10         | 12               |                          |
| `-O3`           | 100        | 20               |                          |
| `-Os`           | 10         | 12               | strips Wasm name section |

Optimization passes run in a fixed-point loop with early exit on convergence.

## Pipeline

The optimizer runs after lowering and before Wasm emission. `optimize.rs` orchestrates steps 1–5; `wir_optimize.rs` handles step 6.

1. Early DCE — remove unreachable functions/types/globals (all levels).
2. Fixed-point iteration loop (skipped at `-O0`):
   1. Container SROA
   2. Function Inlining
   3. LabeledBlock Fusion
   4. Reference Elimination
   5. SROA
   6. Copy Propagation
   7. Common Subexpression Elimination
   8. Store-to-Load Forwarding
   9. Constant Propagation
   10. Constant Folding
   11. Constant Global Promotion
   12. Constant Branch Pruning
   13. Loop-Invariant Code Motion
   14. Condition Implication
   15. Template String Buffer Hoisting
3. Hot Field Scalarization — runs once after the loop converges.
4. Final DCE — clean up code made dead by optimizations.
5. Select Lowering — post-optimization rewrite (all levels).
6. WIR-level optimizations — see [WIR Optimizations](#wir-optimizations).

## TIR Optimization Passes

All TIR passes live in `wado-compiler/src/optimize/`.

### Function Inlining (`inline.rs`)

Replaces small pure-function calls with their body, sized by an expression-count threshold. Eligible callees are pure, non-recursive, non-generic, take/return no references, are not from the core library, and fit under the threshold. `#[inline]` multiplies the threshold 5×, `#[inline(always)]` forces, `#[inline(never)]` blocks.

E2E: [opt_inline.wado](../wado-compiler/tests/fixtures/opt_inline.wado), [opt_inline_backtrack_miscompile.wado](../wado-compiler/tests/fixtures/opt_inline_backtrack_miscompile.wado).

### Container SROA (`container_sroa.rs`)

Decomposes `Array<Tuple<...>>` and `Array<UserStruct>` locals into N parallel `Array<T_k>` locals (AoS → SoA), eliminating the per-element `struct.new` for the container payload. Tuples and user structs are both WasmGC structs at the Wasm level, so the pass treats them uniformly.

A candidate is decomposed only when every use matches a whitelist: `v.push(literal)`, `v.push(other[j])` from another candidate, `v[i] = literal`, `v[i].field`, `v.len()`, `v.is_empty()`, or initialization via `[]` / `Array::with_capacity`. Any other use (bare reference, closure capture, opaque method) marks the local as escaped and propagates to its sources via fixpoint. Cross-candidate index sources require the index to be `is_duplicable_expr` because the rewrite clones it N times. The pass runs first in the loop so the whitelist sees unobfuscated patterns before inlining rewrites them.

E2E: [opt_container_sroa_struct.wado](../wado-compiler/tests/fixtures/opt_container_sroa_struct.wado), [opt_container_sroa_tuple.wado](../wado-compiler/tests/fixtures/opt_container_sroa_tuple.wado), [opt_container_sroa_edge.wado](../wado-compiler/tests/fixtures/opt_container_sroa_edge.wado), [opt_container_sroa_nondup_idx.wado](../wado-compiler/tests/fixtures/opt_container_sroa_nondup_idx.wado).

Future directions:

- [ ] Nested containers (`Array<Array<T>>`).
- [ ] Container fields of structs (via HFS hoisting).
- [ ] Push-to-literal fusion with `array.new_fixed`.
- [ ] Parallel index-assign coalescing.
- [ ] Cross-function propagation via `stores`-aware summaries.

### LabeledBlock Fusion (`labeled_block_fusion.rs`)

Eliminates intermediate GC variant allocations that survive function inlining. When an inlined `Option<T>`-returning helper expands into `let __tmp = label: { ... break Some(v) ... }; if VariantTest(__tmp, Some) { ... }`, the pass merges it into a single labeled block that routes `break null` to the else branch and `break Some(v)` to the then branch, deleting the variant allocation entirely.

E2E: [opt_labeled_block_fusion.wado](../wado-compiler/tests/fixtures/opt_labeled_block_fusion.wado), [opt_fusion_no_dead_break.wado](../wado-compiler/tests/fixtures/opt_fusion_no_dead_break.wado).

### Reference Elimination (`ref_elim.rs`)

Eliminates unnecessary reference bindings introduced during inlining. When `let self: &T = &local_var` is followed only by field accesses, those accesses are rewritten to read fields directly from the original variable.

### Scalar Replacement of Aggregates (`sroa.rs`)

Decomposes struct/tuple locals into individual scalar locals, eliminating GC heap allocations. This is the single most impactful WasmGC optimization. Two-tier escape analysis:

- Safe (non-escaping): only field reads/writes and `Move` wrappers. Fully decomposed.
- Soft escape (reconstructible): escapes to call arguments, returns, or labeled-block breaks. Decomposed with reconstruction at escape sites.
- Hard escape: address taken, captured by closure, or stored into another aggregate. Excluded.

E2E: [opt_sroa.wado](../wado-compiler/tests/fixtures/opt_sroa.wado), [opt_sroa_intraprocedural.wado](../wado-compiler/tests/fixtures/opt_sroa_intraprocedural.wado), [opt_sroa_variant.wado](../wado-compiler/tests/fixtures/opt_sroa_variant.wado), [opt_sroa_stores_ref.wado](../wado-compiler/tests/fixtures/opt_sroa_stores_ref.wado).

### Copy Propagation (`copy_prop.rs`)

Eliminates trivial copy bindings (`let x = y`, `let x = 42`, `let x = true`) by propagating the source value to every use and dropping the dead binding.

E2E: [opt_copy_prop_multi_field.wado](../wado-compiler/tests/fixtures/opt_copy_prop_multi_field.wado), [opt_copy_prop_while_let.wado](../wado-compiler/tests/fixtures/opt_copy_prop_while_let.wado), [copy_prop_mutable_source.wado](../wado-compiler/tests/fixtures/copy_prop_mutable_source.wado).

### Common Subexpression Elimination (`cse.rs`)

Eliminates duplicate pure binary expressions inside loop bodies. When the same expression appears in both the loop guard and the body and its operand locals are not modified between occurrences, it is computed once into a local and reused — covers idiomatic `while p * p <= limit { ... = p * p; ... }` patterns.

E2E: [wir_optimize_cse.wado](../wado-compiler/tests/fixtures/wir_optimize_cse.wado).

### Store-to-Load Forwarding (`store_load_forward.rs`)

When a literal is stored to a local and later loaded with no intervening modification, the load is replaced with the stored value. Selective invalidation at control-flow boundaries only invalidates locals actually modified within branches.

E2E: [opt_hfs_stores_ref_sync.wado](../wado-compiler/tests/fixtures/opt_hfs_stores_ref_sync.wado).

### Constant Propagation (`const_propagation.rs`)

Replaces `GlobalVarGet` references to immutable globals with their constant values.

E2E: [opt_const.wado](../wado-compiler/tests/fixtures/opt_const.wado).

### Constant Folding (`const_folding.rs`)

Evaluates compile-time-known expressions: integer/float arithmetic and comparison, boolean logic, bitwise ops, integer casts. Guards against division by zero and signed `MIN / -1` traps. Also folds boolean identities (`false || x → x`, `true && x → x`, etc.).

E2E: [const_fold.wado](../wado-compiler/tests/fixtures/const_fold.wado), [opt_const_fold_div_zero.wado](../wado-compiler/tests/fixtures/opt_const_fold_div_zero.wado).

### Constant Global Promotion (`const_global_promotion.rs`)

After propagation and folding reduce a global's runtime initialization to a scalar constant, this pass promotes it back to an immutable compile-time constant so the next iteration of constant propagation can substitute it inline.

E2E: [opt_const.wado](../wado-compiler/tests/fixtures/opt_const.wado).

### Constant Branch Pruning (`const_branch_prune.rs`)

Eliminates branches with compile-time-known boolean conditions and simplifies degenerate block patterns (single-expression blocks, trivial labeled blocks, empty blocks). Also performs labeled-block copy propagation: when a block starts with `let x = y` and neither name is modified within, `x` is replaced by `y` and the binding is dropped — flattening residual parameter copies left by inlining.

E2E: [opt_wir_dead_if_zero.wado](../wado-compiler/tests/fixtures/opt_wir_dead_if_zero.wado), [array_bounds_elim_const_wir.wado](../wado-compiler/tests/fixtures/array_bounds_elim_const_wir.wado).

### Loop-Invariant Code Motion (`licm.rs`)

Hoists loop-invariant field accesses out of loops when the target variable is not modified within the loop body.

E2E: [opt_licm_immut_ref.wado](../wado-compiler/tests/fixtures/opt_licm_immut_ref.wado), [opt_licm_immut_ref_method.wado](../wado-compiler/tests/fixtures/opt_licm_immut_ref_method.wado), [opt_licm_mut_ref_no_hoist.wado](../wado-compiler/tests/fixtures/opt_licm_mut_ref_no_hoist.wado).

### Condition Implication (`condition_implication.rs`)

Eliminates conditions implied false by dominating guards. Subsumes the former WIR-level bounds-check elimination at the TIR level. Handles:

- Loop guards: `while i < bound { ... }` proves any inner `i >= bound` false.
- Dominating ifs: `if (var + offset) < bound { ... }` proves `(var + k) >= bound` false for `k <= offset` inside the then-block.
- Short-circuit `||`: in `(var + k) >= bound || expr`, the right operand only executes when `var + k < bound`, eliminating redundant inner bounds checks.
- Early-exit guards: statements after `if (var >= bound) { return; }` know `var < bound`.

E2E: [array_bounds_elim_loop_guard.wado](../wado-compiler/tests/fixtures/array_bounds_elim_loop_guard.wado), [array_bounds_elim_le_guard.wado](../wado-compiler/tests/fixtures/array_bounds_elim_le_guard.wado), [array_bounds_elim_const.wado](../wado-compiler/tests/fixtures/array_bounds_elim_const.wado).

### Template String Buffer Hoisting (`tmpl_hoist.rs`)

Hoists the backing-array allocation of template strings out of loops so each iteration reuses the same buffer. Escape analysis ensures the template result does not survive past the iteration.

E2E: [tmpl_hoist_loop.wado](../wado-compiler/tests/fixtures/tmpl_hoist_loop.wado), [tmpl_hoist_escape_safe.wado](../wado-compiler/tests/fixtures/tmpl_hoist_escape_safe.wado), [tmpl_hoist_fmt_edge.wado](../wado-compiler/tests/fixtures/tmpl_hoist_fmt_edge.wado).

### Hot Field Scalarization (`field_scalarize.rs`)

Hoists frequently accessed struct fields from GC heap objects to local scalar variables for the duration of a loop. Runs once after the fixed-point loop converges to avoid re-triggering from the write-back/re-read statements it inserts. Write-backs are inserted before `return` and `break` statements that exit the HFS loop scope; an optimization sinks write-backs for unlabeled `break` at loop depth 0, since the post-loop write-backs already cover them.

E2E: [opt_hfs_immut_ref_no_reread.wado](../wado-compiler/tests/fixtures/opt_hfs_immut_ref_no_reread.wado), [opt_hfs_immut_ref_sync.wado](../wado-compiler/tests/fixtures/opt_hfs_immut_ref_sync.wado), [opt_hfs_mut_ref_reread.wado](../wado-compiler/tests/fixtures/opt_hfs_mut_ref_reread.wado), [opt_hfs_loop_exit_no_writeback.wado](../wado-compiler/tests/fixtures/opt_hfs_loop_exit_no_writeback.wado).

### Dead Code Elimination (`dce.rs`)

Removes unreachable functions, types, unused string literals, and unused WASI imports via call-graph reachability from the entry point. Also tracks feature usage (Stdout, Stderr, canonical builtins, box primitives) for conditional feature inclusion.

E2E: [global_dce.wado](../wado-compiler/tests/fixtures/global_dce.wado), [global_dce_cross_module.wado](../wado-compiler/tests/fixtures/global_dce_cross_module.wado).

### Select Lowering (`select_lowering.rs`)

Rewrites `if cond { a } else { b }` with two pure branches into `builtin::select(cond, a, b)`, which emits the Wasm `select` instruction. Runs after the fixed-point loop at all levels.

E2E: [select_basic.wado](../wado-compiler/tests/fixtures/select_basic.wado), [select_no_opt.wado](../wado-compiler/tests/fixtures/select_no_opt.wado).

### Visitor Infrastructure

`tir_visitor.rs` and `wir_visitor.rs` provide shared `*MutVisitor` / `*RefVisitor` / `TirOptVisitor` traits used by every pass. Centralizing Block/Loop/If/Seq traversal here keeps individual passes free of duplicated walk logic. `TirOptVisitor` exposes change-tracking (`-> bool`) for fixed-point convergence.

## WIR Optimizations

`wir_optimize.rs` runs after WIR build and before Wasm emission, mutating the `WirPackage` in place. Phases run in order; passes within a phase may iterate.

### Phase 1: Type Representation

- Nullable ref optimization — rewrites type-level representations for nullable references.
- Pre-SROA copy propagation — inlines trivial `alias = source` so SROA can see direct variant access (RefTest/RefCast on source).
- Multi-value return SROA — rewrites functions returning small scalar structs (2–4 fields) to use Wasm multi-value returns, eliminating the boundary GC allocation.
- Single-field parameter SROA — rewrites `ref null S` parameters (single-field struct) to take the scalar field directly. Primary trigger is `Box<T>` from template string interpolation. E2E: [opt_sroa_box_parameter.wado](../wado-compiler/tests/fixtures/opt_sroa_box_parameter.wado), [opt_sroa_single_field.wado](../wado-compiler/tests/fixtures/opt_sroa_single_field.wado).

### Phase 2: Single-Field Struct Local Elimination (Round 1)

Substitutes `StructGet(LocalGet(x), field)` with the inner value when `x` is defined by `LocalSet(x, StructNew { [inner] })`. Runs after parameter SROA so freshly exposed locals are caught.

### Phase 3: Data Flow

- Collapse array append sequences — merges consecutive `append` calls into `array.new_fixed`. E2E: [array_append_collapse.wado](../wado-compiler/tests/fixtures/array_append_collapse.wado).
- Forward struct field constants — tracks known field values (constants and `LocalGet` references) through `StructGet` for constant-index bounds-check elimination. Resolves block-result `StructNew` patterns for single-exit blocks. Uses `stores`-aware alias analysis: locals passed to functions without `stores` declarations are not marked aliased, enabling field forwarding across calls. E2E: [array_bounds_elim_const_wir.wado](../wado-compiler/tests/fixtures/array_bounds_elim_const_wir.wado).

### Phase 4: Library-Specific Rewrites

- Simplify short string appends — rewrites `append(short_const)` into a sequence of `append_char`.
- Constant array data promotion — replaces `array.new_fixed` with `array.new_data` when all elements are compile-time constants of a primitive type (≥16 elements).
- Split large array literals — rewrites `array.new_fixed` with >256 elements into `array.new_default` + `array.set` to avoid pathological JIT register allocation.

### Phase 5: Peephole and Multi-Field Struct Elimination

- Peephole — constant folding, copy elision (cross-scope fresh value tracing for unwrap patterns, `StructNew`/`ArrayNewFixed` field usage, `Return`/`Br` containing `StructNew`), multi-value struct elision at WIR level. E2E: [wir_optimize_unwrap_fresh_elision.wado](../wado-compiler/tests/fixtures/wir_optimize_unwrap_fresh_elision.wado), [wir_optimize_value_copy_if_fresh.wado](../wado-compiler/tests/fixtures/wir_optimize_value_copy_if_fresh.wado), [wir_optimize_negate_eqz.wado](../wado-compiler/tests/fixtures/wir_optimize_negate_eqz.wado), [wir_optimize_branchless_increment.wado](../wado-compiler/tests/fixtures/wir_optimize_branchless_increment.wado).
- Flatten seq assignments — exposes multi-field struct locals for elimination.
- Multi-field struct local elimination — substitutes `StructGet(LocalGet(x), field_k)` with the corresponding field expression when all fields are accessed exactly once.
- Labeled-block copy propagation — flattens trivial labeled blocks holding only a copy. E2E: [wir_optimize_labeled_block_copy_prop.wado](../wado-compiler/tests/fixtures/wir_optimize_labeled_block_copy_prop.wado), [wir_optimize_labeled_block_copy_prop_safety.wado](../wado-compiler/tests/fixtures/wir_optimize_labeled_block_copy_prop_safety.wado).

### Phase 6: Dead Value Elimination

- Dead argument elimination (DAE) — removes unused function parameters and the corresponding arguments at every call site, when all dead-position arguments are side-effect-free. E2E: [wir_optimize_dae.wado](../wado-compiler/tests/fixtures/wir_optimize_dae.wado).
- Dead return value elimination (DRVE) — converts functions whose return value is always dropped to void return.
- Write-only local elimination — removes `LocalSet(x, expr)` when local `x` is never read; iterates to fixed point.

### Phase 7: Global Cleanup

Trivial init-guard removal — removes compiler-generated module-initialization guard blocks when no actual initialization remains.

### Phase 8: Final DCE and Compaction

- Dead type elimination — removes GC type definitions not referenced by any live code (transitive).
- Compact dead items — removes all items marked dead from the module.

## Not Yet Implemented

- [ ] Sparse Conditional Constant Propagation (SCCP) — simultaneous constant propagation and dead branch elimination.
- [ ] Interprocedural SCCP (IPSCCP).
- [ ] Global Value Numbering — generalized CSE with hash-consing (basic loop-level CSE is in `cse.rs`).
- [ ] Peephole / Instruction Combining — algebraic simplification (`x + 0 → x`, `x * 2 → x << 1`, etc.).
- [ ] Dead Store Elimination.
- [ ] Strength Reduction — loop induction-variable optimization.
- [ ] Cross-block Copy Propagation.
- [ ] Return Scalarization via Multi-Value Returns for user-defined functions (already done for builtins).
- [ ] Function Specialization for known constant arguments.
- [ ] Argument Promotion — promote `&T` fields to scalar parameters.
- [ ] Jump Threading.
- [ ] Reassociation — group constants in associative chains.
- [ ] SimplifyCFG — general control-flow-graph simplification.
- [ ] Tail Call Optimization — emit `return_call` for tail-recursive calls.
- [ ] Bounds-check elimination for chained sequential access (`arr[0]; arr[1]; arr[2]`).

## Tried and Found Ineffective

- Empty-array singleton for struct field defaults — sharing a single `array.new<u8>(0)` global across all default `String` initializations in serde `Deserialize` impls. Measured no performance improvement; the GC allocator handles tiny zero-length arrays efficiently enough that the overhead is negligible.
- `array.copy` for `Array::grow` — replacing the element-by-element copy loop with the Wasm `array.copy` instruction. Was several times slower than the loop, likely due to poor JIT optimization of `array.copy` in current runtimes.

## Testing Strategy

- Golden fixtures — `tests/fixtures.golden/*.wir.wado` captures optimized WIR output. Regenerate with `mise run update-golden-fixtures`.
- WIR pattern tests — `wir_expect:Ox` / `wir_not_expect:Ox` in `__DATA__` blocks of E2E fixtures verify specific optimization effects at a given level.
- Correctness E2E — `tests/fixtures/*.wado` ensures optimizations preserve semantics across `-O0`/`-O2` (and `-O1`/`-O3`/`-Os` under `WADO_FULL_TEST=1`).
- Benchmark suite — sieve, mandelbrot, count-prime, fts, zlib (`mise run benchmark-all`).

## References

### Loop Optimizations

- [CSC D70: Compiler Optimization LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf)
- [Cornell CS 6120: Loop Reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/)

### LLVM Optimizations

- [LLVM's Analysis and Transform Passes](https://llvm.org/docs/Passes.html)
- [How LLVM Optimizes a Function](https://blog.regehr.org/archives/1603)
- [Performance Tips for Frontend Authors](https://llvm.org/docs/Frontend/PerformanceTips.html)

### Bounds Check Elimination

- [Array Bounds Check Elimination in CLR](https://learn.microsoft.com/en-us/archive/blogs/clrcodegeneration/array-bounds-check-elimination-in-the-clr)

### WebAssembly

- [Wasm 3.0 Release (September 17, 2025)](https://webassembly.org/news/2025-09-17-wasm-3.0/)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md)
- [V8: WasmGC Porting](https://v8.dev/blog/wasm-gc-porting)
- [Binaryen Optimizer Cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook)

### Escape Analysis

- [V8: WasmGC Porting — Escape Analysis](https://v8.dev/blog/wasm-gc-porting)
- [Scalar Replacement of Aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form)

### General Compiler Optimization

- [Optimizing Compiler (Wikipedia)](https://en.wikipedia.org/wiki/Optimizing_compiler)
- [Can You Trust a Compiler to Optimize?](https://matklad.github.io/2023/04/09/can-you-trust-a-compiler-to-optimize-your-code.html)
