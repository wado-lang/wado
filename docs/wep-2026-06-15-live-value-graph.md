# WEP: The Live ValueGraph — ValueGraph as the Pure-Value IR

This WEP redesigns the NIR optimizer for compile speed by making the ValueGraph
the source of truth for pure values, instead of a side-table re-derived from the
SkelTree on every pass. Pure operand positions in the SkelTree become
`ValueId`s; the graph is built once per function, rewritten eagerly in place via
e-class union, and extracted back to a skeleton form once before WIR build. The
SkelTree stays the effect-and-control schedule.

This is the aegraph mid-end model (build once, eager rewrite with union-find,
single extraction — Cranelift's, not equality saturation): rules apply once and
destructively-by-union, never searched to a fixed point. Equality saturation
stays out of scope.

## Status — complete

`value_of` (the persisted `ExprId -> ValueId` side-table) is **deleted** — criterion 3
met. Every value the optimizer reads now comes from a promoted operand
(born-as-operands) or pure re-derivation over the value pool; an unpromoted skeleton
leaf resolves to `None` (sound: `None` is the finest partition, so a consumer skips
the expr rather than over-merge). Build-once is the default and `rebuilds = 0`
(criterion 1).

Criterion 2 (optimize CPU halved) is **measured and not met — and will not be
pursued further** (decision: 2026-06-24). package-gale optimize phase ≈ **15.7 s**
(dev build, `rebuilds = 0`), vs the ~7.5 s target. The premise was wrong: with
build-once the value-graph build is now only ~6 % of the phase (was 20.76 %); the
real cost is the passes themselves — `peephole` (≈ 4.6 s), the 5-iteration fixpoint,
`licm`, `inline` — and the added born-as-operands passes (`promote_fields`,
`cond_impl_post_promote`, …) roughly offset the build-once saving. A 2× win needs
pass-level work (faster `peephole`, fewer iterations), a separate track from this
value-graph re-architecture. **The build-once + no-side-table invariants stand
regardless** (see `wado-compiler/AGENTS.md`): do not reintroduce rebuilds or a cache.

Validated end state: `cargo test --lib` 738/0; full e2e (O0+O2) **0 / 3030**;
`mise run test` / `mise run test-wado` green. No `WADO_*` promotion gate remains —
the structural / born-as-operands path is the single production path.

Survives by design: `nir_value_graph::builder` still runs — it computes
`loop_entry_values` (licm hoist legality) and the `Builder`-internal working map
that `build_scoped` -> `Engine::scoped_const_reads` -> `store_load_forward` reads.
Folding the build into `lower` (born-at-`lower`) for the criterion-2 CPU win is a
separate later perf item, not a deletion prerequisite.

## Retrospective (how the side-table was retired)

No single "delete the map" leap; each step was validated, full record in this
branch's git history:

- Operand promotion + build-once became the default (`rebuilds = 0`); the value
  passes share one persisted per-function graph.
- BCE moved entirely off `value_of` to **structural matching** (skeleton + value-pool
  reads, a position-aware no-write-between scan, struct-literal projection for an
  invariant `arr.used == N` bound over a `let`-immutable receiver). The old
  value-graph BCE path (~626 lines) was ablation-proven redundant and deleted.
- The value consumers were neutered one at a time — `Engine::value()`'s side-table
  read, then the inline seed read — each leaving the full e2e at **0 / 3030**. The
  decisive measurement: the optimizer's test-pinned behaviour does not depend on the
  side-table; born-as-operands already covers every pinned golden.
- Inline stopped clearing the graph for graph-preserving splices (every inlined call
  pure per `alias::pure_calls` and loop-free), keeping `loop_entry_values` valid; the
  `WADO_VERIFY_VG` oracle caught — and forced the purity half of — that gate before it
  could miscompile.
- The field and its whole maintenance subsystem were physically removed (~2174 lines):
  the `ValueGraphBuild` fields, the engine edit-maintenance machinery, `value_union` /
  the verify oracle, and ~48 value-numbering unit tests.

Dead ends (measured, reverted — do not retry as-is): promoting induction-var `Local`
reads to source-bearing opaques traps `closure_for_loop_mutation`; a query-time
entry-`FieldAccess` materialiser miscompiled ~165 fixtures (reference/aggregate fields
change copy/alias semantics); keeping caller `value_of` across a loop-free-but-impure
inline over-merges two reads of a `&mut` param. Throughline: a value's identity is
sound only when carried by an operand the edits maintain, never re-derived from a
side-table at query time.

## Goal

Make the NIR `optimize` phase **2× faster**. On the package-gale baseline (this
host, dev build) the phase is ~15s; the target is ~7.5s, comfortably under 10s.
Absolute time varies by host; the 2× ratio does not.

This is not a cache-tuning goal. The current optimizer already parks and carries
a _derived_ ValueGraph between passes (`vg_cache` / `carry_vg_cache`); that path
is measured and capped — it shaves a few percent and leaves the graph rebuilt
2.67× per function. It is removed by this WEP, not extended.

## Acceptance criteria

The work is done only when all three hold. None is satisfiable by a band-aid;
each is a static or measured fact, not a "byte-identical and X% faster" proxy.

- [x] **One build per function.** Build-once is now structural: nothing clears
      `Body::value_graph` back to `None`, so it is built lazily on first query and
      reused across every pass. Baseline was `builds=4474`, `first_builds=1678`,
      `rebuilds=2796` — 2.67 builds per function; the `WADO_MEASURE_VG` meter that
      drove this to `rebuilds = 0` (zlib -O2: `builds=139, rebuilds=0`) has since
      been removed — the invariant no longer needs a runtime check, only the
      absence of a clear-and-rebuild path. See the P3 milestone below.
- [ ] **`optimize` CPU halved** on package-gale (~15s → ~7.5s). Measured: still
      ~15.7s — **not met, not pursued** (the bottleneck is the passes, not the
      value-graph build; see Status). Won't-do.
- [x] **The cache is deleted.** Cache half: `vg_cache`, `carry_vg_cache`,
      `CachedAnalysis`, `run_gated_cached` at zero references (the graph lives on
      `Body::values`). Side-table half: the `value_of` `ExprId → ValueId` map is
      **deleted** — every consumer reads `Operand::Value` / the pool, never the
      side-table (see Status above).

The three are mutually reinforcing: deleting the cache (3) requires removing the
re-derivation it caches, which is the build-once change (1), which is what
produces the speedup (2). Any one unmet means the goal is unmet.

Merge gate (overrides incidental proxies): the full suite is green — `mise run
test` (every crate, all e2e fixtures at O0/O2) **and** `mise run test-wado`, with
no pre-existing failure left standing. The operand-promotion migration is part of
this: a promoted `Operand::Value` (e.g. a constant struct receiver of a
`FieldAccess`) must never hit an `as_expr().expect("skeleton operand")` in any
pass. `gale_cli` (kiln-generated code, which exercises promoted shapes the
mainline fixtures do not) is part of the gate. Every `expect("skeleton operand")`
site has been migrated to handle a promoted operand — none remains in the
compiler source.

## Context

A sampling profile (samply, 1 kHz, dev/debug `wado`) of
`wado compile -O2 package-gale/src/main.wado` pins the cost. Percentages are
inclusive shares of total CPU — machine-independent ratios; absolute time varies
by host. `optimize` is **64.83%** of compile CPU. Inside it the dominant cost is
per-function analysis rebuilt per pass and discarded:

- per-function `ValueGraph` build (`builder::build`: the `walk_block` /
  `walk_expr` flow walk, the joins, the hash-cons) — **20.76%**
- allocation churn — `__rust_alloc` **13.94%** + `__rust_dealloc` **13.35%** —
  the maps and vectors each per-pass rebuild creates and drops
- per-session engine setup (`Engine::new`: parent maps, use index, post-order
  seed) — **8.48%**
- `IndexMap::swap_remove_full` — **7.73%** — worklist / use-index churn

By pass wall-time, `licm` (~3.7s), `cse` (~2.5s), and `const_fold` (~0.8s) — the
three value-graph passes — are together **~46% of the phase**.

This cost exists for one structural reason: the ValueGraph is a _derived_
analysis of a _mutable_ SkelTree, and so is the engine's analysis (parent map,
use index, post-order). Every SkelTree edit can stale them, so every pass that
needs pure-value identity re-derives the graph, and every pass re-derives the
engine. The dirty-set gate and the revision-keyed `vg_cache` amortise this only
for functions a pass did not change; a function a pass actually rewrites pays a
fresh re-derivation in the next pass that visits it. The result is 2.67 graph
builds per function and a comparable engine churn — and the ~27% allocation cost
that rebuilding those structures generates.

