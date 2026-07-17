# Optimizer TODO — Smell Backlog

Findings from a full review of `src/optimize.rs`, `src/optimize/`, `src/wir_optimize.rs`, and `src/wir_optimize/` (2026-07-16, tree at 550f7454b — line numbers refer to that commit).

Severity legend:

- P0 — reproduced end-to-end: correct at `-O0`, wrong at `-O2` (or a compiler crash). Per CLAUDE.md these are P0 compiler bugs: minimal e2e fixture, issue, fix — before anything else. Repro sources are embedded below.
- P1 — soundness hole traced through the code but not yet reproduced end-to-end. Attempt a repro first; if it reproduces, it is P0.
- P2 — precision: the analysis is more conservative than the rewrite (or than a sibling pass) and misses optimizations, or a latent bug currently masked by another bug.
- P3 — structure: duplication, dead code, fragile matching, compiler-side performance.

## P0 — reproduced miscompiles and crashes

All reproduced P0s are fixed, each with an `opt_*` E2E fixture (red at `-O2`
before the fix, green after): `match_to_switch` wildcard ordering, the two
`ref_elim` capture-interval holes (loop back-edge, `&mut` alias), trap-erasing
purity in `dae`/`drve`/`elide_local`, the three `labeled_block_fusion` holes
(temp-used-after, hidden break, loop capture), and the `#[inline(always)]`
recursion crash.

## P1 — soundness holes traced by review (repro first, then fix)

### Shared NIR facilities

- [ ] `optimize/dce.rs:2061-2101` — `expr_has_side_effects` treats non-`never` calls as effect-free (only `type_id == NEVER` subexpressions are preserved), contradicting its caller's contract in `remove_dead_global_sets_block`: a dead global initialized by `G = build()` where `build` writes a live global/prints/asserts is deleted wholesale. Fix: treat calls and trapping ops as effects, or reuse `arena_query::is_pure_expr`/`ModRef` instead of a fourth ad-hoc purity predicate.

### NIR passes

- [ ] `optimize/store_load_forward.rs:145-205` — a stored constant is forwarded into a `&`/`&mut` referent position, destroying the place: `obj.f = 5; g(&mut obj.f)` → `g(&mut 5)`, losing the callee's write-back. `extract.rs::is_place_read` guards exactly this in the sibling pass (and `extract.rs::is_ref_place` is dead code). Fix: skip reads in ref/deref-place positions (share `is_place_read`).
- [ ] `optimize/scalar_forward.rs:179-218` — `writes_overlap` is purely syntactic (Assign targets + `MutRef` nodes, root-index equality), missing mutating calls through bare ref-typed locals and writes through previously-captured aliases; the pass consults neither `mod_ref::may_clobber` nor the `address_taken`/`stores_aliased` sets its sibling `store_load_forward` uses. Fix: when the forwarded value reads a field place, reject use statements containing calls (or gate via `ModRef::of_stmt`), and exclude address-taken/aliased roots.

### WIR passes

- [ ] `optimize/array_literal.rs:199-206,224-258,305-312` — with a single target, `temp_binding` accepts impure `let` values, and the abort guards fire only when the temp is read; a consumed-but-never-read impure binding would have its side effect dropped by the window `drain`. Not reachable with today's lowering, but unenforced. Fix: bail (or keep the statement) when a consumed impure binding has zero substituted uses.

## P2 — precision and latent issues

### NIR

- [ ] `optimize/labeled_block_fusion.rs:654-663` — `check_lb_breaks_in_operand` uses `is_some_and`, so any promoted `Operand::Value` (which cannot contain a break) vetoes fusion; the threading twin `validate_exits_in_operand` (1927-1937) correctly uses `is_none_or`. Fix: flip the polarity.
- [ ] `optimize/dce.rs:191,223-358` — `extend_reachable_for_optimizer_passes` also runs during the final DCE where no rewrite can fire, keeping `String::push` and transitive callees alive as pure output bloat. Fix: gate the virtual-edge extension on the pre-loop invocation.
- [ ] `optimize/scalar_forward.rs:81,119-138` — uses nested in a sub-block of the adjacent statement are rejected even for pure non-trapping scalars, and `Div`/`Mod` are banned even with a constant non-zero divisor. Fix: allow sinking into sub-blocks for trap-free values; admit const-non-zero divisors.
- [ ] `optimize/match_to_switch.rs:309-333,347-372` — no cost model on arm cloning: range expansion clones the arm body per value (up to `SWITCH_MAX_RANGE = 1024`) and default-hole filling clones `arms[0]` per hole; `covered` double-counts overlapping specs, skewing the density gate. Fix: add a clones × arm-size budget (or share repeated arms via one labeled block) and de-duplicate covered values.

### WIR


## P3 — structure, duplication, fragility

### Consolidate shared predicates and walkers

