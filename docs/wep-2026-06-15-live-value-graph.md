# WEP: The Live ValueGraph — Single-Session NIR Optimizer

This WEP redesigns the NIR optimizer for compile speed. The current optimizer
runs a fixed-point loop of standalone passes, and each pass that needs pure-
value identity or reaching-defs rebuilds the per-function ValueGraph from
scratch and throws it away. The redesign collapses the intra-procedural passes
into one worklist session per function that builds the ValueGraph once and
keeps it live — maintaining it incrementally through the engine edit API as
rules fire — so the graph is built once per function per outer round instead of
once per pass.

The ValueGraph stays an engine side-table (`engine.value(expr)`); this WEP does
not change the NIR arena. Two heavier promotions the
[Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
WEP describes — moving pure values into IR-level operands, and switching to
equality saturation — are out of scope here and stay in that WEP.

## Context

A sampling profile (samply, 1 kHz, dev/debug `wado`) of
`wado compile -O2 package-gale/src/main.wado` (~37k lines) pins the cost.
Percentages are inclusive shares of total CPU — machine-independent ratios;
absolute time varies by host. `optimize` is 69% of compile CPU, and the gated
intra passes (`run_gated` + `run_gated_cached`) are ~49%. Inside them the
dominant cost is reconstruction rebuilt per pass per function and discarded:

- per-function `ValueGraph` build (`builder::build`, reached via
  `Engine::value`; the `walk_block` / `walk_expr` flow walk) ~20%
- per-session engine setup (`Engine::new`: parent maps, use index, post-order
  seed) ~9%
- flow joins (`join_heap` / `flow_join_two` / `flow_join_n`) ~10%
- alias-set computation (`builder_alias_sets`) ~9%

Together that is ~40% of compile CPU spent building and tearing down
per-function analysis the next pass rebuilds from scratch.

The dirty-set gate amortises this only for _unchanged_ functions: it skips
them, and the revision-keyed `vg_cache` shares one parked `ValueGraph` across
the value-graph passes when the function did not change between them. A
function that _did_ change rebuilds the whole analysis once per pass that
visits it — and a busy function changes often.

The build is compute-bound, not allocation-bound: pooling the builder's output
maps measured no improvement. The cost is the flow walk, the hash-cons, and the
joins — so the only way to remove it is to stop redoing it.

## Why a single session

Re-walking only the changed region of a function — reusing the unchanged prefix
of its ValueGraph — was prototyped, verified byte-identical under a
`WADO_VERIFY_INCREMENTAL` harness, and reverted. It fired on ~0.15% of builds on
the large workload and 0% on the small ones, so it bought nothing.

The failure was architectural, not a bug in the mechanism. Incremental rebuild
needs the parked graph's edits to be the _complete_ delta since it was parked.
In the multi-pass pipeline almost every pass handoff is spoiled: `inline`
restructures bodies wholesale, the flow-sensitive `const_fold` walks the arena
directly, `licm` sits between the value-graph passes. The only clean adjacency
is `cse → store_load_forward`, and `cse` rarely changes anything, so even that
one handoff had nothing to pass downstream.

The mechanism itself is sound and verified. What it needs is an architecture
where _one_ driver owns the graph across _all_ rewrites, so every edit is part
of the delta and the incremental rebuild fires on essentially every query. That
is a single session.

## Decision

Run all intra-procedural rewrites in one worklist session per function, over a
ValueGraph the session keeps live.

- The ValueGraph and its flow state are built once when a function's session
  opens. The engine edit API journals every mutation; before a rule queries a
  value, the engine re-walks only the journaled dirty regions to bring the
  graph current. The graph is never rebuilt from scratch within a session.
- All genuinely intra-procedural rewrites run as `Rule`s interleaved on one
  worklist, the way `peephole.rs` already runs its subset. CSE equality,
  store-load forwarding, flow-sensitive constant folding, condition implication,
  and the loop passes all become queries against the one live graph.
- Rules stay destructive and priority-ordered (rule order in the session, as
  `peephole.rs` encodes it today). The graph stays an engine side-table; the NIR
  arena is unchanged.
- The interprocedural stages (`inline`, `dae`, `drve`, `sroa_param`,
  `container_sroa`, `sroa`, `const_object_globalization`, `dce`) stay distinct
  gated steps; the single session replaces the _per-function intra-procedural_
  inner loop, not the whole-program structure.

## Architecture

### The live graph

`builder::build` does one linear, flow-sensitive walk of a function body: it
threads `current_value` (the `ValueId` of every live local), the heap versions,
reference look-through targets, and the field-store forwarding table, and at
control-flow merges it snapshots each arm and joins them into `Select` values.
It leaves a side-table `value_of: ExprId → ValueId` that the rules query through
`engine.value(expr)`. The worklist does not re-derive flow per query; it reads
the precomputed map. Keeping the graph live therefore means keeping `value_of`
and that flow state current after each edit — not re-deriving anything per pass.

The redesign keeps this builder and this side-table. It changes only _when_ the
walk runs: once per session, then incrementally over the edited regions, instead
of once per pass.

### Journaling edits

The engine edit API (`replace_expr_kind`, `become_expr`, `set_block_stmts`,
`alloc_*`, `clone_expr`) already keeps the parent map, use index, and worklist
coherent. It gains one more responsibility: record each mutated node so the
graph can be brought current later. Mapping a mutated node up the parent map to
the root-block statement that encloses it yields the set of disturbed top-level
statements; an edit to the root statement list itself cannot be localised and
forces a full rebuild (rare, and a safe fallback). This is the verified
`dirty_root_stmts` mechanism from the reverted prototype, reused unchanged.

### Incremental rebuild on query

Before a rule reads `engine.value(...)` (or `value_kind` / `literal_source` /
`loop_entry_value`), the engine settles the journal: it finds the first disturbed
root statement, restores the exact flow-state the clean prefix left at that
point, and re-walks from there to the end of the body, overwriting `value_of` for
the re-walked region. Everything before the first dirty statement is reused.

The correctness argument is the prototype's, already verified: consumers only
test `value(a) == value(b)` (the equivalence relation, never absolute
numbering), and restoring the entry flow-state before the first disturbed
statement reproduces the from-scratch equivalence classes. The reusable pieces —
the per-statement checkpoint of the full flow-state, the `rebuild_incremental`
re-walk, and the `WADO_VERIFY_INCREMENTAL` harness that builds both ways and
asserts observable equality — are retrievable from branch history.

Settling is lazy and coalesced: structural rules that never query the graph
(block flattening, statement fusion) journal their edits but trigger no re-walk;
the cost is paid once, when a value-querying rule next runs, over the union of
the regions disturbed since the last settle. Re-walk granularity is root-block
statements in the MVP; a finer granularity is a later tuning lever if scattered
edits make the re-walked suffix too large.

### The single combined session

The fixed-point loop's intra-procedural passes collapse into one session hosting
the full rule set, interleaved on one worklist — the existing `peephole.rs`
model, scaled up:

- Local rules already on the peephole session stay: `string_push`,
  `array_literal`, `elide_local`, env-free `const_fold`, `const_branch_prune`,
  `match_to_switch`, `ref_elim`, `elide_box_local`, `labeled_block_fusion`,
  `value_copy_elide`.
- The per-function dataflow passes join them as rules querying the live graph:
  `cse`, `store_load_forward`, flow-sensitive `const_fold`,
  `condition_implication`, `copy_prop`, `licm`, `tmpl_hoist`, `field_scalarize`.

A node is revisited only when an edit may have made it reducible — the
re-enqueue the edit API already records. Because there is one session over one
live graph, the incremental rebuild fires on essentially every query, the fire
rate the prototype could never reach in the multi-pass pipeline.

### What stays a distinct step

Interprocedural and whole-function structural passes do not fit a local-node
rule and stay distinct gated steps, in the current order: `inline`, `dae`,
`drve`, `sroa_param`, `container_sroa`, `sroa`, `const_object_globalization`,
`dce`. The session is opened once per function per outer round; an
interprocedural pass that restructures a body (notably `inline`) invalidates the
session's graph for that function, so the next round rebuilds it once and then
keeps it live again. The outer round count bounds the interprocedural cycle
(`inline` shrinks a callee → re-examine its callers); the intra-procedural
worklist self-converges within each round.

### Destructive rewrites

Rules rewrite in place, exactly as the engine's rules do today, so each migrated
rule maps directly onto the pass it replaces and reproduces its CSE / hoisting
materialisation without a new extraction heuristic. Priorities are rule order in
the session; confluence is the obligation the engine already carries.

## Soundness invariants

- No output regression per step (below): each step preserves or improves the
  result, never degrades it. Incidental output differences that neither shrink
  nor grow the result meaningfully are acceptable; a regression is not.
- Graph-liveness equivalence: after any edit, `value(a) == value(b)` holds iff
  it held in a from-scratch rebuild of the post-edit body. Asserted by the
  `WADO_VERIFY_INCREMENTAL` harness on the full E2E suite before the
  from-scratch path is removed.
- Heap-version monotonicity is unchanged: a read carries the version before it;
  a following write bumps it; later reads at that slot get fresh `ValueId`s.
- Rules stay idempotent and confluent-or-priority-ordered.
- Gating still changes only which functions a step visits, never the result of a
  visit, so an imprecise gate costs quality, never correctness.

## Consequences

### Expected effect

- The ValueGraph build (~20%), engine session setup (~9%), and flow joins (~10%)
  collapse to one build + one setup + one join set per function per outer round,
  kept live incrementally thereafter — together ~40% of compile CPU today, the
  bulk of what the redesign removes.
- The alias rebuild (~9%) is computed once per function per round rather than per
  pass.
- Target: package-gale optimise phase ~1.5× faster than the current baseline.
  Aspirational, not committed.
- Code reduction: each migrated dataflow pass loses its bespoke analysis (CSE
  keys, def maps, snapshot/join walkers) and becomes a thinner rule querying the
  shared graph.

### Risks

- Phase ordering: the current pipeline encodes ordering in the pass sequence
  (`container_sroa` before `inline`, `value_copy_demote` after the pre-inline
  peephole, …). Intra-procedural ordering becomes rule priority in the single
  session; the genuinely interprocedural ordering stays explicit. Mis-encoded
  priority costs quality, caught by the `wir_expect` fixtures.
- Re-walk size: if edits scatter across a function, the re-walked suffix can be
  large. Lazy coalescing bounds it to one settle per query-burst; finer
  granularity is the follow-up lever if measurement needs it.
- Live flow at merges and loops: re-deriving `Select` / `LoopPhi` after an edit
  inside a branch or loop body is the subtle part; the prototype already
  exercised it and the verify harness guards it.
- Migrating the flow-sensitive passes (especially `licm` and `field_scalarize`,
  which carry the most bespoke dataflow) onto live-graph queries is the largest
  single piece of work.

### Trade-offs accepted

- The edit API grows journaling and the engine grows settle-on-query, in
  exchange for deleting every pass's bespoke rebuild.
- Arena compaction (dead nodes from in-place rewrites) becomes more worthwhile
  once the body is walked fewer times; tracked as the existing follow-up.

## Roadmap

Each step must not regress output (code size or runtime) on the full fixture +
E2E suite, on `wir_expect` / `wir_not_expect`, and on the benchmark set before
the predecessor is deleted. The `WADO_VERIFY_INCREMENTAL` harness asserts
graph-liveness equivalence throughout the migration.

- [ ] Restore the verified mechanism from history — the per-statement flow-state
      checkpoint, `rebuild_incremental`, `dirty_root_stmts`, and the
      `WADO_VERIFY_INCREMENTAL` harness — as engine internals, not yet wired to
      any pass.
- [ ] Settle-on-query in the engine — the edit API journals dirty regions; a
      value query coalesces the journal and re-walks only the disturbed regions
      before answering. Verified equivalent to a from-scratch rebuild.
- [ ] Combined-session skeleton — extend the `peephole.rs` model to a session
      that can host flow-sensitive rules querying the live graph, with rule
      priority replacing intra-procedural pass order.
- [ ] Fold in `cse` + `store_load_forward` — the adjacency the prototype already
      proved clean. Delete their standalone analysis.
- [ ] Fold in flow-sensitive `const_fold`, `condition_implication`, `copy_prop`
      — delete their per-pass dataflow walkers.
- [ ] Fold in `licm`, `tmpl_hoist`, `field_scalarize` — collapse the fixed-point
      loop's intra-procedural passes into the single session.
- [ ] Retire the intra-procedural iteration count — the worklist self-converges;
      keep an outer round count only for the interprocedural cycle.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the direction; the operand-promotion and equality-saturation steps deferred there stay out of scope here.
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the engine substrate, edit API, and gate this builds on.
- [`docs/optimizer.md`](./optimizer.md) — the pass inventory the single session absorbs.
- The reverted incremental-ValueGraph prototype, in branch history — the checkpoint, `rebuild_incremental`, and verify harness this design reuses.
  </content>
