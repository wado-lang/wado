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
| `-O3`           | 100        | 30               |                          |
| `-Os`           | 10         | 12               | strips Wasm name section |

Optimization passes run in a fixed-point loop with early exit on convergence.

## Pipeline

The optimizer runs after lowering and before Wasm emission. `optimize.rs` orchestrates steps 1–5; `wir_optimize.rs` handles step 6.

1. Early DCE — remove unreachable functions/types/globals (all levels).
2. Fixed-point iteration loop (skipped at `-O0`):
   1. Container SROA
   2. Value-Copy Elision
   3. Function Inlining
   4. LabeledBlock Fusion
   5. Reference Elimination
   6. SROA
   7. Copy Propagation
   8. Common Subexpression Elimination
   9. Store-to-Load Forwarding
   10. Constant Propagation
   11. Constant Folding
   12. Constant Global Promotion
   13. Constant Branch Pruning
   14. Loop-Invariant Code Motion
   15. Condition Implication
   16. Template String Buffer Hoisting
3. Hot Field Scalarization — runs once after the loop converges.
4. Final DCE — clean up code made dead by optimizations.
5. Select Lowering — post-optimization rewrite (all levels).
6. WIR-level optimizations — see [WIR Optimizations](#wir-optimizations).

## TIR Optimization Passes

All TIR passes live in `wado-compiler/src/optimize/`.

### Function Inlining (`inline.rs`)

Replaces small pure-function calls with their body, sized by an expression-count threshold. Eligible callees are pure, non-recursive, non-generic, take/return no references, are not from the core library, and fit under the threshold. `#[inline]` multiplies the threshold 5×, `#[inline(always)]` forces, `#[inline(never)]` blocks.

E2E: [opt_inline.wado](../wado-compiler/tests/fixtures/opt_inline.wado), [opt_inline_backtrack_miscompile.wado](../wado-compiler/tests/fixtures/opt_inline_backtrack_miscompile.wado).

### Value-Copy Elision (`value_copy_elide.rs`)

Strips the synthesized `$value_copy$T<id>(arg)` wrapper from `let x = $value_copy$T(arg)` (and the equivalent `Assign`) bindings whose target is observably read-only — when the source root that `arg` reads from is not assigned, field-mutated, or captured for the rest of the function, eliding the wrapper aliases storage in a way that's externally indistinguishable from the freshly-allocated copy.

Runs once per fixed-point iteration, before `tir/inline`. The inliner expands every reachable `$value_copy$T` body into a labeled block, after which the `Call($value_copy$T, [arg])` shape the elider matches on no longer exists; running before inline is what lets the elider strip wrappers around `match make()? { Ok(v) => v, Err(e) => return Err(e) }`-style `?` desugarings (without the pre-inline ordering, the wrappers in every `parse_*` `?` site would survive through codegen). The only way a fresh wrapper `Call` shape can appear after lowering is for the inliner to expand a function whose body still contains a wrapper — those are caught by the next iteration's run, and if the loop converges (no pass returned `changed`) the inliner did nothing this round so no new wrappers were introduced.

The strip walker descends through every TIR expression that can syntactically embed a `TirBlock` (`If`, `Match`, `Switch`, `Block`, `LabeledBlock`, calls, struct/tuple/variant literals, …) so wrappers nested inside `let x = if cond { let y = $value_copy$T(...); ... } else { ... };` patterns — common in `parse_*` rule bodies — are reached.

E2E: [value_copy_elide_qmark.wado](../wado-compiler/tests/fixtures/value_copy_elide_qmark.wado).

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

### Constant Folding (`const_folding.rs`)

A thin TIR visitor that walks each function body via `opt_walk_expr` and asks the [TIR Interpreter (`tiri`)](#tir-interpreter-tiri) to apply its local rewrite rules at every node. All reduction logic — literal folding, integer cast collapsing, the `&&` / `||` short-circuit identity rules, and `GlobalVarGet` rewriting for immutable globals — lives in `tiri`; this pass owns no rewrite logic of its own.

E2E: [const_fold.wado](../wado-compiler/tests/fixtures/const_fold.wado), [opt_const_fold_div_zero.wado](../wado-compiler/tests/fixtures/opt_const_fold_div_zero.wado).

### TIR Interpreter (`tiri`)

`tiri` (`src/tiri.rs`) is the partial evaluator that backs constant folding. The canonical entry point is

```rust
Interpreter::new(type_table).reduce(&expr) -> TirExpr
```

`reduce` is idempotent and monotone: it always returns a (possibly identical) `TirExpr`, leaving literal leaves with their original lexical repr (`0xFF` is not rewritten to `255`). Visitor drivers that already walk every TIR kind via `tir_visitor::opt_walk_expr` use `reduce_local(&mut TirExpr) -> bool` instead, which performs only the single-node rewrite at `expr`. Unit tests can use `reduce_to_value(&TirExpr) -> Option<Value>` to extract a `Value` directly.

Today the engine reduces literal-only Binary / Unary / Cast expressions, the short-circuit identity rules `false || X → X` and `true && X → X` (and their right-hand variants), `let`-bound locals via a per-function `env`, `if` expressions and statements (constant-condition splice and both-arms-equal collapse), and `match` expressions over payload-free patterns (constant-scrutinee chosen-arm splice and all-arms-equal collapse, covering wildcard / integer / bool / char literals, integer and char ranges, or-patterns, and `ConstantValue`). Future work — payload-aware variant matching, bounded loop unrolling, pure function inlining, and a complementary wasm-CTFE backend — is described in [WEP: TIR Interpreter Evolution Plan](./wep-2026-04-27-tir-interpreter.md).

Unit tests: [`wado-compiler/tests/tiri.rs`](../wado-compiler/tests/tiri.rs).

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

Hoists frequently accessed struct fields from GC heap objects to local scalar variables for the duration of a loop. Runs once after the fixed-point loop converges to avoid re-triggering from the write-back/re-read statements it inserts.

Sync placement is dataflow-driven. For each scalarized field `(L, F)` (with scalar local `__hfs_F`), the walker tracks one of three states per program point: `Both` (`__hfs_F == L.F`), `ScalarOnly` (`__hfs_F` holds the truth, `L.F` is stale), or `FieldOnly` (`L.F` holds the truth, `__hfs_F` is stale). A scalar write transitions to `ScalarOnly`; a `&mut T` call transitions to `FieldOnly`; a `&T` call requires field-canonical state but does not change it. Sync is emitted only at transitions: `ScalarOnly → Both/FieldOnly` writes back, `FieldOnly → Both/ScalarOnly` re-reads, and `Both → *` is a relabel with no sync. Consecutive `&mut` calls therefore produce zero inter-call sync — once the state is `FieldOnly`, every subsequent `&mut` call's pre-state requirement is satisfied without any sync stmt.

Branch joins (`If`/`Switch`/`Match`) walk each arm with cloned entry state and pick a per-candidate join target; convergence sync is inserted at each arm's exit. A call in one match arm can never trigger sync that clobbers a sibling scalar-update arm (issue #1008). Loops commit any `ScalarOnly` candidate before the body runs (so inner reads see an up-to-date field) and join entry-state with body-exit-state for the post-loop state — capturing both the zero-iterations and the `>= 1`-iteration paths. Escape paths (`return`, `break` to a non-enclosing label) commit `ScalarOnly` candidates so the field is canonical at exit. The unlabeled `break` at `loop_depth 0` shortcut elides this pre-break commit since the body-end force-`Both` already covers the same scalars.

Match arm bodies whose value is non-unit (and arm blocks of non-unit `If`/`Switch`) capture the trailing expression into a per-type pooled `__hfs_call_*` temp before appending convergence sync, so the block still evaluates to the original arm's value. All other call sites use stmt-level sync injection — no temp.

E2E: [opt_hfs_immut_ref_no_reread.wado](../wado-compiler/tests/fixtures/opt_hfs_immut_ref_no_reread.wado), [opt_hfs_immut_ref_sync.wado](../wado-compiler/tests/fixtures/opt_hfs_immut_ref_sync.wado), [opt_hfs_mut_ref_reread.wado](../wado-compiler/tests/fixtures/opt_hfs_mut_ref_reread.wado), [opt_hfs_loop_exit_no_writeback.wado](../wado-compiler/tests/fixtures/opt_hfs_loop_exit_no_writeback.wado), [hfs_match_scalar_arm_mixed_with_call_arm.wado](../wado-compiler/tests/fixtures/hfs_match_scalar_arm_mixed_with_call_arm.wado), [hfs_match_let_value_non_unit.wado](../wado-compiler/tests/fixtures/hfs_match_let_value_non_unit.wado), [hfs_match_guarded_arm.wado](../wado-compiler/tests/fixtures/hfs_match_guarded_arm.wado), [hfs_match_guard_with_call.wado](../wado-compiler/tests/fixtures/hfs_match_guard_with_call.wado), [hfs_multi_call_in_expression.wado](../wado-compiler/tests/fixtures/hfs_multi_call_in_expression.wado), [hfs_if_let_value_non_unit.wado](../wado-compiler/tests/fixtures/hfs_if_let_value_non_unit.wado), [hfs_early_return_with_wrapped_call.wado](../wado-compiler/tests/fixtures/hfs_early_return_with_wrapped_call.wado).

### Dead Code Elimination (`dce.rs`)

Removes unreachable functions, types, unused string literals, and unused WASI imports via call-graph reachability from the entry point. Also tracks feature usage (Stdout, Stderr, canonical builtins, box primitives) for conditional feature inclusion.

E2E: [global_dce.wado](../wado-compiler/tests/fixtures/global_dce.wado), [global_dce_cross_module.wado](../wado-compiler/tests/fixtures/global_dce_cross_module.wado).

### Select Lowering (`select_lowering.rs`)

Rewrites `if cond { a } else { b }` with two pure branches into `builtin::select(cond, a, b)`, which emits the Wasm `select` instruction. Runs after the fixed-point loop at all levels.

E2E: [select_basic.wado](../wado-compiler/tests/fixtures/select_basic.wado), [select_no_opt.wado](../wado-compiler/tests/fixtures/select_no_opt.wado).

### Visitor Infrastructure

`tir_visitor.rs` and `wir_visitor.rs` provide shared `*MutVisitor` / `*RefVisitor` / `TirOptVisitor` traits used by every pass. Centralizing Block/Loop/If/Seq traversal here keeps individual passes free of duplicated walk logic. `TirOptVisitor` exposes change-tracking (`-> bool`) for fixed-point convergence.

## Lowering Optimizations

TIR→WIR lowering (`wir_build/`) also avoids emitting redundant shapes in a few targeted spots. These are not fixed-point passes; they fire once while the cascade is being built and are effective at all optimization levels including `-O0`.

### Exhaustive Match Last-Arm Elision (`wir_build/pattern_match.rs`)

For `match` expressions whose unguarded arms exhaustively cover every case of the scrutinee's variant or enum type, the final arm in source order is guaranteed to match by exclusion — its pattern test and the trailing `unreachable` fallback are both dead. `translate_match` recognises this via `compute_emitted_as_irrefutable` and treats the last arm as irrefutable (bindings + body only, no surrounding `If`). Removes one pattern test and one branch per `?` on the hot path of every `Result`/`Option`-heavy function, which is a significant fraction of deserializers.

Conservative — only fires when every arm is `Variant`, `Enum`, or a one-level `Or` of those (with no guards, distinct case indices, and a count equal to the total cases of the scrutinee type). Anything else (wildcards, literals, ranges, guards, nested `Or`s) falls back to the standard `unreachable`-tailed cascade.

E2E: [pattern_match_exhaustive_variant_last_arm.wado](../wado-compiler/tests/fixtures/pattern_match_exhaustive_variant_last_arm.wado), [pattern_match_non_exhaustive_keeps_fallback.wado](../wado-compiler/tests/fixtures/pattern_match_non_exhaustive_keeps_fallback.wado).

## WIR Optimizations

`wir_optimize.rs` runs after WIR build and before Wasm emission, mutating the `WirPackage` in place. Phases run in order; passes within a phase may iterate.

### Phase 1: Type Representation

- Nullable ref optimization — rewrites type-level representations for nullable references.
- Pre-SROA copy propagation — inlines trivial `alias = source` so SROA can see direct variant access (RefTest/RefCast on source).
- Multi-value return SROA — rewrites functions returning small scalar structs (2–4 fields) to use Wasm multi-value returns, eliminating the boundary GC allocation.
- Single-field parameter SROA — rewrites `ref null S` parameters (single-field struct) to take the scalar field directly. Primary trigger is `Box<T>` from template string interpolation. E2E: [opt_sroa_box_parameter.wado](../wado-compiler/tests/fixtures/opt_sroa_box_parameter.wado), [opt_sroa_single_field.wado](../wado-compiler/tests/fixtures/opt_sroa_single_field.wado).

### Phase 2: Single-Field Struct Local Elimination (Round 1)

Substitutes `StructGet(LocalGet(x), field)` with the inner value when `x` is defined by `LocalSet(x, StructNew { [inner] })`. Runs after parameter SROA so freshly exposed locals are caught.

Two complementary variants run in sequence:

- Re-evaluation-safe elision (`elide_single_field_struct_locals`) — substitutes when the inner field initializer is referentially transparent (no heap reads, no calls, no allocations). Safe regardless of how far apart def and use are.
- Adjacent-use elision (`elide_adjacent_single_use_struct_locals`) — relaxes the purity check by relying on adjacency instead. Fires when the local has exactly one def + one use, the use is the immediately-following sibling instruction (skipping intervening `Nop`s), and the use is the leftmost-evaluated descendant of that instruction. Recovers the very common `Box<T>` boxing+inlining pattern (e.g. `Box<char> { value: <heap-reading block> }` followed by `.value`) where the inner reads heap state but no intervening operation could mutate it.

### Phase 3: Data Flow

- Collapse array append sequences — merges consecutive `append` calls into `array.new_fixed`. E2E: [array_append_collapse.wado](../wado-compiler/tests/fixtures/array_append_collapse.wado).
- Forward struct field constants — tracks known field values (constants and `LocalGet` references) through `StructGet` for constant-index bounds-check elimination. Resolves block-result `StructNew` patterns for single-exit blocks. Uses `stores`-aware alias analysis: locals passed to functions without `stores` declarations are not marked aliased, enabling field forwarding across calls. E2E: [array_bounds_elim_const_wir.wado](../wado-compiler/tests/fixtures/array_bounds_elim_const_wir.wado).

### Phase 4: Library-Specific Rewrites

- Simplify short string appends — rewrites `append(short_const)` into a sequence of `append_char`.
- Constant array data promotion — replaces `array.new_fixed` with `array.new_data` when all elements are compile-time constants of a primitive type (≥16 elements).
- Split large array literals — rewrites `array.new_fixed` with >256 elements into `array.new_default` + `array.set` to avoid pathological JIT register allocation.

### Phase 5: Peephole and Multi-Field Struct Elimination

- Peephole — constant folding, multi-value struct elision at WIR level. E2E: [wir_optimize_negate_eqz.wado](../wado-compiler/tests/fixtures/wir_optimize_negate_eqz.wado), [wir_optimize_branchless_increment.wado](../wado-compiler/tests/fixtures/wir_optimize_branchless_increment.wado).
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

- Dead defined-function elimination — `mark_unreachable_defined_functions` walks `module.exports` + `module.elements` and BFSes the WIR call graph, marking unreachable defined-function indices as dead. Catches functions orphaned by Phase 3 `collapse_array_push_sequences` (e.g. `Array<T>::push` / `::grow` instantiations whose only call site was a single-element array literal). Marks via `module.dead_func_indices`; the actual removal + reindexing happens in compaction. The pass reads the `WirFuncId` ↔ array-index offset from `WirPackage::defined_func_base`, so the same implementation handles both the GC module (`DEFINED_FUNC_BASE`) and the linear-memory module (`0`); the latter is invoked from `codegen/component.rs::lower_core_module` where `dead_type_indices` is also populated to mirror the mem module's 1:1 function/type correspondence. E2E: [wir_optimize_dce_orphan_push.wado](../wado-compiler/tests/fixtures/wir_optimize_dce_orphan_push.wado).
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