## Why source-of-truth, not incremental rebuild or a richer cache

Two cheaper-looking patches were considered and rejected.

Incremental rebuild — re-derive only the changed region of the side-table — was
prototyped, verified equivalent under a `WADO_VERIFY_INCREMENTAL` harness, and
reverted: it fired on ~0.15% of builds on the large workload and 0% on the small
ones, and it is the wrong shape — it makes the _re-derivation of a derived
side-table_ cheaper, when the side-table only needs re-deriving because it is
derived.

A richer cache — park the graph and reuse it whenever the body is unchanged —
caps low for the same reason: it still rebuilds whenever a pass changes the
function, which is exactly the common case, and it cannot reach 2×. It is the
current `vg_cache` / `carry_vg_cache` path, and this WEP deletes it.

So this WEP removes the reason re-derivation exists: the graph stops being a
shadow of the SkelTree and becomes where pure values _live_. Flow is resolved
into the graph once, at build, and frozen there; a rewrite is a union of two
e-classes, which every user sees through `find()` without any re-walk. There is
no derived form to bring current, and nothing to cache.

## Decision

Promote the ValueGraph to the pure-value IR and drive it aegraph-style; apply
the same build-once discipline to the engine's per-function analysis.

- Pure operand positions in the SkelTree (literals, `Binary`, `Unary`, `Cast`)
  carry a `ValueId`. `lower::translate` builds the graph directly; the
  `value_of: ExprId → ValueId` side-table and its per-pass rebuild are gone.
