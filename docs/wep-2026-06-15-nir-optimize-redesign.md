# WEP: NIR Optimize Redesign for Compile Speed

This WEP redesigns the NIR optimizer around a single intra-procedural
worklist that builds the ValueGraph once per function and maintains it
through the edit API, instead of rebuilding per-pass analysis on every
sweep. It takes Stages 7 – 8 of the
[Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
as its base but splits them: it adopts Stage 7 (ValueGraph promoted to the
source of truth for pure values) and the single-worklist driver, and
declines Stage 8 (equality saturation). The goal is compile-time speed,
not algebraic-output quality.

## Context

The substrate WEP delivered the arena NIR, the engine edit API, the
hash-consed ValueGraph as an engine side-table, the per-function dirty-set
gate, and the Stage 9 interprocedural worklist. What it left in place is the
cost it was built to remove.

A native sampling profile of `wado compile -O2` on package-gale puts
`run_gated` (the gated intra passes) at ~48% of compile CPU. Inside it the
dominant cost is reconstruction that is rebuilt per pass per function and
then discarded:

- per-function `ValueGraph` build (`builder::build` + `Engine::value`) ~22%
- flow joins (`join_overlay` / `join_heap` / `flow_join`) ~16%
- alias-set computation ~14%

The required path can only amortise this for _unchanged_ functions: the
dirty-set gate skips them, and the revision-keyed `vg_cache` (gate.rs) shares
one parked `ValueGraph` across `cse` / `store_load_forward` /
`condition_implication` when the function did not change between them. A
function that _did_ change rebuilds the whole analysis once per pass that
visits it.

The incremental-ValueGraph prototype (build the changed region only, reusing
the unchanged prefix) was the obvious next lever. It was built, verified
byte-identical under a `WADO_VERIFY_INCREMENTAL` harness, and reverted: it
fired on ~0.15% of builds on the large workload and 0% on the small ones. The
cause is architectural, not implementation — the multi-pass pipeline spoils
every cross-pass journal adjacency (`inline` restructures bodies wholesale,
`const_fold` walks the arena directly, `licm` sits between the value-graph
passes). Raising the fire rate to ~100% requires the architecture where _one_
driver maintains the graph across all rewrites. That is Stage 7 – 8.

## The split Stages 7 – 8 bundle

The direction WEP names two stages and bundles them. They are separable, and
only one of them serves compile speed.

- Stage 7 — promote the ValueGraph from a side-table to the source of truth
  for pure values: pure literal / `Binary` / `Unary` / `Cast` slots become
  `Operand::Value(ValueId)`, `lower::translate` builds values directly, the
  `value_of` side-table is gone. This is the structural prerequisite for the
  graph to be _built once and maintained_ rather than rebuilt.
- Stage 8 — replace destructive rules with equality saturation: run all rules
  non-destructively to a budget-bounded fixed point, then extract a
  cost-minimal Skel form per `Operand::Value`. This unlocks algebraic
  exploration (re-association, strength-reduction-per-use,
  share-vs-duplicate).

The direction WEP already argues Stage 8's payoff is marginal for Wado: the
output is Wasm, which the host JIT re-optimizes, recovering most of the
algebraic wins; Cranelift's aegraph reports 5 – 10% on native-AOT, and the
JIT-target number is much smaller. Stage 8 also carries the costs that make
it the wrong choice for a _speed_ redesign: saturation tuning, e-graph
extraction, rule-explosion control, and a much harder byte-output-identity
argument.

So the compile-speed optimum is the point the direction WEP does not name:
Stage 7's promotion plus a single _destructive_ worklist that maintains the
graph incrementally. Call it Stage 7.5.

## Decision

Adopt Stage 7.5: a single per-function worklist driver over a promoted,
incrementally-maintained ValueGraph. Decline Stage 8.

- The ValueGraph is the source of truth for pure values (Stage 7). It is
  built once when a function's session opens and maintained by the edit API
  as rules fire — never rebuilt per pass.
- All genuinely intra-procedural rewrites run as `Rule`s interleaved on one
  worklist per function, the way `peephole.rs` already runs its subset.
  Reaching-def, CSE equality, store-load forwarding, flow-sensitive constant
  folding, and condition implication all become queries against the one
  maintained graph + flow state.
- Rules stay destructive and priority-ordered (rule order in the session, as
  `peephole.rs` encodes it today). No saturation, no cost-based extraction.
- The interprocedural stages (`inline`, `dae`, `drve`, `sroa_param`,
  `container_sroa`, `sroa`, globalization, `dce`) stay distinct gated steps;
  the single worklist replaces the _per-function intra-procedural_ inner loop,
  not the whole-program structure.

## Architecture

### Promoted ValueGraph (Stage 7)

`lower::translate` builds `ValueId`s directly for pure operand positions;
pure `ExprKind` variants (literal / `Binary` / `Unary` / `Cast`) leave the
SkelTree and become `Operand::Value(ValueId)`. `wir_build` follows `Operand`
and emits the same WIR. The `value_of: IndexMap<ExprId, ValueId>` side-table
and the lazy `Engine::value(expr)` bridge retire (the ~200 – 300 line bridge
budget the direction WEP recorded). The version-tracking algorithm, the
`ValuePool`, and the rules survive unchanged.

### Single combined intra-procedural worklist

The fixed-point loop of N standalone whole-tree passes collapses into one
engine session per function hosting the full intra-procedural rule set,
interleaved on one worklist. This is the existing `peephole.rs` model scaled
to all the local + flow-sensitive rules:

- Local rules already on the peephole session stay: `string_push`,
  `array_literal`, `elide_local`, env-free `const_fold`, `const_branch_prune`,
  `match_to_switch`, `ref_elim`, `elide_box_local`, `labeled_block_fusion`,
  `value_copy_elide`.
- Migrate the per-function dataflow passes onto the same session as rules
  querying the shared graph + flow state: `cse`, `store_load_forward`,
  flow-sensitive `const_fold`, `condition_implication`, `copy_prop`, `licm`,
  `tmpl_hoist`, `field_scalarize`.

A node is revisited only when an edit may have made it reducible — the
worklist re-enqueue the edit API already records. The incremental fire rate
is ~100% by construction: every rewrite goes through the edit API on the one
live graph, so no rule rebuilds anything.

### Incremental graph maintenance through the edit API

The edit API already keeps the parent map and use index coherent and
re-enqueues the affected neighbourhood. Extend it to keep the ValueGraph and
flow state coherent, reusing the reverted prototype's verified mechanism
(retrievable from branch history) at per-edit rather than per-pass
granularity:

- `replace_expr_kind` / `become_expr` — re-derive the `ValueId` of the
  mutated node and re-walk its enclosing region from the nearest checkpoint to
  the end (the prototype's `rebuild_incremental`, scoped to the disturbed
  root-statement span).
- a write edit (Assign-to-FieldAccess, non-pure Call) — bump the affected
  field slot's `HeapVersion`, so later reads at that slot get fresh
  `ValueId`s.
- `set_block_stmts` / `alloc_*` — extend the checkpoint chain over the new
  statements.

The prototype's correctness argument carries over: consumers only test
`value(a) == value(b)` (the equivalence relation, never absolute numbering),
and restoring the entry flow-state before the first disturbed statement
reproduces the from-scratch equivalence classes.

### One shared flow state

`current_value: HashMap<local, ValueId>`, heap versions, `ref_targets`, and
the literal-source map are threaded once through the single worklist; every
flow-sensitive rule reads them instead of running its own snapshot / restore /
join walk. The ~16% spent in `join_overlay` / `join_heap` / `flow_join`
collapses to one join per merge per function instead of one per pass per
merge per function.

### What stays a distinct step

Interprocedural and whole-function structural passes do not fit a local-node
rule and stay distinct gated steps, in the current order:

- `inline`, `dae`, `drve`, `sroa_param`, `container_sroa`, `sroa`,
  `const_object_globalization`, `dce`.
- Their _enabling analyses_ (alias sets, mod/ref) route through the shared
  graph where they overlap with intra-procedural queries, so the ~14% alias
  rebuild is computed once per function per outer round rather than per pass.
- `inline` restructures a body wholesale; after it edits a caller, the
  inlined span's graph + flow is rebuilt incrementally (the same
  region-rebuild mechanism), not the whole function.

