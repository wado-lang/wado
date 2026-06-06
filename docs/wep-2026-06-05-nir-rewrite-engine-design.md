# WEP: NIR Rewrite Engine — Detailed Design

This is the follow-up the
[Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
WEP promised: it specifies the engine, the worklist discipline, and the
migration steps before code lands. It builds on the
[NIR Skeleton Arena](./wep-2026-06-05-nir-skeleton-arena.md) (Layer 1), whose
stage-1 is done — `Body` is the canonical `NirFunction.body` and the optimize
passes bridge to it per function. This WEP is "Phase 4" of the arena migration.

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

Dead nodes are not freed mid-run (liveness is reachability from `root`); a
compaction folds into the arena → tree lowering the bridges already do.

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

- [x] A. Substrate — `Engine` session (`nir_engine.rs`): `parent` + `uses`
      builders, the dedup worklist seeded post-order, `NodeRef` +
      `Body::for_each_child`. Additive, not wired into the loop; unit tests
      cover the use index, parent links, and worklist seeding.
- [x] B. Engine core — `replace_expr_kind` (in-place edit keeping the use
      index coherent, re-parenting new children, re-enqueuing parent + new
      children), the `Rule` trait, and `run()` (worklist to a local fixed
      point with per-node rule retry). A demo `FoldAddMulConst` rule + unit
      test drives the full loop bottom-up (`(1+2)*4 → 12`). Still additive.
      - Correction: `const_fold` is a poor _first production_ rule. The
      `const_folding` pass is not a pure peephole — it threads a per-function
      `env` of constant locals and field knowledge through a flow-sensitive
      walk (`niri::reduce_local` is the single-node part, but the pass does
      more). Replacing it with a `reduce_local`-only rule would fold _less_
      and break golden / `wir_expect` fixtures. So the env-driven part stays a
      flow-sensitive pass; the first production rule in C is a genuinely-local
      one (`select_lowering` or `match_to_switch`).
- C. Migrate the passes off the `body_block()` bridge one at a time, e2e
  bit-identical at every step. Genuinely-local peephole passes become engine
  `Rule`s; whole-function / flow-sensitive passes keep their own walkers but
  read and mutate the arena `Body` directly. A shared `optimize/arena_query`
  module holds the arena counterparts of the tree helpers (`is_local`,
  `expr_mentions_local`, `stmt_mentions_local`, `is_pure_expr`); the tree
  helpers stay for the not-yet-ported tree consumers. The engine gained an
  `apply_block` entry point + `set_block_stmts` (statement-list edits),
  `is_local_read` (use-index liveness), and a `build_uses` that walks live
  nodes from the root so dead nodes left by a prior in-place pass are not
  counted once two arena passes run back-to-back.
  - Ported so far:
  - [x] `select_lowering` — expr `Rule` (`If` → `builtin::select`).
  - [x] `match_to_switch` — expr `Rule` (dense `Match` → `Switch`);
        param/global/struct-field defaults reuse it via a wrap-in-`Body`
        helper.
  - [x] `string_push` — block `Rule` (`push_str("…")` → per-byte `push`).
  - [x] `array_literal` — block `Rule` (builder window → `ArrayLiteral`).
  - [x] `elide_local` — block `Rule` on `is_local_read`.
  - [x] `value_copy_elide` — direct arena walk (single-pass strip).
  - [x] `drve` — direct arena walks (bodies); globals stay tree.
  - [x] `dae` — arena dead-param detection / validation / renumber / rewrite;
        globals via a wrap-in-`Body` helper. Last consumer of the tree
        `is_pure_expr` / `collect_reads_in_block`, now removed; arena
        counterparts live in `optimize/arena_query`.
  - [x] `cse` — direct arena walk (loop-level CSE).
  - [x] `ref_elim` — direct arena walks; lazy referent resolution, single-use
        deref source moved.
  - [x] `const_object_globalization` — arena read-only gate + read rewrite;
        globals via wrap-in-`Body`.
  - [x] `const_branch_prune` — bottom-up arena walk; last user of the shared
        `visit_project_functions` tree bridge.
  - [x] `sroa_param` — arena validation + callee / call-site rewrite; globals
        via wrap-in-`Body`.
  - [x] `multi_value_return` — pure arena classification (sets `return_abi`).
  - [x] `elide_box_local` — arena body traversal; `ModRef` + the
        leftmost-walker run on materialized subtrees (`Body::to_tree_*`).
  - [x] `sroa` — arena candidate / escape analysis + decomposition rewrite.
  - [x] `copy_prop` — arena binding/usage analysis + substitute-and-remove.
  - [x] `store_load_forward` — arena flow-sensitive forwarding; modified-locals
        cache keyed by `BlockId`.
  - [x] `value_copy_demote` — arena element-immutability proof
        (`ElementClean` / `ElementImmutable` recurse via `for_each_child`);
        callee bodies cloned per `verify`; shallow sibling synthesized by
        cloning the function and renaming `array_clone` in its arena body.
  - [x] `tmpl_hoist` — arena escape analysis + buffer/Formatter hoisting;
        new nodes (hoisted `Let`, field-reset `Assign`, normalized Formatter
        literal) pushed straight into the arena; rename walks navigate by id.
  - [x] `condition_implication` — arena taint/DefMap analysis; the three
        eliminators drive a local `ArenaOptVisitor` (mirrors `NirOptVisitor`'s
        default walk); a guard-implied condition is rewritten to `false`
        in place. DefMap-resolution helpers are value-typed and unchanged.
  - [x] `container_sroa` — arena candidate collection + whitelist escape
        analysis (fixpoint) + per-field rewrite. Synthesized per-field
        calls are pushed into the arena; duplicated sub-expressions
        (capacity, index) deep-clone via the new `Body::clone_expr`.
  - [x] `licm` — arena modified-var / ref-binding / hoist-candidate
        analysis and in-place field-access replacement; hoist `Let`s pushed
        into the arena. The hoist/replace/ref walks share a `*_child_nodes`
        enumerator that reproduces the tree walk's pattern-excluding child set
        (so hoist-local numbering stays identical).
  - [x] `dce` — the body-touching helpers (array-clone-elem / bytes-literal /
        inspect-signature collectors, the `DceWalker` fact walk, and dead
        `GlobalVarSet` removal) read/mutate the arena; the reachability /
        type / function / global graph machinery is body-independent and
        unchanged. Global initializers (still tree NIR) wrap in a one-stmt
        `Body` for the walk.
  - Remaining: `labeled_block_fusion`, `inline`, `field_scalarize`.
  - Added `Body::clone_expr` (structural arena deep-clone) — the
    non-engine counterpart of `Engine::clone_expr`, for rewrites that
    duplicate a subtree.
  - Deferred: `const_folding` — unlike the others it drives the whole `niri`
    interpreter (tree-shaped) over the body with flow-sensitive env threading,
    so it stays on the bridge until `niri` itself is ported to the arena
    (a dedicated effort). The subtree-materialization trick does not apply (the
    interpreter consumes the _whole_ body, not a small leaf subtree).
  - Flow-sensitive (keep walkers, read arena): `const_folding`'s env walk,
    `copy_prop`, `licm`, `field_scalarize`, `store_load_forward`.
  - When the last bridge is gone the per-pass `Body ↔ tree` conversions
    vanish and the arena flows lower → optimize → wir_build with no
    converter — completing Phase 3's goal. Measure the speed win here.

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
