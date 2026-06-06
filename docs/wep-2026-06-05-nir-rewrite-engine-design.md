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

The old fixed-point loop and the engine co-exist during the port.

- [x] A+B. Engine substrate + core (`nir_engine.rs`): the per-function session
      (`parent` map, local `uses` index, post-order worklist, `Body::for_each_child`),
      the coherent edit API (`replace_expr_kind`, `set_block_stmts` / `apply_block`,
      `alloc_*`, `clone_expr`, `is_local_read`), the `Rule` trait, and the `run`
      driver. `build_uses` walks live-from-root, so dead nodes a prior in-place pass
      left are not counted when two arena passes run back-to-back.
  - Learning: `const_fold` is a poor _first_ rule — it is not a pure peephole
    (it threads a flow-sensitive env of constant locals + field knowledge), so a
    `reduce_local`-only rule would fold _less_ and break `wir_expect` fixtures.
    The first production rules were genuinely-local (`select_lowering`,
    `match_to_switch`).
- [x] C. Every optimizer pass (≈30) moved off the `body_block()` bridge, e2e
      bit-identical at every step, using one of four patterns:
  - Engine `Rule` (local peepholes): `select_lowering`, `match_to_switch`,
    `string_push`, `array_literal`, `elide_local`.
  - Direct arena walk (whole-function analysis + in-place rewrite): `drve`,
    `dae`, `cse`, `ref_elim`, `const_object_globalization`, `const_branch_prune`,
    `sroa`, `sroa_param`, `multi_value_return`, `container_sroa`,
    `condition_implication`, `dce`, `value_copy_elide`, `value_copy_demote`,
    `tmpl_hoist`, `labeled_block_fusion`.
  - Flow-sensitive (own dataflow walker, reads/mutates the arena): `copy_prop`,
    `licm`, `store_load_forward`, and `const_folding`'s env walk.
  - Subtree materialization (tree-shaped transform on a read-only clone, lowered
    back): `inline` (callee remap), `field_scalarize` (per-loop state machine),
    `elide_box_local`, and `const_folding`'s CTFE callee tails.
  - Shared infrastructure: `optimize/arena_query` (arena `is_local`,
    `expr_mentions_local`, `is_pure_expr`, `collect_reads`, `has_break_to`);
    `Body::{for_each_child, clone_expr, clone_block, lower_expr, lower_block,
    to_tree_*}`. NIR positions that are still tree (param / global / struct-field
    defaults, global initializers) reuse a pass's own logic via a
    wrap-in-`Body` helper.
  - `const_folding` (niri), the last and hardest pass, drives the whole
    interpreter with flow-sensitive env threading. Ported in three bit-identical
    stages: an arena evaluator (`*_lattice_a` / `try_fold_a`), an arena rewriter
    (`reduce_local_a` / `rewrite_*_a` / `reduce_to_lattice_a` — which skips the
    tree path's defensive `reduce_in_place` since the visitor folds every child
    bottom-up first), then the `ConstFoldVisitor` walking arena ids with the
    branch-aware field-env snapshot/restore/join intact. `build_alias_info` runs
    on a read-only materialization; `Value::from_arena_literal` and
    `alias::recognize_value_copy_a` were added.

### Outcome

The `body_block` bridge is gone from the optimizer — the only remaining
`body_block()` callers are the read-only diagnostics (`nir_unparse`, `remarks`).
The per-pass `Body ↔ tree` round-trips have vanished and the arena flows
lower → optimize → wir_build with no converter (`wir_build` reads `body.exprs`
directly) — completing Phase 3's goal. The single remaining tree→arena
conversion is the one-time `from_block` at the lower boundary
(`lower::translate` still builds a tree, then converts once).

Measured speed win (A/B: peak-bridge `05b532fc` — every optimizer pass on the
bridge — vs the arena-direct HEAD; byte-identical input, since the target
sources and the `include_str!`-embedded `lib/` stdlib are unchanged between the
two commits; median of 9 runs, dev profile). The isolated NIR optimize phase:

| module          | bridge  | arena   | speedup |
| --------------- | ------- | ------- | ------- |
| zlib            | 75 ms   | 67 ms   | 1.12×   |
| fts             | 327 ms  | 220 ms  | 1.49×   |
| json_catalog_v2 | 909 ms  | 593 ms  | 1.53×   |
| sqlite_parse    | 7455 ms | 4469 ms | 1.67×   |

The win scales with optimizer load — the bridge cost is a `to_block` +
`from_block` per pass per fixpoint iteration — so a small module sees ~11 %
while a heavy one is cut 40 % (~3 s, flowing to a 24 % shorter total compile).

### Follow-ups

- [ ] Arena compaction. Passes now mutate in place and leave orphaned (dead)
      nodes in `Body`; nothing reclaims them (the old `set_body_block` re-lowering
      used to densify). Every consumer traverses from `root` and `build_uses`
      already ignores orphans, so this is correct, but a heavy module's maps carry
      several× the live node set. A from-`root` re-lowering (or mark-sweep) at the
      end of optimize would reclaim it if memory becomes a concern.
- [ ] `lower::translate` emits the arena directly, dropping the one-time
      tree-build + `from_block` at the lower boundary.

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