### Why destructive, not saturation

Destructive single-worklist preserves the soundness invariant that has gated
every migration: byte-output-identity with the pass it replaces. A saturating
driver makes that argument far harder (extraction must reproduce the exact
materialisation the old CSE / hoisting heuristics produced) for a payoff the
host JIT largely recovers. Priorities are encoded by rule order, the way
`peephole.rs` already does; confluence is the same obligation the engine
already carries. If a future native-AOT backend lands, Stage 8 reactivates on
top of this driver — the graph, rules, and worklist all carry over.

## Migration plan

Each step is byte-output-identical to its predecessor on the full fixture +
E2E suite and on package-gale before the predecessor is deleted, the same
discipline the Stage 3 – 6 migrations followed.

- [ ] Stage 7a — `Operand::Value` on pure literal slots; `lower::translate`
      builds them; `wir_build` follows. WIR byte-identical.
- [ ] Stage 7b — extend `Operand::Value` to `Binary` / `Unary` / `Cast`;
      retire the `value_of` side-table and the `Engine::value` bridge.
- [ ] Stage 7.5a — edit-API graph maintenance: per-edit incremental
      re-derivation of `value_of` + heap versions, behind a
      `WADO_VERIFY_INCREMENTAL` equivalence harness asserting parity with a
      from-scratch rebuild.
