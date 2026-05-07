# HFS dataflow rewrite — WIP

## Goal

Replace HFS's mechanical "wrap each call site with Block { write_back; ...; re_read; ... }" with a dataflow-driven sync placement that emits **only** the syncs strictly needed by state transitions. Targets the user's "理論上のベスト（redundant ゼロ）" ask.

## Design — Canonical-side state machine

For each scalarized field `(L, F)` (with scalar local `_hfs_F`), at each program point the walker tracks one state per candidate:

| State        | Meaning                                            |
| ------------ | -------------------------------------------------- |
| `Both`       | `_hfs_F == L.F` (both sides equal)                 |
| `ScalarOnly` | `_hfs_F` is the truth, `L.F` is stale              |
| `FieldOnly` | `L.F` is the truth, `_hfs_F` is stale              |

Operation requirements / effects:

| Operation                | Pre-state required        | Post-state    |
| ------------------------ | ------------------------- | ------------- |
| scalar read              | `{Both, ScalarOnly}`      | unchanged     |
| scalar write             | (any)                     | `ScalarOnly`  |
| call w/ `&T` arg         | `{Both, FieldOnly}`       | unchanged     |
| call w/ `&mut T` arg     | `{Both, FieldOnly}`       | `FieldOnly`   |

Sync emitted only at transitions:

- `ScalarOnly → Both/FieldOnly`: `write_back` (`L.F = _hfs_F`)
- `FieldOnly → Both/ScalarOnly`: `re_read` (`_hfs_F = L.F`)
- `Both → ScalarOnly/FieldOnly`: just relabel; no sync

## Convergence rules

- **Branch joins**: each arm walked with cloned entry state. After arms, pick join target via `pick_join_target_for_candidate`:
  - All arms agree → that state
  - Subset of `{Both, ScalarOnly}` → `ScalarOnly` (weaker, no sync)
  - Subset of `{Both, FieldOnly}` → `FieldOnly`
  - Mix of `ScalarOnly + FieldOnly` → `ScalarOnly` (heuristic; lookahead-based picker is the next refinement)
- **Loop body end (back-edge)**: force every candidate to `Both` (matches loop entry from pre-load). Ensures back-edge invariant.
- **Escape (return / non-enclosing break)**: commit `ScalarOnly` candidates via `write_back`. After this, field is canonical; `_hfs_F` is irrelevant outside the loop. Escape variant of `commit_scalar_for_escape`.
- **Post-loop write-back**: always empty in the new design — body-end force-Both + escape commits guarantee field is canonical at every loop exit.

## Temp local pool

