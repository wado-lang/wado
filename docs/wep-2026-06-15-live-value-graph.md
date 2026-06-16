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

- [ ] **One build per function.** `WADO_MEASURE_VG` reports `rebuilds = 0` (only
      `first_builds`). Baseline today: `builds=4474`, `first_builds=1678`,
      `rebuilds=2796` — 2.67 builds per function.
- [ ] **`optimize` CPU halved** on package-gale (~15s → ~7.5s), measured by the
      sampling profile and wall time.
- [ ] **The cache is deleted.** `vg_cache`, `carry_vg_cache`, `CachedAnalysis`,
      and `run_gated_cached` reach zero references; the `value_of`
      `ExprId → ValueId` side-table retires. If any survives, the graph is still
      derived and the redesign is unfinished.

The three are mutually reinforcing: deleting the cache (3) requires removing the
re-derivation it caches, which is the build-once change (1), which is what
produces the speedup (2). Any one unmet means the goal is unmet.

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

## Current state and what it leaves to do

The primitives the build-once needs already exist, landed earlier on this branch:

- Union-find + deferred congruence rebuild on the `ValuePool` — equivalence by
  `find()`; constant kinds win the representative so a class containing a literal
  resolves to it. Exposed on the engine (`value_find` / `value_union` /
  `rebuild_value_congruence`).
- The extraction keystone — `extract::materialize_literal`, the graph→skeleton
  primitive that resolves through the representative. `store_load_forward` routes
  through it.

What is _not_ done, and is the actual subject of this WEP: build-once. The graph
is still derived and rebuilt 2.67× per function. The interim wins on this branch
— sharing one parked build across `cse`→`licm` and re-tagging it through
`const_fold` (`carry_vg_cache`) — are the cache path this WEP deletes, not
progress toward the goal. They are removed when operand promotion lands.

## Roadmap

Each step must not regress output (code size or runtime) on the full fixture +
E2E suite, on `wir_expect` / `wir_not_expect`, and on the benchmark set. Progress
is tracked against the three acceptance criteria, not against incidental speedups.

- [x] **De-risk maintenance cost — green light.** `WADO_MEASURE_VG` now counts,
      across the whole optimize, the genuinely-new skeleton nodes the edit API
      creates and the in-place structural edits it makes — the exact work
      pointwise maintenance does, with no re-walk of the untouched remainder. On
      package-gale: **33,672 new nodes + 94,808 in-place edits = 128,480 edits =
      13.5% of the 948,401 rebuild node-walks.** The current per-pass rebuild
      does ~7.4× more work than maintenance would, and under operand promotion
      those rebuilds leave the optimize phase entirely (the graph is built once
      at lower). This refutes the earlier "71.8% irreducible" figure, which was
      the incremental-_rebuild_ model (re-walk from the first divergent statement
      to the end) — pointwise maintenance never re-walks, so its cost tracks the
      edit count, not the disturbed suffix. `rebuilds = 0` is reachable far below
      the rebuild it replaces; the wide change is justified.
- [x] **Phase A — representation.** The arena's operand slots carry
      `Operand { Value(ValueId), Expr(ExprId) }`; `lower` emits `Operand`; every
      consumer (engine, builder, passes, WIR build, niri, unparser) handles it;
      the extraction seams (`translate_operand` / `splice_operand` /
      `walk_operand`) are in place. All operands are `Operand::Expr`, so behavior
      is unchanged — the full suite (769 lib + 2958 e2e) is green. This is the
      safe scaffold; it meets no acceptance criterion yet.
- [~] **Phase B — make values live.** In progress. Landed: `ValuePool` records a
  per-value source type (`set_type` / `type_of`), since `ValueKind` is
  type-erased and extraction needs the width; a constant-value extractor
  (`extract_value`) that materialises `Int`/`Float`/`Bool`/`Char`/`String`/
  `Null`/`Unit` from the pool; `Body::map_operands` for in-place operand
  rewrite. Two design findings shape the rest:
  - **Single pool.** Promotion writes values into `Body::values`, but the per-pass
    `builder::build` makes a _fresh_ pool, so a promoted `Operand::Value` it never
    interned is unresolvable. Phase B must unify on `Body::values` as the one
    owned pool — built once, the builder retired or repurposed to populate it,
    the value-graph passes reading it. Promotion cannot be a standalone
    byte-identical step before this.
  - **Scheduling extraction.** Materialising a _non-constant_ value
    (`Binary(Opaque, 1)`) needs the skeleton computation behind the `Opaque`
    operand (a `Local` read, a `Call` result). The graph alone cannot re-emit it,
    so the effectful sub-results must stay scheduled in the skeleton and the
    extractor reads them — the WEP's "extraction is the main regression risk."
    Constants extract without this; non-constants need the scheduler.
