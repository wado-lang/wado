# WEP: The Live ValueGraph — Single-Session NIR Optimizer

This WEP redesigns the NIR optimizer around a single intra-procedural
worklist that builds the ValueGraph once per function and keeps it live
through the edit API, instead of rebuilding per-pass analysis on every
sweep. It promotes the ValueGraph from an engine side-table to the source
of truth for pure values, then runs one destructive worklist per function
over it. The goal is compile-time speed.

## Context

The substrate WEP delivered the arena NIR, the engine edit API, the
hash-consed ValueGraph as an engine side-table, the per-function dirty-set
gate, and the interprocedural worklist. What it left in place is the cost it
was built to remove.

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
driver keeps the graph live across all rewrites.

## Operand promotion

Promote the ValueGraph from a side-table to the source of truth for pure
values: pure literal / `Binary` / `Unary` / `Cast` slots become
`Operand::Value(ValueId)`, `lower::translate` builds values directly, the
`value_of` side-table is gone. This is the structural prerequisite for the
graph to be _built once and kept live_ rather than rebuilt — without it, every
pass has to re-derive a fresh `value_of` from the SkelTree.

Operand promotion plus a single _destructive_ worklist that keeps the graph
live is the state the rest of this WEP calls **the live ValueGraph**.

## Decision

Adopt the live ValueGraph: a single per-function worklist driver over a
promoted ValueGraph kept coherent through the edit API.

- The ValueGraph is the source of truth for pure values (operand promotion).
  It is built once when a function's session opens and kept live by the edit
  API as rules fire — never rebuilt per pass.
- All genuinely intra-procedural rewrites run as `Rule`s interleaved on one
  worklist per function, the way `peephole.rs` already runs its subset.
  Reaching-def, CSE equality, store-load forwarding, flow-sensitive constant
  folding, and condition implication all become queries against the one live
  graph + flow state.
- Rules stay destructive and priority-ordered (rule order in the session, as
  `peephole.rs` encodes it today).
- The interprocedural stages (`inline`, `dae`, `drve`, `sroa_param`,
  `container_sroa`, `sroa`, globalization, `dce`) stay distinct gated steps;
  the single worklist replaces the _per-function intra-procedural_ inner loop,
  not the whole-program structure.

## Architecture

### Promoted operands

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

### Keeping the graph live through the edit API

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

### Destructive rewrites

Rules rewrite the graph in place, exactly as the engine's rules do today, so
each migrated rule maps directly onto the pass it replaces — the destructive
driver reproduces the existing CSE / hoisting materialisation without a new
extraction heuristic. Priorities are encoded by rule order, the way
`peephole.rs` already does; confluence is the same obligation the engine
already carries.

## Migration plan

Each step must not regress output (code size or runtime) on the full fixture +
E2E suite, on `wir_expect`/`wir_not_expect`, and on the benchmark set before
the predecessor is deleted. An improvement is welcome; incidental output
differences that neither shrink nor grow the result meaningfully are
acceptable. A regression is not.

- [ ] Promote literal operands — `Operand::Value` on pure literal slots;
      `lower::translate` builds them; `wir_build` follows. A representation
      change, so WIR is expected to stay the same.
- [ ] Promote arithmetic operands — extend `Operand::Value` to `Binary` /
      `Unary` / `Cast`; retire the `value_of` side-table and the
      `Engine::value` bridge.
- [ ] Keep the graph live — edit-API graph maintenance: per-edit incremental
      re-derivation of `value_of` + heap versions, behind a
      `WADO_VERIFY_INCREMENTAL` equivalence harness asserting parity with a
      from-scratch rebuild.
- [ ] Share one flow state — thread it through the worklist; migrate `cse` +
      `store_load_forward` onto it (the clean adjacency the prototype already
      proved). Delete their standalone analysis.
- [ ] Fold in the flow-sensitive rules — migrate flow-sensitive `const_fold`,
      `condition_implication`, `copy_prop` onto the shared session; delete
      their per-pass dataflow.
- [ ] Collapse the inner loop — migrate `licm`, `tmpl_hoist`,
      `field_scalarize`; fold the fixed-point loop's intra-procedural passes
      into the single session. The interprocedural steps and the outer round
      driver remain.
- [ ] Retire the intra-procedural iteration count — the worklist
      self-converges; keep an outer round count only for the interprocedural
      cycle (`inline` → re-examine callers).

## Soundness invariants

- No output regression per step (as above): each step preserves or improves
  the result, never degrades it. Operand promotion is a representation change
  with no intended output effect; the dataflow migrations may shift output
  incidentally, which is fine as long as it is not a regression.
- Rules stay idempotent and confluent-or-priority-ordered; priorities are rule
  order in the session.
- Graph-liveness equivalence: after any edit, `value(a) == value(b)` holds iff
  it held in a from-scratch rebuild of the post-edit body. Verified by the
  `WADO_VERIFY_INCREMENTAL` harness on the full E2E suite before the
  from-scratch path is removed.
- Heap-version monotonicity is unchanged: a read carries the version before
  it; a following write bumps it; later reads get fresh `ValueId`s.
- Gating still changes only which functions a step visits, never the result of
  a visit, so an imprecise gate costs quality, never correctness.

## Consequences

### Expected effect

- The ~22% (build) + ~16% (joins) of `run_gated` collapse to one build + one
  join set per function per outer round, kept live incrementally thereafter —
  the bulk of the per-pass reconstruction the redesign targets.
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
- Live flow at merges and loops: recomputing `Select` / `LoopPhi` on an edit
  inside a branch or loop body is the subtle part; the verify harness is the
  guard, and the prototype already exercised the loop case.
- `inline`-driven wholesale restructuring keeps a region rebuild rather than a
  pure incremental update; if that region is most of the function the saving
  shrinks, but it is still one rebuild instead of one-per-downstream-pass.

### Trade-offs accepted

- The edit API grows graph + flow maintenance, raising its complexity in
  exchange for deleting every pass's bespoke rebuild.
- Arena compaction (dead nodes from in-place rewrites) becomes more worthwhile
  once the body is walked fewer times; tracked as the existing follow-up.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the direction; defines operand promotion (adopted here) and the exploratory equality-saturation driver (out of scope here).
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the landed engine substrate, edit API, and gate.
- [`docs/optimizer.md`](./optimizer.md) — the pass inventory the single worklist absorbs.
- The reverted incremental-ValueGraph prototype, in branch history (`feat(optimize): edit journal on the NIR engine`, `incremental ValueGraph rebuild core`, `wire incremental ValueGraph through the gate`) — the verified mechanism the live-graph edit API reuses per-edit.
  </content>