- `WalkCtx::temp_pool: IndexMap<TypeId, Vec<u32>>`.
- `alloc_temp(type_id)` pops from pool or allocates fresh `_hfs_call_<idx>`.
- `free_temp(idx, type_id)` pushes back.
- Used only for non-unit-typed Match arm body convergence wrappers (where the original body's value must survive the trailing sync). All other call sites use stmt-level sync injection (no temp).

## File layout

`wado-compiler/src/optimize/field_scalarize.rs`:

- Old `replace_in_*` family is gone (lines 1803→end).
- New section "Replacement pass — dataflow-driven sync placement" (after `count_field_accesses_in_*`):
  - `CanonState`, `ScalarStates`, `WalkCtx` types
  - `init_states`, `sync_to_target`, `state_transition_stmt`, `pick_join_targets`, `insert_convergence_at_block_end`, `build_convergence_block`
  - `walk_block`, `walk_stmt`, `commit_scalar_for_escape`, `walk_branching_block_if`, `walk_nested_loop`
  - `walk_expr` (entry), `walk_call_expr`, `walk_other_expr_kinds`
  - `walk_expr_branches_if`, `walk_expr_branches_switch`, `walk_expr_branches_match`
  - `wrap_expr_with_prefix`, `emit_convergence_at_arm_body_end`
  - `SyncFields`, `accumulate_call_sync`, `is_immut_ref_arg`, `add_sync_fields_for_arg`, `extract_gc_local_index`, `add_all_fields_for_local`, `is_gc_heap_type`

`scalarize_loop` updated to call `process_loop_body(...)` instead of `replace_in_block(...)` and to return empty `post_stmts`.

## Status — current commit (uncommitted)

- Build: ✅ passes (`cargo build -p wado-compiler`).
- Unit-style HFS fixtures (115 of them): ✅ all pass.
- New non-unit / branching fixtures: 6 of 7 pass on stdout. The fixture-level `wir_expect:O2 = ["let _hfs_call_"]` assertions are stale for the new design (most call sites no longer need a temp because sync is at stmt level).
- 1 fixture has a real semantic bug being fixed:
  - `hfs_match_guard_with_call.wado` exposes the guard pre-stmts placement bug. Originally I lumped guard's pre-stmts and body's pre-stmts together into the body wrapper, which let the guard's mutation be overwritten by the body wrapper's write_back (run after guard). Fixed in the WIP code: guard pre-stmts wrap the GUARD expression in a Block; body pre-stmts wrap the BODY expression. (Last edit in the file replaces `wrap_arm_body_with_prefix` with two calls to a generalized `wrap_expr_with_prefix`.)
- Build re-tested after the fix: ✅ passes. Test pass needs to be re-run.

## Remaining steps

1. **Re-run HFS fixtures** to confirm `hfs_match_guard_with_call` semantics correctness.
2. **Update fixture assertions** that expected `let _hfs_call_*` for stmt-level call sites (they're correct under the new design without the temp; assertion was specific to old design).
3. **Update `wir_not_expect:O2`** patterns on the failing-bug fixtures (`hfs_inline_arm_mixed_with_call_arm.wado`, `hfs_match_scalar_arm_mixed_with_call_arm.wado`) — index references like `_hfs_v_18` may shift slightly with the new walker.
4. **Full E2E run** — the old whole-stmt sync goldens (serde-* etc.) will all change to reflect the smaller, transition-only sync. Many goldens to regenerate via `mise run update-golden-fixtures`.
5. **on-task-done** validation.
6. **Add a new fixture** that locks "consecutive `&mut` calls have NO inter-call sync" (zero-redundancy invariant — the marquee feature of this rewrite).
7. **Optional polish**:
   - Lookahead-based `pick_join_target` (peek at the next stmt's first interaction with each candidate, decide ScalarOnly vs FieldOnly precisely). Currently defaults to `ScalarOnly` heuristic for `{Scalar, Field}` mixes. Theoretical-best for any specific call/scalar-update sequence requires lookahead.
   - Smart nested-loop handling: `walk_nested_loop` currently force-FieldOnly's all candidates after the inner loop. A more precise model would track inner loop body's effect on each candidate (touched / untouched), only flipping candidates the inner actually mutates.

## Known invariants to preserve

- **Issue #1008 fixtures** (mixed scalar-arm + call-arm match) must stay correct: ✅ the new design's per-arm walk + lattice join inherently solves this (each arm has its own state evolution).
- **Inline interactions** (`hfs_inline_arm_mixed_with_call_arm.wado`): ✅ no longer treated as special; LabeledBlock is a normal block in the walker.
- **Branchless-increment guard** (`fold_branchless_increment` in `wir_optimize/peephole.rs:writes_local`): kept; protects against any future state transition that emits `local.set` inside an expression context.
- **`TaskReturn` unreachable!**: kept; fail-loud on assumption violation.
- **Function-wide alias scan** (`collect_function_aliased_locals`): kept; pre-existing correctness mechanism, orthogonal to sync placement.

## Why this is the right design

- The sync invariant becomes the **state lattice itself**, not a structural pattern in the IR. Any future change that respects the state transitions will be correct by construction.
- Branch independence is **structural** (per-arm cloned state, target selection at join), not a special case bolted onto a "wrap calls" mechanism. Match/If/Switch all use the same join algorithm.
- Temp pool reuses a single `_hfs_call_<idx>` per type across all wrappers in a function — no double-allocation of equivalent locals.
- `result_used` plumbing isn't actually needed in this design because stmt-level sync is the default; only Match arm-body convergence wraps need a temp, and they always know if the arm body is unit-typed (skip temp) or non-unit (use temp). Effectively `result_used` collapses to "is the arm body unit-typed?" at the only call site that cares.

## Branch / commits

- Branch: `claude/fix-hfs-sync-inlining-GQhZ1`
- Last pushed commit: `8212cb2` (P0-P2 polish + fold_branchless_increment fix on top of dataflow-redesign-stub).
- Working-tree changes (uncommitted right now): the dataflow rewrite per this doc.