- [ ] Stage 7.5b — one shared flow state threaded through the worklist;
      migrate `cse` + `store_load_forward` onto it (the clean adjacency the
      prototype already proved). Delete their standalone analysis.
- [ ] Stage 7.5c — migrate flow-sensitive `const_fold`, `condition_implication`,
      `copy_prop` onto the shared session; delete their per-pass dataflow.
- [ ] Stage 7.5d — migrate `licm`, `tmpl_hoist`, `field_scalarize`; collapse
      the fixed-point loop's intra-procedural inner passes into the single
      session. The interprocedural steps and the outer round driver remain.
- [ ] Stage 7.5e — retire `OptConfig::iterations` for the intra-procedural
      part (the worklist self-converges); keep an outer round count only for
      the interprocedural cycle (`inline` → re-examine callers).

## Soundness invariants

- Byte-output-identity per step (as above). The promotion (Stage 7) may alter
  the NIR shape; the WIR output stays byte-identical.
- Rules stay idempotent and confluent-or-priority-ordered; priorities are rule
  order in the session.
- Graph-maintenance equivalence: after any edit, `value(a) == value(b)` holds
  iff it held in a from-scratch rebuild of the post-edit body. Verified by the
  `WADO_VERIFY_INCREMENTAL` harness on the full E2E suite before the
  from-scratch path is removed.
- Heap-version monotonicity is unchanged: a read carries the version before
  it; a following write bumps it; later reads get fresh `ValueId`s.
- Gating still changes only which functions a step visits, never the result of
  a visit, so an imprecise gate costs quality, never correctness.

## Consequences

### Expected effect

- The ~22% (build) + ~16% (joins) of `run_gated` collapse to one build + one
  join set per function per outer round, maintained incrementally thereafter
  — the bulk of the per-pass reconstruction the redesign targets.
- The ~14% alias rebuild is computed once per function per round rather than
  per pass.
- Target: package-gale optimise phase ~1.5× faster than the substrate-only
  baseline (the direction WEP's aspirational number; this is the path to it).
- Code reduction: the `optimize/` directory shrinks as each migrated dataflow
  pass loses its bespoke analysis (CSE keys, def maps, snapshot/join walkers)
  and becomes a thinner rule querying the shared graph.

### Risks

- Phase ordering: the current pipeline encodes ordering in the pass sequence
  (`container_sroa` before `inline`, `value_copy_demote` after the pre-inline
  peephole, …). Intra-procedural ordering becomes rule priority in the single
  session; the genuinely interprocedural ordering stays explicit. Mis-encoded
  priority costs quality, caught by the `wir_expect` fixtures.
- Incremental flow at merges and loops: recomputing `Select` / `LoopPhi` on an
  edit inside a branch or loop body is the subtle part; the verify harness is
  the guard, and the prototype already exercised the loop case.
- `inline`-driven wholesale restructuring keeps a region rebuild rather than a
  pure incremental update; if that region is most of the function the saving
  shrinks, but it is still one rebuild instead of one-per-downstream-pass.

### Trade-offs accepted

- No algebraic exploration beyond what destructive rules already do; recovered
  by the host JIT, per the direction WEP's measured argument.
- The edit API grows graph + flow maintenance, raising its complexity in
  exchange for deleting every pass's bespoke rebuild.
- Arena compaction (dead nodes from in-place rewrites) becomes more worthwhile
  once the body is walked fewer times; tracked as the existing follow-up.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the direction and the Stage 7 – 8 definitions this WEP splits.
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the landed engine substrate, edit API, and gate.
- [`docs/optimizer.md`](./optimizer.md) — the pass inventory the single worklist absorbs.
- The reverted incremental-ValueGraph prototype, in branch history (`feat(optimize): edit journal on the NIR engine`, `incremental ValueGraph rebuild core`, `wire incremental ValueGraph through the gate`) — the verified mechanism Stage 7.5a reuses per-edit.
  </content>
  </invoke>
