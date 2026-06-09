# WEP: NIR Rewrite Engine — Detailed Design

Follow-up to the
[Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
WEP: it specifies the engine that runs local NIR rewrites over the
[NIR Skeleton Arena](./wep-2026-06-05-nir-skeleton-arena.md) `Body`. Status: the
engine substrate is complete — every optimizer pass reads the arena (the
`Body ↔ tree` bridge is gone), the unified peephole rules share one session, and
a per-function dirty-set gate drives the fixed-point loop. This is the substrate,
not the full combine: ~15 intra-procedural passes still run as standalone
whole-tree passes inside the global loop. Migrating them onto the single
worklist and removing the loop is the open continuation, specified in the
direction WEP.

## Context

The arena made `Body` the canonical `NirFunction.body`, but a worklist needs a
parent map (to re-enqueue a node's context after an edit) and a local use index
(to re-enqueue a local's uses). Those live in a per-function engine session, not
on `Body` (which would burden every constructor).

## Engine

A single worklist-driven engine (`nir_engine.rs`) runs local intra-procedural
rewrites over one function's `Body` to a local fixed point, visiting a node only
when an edit might have made it reducible.

### Session

`Engine::new(body)` does one O(n) pass to build:

- parent maps — nearest id-bearing ancestor per category (`expr`/`stmt`/`block`/`pat`); arms / fields / call args are transparent.
- use index — `IndexMap<u32, LocalUses>` where `LocalUses { def: Option<StmtId>, reads: Vec<ExprId> }`; built by walking from `root`, so orphaned (dead) nodes are not counted.
- worklist — a `VecDeque<NodeRef>` plus an in-queue set, seeded with every node in post-order (children before parents).

### Edit API

Rules never touch `body.<map>` directly; they go through the session, which keeps
the parent map and use index coherent and re-enqueues the affected neighbourhood:

- `alloc_expr` / `alloc_stmt` / … — push a node, parent its id children, register any `Local` mention.
- `replace_expr_kind(id, kind)` — rewrite in place (the id is stable); fix the use index for the old/new `Local` mention, re-parent the new children, enqueue the parent and new children.
- `become_expr(dst, src)` — promote `src`'s node content into `dst` (collapse a wrapper onto a nested child); `src` becomes a dead `Unit`.
- `set_block_stmts(block, stmts)` — replace a statement list, re-parenting and enqueuing.
- `clone_expr` — deep-copy a subtree into fresh ids.

Dead nodes are not freed mid-run (liveness is reachability from `root`, and the
use index ignores orphans). Arena compaction at end-of-optimize is an open
follow-up, wanted only if memory becomes a concern.

### Rules and worklist

A rule is a single-node local rewrite with `apply_expr(engine, id) -> bool` and a
sibling `apply_block` for statement-list rewrites. At a popped node the engine
tries the registered rules in order; the first that returns `changed` re-tries at
the same node (its kind may now match another rule). Re-enqueue is exactly what
the edit API recorded — a node's parent after a structural change, a local's uses
after its def changed — so there is no whole-tree or global-convergence sweep.
Rules must be idempotent (so the per-node retry terminates) and confluent or
priority-ordered.

### Unified peephole session

`optimize/peephole.rs` runs the position-flexible local rules — `string_push`,
`array_literal`, `elide_local`, the env-free subset of `const_folding` (literal
`Binary`/`Unary`/`Cast` + pure CTFE), and `const_branch_prune` — over one session
per function, interleaved on a single worklist. Invoked pre-inline and
post-inline each iteration. `match_to_switch` (runs first, and at `-O0`) and
`select_lowering` (terminal post-loop lowering) keep standalone sessions.

The flow-sensitive folds of `const_folding` (env-bound locals, forwarded fields,
immutable-global reads, constant-branch collapse) need per-function dataflow
(snapshot/restore/join) and stay in the standalone `fold_constants` walker.

## Per-function dirty-set gating

A `FunctionGate` (`optimize/gate.rs`) lets every loop pass skip functions
unchanged since it last ran. Each function has a monotonic revision; each pass a
per-function watermark. A per-function pass runs a function only when
`revision > watermark` (`run_gated`); any pass that changes a function calls
`mark_changed`, which bumps its revision and that of its 1-hop call-graph
neighbours. Interprocedural passes (`inline`, `dae`, `drve`, `sroa_param`,
`value_copy_demote`) scan all functions but report exactly the ones they touch.
`FunctionId` is a function's index in `NirPackage::functions` (stable within one
optimizer run).

Soundness: gating changes only which functions a pass visits, never the result of
a visit. Every loop pass is an optimization, so an imprecise gate costs only
optimization quality, never correctness — verified by the full E2E suite
(including `wir_expect` shape assertions). The call graph is built once and not
refreshed when a pass shifts a function's edges; stale edges only reduce
propagation precision.

## What stays outside the engine

- Flow-sensitive passes (`field_scalarize`, `licm`, `tmpl_hoist`,
  `value_copy_demote`, `store_load_forward`, the flow-sensitive `const_folding`)
  keep their own dataflow walkers, reading the arena directly.
- Interprocedural stages (`inline`, `dce`, `dae`, `drve`, globalization) run as
  distinct steps, gated as above.

## Status

- Phase 4 — engine substrate, edit API, `Rule`/`run`; every pass moved off the
  `Body ↔ tree` bridge.
- Phase 5 — the tree representation removed: `lower::translate` builds the arena
  directly, the bridge and tree enums / tree visitor are deleted, NIR is
  arena-only.
- Phase 6 — local rules consolidated onto the shared peephole session, and the
  full fixed-point loop put on the dirty-set gate (every pass reports change at
  function granularity).

Measured (isolated NIR optimize phase, dev profile): the arena-direct engine cut
a heavy module's optimize phase ~40 % vs the peak-bridge baseline (zlib 1.12×,
fts 1.49×, sqlite_parse 1.67×); dirty-set gating then cut it a further ~50 % on
the Gale-generated SQLite parser at `-O2`.

## Out of scope

- Layer 2 (hash-consed value e-graph / GVN) — re-decided after the engine, per
  the worklist-engine WEP's sequencing.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the direction.
- [NIR Skeleton Arena (Layer 1)](./wep-2026-06-05-nir-skeleton-arena.md) — the substrate.
- [Wado Optimizer](./optimizer.md) — the pass inventory the engine drives.
