# WEP: Worklist-Driven NIR Rewrite Engine

This WEP sets the terminal architecture for the NIR optimizer: a single
worklist-driven rewrite engine — the _combine_ — over the structured NIR
arena, with a call-graph-driven interprocedural driver replacing the global
fixed-point loop. The engine substrate and a dirty-set gate over the old loop
have landed (see the [detailed design](./wep-2026-06-05-nir-rewrite-engine-design.md)
and the [NIR Skeleton Arena](./wep-2026-06-05-nir-skeleton-arena.md)); the
combine itself — every intra-procedural rewrite as a rule on one worklist, and
the global loop removed — is the open continuation specified here. Reaching it
was the original intent of this WEP; the substrate was step one.

## Context

The optimizer was historically ~31 independent passes, each a full mutating
walk over every function, run inside a global fixed-point loop. Two structural
changes have since landed:

- The `Body ↔ tree` bridge is gone; NIR is an arena (`nir_arena.rs`) and the
  worklist engine (`nir_engine.rs`) exists, with a parent map, a local use
  index, an edit API, and a `Rule`/`run` driver.
- A per-function dirty-set gate (`optimize/gate.rs`) lets each pass skip
  functions unchanged since it last ran.

What did _not_ change is the architecture the first version of this WEP set out
to replace. Only a handful of rewrites actually run as rules on the engine: the
unified peephole session (`string_push`, `array_literal`, `elide_local`, the
env-free subset of `const_folding`, `const_branch_prune`), plus the standalone
`match_to_switch` and `select_lowering` sessions. The other ~15 intra-procedural
passes — `sroa`, `copy_prop`, `cse`, the flow-sensitive `const_folding`,
`store_load_forward`, `ref_elim`, `elide_box_local`, `labeled_block_fusion`,
`container_sroa`, `value_copy_elide` / `value_copy_demote`, `tmpl_hoist`,
`condition_implication`, `dae`, `drve` — are still standalone whole-tree passes,
driven by the global fixed-point loop (`run_optimization_passes`). The gate made
each sweep cheaper; it did not remove the `N passes × M iterations` shape.

A native profile of `wado compile` on `package-gale` (34.5k lines, the largest
in-tree program) pins the cost to that surviving shape:

- `optimize` is ~52% of the whole compile (~5.5s after engine-buffer pooling).
- 1,572 functions carry a body; the median body is 31 NIR nodes (p90 209, p99
  923, max 4,095) over 132,645 nodes total. Effective cost is ~41µs per node —
  far above node-level work, because each node is walked tens of times.
- The loop runs ~20 passes per iteration and converges in 5 iterations at `-O2`
  (~85 whole-program sweeps), each pass reconstructing whatever per-function
  analysis it needs.
- Monomorphization duplicates only 13% of nodes, so "optimize each generic
  once" is not the lever. The lever is the multiplicative constant —
  `passes × iterations × per-function reconstruction` — not the IR size.

Incremental tuning (allocation pooling, the gate) removes constant factors but
cannot change that the program is swept ~85 times. The remaining win is
architectural: finish the combine.

## Decision

The terminal optimizer architecture is the structured-NIR combine:

- Intra-procedural: one per-function worklist runs _all_ local rules to a local
  fixed point in a single traversal-with-revisits. A node is visited only when
  an edit to its neighbourhood might have made it reducible. There is no
  per-pass whole-tree walk and no global fixed-point sweep.
- Interprocedural: `inline`, `dae`, `drve`, `sroa_param`, and globalization run
  off a call-graph worklist. A function is combined once; an interprocedural
  edit re-queues only the functions it can affect (a callee shrank → re-combine
  its callers; a signature changed → re-combine its call sites), which are then
  re-combined. The global "repeat ~20 passes M times" loop and
  `OptConfig::iterations` are removed.
- Flow-sensitive dataflow (const-prop across branches, copy propagation,
  store-to-load forwarding, dominator-implied conditions) is served by a
  value-numbering / reaching-def side-table the engine maintains incrementally
  alongside the parent map and use index — not re-derived per pass, and not via
  full SSA.

One change, three wins, unchanged from the original framing but now concrete:

- Speed: removes the `passes × iterations` multiplier and the per-pass analysis
  reconstruction; per function the work trends to `O(nodes + edits)` plus
  interprocedural propagation, instead of ~85 sweeps.
- Maintainability: one engine with one rule set replaces ~20 separately-driven
  passes and their hand-tuned loop ordering; adding a rewrite is adding a rule.
- Correctness: a single explicit worklist discipline (what re-enqueues what)
  replaces emergent interactions between whole-tree passes and a global fixed
  point; most phase-ordering hazards dissolve because rules co-exist.

## Why the combine is the destination, not a stepping stone

The natural objection is that a fast optimizer "should" be SSA, making the
structured-IR combine throwaway work. It is not, for a Wasm target:

- Wado emits structured Wasm (block / loop / if; no arbitrary CFG). Classical
  SSA needs SSA construction plus a relooper / stackifier to re-emit structured
  control flow — overhead that buys nothing for Wado's problem and dissolves the
  structure the backend re-emits directly. This is the same reason NIR keeps its
  tree shape (see [NIR Layer](./wep-2026-05-11-nir.md)).
- Binaryen, the production-grade Wasm optimizer, is a structured-AST worklist
  rewriter, not a classical SSA optimizer; it carries SSA-style _local_ analyses
  (`LocalGraph`) only where they pay. That is exactly this design.
