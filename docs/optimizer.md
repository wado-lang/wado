# Wado Optimizer

This document describes the optimization passes implemented in the Wado compiler. Each pass description links to a representative E2E fixture under `wado-compiler/tests/fixtures/`.

The optimizer rewrites the Normalized IR (NIR; see [WEP: NIR Layer](./wep-2026-05-11-nir.md)) in place before lowering to WIR. Pass span names used by `WADO_LIST_PASSES` / `WADO_SKIP_PASS` / `WADO_DUMP_PASS_*` carry a `nir/` prefix.

## Philosophy

When WebAssembly provides a native instruction for a feature, prefer it over a complex compiler transformation. This keeps the compiler small, leverages runtime JIT optimizations, and produces smaller output.

Examples: `select` for branchless conditionals, `array.copy`/`array.fill` for bulk operations, `return_call` for tail calls (planned).

## Optimization Levels

All levels run DCE (Dead Code Elimination) on functions, types, and globals.

| Flag            | Iterations | Inline Threshold | Notes                                             |
| --------------- | ---------- | ---------------- | ------------------------------------------------- |
| `-O0`           | 0          | N/A              | DCE only + `match_to_switch` + post-loop rewrites |
| `-O1`           | 2          | 4                |                                                   |
| `-O2` (default) | 10         | 13               |                                                   |
| `-O3`           | 30         | 32               |                                                   |
| `-Os`           | 10         | 13               | strips Wasm name section                          |

Optimization passes run in a fixed-point loop with early exit on convergence. The post-loop rewrites the Wasm backend depends on (`match_to_switch`, `select_lowering`, `multi_value_return`) run at every level, including `-O0`.

## Pipeline

The optimizer runs after lowering and before Wasm emission. `optimize.rs` orchestrates steps 1–6; `wir_optimize.rs` handles step 7.

1. Early DCE — remove unreachable functions/types/globals (all levels).
2. Fixed-point iteration loop (skipped at `-O0`):
   1. Match → Switch
   2. Container SROA
   3. Value-Copy Elision
   4. Value-Copy Demotion
   5. Short `push_str` Simplification
   6. Single-Field Parameter SROA
   7. Function Inlining
   8. List-Literal Materialization
   9. Adjacent-Use Box-Local Elision
   10. LabeledBlock Fusion
   11. Reference Elimination
   12. SROA
   13. Copy Propagation
   14. Dead Argument Elimination
   15. Dead Return Value Elimination
   16. Write-Only Local Elimination
   17. Common Subexpression Elimination
   18. Store-to-Load Forwarding
   19. Constant Folding
   20. Constant Branch Pruning
   21. Loop-Invariant Code Motion
   22. Condition Implication
   23. Template String Buffer Hoisting