- [ ] One may-trap / purity oracle: `arena_query::is_pure_expr`, `mod_ref` `may_trap`, `elide_box_local::LeftmostWalk`, `select_lowering::is_select_eligible`, and `dce::expr_has_side_effects` each encode their own effect/trap taxonomy and already disagree (root cause of the P0 trap-deletion bug). Centralize per-`ExprKind`/`NirBinaryOp` classification in `arena_query`/`mod_ref`.
- [ ] One "locals possibly mutated in subtree" query: the canonical implementation now exists (`arena_query::locals_possibly_mutated`, backed by the shared witness dispatch `for_each_mutated_root`; copy_prop migrated). Remaining: migrate `const_folding::record_loop_write` and `condition_implication::node_modifies` onto it and drop its `#[allow(dead_code)]`. Note: the receiver-wrapper caveat is documented at the query (boxing erases `&mut`/`&` on boxed-scalar receivers — mutation is recognized by the declared pre-boxing bit, not the wrapper shape).
- [ ] `optimize/labeled_block_fusion.rs:395-508,551-701,1839-1994` (+ the two transform walkers and `subst_variant_payload_*`) — three near-identical break-exit walkers plus two rewriters with subtly different coverage; the divergences are the P0/P2 fusion bugs. Factor one label-aware exit visitor parameterized by a per-exit callback.
- [ ] `optimize/multi_value_return.rs:456-734` + `optimize/dae.rs:278-292` + `optimize/drve.rs:216-290` — triplicated call-site-scan walkers that diverge (multi_value_return skips global initializer bodies; the others scan them). Extract a shared arena walker parameterized by a call-shape callback; align initializer coverage.
- [ ] `optimize/sroa.rs:527-605,647-694,697-952,1016-1107` — four copy-paste traversals (`escape_*`, `field_access_node`, `soft_*`, `rewrite_*`) with the `Assign`-target special case maintained in triplicate; `soft_expr` threads 7 params through two `too_many_arguments` shims. Consolidate into one parameterized walker.

### String-typed and fragile identity

- [ ] `optimize/inline.rs:273-317,402-457,1220-1236` — the call graph and candidate map are keyed on `"{module_path}/{name}"` strings (with a 30-line doc about a mangling divergence the scheme must sidestep) although every call site carries a resolved `FuncId`. Key on `FuncId`; delete `function_inline_key` and the name-fallback heuristic.
- [ ] `optimize/dce.rs:967-1047,1049-1076` — `record_call`/`record_method_call` parse mangled names inline (`find("::")`, `find('^')`) and duplicate the base-name reconstruction; name-format knowledge belongs in `name.rs`. Move it there.
- [ ] `optimize/dae.rs:150-165` + `optimize/array_literal.rs:57-95` — closure inspect impls matched by literal `"Inspect"`/`"InspectAlt"` strings; `builtin::array_new` found by name while the same pass uses `CompilerItem::ListPush` for push. Add `CompilerItem` markers.
- [ ] `wir_optimize/nullable_ref.rs:454-510` — variant rewrites key on `field_name == "discriminant"` / `starts_with("payload_")` and one exact operand orientation, mirroring what `wir_build/pattern_match.rs` emits today; drift produces an invalid-Wasm ICE. Centralize the naming/shape constructors shared with the builder.

### Dead code, magic numbers, wasted work

- [ ] `optimize/extract.rs:18,27-46,274-555,583-596` — file-wide `#![allow(dead_code)]` hides that `ExtractLiteralRule` is wired into no production session and `is_ref_place` is unused; `freeze_pure_arith` is a ~280-line god function mixing three strategies. Drop the blanket allow, delete/cfg-gate unused items, split the function.
- [ ] `optimize/dae.rs:169-205` vs `optimize/sroa_param.rs:697-727` — `is_eligible` is a near-verbatim copy diverging only in trait-method policy. Extract a shared pinning helper with an explicit policy flag; also delete `SroaInfo.struct_type_id` (`#[allow(dead_code)]`) and consider porting `sroa_param` onto the engine edit API.
- [ ] `optimize/scalar_forward.rs:63-98` + `store_load_forward.rs:209-217` + `extract.rs:599-607` — `is_assign_target` re-implemented three times beside a private engine copy (`nir_engine.rs:739-744`); `scalar_forward::apply_block` restarts from the block head after each single fold (quadratic on long inliner-generated blocks). Expose the engine helper; continue the scan past a fold.
- [ ] `optimize/dce.rs:1660-1663` — primitive types seeded as `for i in 0..18 { types.insert(TypeId(i)) }`; adding a primitive silently desynchronizes. Replace with a `TypeTable` constant/iterator.
- [ ] `optimize/loop_version_bce.rs:487-524,836-852` — `constify_check_temp` overwrites the first `let` of the temp it finds without verifying the initializer is the eliminated comparison, while `alloc_local_set` itself mints second `let`s for existing locals; `analyze_loop` versions only the first check's bound. Verify the matched let structurally; iterate candidate bounds.
- [ ] `optimize/store_load_forward.rs:142-144` — `forward_at_root`'s doc claims a second caller (a "combined cse+forward session") that no longer exists. Fix the comment and visibility.
- [ ] `wir_optimize/branch_hint.rs:240-259` — `br_only_arm` tolerates `ColdPath` markers but `else_is_empty` does not, blocking the `br_if` selection asymmetrically. Accept `ColdPath` in both (or document why not).
- [ ] `wir_optimize.rs:147-167,208-209` — half the WIR passes (`forward_struct_field_constants`, `promote_constant_arrays_to_data`, `split_large_array_literals`, the `run_peephole` loop, `flatten_seq_assignments`, `elide_multi_field_struct_locals`, `remove_trivial_init_globals`, `cleanup`) bypass `wir_pass`, so `WADO_LIST_PASSES`/`WADO_SKIP_PASS`/`WADO_DUMP_PASS_*` cannot see them. Wrap every pass.