- Where SSA genuinely wins — flow-sensitive value numbering — the engine takes
  it locally via the reaching-def / numbering side-table, with no CFG
  roundtrip.

So the combine investment is terminal: the worklist discipline, the
interprocedural driver, the Wado-specific rewrite semantics (value-copy
elimination, aliasing, places), and the structured arena all persist regardless.
Full SSA / sea-of-nodes stays rejected.

## IR substrate

The substrate is the two layers the first version of this WEP proposed, split on
the only distinction rewriting cares about — whether a node is a
referentially-transparent value or an ordered effect.

- Layer 1 — effect skeleton: a flat per-function arena (`NodeId` + parent + use
  index) for statements, control flow, places, memory writes, and effectful
  expressions. Parent is unique; the worklist is the classic tree walk. Landed:
  this is the current `Body` (see [NIR Skeleton Arena](./wep-2026-06-05-nir-skeleton-arena.md)).
- Layer 2 — pure-value e-graph: a hash-consed acyclic e-graph for
  referentially-transparent expressions, whose value ids the skeleton's leaf
  operands reference. Rewrites are non-destructive (add an equivalence), CSE /
  GVN fall out for free, and a final extraction picks the best form. Optional
  accelerator, on the reference of Cranelift's aegraph mid-end. It is revisited
  only if the Layer-1 value-numbering side-table proves insufficient for CSE /
  GVN; the combine does not depend on it.

The pure partition stays small because Wado's aliasing, places, and value-copy
semantics make few reads referentially transparent (a `FieldAccess` read is not
pure across an intervening write), gated by `optimize/mod_ref.rs`. So Layer 1
plus a numbering side-table is expected to carry the flow-sensitive folds; Layer
2 is a measured follow-up, not a prerequisite.

## Migration plan

The engine, edit API, and gate exist. The remaining work moves each standalone
intra-procedural pass into the shared combine session and then deletes the
global loop. Each migrated rule must be byte-output-identical to the pass it
replaces, on the full fixture + E2E suite and on `package-gale`, before the old
pass is removed — the old and new paths co-exist during migration.

- [x] Engine substrate, edit API, `Rule`/`run`; arena-only NIR (Phases 4–5).
- [x] Per-function dirty-set gate over the existing loop (Phase 6).
- [x] Reduce per-session allocation (`EngineBuffers` pooling; `Engine::new`
      allocations cut, byte-identical).
- [ ] Migrate the position-flexible structural rules into the shared session:
      `ref_elim`, `elide_box_local`, `labeled_block_fusion`, `container_sroa`,
      `sroa`, `value_copy_elide` / `value_copy_demote`, `tmpl_hoist`.
- [ ] Add the engine-maintained value-numbering / reaching-def side-table;
      migrate `copy_prop`, `store_load_forward`, `condition_implication`, and
      the flow-sensitive half of `const_folding` into rules over it.
- [ ] CSE / GVN over the same numbering (or Layer 2, only if measured
      necessary).
- [ ] Replace the global fixed-point loop with the interprocedural call-graph
      worklist driving `inline` / `dae` / `drve` / `sroa_param` plus targeted
      re-combine; remove `OptConfig::iterations`.
- [ ] Keep terminal / once-only stages explicit pre- or post-combine, not loop
      members: `match_to_switch`, `select_lowering`, `multi_value_return`,
      `field_scalarize`, `const_object_globalization`, and `dce`.

## Soundness invariants

- Byte-identical co-existence: a pass is deleted only after its rule form
  reproduces its output on the full suite, so a migration can never silently
  regress codegen.
- Rules are idempotent (the per-node retry terminates) and either confluent or
  priority-ordered.
- The interprocedural worklist must re-combine every function an edit can
  affect; over-approximation only costs a redundant re-combine, under-
  approximation drops an optimization — the same one-sided safety argument as
  the gate (every loop pass is optional, so imprecision costs quality, never
  correctness).
- The few genuine ordering constraints today encoded as loop position (e.g.
  value-copy wrappers must be visible before `inline` expands them) become
  explicit rule priorities or named pre-stages, not emergent loop order.

## Consequences

- Expected effect: the `passes × iterations` multiplier and per-pass analysis
  reconstruction disappear; the realistic near-term target is 2–3× on the
  optimize phase, with the numbering side-table and once-only interprocedural
  driving carrying it further. "Optimize in ~1s" is the aspiration that motivates
  the redesign, not a committed number.
- Risk is concentrated in the flow-sensitive migration (value-numbering
  correctness), de-risked by the byte-identical co-existence gate per pass.
- Arena compaction (dead nodes from in-place rewrites are not freed mid-run;
  ~1.66× bloat measured at end-of-optimize on `package-gale`) becomes more
  worthwhile once the combine walks bodies fewer times; tracked as a separate
  follow-up, wanted only if it measures.

Out of scope: the resolver, monomorphizer, lowering, the WIR optimizer, and
codegen. Codegen must not regress: the combine has to reach at least the current
fixed point's result on existing fixtures.

## See also

- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the landed engine substrate, edit API, and gate.
- [NIR Skeleton Arena (Layer 1)](./wep-2026-06-05-nir-skeleton-arena.md) — the substrate.
- `docs/optimizer.md` — the current pass inventory the combine absorbs.
- The profiling workflow behind the numbers above:
  `.claude/skills/profiling-wado-compiler`.