- The graph is hash-consed with a union-find over e-classes. Flow is frozen at
  build via `Select` (merges), `LoopPhi` (loop recurrence), `Opaque` (params,
  unknowns), and per-field `HeapVersion` reads. A pure-value rewrite unions two
  classes; users resolve through `find()`. No re-derivation.
- Rules apply eagerly and once per match, not searched to saturation. Priorities
  are rule order, as `peephole.rs` encodes today. Congruence is maintained by a
  deferred e-graph rebuild after a batch of unions (egg/aegraph-style), not by
  re-walking the SkelTree.
- The engine's parent map, use index, and post-order are built once per function
  and maintained through the edit API, not rebuilt per pass. `Engine::new`'s
  per-pass cost retires the same way the graph's does.
- The SkelTree stays the effect-and-control schedule: statement order, control
  flow, and effectful / allocation-bearing expressions (`Call`, `Assign` to
  heap, `StructLiteral` / `TupleLiteral` / `ArrayLiteral`). It is rewritten by
  skeleton rules on the existing worklist engine.
- One extraction pass, before WIR build, walks the SkelTree and lowers each
  pure `ValueId` operand to a concrete form, materialising a shared value once
  only when sharing beats duplication.

## Architecture

### Operand promotion

`lower::translate` emits a `ValueId` for every pure operand position instead of
a pure `ExprKind`. The pure literal / `Binary` / `Unary` / `Cast` variants leave
the SkelTree; the slots that referenced them hold `ValueId`s. Effectful and
control-flow `ExprKind`s keep their skeleton form, with their pure operands
promoted. WIR build no longer matches the pure `ExprKind` arms — it consumes the
extractor's output. This is a wide but mechanical change across the arena,
lowering, WIR build, the unparser, and the passes that match pure `ExprKind`s;
it is load-bearing, so its breadth is accepted rather than worked around.

### The graph as IR

The existing builder already resolves a function into the frozen-flow form this
needs: it threads `current_value`, the heap versions, and reference targets in
one linear walk, and constructs `Select` at merges and `LoopPhi` at loops. The
redesign runs that build once per function — at lower — and then maintains it.
Two additions make it an IR rather than a side-table:

- A union-find over `ValueId`s, so a rewrite that proves `a ≡ b` unions their
  classes and every user resolves the representative through `find()`.
- Deferred congruence rebuild: after a batch of unions, re-canonicalise the
  hash-cons so structurally-equal parents re-merge (a node whose child's
  representative changed may now equal another node). This is bounded work over
  the touched classes, not a SkelTree re-walk.

### Optimizations that become graph properties

Once pure values live in the graph, a cluster of today's passes is subsumed and
their per-pass walks disappear:

- CSE / GVN — identical pure values share a `ValueId` by hash-consing. No pass.
- Copy propagation of pure copies — `let x = y` makes `x` resolve to `y`'s
  `ValueId`. No pass.
- Constant folding (env-free and flow-sensitive) — folding rewrites a node to a
  literal and unions; the flow-sensitive folds read the frozen flow already in
  the graph. niri's CTFE stays the evaluator.
- Store-load forwarding — a read at a `(receiver, field, heap_ver)` already
  shared with a prior write resolves to the stored value by construction.
- Loop-invariant detection — a value is loop-invariant iff its class does not
  transitively depend on that loop's `LoopPhi`; a structural query, not a
  dataflow walk. Hoisting becomes an extraction decision (materialise the
  invariant value in the pre-header).
- Condition implication — a condition whose `ValueId` equals a value a
  dominating guard proved is folded; the guard fact keys on a `ValueId`, so it
  goes stale by construction when the operand's class changes.

### What stays a skeleton rule