- [ ] **Maintain the graph through structural passes.** `inline` / `sroa` /
      `dae` / `drve` grow or union the live graph through their edits; no pass
      triggers a rebuild. Drives `rebuilds` toward 0.
- [ ] **Maintain the engine analysis.** Parent map, use index, and post-order are
      built once and updated through the edit API; `Engine::new`'s per-pass cost
      retires.
- [ ] **Delete the cache.** Remove `vg_cache`, `carry_vg_cache`,
      `CachedAnalysis`, and `run_gated_cached`. Acceptance criterion 3.
- [ ] **Subsume the dataflow passes into graph structure.** CSE, copy-prop, and
      store-load-forward become hash-cons / `find()` results; delete the passes.
      Then flow-sensitive `const_fold` and `condition_implication` as graph
      rewrites (const_fold keeps niri for pure CTFE calls). Then loop-invariant
      hoisting as the extractor's placement decision; retire `licm`'s
      pure-arithmetic hoisting.
- [ ] **Cost-based extraction.** Replace straight extraction with a share-vs-
      duplicate cost model; tune against benchmarks.
- [ ] **Retire the intra-procedural iteration count.** The graph and worklist
      self-converge; keep an outer round count only for the interprocedural cycle.

## Operand-promotion migration order

The representation change is wide and lands red across the pipeline. The order
below keeps each layer's intent clear so the work is resumable mid-change. The
verify harness (`partitions_agree`) and `maintain_pure_value` are in place.

- [x] **IR foundation.** `Operand { Value(ValueId), Expr(ExprId) }`;
      `Body::values: ValuePool` (function-owned graph). Compiles; nothing
      consumes them yet.
- [ ] **Arena.** Remove the pure `ExprKind` variants (`IntLiteral`, `FloatLiteral`,
      `BoolLiteral`, `CharLiteral`, `StringLiteral`, `Null`, `Unit`, `Binary`,
      `Cast`, and the pure `Unary` ops `Neg` / `Not` / `BitNot` — `Ref` / `MutRef`
      / `Deref` stay). Operand-bearing fields (`Assign.value`, `Let.value`,
      `Return.value`, call args, receivers, conditions, scrutinees, literal
      elements, …) become `Operand`. Update `for_each_child` / `clone_*` to recurse
      only into `Operand::Expr`. A skeleton placeholder replaces `ExprKind::Unit`'s
      dead-node role (`become_expr`).
- [ ] **lower::translate.** Build the graph while lowering: a pure expression
      interns into `Body::values` and yields `Operand::Value`; an effectful one
      stays an `ExprId`. Thread `current_value` / heap versions as the old
      `builder` did — this merges the value-graph build into lowering. `value_of`
      and `nir_value_graph::builder` retire (the builder's flow walk moves here).
- [ ] **Engine.** `value()` resolves an `Operand`; the edit API maintains
      `Body::values` in place (`maintain_pure_value` for new pure nodes, `set_value`
      for flow-sensitive ones); drop the lazy `value_graph` cache and the
      `CachedAnalysis` / `vg_cache` / `run_gated_cached` plumbing.
- [ ] **Passes.** Each pass matching a pure `ExprKind` switches to `Operand` /
      `Body::values` queries; structural passes maintain the graph at splice
      points. cse / copy-prop / slf collapse into graph identity.
- [ ] **WIR build + unparser + niri + type-repr.** Consume `Operand`: a `Value`
      extracts from the graph (the extractor), an `Expr` lowers the subtree.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the direction; equality saturation stays deferred there.
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the engine substrate, edit API, and gate this builds on.
- [`docs/optimizer.md`](./optimizer.md) — the pass inventory the graph absorbs.
- Cranelift's aegraph mid-end and `egg` (https://egraphs-good.github.io/) — the build-once, eager-rewrite, single-extraction model this adapts.
