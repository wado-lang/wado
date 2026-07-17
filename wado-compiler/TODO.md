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


### NIR passes


### WIR passes


## P2 — precision and latent issues

### NIR


### WIR


## P3 — structure, duplication, fragility

### Consolidate shared predicates and walkers

- [ ] One may-trap / purity oracle: `arena_query::is_pure_expr`, `mod_ref` `may_trap`, `elide_box_local::LeftmostWalk`, `select_lowering::is_select_eligible`, and `dce::expr_has_side_effects` each encode their own effect/trap taxonomy and already disagree (root cause of the P0 trap-deletion bug). Centralize per-`ExprKind`/`NirBinaryOp` classification in `arena_query`/`mod_ref`.
- [x] One "locals possibly mutated in subtree" query — canonical `arena_query::locals_possibly_mutated` exists (copy_prop uses it). Migrating `const_folding::record_loop_write` and `condition_implication::node_modifies` is deliberately NOT done: `record_loop_write` intentionally excludes field/index/payload writes (niri scalar-lattice precision), and `node_modifies` uses different receiver semantics and runs on drivers with no MutationOracle — migrating either would regress precision. Accepted residual.
- [ ] `optimize/labeled_block_fusion.rs:395-508,551-701,1839-1994` (+ the two transform walkers and `subst_variant_payload_*`) — three near-identical break-exit walkers plus two rewriters with subtly different coverage; the divergences are the P0/P2 fusion bugs. Factor one label-aware exit visitor parameterized by a per-exit callback.
- [ ] `optimize/multi_value_return.rs:456-734` + `optimize/dae.rs:278-292` + `optimize/drve.rs:216-290` — triplicated call-site-scan walkers that diverge (multi_value_return skips global initializer bodies; the others scan them). Extract a shared arena walker parameterized by a call-shape callback; align initializer coverage.
- [ ] `optimize/sroa.rs:527-605,647-694,697-952,1016-1107` — four copy-paste traversals (`escape_*`, `field_access_node`, `soft_*`, `rewrite_*`) with the `Assign`-target special case maintained in triplicate; `soft_expr` threads 7 params through two `too_many_arguments` shims. Consolidate into one parameterized walker.

### String-typed and fragile identity

- [ ] `optimize/inline.rs:273-317,402-457,1220-1236` — the call graph and candidate map are keyed on `"{module_path}/{name}"` strings (with a 30-line doc about a mangling divergence the scheme must sidestep) although every call site carries a resolved `FuncId`. Key on `FuncId`; delete `function_inline_key` and the name-fallback heuristic.
- [ ] `optimize/dae.rs:150-165` + `optimize/array_literal.rs:57-95` — closure inspect impls matched by literal `"Inspect"`/`"InspectAlt"` strings; `builtin::array_new` found by name while the same pass uses `CompilerItem::ListPush` for push. Add `CompilerItem` markers.
- [ ] `wir_optimize/nullable_ref.rs:454-510` — variant rewrites key on `field_name == "discriminant"` / `starts_with("payload_")` and one exact operand orientation, mirroring what `wir_build/pattern_match.rs` emits today; drift produces an invalid-Wasm ICE. Centralize the naming/shape constructors shared with the builder.

### Dead code, magic numbers, wasted work

- [ ] `optimize/dae.rs:169-205` vs `optimize/sroa_param.rs:697-727` — `is_eligible` is a near-verbatim copy diverging only in trait-method policy. Extract a shared pinning helper with an explicit policy flag; also delete `SroaInfo.struct_type_id` (`#[allow(dead_code)]`) and consider porting `sroa_param` onto the engine edit API.