Structural and effectful rewrites do not reduce to a graph union and stay rules
on the worklist engine: block flattening / dead-statement pruning, `match_to_switch`,
`labeled_block_fusion`, `ref_elim`, `elide_box_local`, `value_copy_*`, `sroa`,
`container_sroa`, `field_scalarize`, `inline`, `dae`, `drve`,
`const_object_globalization`, `dce`. A skeleton rewrite that changes control
flow keeps the graph coherent pointwise: pruning a branch unions the dead
`Select` arm's class into the surviving arm; splicing an inlined body or an SROA
split builds value nodes for the _new_ skeleton subtree (monotone graph growth
at the splice point, not a re-derivation of existing flow).

This pointwise maintenance through structural edits is the load-bearing claim of
the whole design and its chief risk: `inline` and `sroa` must grow the graph at
the splice/decomposition point without re-walking the untouched remainder. The
roadmap measures this before the wide operand-promotion change is committed (see
Roadmap, de-risk step).

### Extraction

Before WIR build, one pass walks the SkelTree and lowers each pure `ValueId`
operand to a concrete skeleton/WIR form, choosing per multi-use value whether to
re-compute it at each use or materialise it once into a hoisted temp. This is the
one genuinely new analysis and the main code-quality risk: the cost model must
not emit worse code than today's CSE / hoisting heuristics. The migration
de-risks it by reproducing the current materialisation first (extract each value
at the sites and shapes the old passes produced), then improving the cost model
behind benchmarks.

## Soundness invariants

- No output regression per step (below): each step preserves or improves the
  result; incidental differences that neither shrink nor grow the result
  meaningfully are acceptable, a regression is not.
- Union soundness: two `ValueId`s are unioned only when they denote the same
  value in every execution reaching that point — the same obligation today's
  CSE / copy-prop / forwarding rules already discharge, now expressed once.
- Flow-freeze validity: a read's `ValueId` is fixed at build to the value
  dominant at that point; a control-flow rewrite that changes which value is
  dominant must union or rebuild the affected classes, never leave a read
  pointing at a value that no longer reaches it. Guarded by the verify harness.
- Heap-version monotonicity is unchanged.
- Extraction equivalence: the extracted skeleton computes, for every effectful
  position, the same values in the same effect order as the pre-extraction
  graph + skeleton.
- Gating still changes only which functions a step visits, never the result.

## Consequences

### Expected effect

- The per-pass re-derivation (build 20.76%, engine setup 8.48%, and the bulk of
  the ~27% allocation churn they generate) collapses to one build per function,
  maintained in place. The three value-graph passes (~46% of the phase by
  wall-time) stop carrying their own build.
- A cluster of dataflow passes (CSE, copy-prop, the bulk of const-fold,
  store-load-forward, loop-invariant hoisting) stops being passes and becomes
  graph structure, removing both their walks and their bespoke analysis caches.
- The cache machinery and the `value_of` side-table are deleted outright.

### Risks

- Pointwise maintenance through structural edits (`inline`, `sroa`) is the
  central correctness-and-cost surface: graph growth at the splice point must not
  degrade into a re-walk, or acceptance criterion 1 (`rebuilds = 0`) and the 2×
  both slip. De-risked: the measured maintenance work is 13.5% of the rebuild it
  replaces (see Roadmap), so the budget is wide — the risk is correctness of the
  splice-point growth, not its cost.
- Operand-promotion breadth: a wide change across arena, lowering, WIR build, the
  unparser, and pure-`ExprKind`-matching passes. Mechanical but large.
- Extraction cost model: a weak model regresses code size or runtime. Mitigated
  by reproducing current materialisation first, then tuning.
- Congruence maintenance: deferred e-graph rebuild after unions has its own cost;
  it must stay below the re-derivation it replaces. Measured per step.

### Trade-offs accepted

- The graph gains a union-find and a congruence rebuild; the optimizer gains an
  extractor. In exchange every pure-value pass's per-pass rebuild, the engine's
  per-pass setup, the cache machinery, and the `value_of` side-table are deleted.
- Arena compaction (dead skeleton nodes from in-place rewrites) becomes more
  worthwhile once bodies are walked fewer times; tracked as the existing
  follow-up.

## Execution history

The multi-session working log — the promote-early keystone, the build-once persist
milestone, the per-pass migration order, the i128 miscompile root-cause, the
born-frozen constant-promotion variants, and the structural-BCE bring-up — is
superseded by the achieved end state (Status / Retrospective above) and removed for
concision. The full record is in this branch's git history; the durable design it
converged on is captured in Architecture / Soundness invariants / Decision above.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the direction; equality saturation stays deferred there.
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the engine substrate, edit API, and gate this builds on.
- [`docs/optimizer.md`](./optimizer.md) — the pass inventory the graph absorbs.
- Cranelift's aegraph mid-end and `egg` (https://egraphs-good.github.io/) — the build-once, eager-rewrite, single-extraction model this adapts.