3. Hot Field Scalarization — runs once after the loop converges.
4. Final DCE — clean up code made dead by optimizations.
5. Select Lowering — post-optimization rewrite (all levels).
6. Multi-Value Return classification — marks tuple/struct-returning functions whose every return site is a fresh literal and every call site destructures, so WIR build can emit the Wasm multi-value ABI (all levels).
7. WIR-level optimizations — see [WIR Optimizations](#wir-optimizations).

## NIR Optimization Passes

All NIR passes live in `wado-compiler/src/optimize/`. The optimizer module-level doc (`src/optimize.rs`) is the authoritative pass index; the sections below add E2E fixture pointers and per-pass design notes.

### Function Inlining (`inline.rs`)

Replaces small pure-function calls with their body, sized by an expression-count threshold. Eligible callees are pure, non-recursive, non-generic, take/return no references, are not from the core library, and fit under the threshold. `#[inline]` multiplies the threshold 5×, `#[inline(always)]` forces, `#[inline(never)]` blocks.

E2E: [opt_inline.wado](../wado-compiler/tests/fixtures/opt_inline.wado), [opt_inline_backtrack_miscompile.wado](../wado-compiler/tests/fixtures/opt_inline_backtrack_miscompile.wado).

### List-Literal Materialization (`array_literal.rs`)

Rewrites the `List<T>` builder window — an `List<T> { repr: array_new(N), used: 0 }` struct followed by `N` `List::push` calls — into `NirExprKind::ArrayLiteral { elements }`, giving a constant array the same first-class, analyzable value shape `StructLiteral` / `TupleLiteral` already have (`wir_build` lowers it to `array.new_fixed`). Runs after `inline`, which expands the `SequenceLiteralBuilder` `new_literal` / `push_literal` / `build` methods to expose the raw window. The push place may be the bound local (a direct `[…] as List<T>` literal) or a field of it (a custom `SequenceLiteralBuilder` that wraps an `List<T>`, e.g. `SeqVec { items: List<T> }`, `Bag { keys, values }` with interleaved per-field pushes). `List::push` is matched by its `CompilerItem::ListPush` marker. Single-use pure element temps that inlining leaves between pushes are resolved and dropped, guarded so a temp read elsewhere is never dangled. Empty literals (`array_new(0)`) are left growable. Subsumes the retired WIR `collapse_array_push_sequences`; see [WEP 2026-05-31](./wep-2026-05-31-nir-array-literal.md).

E2E: [array_literal_nir_materialize.wado](../wado-compiler/tests/fixtures/array_literal_nir_materialize.wado), [array_literal_side_effect_element.wado](../wado-compiler/tests/fixtures/array_literal_side_effect_element.wado), [array_append_collapse.wado](../wado-compiler/tests/fixtures/array_append_collapse.wado).

### Single-Field Parameter SROA (`sroa_param.rs`)

Rewrites internal functions whose parameter type is `&S` / `&mut S` for some single-field struct `S` (with `Box<T>` from `&primitive` auto-boxing the canonical case) to take the inner scalar `T` directly. At call sites, the corresponding `StructLiteral S { field: val }` allocation is replaced with `val`, eliminating heap traffic. Runs immediately before `inline` so the rewritten signature propagates through the rest of the fix-point loop.

Pins exports, CM bridges, dispatch wrappers, trait methods, allocator entry points, closure-functor `__call` methods, and `$value_copy$T<id>` helpers — all carry ABI / alias contracts the rewrite must not disturb. Validates each candidate's body to reject param writes (direct `Local` write, `Local.field = …`, `Local[i] = …`, `*Local = …`) and forwards through chained Call / MethodCall positions that are themselves SROA candidates. When the SROA'd parameter is the receiver of a MethodCall, the call is converted to a plain Call so that NIR DCE — which dispatches non-monomorphized MethodCalls by receiver type — still finds the rewritten callee. The receiver / arg's auto-ref wrapper (`&local` / `&mut local` synthesised by the lower phase) is peeled before the FieldAccess wrap, otherwise the inliner's type-driven param binding would emit `let self: T = &x.field;`. NIR analog of the legacy `wir_optimize/sroa_param.rs`.

Aliasing guard: a reference parameter is skipped when another reference parameter of the same function points at the same single-field struct (`param_may_alias_sibling`). Wado has no borrow checker and references alias, so `f(&mut n, &mut n)` is legal and the two parameters must observe each other's writes; rewriting one to a by-value scalar would snapshot it at the call site (`f(n.value)`) and drop writes made through the aliasing parameter during the call. By-value struct params are independent copies and are unaffected. E2E: [opt_sroa_param_aliased_ref.wado](../wado-compiler/tests/fixtures/opt_sroa_param_aliased_ref.wado).

### Adjacent-Use Box-Local Elision (`elide_box_local.rs`)

Targets the common `Box<T>` pattern produced by `lower::translate::wrap_in_box` once `sroa_param` strips the receiver / arg side down to a scalar: `let x = Box{value: inner}; … x.value …`. When `x` is defined exactly once and read exactly once via `FieldAccess { Local(x), field_name }`, this pass substitutes the single-field initializer at the use site and drops the `Let`. Soundness is witnessed by `mod_ref::can_move_past` on every intervening sibling statement (linear control transfer, no read of the candidate, no clobber of any local / global / heap / memory location the inner reads, no trap that races with one in the inner). The identity-escape gate consults `NirFunction::address_taken_locals` and `stores_aliased_locals`. NIR analog of the retired WIR-level `elide_adjacent_single_use_struct_locals` — at NIR the substituted expressions feed back into the same fix-point loop where `copy_prop` / `const_fold` / `dce` can fold them further. E2E: [opt_elide_adjacent_struct_local.wado](../wado-compiler/tests/fixtures/opt_elide_adjacent_struct_local.wado), [opt_elide_adjacent_struct_local_intervening_copy.wado](../wado-compiler/tests/fixtures/opt_elide_adjacent_struct_local_intervening_copy.wado).

### Value-Copy Elision (`value_copy_elide.rs`)

Strips the synthesized `$value_copy$T<id>(arg)` wrapper from `let x = $value_copy$T(arg)` (and the equivalent `Assign`) bindings whose target is observably read-only — when the source root that `arg` reads from is not assigned, field-mutated, or captured for the rest of the function, eliding the wrapper aliases storage in a way that's externally indistinguishable from the freshly-allocated copy.

Runs once per fixed-point iteration, before `inline`. The inliner expands every reachable `$value_copy$T` body into a labeled block, after which the `Call($value_copy$T, [arg])` shape the elider matches on no longer exists; running before inline is what lets the elider strip wrappers around `match make()? { Ok(v) => v, Err(e) => return Err(e) }`-style `?` desugarings (without the pre-inline ordering, the wrappers in every `parse_*` `?` site would survive through codegen). The only way a fresh wrapper `Call` shape can appear after lowering is for the inliner to expand a function whose body still contains a wrapper — those are caught by the next iteration's run, and if the loop converges (no pass returned `changed`) the inliner did nothing this round so no new wrappers were introduced.

The strip walker descends through every NIR expression that can syntactically embed a `NirBlock` (`If`, `Match`, `Switch`, `Block`, `LabeledBlock`, calls, struct/tuple/variant literals, …) so wrappers nested inside `let x = if cond { let y = $value_copy$T(...); ... } else { ... };` patterns — common in `parse_*` rule bodies — are reached.

E2E: [value_copy_elide_qmark.wado](../wado-compiler/tests/fixtures/value_copy_elide_qmark.wado).

### Value-Copy Demotion (`value_copy_demote.rs`)

Demotes a deep `$value_copy$T` of an `List<E>` to a shallow spine copy when the binding's elements are provably never mutated through it. Where `value_copy_elide` removes a copy whose target is fully read-only (aliasing the binding to the source), demotion handles a copy whose target is only _spine_-mutated (`sort`, `push`, …): full elision is unsound, but a shallow copy is still safe — the binding gets its own `repr` spine while sharing the element objects, which are immutable through this handle. The deep per-element copy (`array_clone` over a value-typed element) is rewritten to a shallow `array_clone_shallow` via a synthesized `$value_copy$T<id>$shallow` sibling helper.

The precondition is verified by an element-immutability analysis. A `&mut self` method (`List::sort`, `push`, …) is _element-immutable_ when, by a taint walk over its body, no value derived from `self` (its spine or an element) is field-written, `&mut`-borrowed, or handed to an opaque callee — only spine builtins and `&`-immutable forwarding are allowed. The demote site itself is eligible when every use of the bound handle (and of the source the copy reads from) is element-clean: spine-only methods, index/field reads, by-value or `&` argument passing.

The analysis is conservative — an unrecognized shape rejects demotion (no change), never miscompiles. Naming compiler intrinsics (`builtin::array_*`) is sound because they are not stdlib identifiers; stdlib method behaviour is _derived_ from the body, never hardcoded by name.

<!-- TODO(optimizer): expose the element-immutability analysis to `container_sroa` so its hardcoded `push`/`is_empty`/`len`/indexing whitelist can be replaced by a query that accepts any element-immutable `&self`/`&mut self` method. -->
<!-- TODO(optimizer): support nested-container demotion (`List<List<T>>`). The recursion guard at `is_element_immutable_method` returns `false` for any recursive call site, so the immutability proof bottoms out at the first unknown shape. -->

E2E: [value_copy_demote.wado](../wado-compiler/tests/fixtures/value_copy_demote.wado).

### Container SROA (`container_sroa.rs`)

Decomposes `List<Tuple<...>>` and `List<UserStruct>` locals into N parallel `List<T_k>` locals (AoS → SoA), eliminating the per-element `struct.new` for the container payload. Tuples and user structs are both WasmGC structs at the Wasm level, so the pass treats them uniformly.

A candidate is decomposed only when every use matches a whitelist: `v.push(literal)`, `v.push(other[j])` from another candidate, `v[i] = literal`, `v[i].field`, `v.len()`, `v.is_empty()`, or initialization via `[]` / `List::with_capacity`. Any other use (bare reference, closure capture, opaque method) marks the local as escaped and propagates to its sources via fixpoint. Cross-candidate index sources require the index to be `is_duplicable_expr` because the rewrite clones it N times. The pass runs first in the loop so the whitelist sees unobfuscated patterns before inlining rewrites them.

E2E: [opt_container_sroa_struct.wado](../wado-compiler/tests/fixtures/opt_container_sroa_struct.wado), [opt_container_sroa_tuple.wado](../wado-compiler/tests/fixtures/opt_container_sroa_tuple.wado), [opt_container_sroa_edge.wado](../wado-compiler/tests/fixtures/opt_container_sroa_edge.wado), [opt_container_sroa_nondup_idx.wado](../wado-compiler/tests/fixtures/opt_container_sroa_nondup_idx.wado).

Future directions:

- [ ] Nested containers (`List<List<T>>`). Tracked at `container_sroa.rs:16`.
- [ ] Container fields of structs (via HFS hoisting).
- [ ] Push-to-literal fusion with `array.new_fixed`.
- [ ] Parallel index-assign coalescing.
- [ ] Cross-function propagation via `stores`-aware summaries.
- [ ] Consult `value_copy_demote`'s element-immutability analysis instead of the hardcoded use-shape whitelist; would accept arbitrary element-immutable methods.

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

### Dead Argument Elimination (`dae.rs`)

Removes parameters that the callee body never reads, together with the corresponding argument expression at every call site. Dropped arguments must be pure so removal cannot change observable behaviour.

After shrinking the parameter list, the pass renumbers the function's `locals[]` so that `params[k].local_index == k` continues to hold — `wir_build/translate.rs` declares `locals[i for i >= params.len()]` as body locals, and a stale dead-param slot left in place would silently re-emit a duplicate WIR `DeclareLocal` with the same name as a live param. Body `Local`, pattern `Binding`, closure `outer_index`, and `VariadicForOf` `binding_local` all get the same remap.

Pinning is conservative: CM bridges (`is_cm_export`, `is_cm_binding`, `is_dispatch_wrapper`), `is_ambient` functions, builtin / wasm-asset modules, trait methods (vtable-shaped), and any function whose pointer is taken via `FuncRef` are all skipped. `is_export` and `is_async` are _not_ pinned — every user `export fn` reaches the runtime through its synthesised `is_cm_export` wrapper (which is pinned), and `is_async` is just propagated source metadata that has no call-shape constraint after desugar lowers the body to `cm_raw_call task-return(...)`.
A method whose `self` is dead is rewritten by the rewriter at every call site: `MethodCall(recv, name, args)` collapses to `Call(method_func, args)`. The validator gates this on receiver purity so dropping the receiver evaluation cannot strip an observable effect.

E2E: [wir_optimize_dae.wado](../wado-compiler/tests/fixtures/wir_optimize_dae.wado).

### Dead Return Value Elimination (`drve.rs`)

Converts a non-void function whose return value is always dropped at every call site into a void-returning function. Every `Return { value: Some(_) }` becomes `Return { value: None }` once the value is verified pure, and the call sites stay structurally identical (the call expression now produces `Unit`).

Conservative scope to avoid breaking fixture-asserted body shapes:

- The return type must be heap-allocated (`Struct` / `Variant` / `BuiltinArray` / `GenericInstance`). Primitive-returning helpers like `fn f() -> i32 { return c.threshold + c.scale; }` save nothing from being voided and can break test fixtures that assert post-optimizer body shape.
- The body must end in an explicit `Return { value: Some(_) }` so we never have to reason about an implicit trailing-value return path.
- Every other `Return` in the body must also carry a pure value.
- Every observed call site must appear as a top-level `Expr(Call(f, ...))` / `Expr(MethodCall(f, ...))`. Any nested or `Let`-bound use disqualifies the candidate.
- At least one observed call site must exist (otherwise DCE would delete the function anyway).

After conversion, the pass also rewrites `expr.type_id` on every call site of a converted function. Without this step, `Expr(Call(f))` in stmt position still claims the old return type and `wir_build/translate.rs` wraps the call in `Drop`, underflowing the Wasm stack.

### Write-Only Local Elimination (`elide_local.rs`)

Eliminates `let x = expr;` bindings where the local `x` is never read, never has its address taken, and never escapes via closure capture or a `stores`-aliased call. When `expr` is pure the entire statement is removed; otherwise the binding is replaced by `Expr(expr)` so the side effect still runs.

Closure captures' `outer_index` are conservatively counted as reads, even though the closure body uses its own local-index namespace — the over-mark only suppresses elision and never produces a wrong transform.

### Common Subexpression Elimination (`cse.rs`)

Eliminates duplicate pure binary expressions inside loop bodies. When the same expression appears in both the loop guard and the body and its operand locals are not modified between occurrences, it is computed once into a local and reused — covers idiomatic `while p * p <= limit { ... = p * p; ... }` patterns.

E2E: [wir_optimize_cse.wado](../wado-compiler/tests/fixtures/wir_optimize_cse.wado).

### Store-to-Load Forwarding (`store_load_forward.rs`)

When a literal is stored to a local and later loaded with no intervening modification, the load is replaced with the stored value. Selective invalidation at control-flow boundaries only invalidates locals actually modified within branches.

E2E: [opt_hfs_stores_ref_sync.wado](../wado-compiler/tests/fixtures/opt_hfs_stores_ref_sync.wado).

### Constant Folding (`const_folding.rs`)

A thin NIR visitor that walks each function body via `opt_walk_expr` and asks the [NIR Interpreter (`niri`)](#nir-interpreter-niri) to apply its local rewrite rules at every node. All reduction logic — literal folding, integer cast collapsing, the `&&` / `||` short-circuit identity rules, and `GlobalVarGet` rewriting for immutable globals — lives in `niri`; this pass owns no rewrite logic of its own.

E2E: [const_fold.wado](../wado-compiler/tests/fixtures/const_fold.wado), [opt_const_fold_div_zero.wado](../wado-compiler/tests/fixtures/opt_const_fold_div_zero.wado).

### NIR Interpreter (`niri`)

`niri` (`src/niri.rs`) is the partial evaluator that backs constant folding. The canonical entry point is

```rust
Interpreter::new(type_table).reduce(&expr) -> NirExpr
```

`reduce` is idempotent and monotone: it always returns a (possibly identical) `NirExpr`, leaving literal leaves with their original lexical repr (`0xFF` is not rewritten to `255`). Visitor drivers that already walk every NIR kind via `nir_visitor::opt_walk_expr` use `reduce_local(&mut NirExpr) -> bool` instead, which performs only the single-node rewrite at `expr`. Unit tests can use `reduce_to_value(&NirExpr) -> Option<Value>` to extract a `Value` directly.

Today the engine reduces literal-only Binary / Unary / Cast expressions, the short-circuit identity rules `false || X → X` and `true && X → X` (and their right-hand variants), `let`-bound locals via a per-function `env`, `if` expressions and statements (constant-condition splice; bool-arms collapse — `if cond { true } else { false } → cond` and the inverted `→ !cond`; both-arms-equal collapse), and `match` expressions over payload-free patterns (constant-scrutinee chosen-arm splice; the two-arm `match X { Enum::Case => true, _ => false } → X == Enum::Case` collapse that subsumes the `matches` operator's shape for enum scrutinees; all-arms-equal collapse, covering wildcard / integer / bool / char literals, integer and char ranges, or-patterns, and `ConstantValue`). Future work — payload-aware variant matching, bounded loop unrolling, pure function inlining, and a complementary wasm-CTFE backend — is described in [WEP: NIR Interpreter Evolution Plan](./wep-2026-04-27-nir-interpreter.md).

Unit tests: [`wado-compiler/tests/niri.rs`](../wado-compiler/tests/niri.rs).

### Constant Branch Pruning (`const_branch_prune.rs`)

Eliminates branches with compile-time-known boolean conditions and simplifies degenerate block patterns:

- Empty blocks → `()`.
- Single-expression blocks (`{ expr }`) → `expr`.
- Tail-break-only labeled blocks (`label: { stmts...; break label: V }`) → `{ stmts...; V }` when the only reference to `label` is the trailing break.
- Stmt-position tail-break-only labeled blocks (`label: { stmts...; break label; }`) → straight-line `stmts...`.
- Dead statements after a `break` / `continue` / `return` in the same stmt list.
- Labeled-block copy propagation: when a block starts with `let x = y` and neither name is modified within, `x` is substituted by `y` and the binding is dropped — flattening residual parameter copies left by inlining.

`__tmpl:` labeled blocks are carved out during the optimizer fixpoint so that `tmpl_hoist` can anchor on them. A separate post-fixpoint invocation (`nir/branch_prune_final`) flattens them once `tmpl_hoist` has finished, iterating until convergence.

E2E: [opt_wir_dead_if_zero.wado](../wado-compiler/tests/fixtures/opt_wir_dead_if_zero.wado), [array_bounds_elim_const_wir.wado](../wado-compiler/tests/fixtures/array_bounds_elim_const_wir.wado), [opt_dce_break_then_unreachable.wado](../wado-compiler/tests/fixtures/opt_dce_break_then_unreachable.wado), [opt_dce_tail_break_flatten.wado](../wado-compiler/tests/fixtures/opt_dce_tail_break_flatten.wado), [opt_dce_trap_preserved_unread_let.wado](../wado-compiler/tests/fixtures/opt_dce_trap_preserved_unread_let.wado).

### Loop-Invariant Code Motion (`licm.rs`)

Hoists loop-invariant field accesses out of loops when the target variable is not modified within the loop body.

Nested reference-field chains (`a.b.c`) are hoisted one level per fixpoint iteration: a mutate-through-reference write (`a.b.pos = x`, a pure field chain) assigns a field of the inner object `*a.b` and so is no longer treated as a full clobber of the root local `a`. This lets LICM hoist `a.b` into a local, then `a.b.input`, then `a.b.input.repr`, sharing the loads across the loop. It is the engine behind the JSON deserializer's `JsonStructAccess { de: &mut JsonDeserializer }` key-scan loop, whose reads go through `self.de.input.repr` while `self.de.pos` is bumped in place. Writes that are not pure field chains (`a[i].c = x`, `(*p).c = x`) still mark the root fully modified.

Aliasing soundness: the per-local / `let a = b` alias tracking only sees writes through `x` itself or its copies. Wado references alias (a write through one `&mut T` is observed through any other `&T`/`&mut T` to the same object), so to hoist `x.f` for a reference-typed `x`, LICM additionally requires that field `f` of that pointee type is not written anywhere in the loop — `written_field_types` records every field write keyed by `(pointee_type, field_index)`, and `is_reference_field_aliasing_written` blocks the hoist otherwise. This sidesteps heap alias analysis: it keeps the nested-chain hoist sound when two references collide (`g.a` / `g.b` to one node, or two `&mut` params to one object) while still hoisting genuinely invariant fields (the deserializer's `input` is read-only; only `pos` is written).

E2E: [opt_licm_immut_ref.wado](../wado-compiler/tests/fixtures/opt_licm_immut_ref.wado), [opt_licm_immut_ref_method.wado](../wado-compiler/tests/fixtures/opt_licm_immut_ref_method.wado), [opt_licm_mut_ref_no_hoist.wado](../wado-compiler/tests/fixtures/opt_licm_mut_ref_no_hoist.wado), [opt_licm_nested_ref_chain.wado](../wado-compiler/tests/fixtures/opt_licm_nested_ref_chain.wado), [opt_licm_aliased_mut_ref_params.wado](../wado-compiler/tests/fixtures/opt_licm_aliased_mut_ref_params.wado), [opt_licm_aliased_ref_fields.wado](../wado-compiler/tests/fixtures/opt_licm_aliased_ref_fields.wado).

### Condition Implication (`condition_implication.rs`)

Eliminates conditions implied false by dominating guards. Subsumes the former WIR-level bounds-check elimination at the NIR level. Handles:

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

### Match → Switch (`match_to_switch.rs`)

Rewrites `match` expressions whose scrutinee is a dense integer or enum into a `Switch` node, which lowers to a Wasm `br_table` rather than a chain of `br_if`. Runs first in every fixed-point iteration so subsequent passes see the `Switch` shape their variant-walking arms already handle, and also at `-O0` so the `br_table` path stays live when the optimizer loop is skipped.

### Select Lowering (`select_lowering.rs`)

Rewrites `if cond { a } else { b }` with two leaf-pure branches into `builtin::select(cond, a, b)`, which emits the Wasm `select` instruction. Runs after the fixed-point loop at all levels.

Leaf-pure shapes: duplicable leaves (`Local`, integer / float / bool / char literals), `Unary { Neg | Not | BitNot }` over a leaf-pure operand, `Binary { non-Div, non-Mod }` over two leaf-pure operands, and `Cast` of a leaf-pure value. Calls, `Deref`, `Ref` / `MutRef`, division, modulo, and aggregate constructors stay branched — they either trap or have side effects that an unconditionally-evaluated arm cannot replicate safely.

E2E: [select_basic.wado](../wado-compiler/tests/fixtures/select_basic.wado), [select_extended_arms.wado](../wado-compiler/tests/fixtures/select_extended_arms.wado), [select_no_opt.wado](../wado-compiler/tests/fixtures/select_no_opt.wado), [select_no_opt_trapping_arms.wado](../wado-compiler/tests/fixtures/select_no_opt_trapping_arms.wado).

### Multi-Value Return Classification (`multi_value_return.rs`)

Marks tuple- or user-struct-returning functions whose every return site is a fresh literal (`TupleLiteral` / `StructLiteral`) and whose every call site destructures via `FieldAccess` on the bound temp. WIR build (`wir_build::translate::try_emit_multi_value_let`) reads the marker to emit the multi-value Wasm signature on the function definition and to rewrite call-site `let __tmp = Call(f)` into `MultiValueLocalBind [__tmp_0, …] = Call(f)` with subsequent `FieldAccess` reads going to the split locals directly. Runs after every other NIR transformation so the analysis sees the final shape.

The variant-return path is a WIR-level rewrite (`wir_optimize::variant_return_sroa`); see [Phase 1: Type Representation](#phase-1-type-representation).

<!-- TODO(optimizer): the analysis conservatively requires literal returns at every return site and field-destructuring at every call site (multi_value_return.rs:311, 491). Adding a per-callee summary that promotes once-bound multi-value temps to direct destructures would unlock multi-value for the common "build a result struct, return it" pattern wrapped in helper layers. -->

### Visitor Infrastructure

`nir_visitor.rs` and `wir_visitor.rs` provide shared `*MutVisitor` / `*RefVisitor` / `*OptVisitor` traits used by every pass that does plain pre/post-order traversal. Centralizing Block/Loop/If/Seq traversal here keeps individual passes free of duplicated walk logic. `NirOptVisitor` exposes change-tracking (`-> bool`) for fixed-point convergence; free functions `opt_walk_block/_stmt/_expr/_pattern` recurse into children, and `visit_project_functions` drives a visitor across every function body in a `NirPackage`.

Passes that need flow-sensitive state (per-block scope tracking, per-iteration dataflow lattices, branch-join state convergence) keep their own walkers — `field_scalarize`, `licm`, `tmpl_hoist`, `value_copy_demote`, and `store_load_forward` fall in this bucket and intentionally do not use the generic visitor.

## Lowering Optimizations

NIR→WIR lowering (`wir_build/`) also avoids emitting redundant shapes in a few targeted spots. These are not fixed-point passes; they fire once while the cascade is being built and are effective at all optimization levels including `-O0`.

### Exhaustive Match Last-Arm Elision (`wir_build/pattern_match.rs`)

For `match` expressions whose unguarded arms exhaustively cover every case of the scrutinee's variant or enum type, the final arm in source order is guaranteed to match by exclusion — its pattern test and the trailing `unreachable` fallback are both dead. `translate_match` recognises this via `compute_emitted_as_irrefutable` and treats the last arm as irrefutable (bindings + body only, no surrounding `If`). Removes one pattern test and one branch per `?` on the hot path of every `Result`/`Option`-heavy function, which is a significant fraction of deserializers.

Conservative — only fires when every arm is `Variant`, `Enum`, or a one-level `Or` of those (with no guards, distinct case indices, and a count equal to the total cases of the scrutinee type). Anything else (wildcards, literals, ranges, guards, nested `Or`s) falls back to the standard `unreachable`-tailed cascade.

E2E: [pattern_match_exhaustive_variant_last_arm.wado](../wado-compiler/tests/fixtures/pattern_match_exhaustive_variant_last_arm.wado), [pattern_match_non_exhaustive_keeps_fallback.wado](../wado-compiler/tests/fixtures/pattern_match_non_exhaustive_keeps_fallback.wado).

## WIR Optimizations

`wir_optimize.rs` runs after WIR build and before Wasm emission, mutating the `WirPackage` in place. Phases run in order; passes within a phase may iterate.

### Phase 1: Type Representation

- Nullable ref optimization — rewrites type-level representations for nullable references.
- Pre-SROA copy propagation — inlines trivial `alias = source` so SROA can see direct variant access (RefTest/RefCast on source).
- Variant-return SROA — rewrites functions returning a small variant (`(i32 disc, payload_0, ...)` lowering, total arity 2–4) to use Wasm multi-value returns, eliminating the boundary GC allocation. Tuple- and user-struct-return ABIs are decided by the NIR-level `optimize::multi_value_return` classifier; this pass handles only the variant case, whose layout (shared-vs-per-case payload offsets) is WIR-specific.

Single-field parameter SROA used to live here; it moved to NIR (`optimize::sroa_param`) so the rewritten signature feeds the rest of the NIR fix-point loop (inline / copy_prop / dce / cse / const_fold) instead of running once after WIR build. E2E (still valid): [opt_sroa_box_parameter.wado](../wado-compiler/tests/fixtures/opt_sroa_box_parameter.wado), [opt_sroa_single_field.wado](../wado-compiler/tests/fixtures/opt_sroa_single_field.wado).

### Phase 2: Single-Field Struct Local Elimination (Round 1)

Substitutes `StructGet(LocalGet(x), field)` with the inner value when `x` is defined by `LocalSet(x, StructNew { [inner] })`. Runs after parameter SROA so freshly exposed locals are caught.

Two complementary variants run in sequence:

- Re-evaluation-safe elision (`elide_single_field_struct_locals`) — substitutes when the inner field initializer is referentially transparent (no heap reads, no calls, no allocations). Safe regardless of how far apart def and use are. The relaxed adjacent-use variant that used to follow this pass moved to NIR (`optimize::elide_box_local`); see the NIR section below.

### Phase 3: Data Flow

List literals arrive as `array.new_fixed` directly (the NIR [List-Literal Materialization](#array-literal-materialization-array_literalrs) pass + `wir_build` lowering); the WIR-level collapse that used to reconstruct them here is retired.

- Forward struct field constants — tracks known field values (constants and `LocalGet` references) through `StructGet` for constant-index bounds-check elimination. Resolves block-result `StructNew` patterns for single-exit blocks. Uses `stores`-aware alias analysis: locals passed to functions without `stores` declarations are not marked aliased, enabling field forwarding across calls. E2E: [array_bounds_elim_const_wir.wado](../wado-compiler/tests/fixtures/array_bounds_elim_const_wir.wado).

### Phase 4: Library-Specific Rewrites

- Simplify short string appends — rewrites `append(short_const)` into a sequence of `append_char`.
- Constant array data promotion — replaces `array.new_fixed` with `array.new_data` when all elements are compile-time constants of a primitive type (≥16 elements).
- Split large array literals — rewrites `array.new_fixed` with >256 elements into `array.new_default` + `array.set` to avoid pathological JIT register allocation.

### Phase 5: Peephole and Multi-Field Struct Elimination

- Peephole — constant folding and small Wasm-instruction-selection rewrites that have no NIR analogue. Constant integer-comparison folding; dead `If` elimination; `eqz` folding (`i32.eq(x, 0)` → `i32.eqz x`) and negated-comparison folding (`i32.eqz(i32.le_s a b)` → `i32.gt_s a b`); branchless increment; redundant byte-mask elision; sign-extension folding (`i32.extend8_s(i32.load8_u a)` → `i32.load8_s a`, the idempotent re-extend cases, and `i32.extend8_s(x & 0xFF)` → `i32.extend8_s x`); redundant `ref.cast` / `ref.test` elimination against the WIR static type (exact-`type_id` identity casts collapse to the operand, or to `ref.as_non_null` when only the non-null assertion remains; always-true tests fold to `1`); GC-operand nullability relaxation; and `local.set` + first-`local.get` fusion into `local.tee`. The redundant-`ref.as_non_null` elision keys on `WirInstr::is_nonnull_result` (which now also recognises non-nullable `ref.cast`) and lives in `cleanup`. E2E: [wir_optimize_negate_eqz.wado](../wado-compiler/tests/fixtures/wir_optimize_negate_eqz.wado), [wir_optimize_branchless_increment.wado](../wado-compiler/tests/fixtures/wir_optimize_branchless_increment.wado), [wir_optimize_sign_extend.wado](../wado-compiler/tests/fixtures/wir_optimize_sign_extend.wado), [wir_optimize_local_tee.wado](../wado-compiler/tests/fixtures/wir_optimize_local_tee.wado).
- Flatten seq assignments — exposes multi-field struct locals for elimination.
- Multi-field struct local elimination — substitutes `StructGet(LocalGet(x), field_k)` with the corresponding field expression when all fields are accessed exactly once.
- Labeled-block copy propagation — flattens trivial labeled blocks holding only a copy. E2E: [wir_optimize_labeled_block_copy_prop.wado](../wado-compiler/tests/fixtures/wir_optimize_labeled_block_copy_prop.wado), [wir_optimize_labeled_block_copy_prop_safety.wado](../wado-compiler/tests/fixtures/wir_optimize_labeled_block_copy_prop_safety.wado).

### Phase 6: Write-Only Local Elimination (WIR-synthesised locals)

Write-only-local elimination is split across two layers. The NIR pass (`optimize::elide_local`) handles locals that originate at NIR (user `let`, SROA / variant-lowering shadow temps); it lives in the fixed-point loop so the freshly dead expressions feed `copy_prop` / `const_fold` / `dce` in the same iteration. The WIR pass here in Phase 6 handles locals that the WIR builder synthesises during lowering — `__match_scrut_N` for match scrutinee binding, `__pair_temp_N` and `__mv_lo_N` / `__mv_hi_N` for Future / Stream pair returns and wide-int multi-value bindings — that no NIR pass can reach. Both passes rewrite `LocalSet(x, v)` to `Drop(v)` (or `Nop` when `v` is pure) only when `x` is never read.

DAE and DRVE live at NIR (`optimize::dae`, `optimize::drve`) alongside `inline` / `copy_prop` / `const_fold` / `dce`.

### Phase 7: Global Cleanup

Constant global-initializer promotion (`const_global.rs`) — a user-immutable global (`global X = …`) whose non-constant initializer was extracted into an `__initialize_module` runtime assignment is promoted back to an eager Wasm constant once NIR optimization has folded that assignment to a const (`struct.new` / `array.new_fixed` / scalar). The value is moved into the global's `init`, the global is marked immutable, and the now-redundant `GlobalSet`(s) are dropped — even when the init is inlined into a duplicated `__inline___initialize_modules` guard block. Const-ness is decided here, once, via `WirInstr::is_const_expressible` (which `codegen`'s `push_const_instrs` mirrors); this subsumes the former scalar-only NIR `const_global_promotion`. Strings stay lazy because a `String`'s `array.new_data<u8>` repr is not a valid Wasm constant instruction. See [WEP: Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md). E2E: [const_global_object.wado](../wado-compiler/tests/fixtures/const_global_object.wado), [const_global_entry.wado](../wado-compiler/tests/fixtures/const_global_entry.wado).

Trivial init-guard removal — removes compiler-generated module-initialization guard blocks when no actual initialization remains.

### Shared facilities

- Per-expression mod/ref summary (`optimize/mod_ref.rs`) — `ModRef::of_expr(...)` / `ModRef::of_stmt(...)` returns a conservative `(local_reads, local_writes, global_reads, global_writes, heap, memory, control, calls, allocates, may_trap)` summary of a `NirExpr` or `NirStmt` and its sub-tree. Passes consume it through three predicates: `is_re_evaluation_safe` (can the expression be moved to a later program point?), `may_clobber` (could `self`'s writes invalidate `other`'s reads?), and the `can_move_past` convenience (the common "skip an intervening statement while erasing a candidate local" check used by `elide_box_local`). Wasm-semantics-accurate on calls: callees cannot reach the caller's Wasm locals, so a call clobbers only `global_reads` / `heap.reads` / `memory.reads`. Unrelated to Wado's algebraic-effect / `with`-clause machinery in `effect_check.rs`; the name follows the LLVM `ModRefInfo` / GCC `mod`/`ref` convention from classical compiler optimization. Granularity is intentionally coarse for now (single read/write bits per heap and memory channel, "calls clobber everything-but-locals"); refining the internal representation does not require call-site churn because passes never inspect it directly. The WIR-level predecessor lived at `wir_optimize/mod_ref.rs` and was retired when its sole consumer (`elide_adjacent_single_use_struct_locals`) moved to NIR.

### Phase 8: Final DCE and Compaction

- Dead defined-function elimination — `mark_unreachable_defined_functions` walks `module.exports` + `module.elements` and BFSes the WIR call graph, marking unreachable defined-function indices as dead. Catches functions whose only call site never materialized (e.g. `List<T>::push` / `::grow` instantiations for a single-element array literal, whose `push` chain the NIR [List-Literal Materialization](#array-literal-materialization-array_literalrs) pass turns into `array.new_fixed`). Marks via `module.dead_func_indices`; the actual removal + reindexing happens in compaction. The pass reads the `WirFuncId` ↔ array-index offset from `WirPackage::defined_func_base`, so the same implementation handles both the GC module (`DEFINED_FUNC_BASE`) and the linear-memory module (`0`); the latter is invoked from `codegen/component.rs::lower_core_module` where `dead_type_indices` is also populated to mirror the mem module's 1:1 function/type correspondence. E2E: [wir_optimize_dce_orphan_push.wado](../wado-compiler/tests/fixtures/wir_optimize_dce_orphan_push.wado).
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
- [ ] Function Specialization for known constant arguments.
- [ ] Argument Promotion — promote `&T` fields to scalar parameters.
- [ ] Jump Threading.
- [ ] Reassociation — group constants in associative chains.
- [ ] SimplifyCFG — general control-flow-graph simplification.
- [ ] Tail Call Optimization — emit `return_call` for tail-recursive calls.
- [ ] Bounds-check elimination for chained sequential access (`arr[0]; arr[1]; arr[2]`).

## Tried and Found Ineffective

- Empty-array singleton for struct field defaults — sharing a single `array.new<u8>(0)` global across all default `String` initializations in serde `Deserialize` impls. Measured no performance improvement; the GC allocator handles tiny zero-length arrays efficiently enough that the overhead is negligible.
- `array.copy` for `List::grow` — replacing the element-by-element copy loop with the Wasm `array.copy` instruction. Was several times slower than the loop, likely due to poor JIT optimization of `array.copy` in current runtimes.

## Testing Strategy

- Golden fixtures — `tests/generated/fixtures/*.wir.wado` captures optimized WIR output. Regenerate with `mise run update-golden-fixtures`.
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

- [List Bounds Check Elimination in CLR](https://learn.microsoft.com/en-us/archive/blogs/clrcodegeneration/array-bounds-check-elimination-in-the-clr)

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
