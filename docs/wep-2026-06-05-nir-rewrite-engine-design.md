# WEP: NIR Rewrite Engine — Detailed Design

This is the follow-up the
[Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
WEP promised: it specifies the engine, the worklist discipline, and the
migration. It builds on the
[NIR Skeleton Arena](./wep-2026-06-05-nir-skeleton-arena.md) (Layer 1) — `Body`
is the canonical `NirFunction.body`. This WEP is "Phase 4" of the arena
migration, now complete: the engine is implemented, every optimizer pass runs
on the arena, and the `body_block` bridge is gone (see Migration → Outcome).

## Context

The arena exists but the parent map, local use index, and mutating edit API
were deferred (skeleton-arena WEP, Phase 4). Without them there is no worklist:
a rewrite cannot find the node's parent to re-enqueue, nor a local's uses. The
optimize passes are still 31 independent whole-tree walks in a global
fixed-point; the per-pass `Body ↔ tree` bridges currently _add_ traversal
rather than remove it. The engine is what turns that around.

## Decision

A single worklist-driven engine runs the local (intra-procedural, peephole)
rewrites over one function's `Body`, to a local fixed point, visiting a node
only when it might be reducible.

### Engine session

The parent map and use index are not stored on `Body` (they would burden every
`from_block`). They live in an engine session built once per function run:

```
struct Engine<'a> {
    body: &'a mut Body,
    parent: Parents,        // NodeRef -> Option<NodeRef>, per category
    uses: UseIndex,         // local index -> { def, reads, writes }
    worklist: Worklist,     // dedup queue of NodeRef
    dirty: Vec<NodeRef>,    // re-enqueue scratch filled by the edit API
}
```

`Engine::new(body)` does one O(n) pass to populate `parent` and `uses`, seeds
the worklist with every node in post-order, then `run(rules)` drains it.

- `Parents`: one `SecondaryMap<Id, PackedOption<NodeRef>>` per category
  (`cranelift_entity`). Parent is the nearest id-bearing ancestor; arms /
  fields / call args are transparent.
- `UseIndex`: `IndexMap<u32, LocalUses>` with `LocalUses { def: PackedOption<StmtId>,
  reads: Vec<ExprId>, writes: Vec<ExprId> }`. `reads` = `Local` expr nodes;
  `writes` = the place forms (`Assign` target `Local`, `Local.field = …`,
  `&local` / `&mut local`).
- `Worklist`: `VecDeque<NodeRef>` plus an "in-queue" bit per node so a node is
  never queued twice.

### Edit API

Rules never touch `body.<map>` directly. They go through the session, which
keeps `parent` / `uses` coherent and pushes affected nodes into `dirty`:

- `alloc_expr(kind, type_id, span) -> ExprId` (and `_stmt` / `_block` / `_pat`):
  push the node, set `parent` of any id children to it, register any `Local`
  mention in `uses`.
- `replace_kind(id, new_kind)`: rewrite in place — the id is stable, so worklist
  entries and parent links survive. Diff old vs new child / local sets; fix
  `parent` and `uses`; push `parent[id]` (and changed local def/use neighbours)
  into `dirty`.
- `set_child(parent, slot, new)`: re-point one operand slot; fix `parent[new]`,
  push `parent`.
- `splice(block, range, stmts)`: statement-list edit; fix parents and uses for
  the spliced range.

Dead nodes are not freed mid-run (liveness is reachability from `root`). This
design assumed the per-pass bridge's arena → tree lowering would compact them
for free; with the bridge now gone, compaction is an open follow-up (see
Migration → Follow-ups).

### Rules

A rule is a single-node local rewrite:

```
trait Rule {
    fn apply(&self, e: &mut Engine, id: ExprId) -> bool;   // most rules are expr-typed
}
```

Statement / block rules get sibling entry points (`apply_stmt` / `apply_block`)
for the few rewrites that target those (dead-stmt-after-break, block flatten).
Rules are registered in a fixed priority list. At a popped node the engine tries
rules in order; the first that returns `true` (changed) re-processes the node
(its kind may now match a different rule) before moving on.

### Worklist discipline

```
seed: push every node post-order.
loop:
    pop id (clear its in-queue bit)
    for rule in rules:
        if rule.apply(engine, id):
            drain `dirty` into the worklist (parent + use neighbours)
            re-try rules at id
            break
until worklist empty.
```

Re-enqueue is exactly what the edit API recorded: a node's `parent` after any
structural change, and a local's def / use nodes after its def changed. No
whole-tree sweep, no global convergence sweep.

Rule-conflict policy: rules must be confluent at a node (order-independent
final result) or ordered so the priority list is the tie-break. Each rule is
idempotent (re-applying to its own output is a no-op), so the per-node retry
terminates.

## What stays outside the engine

- Interprocedural stages — `inline`, `dce`, `dae`, `drve`, globalization — run
  as distinct steps around the engine, unchanged. Optionally gated later by a
  per-function dirty set so the engine only re-runs on functions they touched.
- Flow-sensitive passes — `field_scalarize`, `licm`, `tmpl_hoist`,
  `value_copy_demote`, `store_load_forward` — keep their own dataflow walkers
  but read the arena (their bridges drop as they port).

## Migration

The engine landed incrementally — the old fixed-point loop co-existing with it
until every pass had moved — each step staying e2e bit-identical.

- [x] Engine substrate + core (`nir_engine.rs`): the per-function session
      (parent map, local `uses` index, post-order worklist,
      `Body::for_each_child`), the coherent edit API (`replace_expr_kind`,
      `apply_block`, `alloc_*`, `clone_expr`, …), the `Rule` trait, and the
      `run` driver.
- [x] Every optimizer pass moved off the `Body ↔ tree` bridge in one of four
      shapes: engine `Rule` peepholes (`select_lowering`, `match_to_switch`,
      `string_push`, `array_literal`, `elide_local`); direct arena walks
      (`drve`, `dae`, `cse`, `ref_elim`, `sroa`/`sroa_param`, `dce`,
      `const_object_globalization`, `condition_implication`,
      `value_copy_elide`/`_demote`, `tmpl_hoist`, `labeled_block_fusion`,
      `multi_value_return`, `container_sroa`, `const_branch_prune`);
      flow-sensitive own-walkers (`copy_prop`, `licm`, `store_load_forward`);
      and `const_folding` (niri), staged as an arena evaluator (`*_lattice_a`),
      an arena rewriter (`reduce_local_a`), then the `ConstFoldVisitor` over
      arena ids with the field-env snapshot/restore/join intact. Shared infra:
      `optimize/arena_query` + `Body::{for_each_child, clone_*, lower_*}`.

Measured win (peak-bridge vs arena-direct; isolated NIR optimize phase; median
of 9; dev profile): zlib 1.12×, fts 1.49×, json_catalog_v2 1.53×,
sqlite_parse 1.67×. The bridge cost (`to_block` + `from_block` per pass per
fixpoint iteration) scales with optimizer load — a heavy module's optimize
phase is cut ~40 %.

## Phase 5 — NIR tree retirement

The engine made the arena canonical for the optimizer; Phase 5 removes the tree
representation outright. Per-step detail is in the git history (`Phase 5`
commits); the compressed record:

- [x] `lower::translate` builds the arena `Body` directly — the one-time
      tree-build + `from_block` at the lower boundary is gone.
- [x] The remaining tree-coupled passes / positions ported to the arena:
      `mod_ref`, `alias`, `elide_box_local` (incl. its leftmost-evaluated-use
      walker), `field_scalarize`'s function-wide alias scan +
      `collect_locals_introduced`, `inline` (full core — a cross-`Body`
      `splice_*` that local/label-remaps and converts `return` → `break`),
      `wir_build::collect_let_names`, and the diagnostics `nir_unparse` /
      `remarks`.
- [x] `NirGlobal.initializer` is an arena `ExprBody` (a newtype over a
      single-`Expr`-statement `Body`); the write-only NIR
      `NirParam`/`NirField.default_expr` fields are deleted.
- [x] `nir_visitor` (the tree visitor) and `NirFunction::body_block` are
      deleted — no consumers remain.

Remaining — the deletions (both transform-core ports are done; production is
now free of the `Body ↔ tree` bridge):

- [x] D2 — `field_scalarize` per-loop machinery → arena. `scalarize_loop` /
      `process_loop_body` / the `walk_block` / `walk_stmt` / `walk_expr` dataflow
      walker, sync emission, branch/switch/match joins, escape commits, and temp
      pooling all mutate `body.exprs` / `body.stmts` / `body.blocks` in place;
      the `FieldUsageCache` builder (`collect_param_field_usage_*`) and the
      call-sync analysis read arena ids directly. `scalarize_loop_at` no longer
      round-trips through `to_tree_block` / `lower_block`. This was the last
      production caller of the bridge.
- [x] E — niri tree interpreter + `const_folding` CTFE → arena. `try_call_fold_a`
      reads the callee tail id via `single_tail_expression_a` and reduces it on a
      cloned scratch `Body` with `reduce_in_place_a` (the arena analogue of
      `reduce_in_place` + `reduce_to_lattice`), removing the `Body::to_block`
      materialization from the production CTFE path. The tree interpreter cluster
      now survives only to back `tests/niri.rs`.
- [ ] Delete the `Body ↔ tree` bridge (`Lower` / `from_block` / `to_block` /
      `to_tree_*` / `lower_*`, `set_body_block`) once the unit-test builders that
      still construct trees (`mod_ref`, `elide_box_local`, `tmpl_hoist`,
      `nir_engine`, `tests/niri`) are migrated to arena builders, and the
      now-test-only niri tree interpreter cluster (`reduce` / `reduce_to_lattice`
      / `reduce_in_place` / `reduce_local` / `try_call_fold` / `expr_to_lattice`
      / `try_fold` / `single_tail_expression` / …) is deleted with them.
- [ ] Delete the tree enums (`NirExpr` / `NirExprKind` / `NirStmt` /
      `NirStmtKind` / `NirBlock` / `NirPattern` / `NirMatchArm` /
      `NirStructField` / `NirStructPatternField` / `CallArg`) and `nir_visitor`
      — tree retired.

Handoff notes:

- D2 ported in place: the walker mutates the live arena `Body`, so it needs no
  cross-`Body` clone — field rewrites flip the node kind on the stable `ExprId`,
  and arm-body wrapping moves the original node to a fresh id so the parent's
  `ExprId` still resolves. E reuses whole-`Body` `Clone` (the single-statement
  callee body is tiny) rather than a subtree clone. A reusable cross-`Body`
  deep-clone-with-remap is therefore still only wanted by `inline`'s `splice_*`
  cluster; lifting it into `nir_arena` (next to the intra-`Body` `clone_*`, which
  cannot share its `&mut self` vs `&mut dst` + `&src` borrow shape) remains a
  cleanup, not a blocker.
- Arena compaction is still open: in-place passes leave orphaned (dead) nodes in
  `Body`. Traversal is from `root` and `build_uses` ignores orphans, so it is
  correct, but a from-`root` re-lowering (or mark-sweep) at the end of optimize
  would reclaim them if memory becomes a concern.

## Out of scope

- Layer 2 (the hash-consed value e-graph / GVN). Re-decided after the engine
  lands, per the worklist-engine WEP's sequencing.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
  — the direction.
- [NIR Skeleton Arena (Layer 1)](./wep-2026-06-05-nir-skeleton-arena.md) — the
  substrate this engine drives.
- `optimize/mod_ref.rs` — the referential-transparency check a later Layer 2
  would gate on.
