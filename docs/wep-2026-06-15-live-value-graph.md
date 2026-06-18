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
- [x] **Phase B.1 — extraction proven (constants), end-to-end.** `ValuePool`
      records a per-value source type (`set_type` / `type_of`) and allocates each
      promoted literal un-shared (`alloc_unshared`) so a type-erased `7: i32` and
      `7: i64` keep distinct widths. `extract_value` materialises constant value
      kinds back to WIR; the builder seeds its pool from `Body::values` so a
      promoted id resolves. The whole WIR-build dispatch now takes `Operand`
      (`translate_operand` / `operand_type_id`; index / cast / match / switch /
      tuple+array literals / variant-construct payload / indirect-call args /
      SIMD lane / canonical-ABI). `promote_literals` is wired as the last optimize
      step (promote-late). Result: package-gale -O2 is **byte-identical**
      (740267 bytes) and the **full e2e suite (2982 fixtures) is green** — the
      WEP's flagged "main regression risk" (extraction) is de-risked on a real
      workload. This is necessary groundwork; promote-late runs after every pass,
      so it moves no acceptance criterion yet (`rebuilds` unchanged at 2796).
- [ ] **Phase B.2 — promote early + migrate the literal-matching passes.** Move
      promotion ahead of the value passes so they read `Operand::Value`. The ~10
      passes that structurally match operand literals (`const_fold`, `peephole`,
      `const_object_globalization`, `container_sroa`, `copy_prop`,
      `condition_implication`, `array_literal`, `elide_box_local`, …) must read a
      promoted constant from the pool instead, or they regress (miss folds). This
      is the gating migration for build-once.
- [ ] **Phase B.3 — scheduling extraction for non-constant values.** Materialising
      a `Binary(Opaque, 1)` needs the skeleton computation behind the `Opaque`
      operand (a `Local` read, a `Call` result). The graph alone cannot re-emit
      it, so the effectful sub-results stay scheduled in the skeleton and the
      extractor reads them. Constants extract without this; non-constants need the
      scheduler. Required to fully retire `value_of`.
- [x] **Cache deleted (criterion 3, cache half) — graph relocated to the body.**
      `build` now writes into `Body::values` (the one persistent pool; ids stay
      stable across builds), and `ValueGraphBuild` drops its own pool. The
      revision-keyed cross-pass cache and all its machinery are gone:
      `vg_cache`, `carry_vg_cache`, `CachedAnalysis`, `run_gated_cached`,
      `Engine::with_analysis` / `into_analysis` reach zero references. cse / licm /
      store_load_forward build their session fresh via `run_gated`. Verified e2e
      (2982 fixtures green); package-gale byte-identical. The cache was masking
      rebuilds: `WADO_MEASURE_VG` rebuilds 2796 → 5988 (cse and licm no longer
      reuse one build across const_fold). That increase is intentional and honest.
      `value_of` retirement (the side-table half of criterion 3) still needs full
      promotion (Phase B.2/B.3).

      Architectural finding that fixes the next step: cross-session graph reuse
      (cse → licm sharing one build) requires storing the build-config (alias
      sets, param seeding) *with* the graph — structurally the just-deleted
      `CachedAnalysis`. So a body-owned, config-keyed reuse is the cache by
      another name and is rejected. The only no-cache route to `rebuilds = 0` is
      maintenance: build once and keep current through every edit, so there is no
      config to check and nothing to cache. The two maintenance items below are
      therefore the whole of criteria 1 and 2.
- [ ] **Maintain the graph through structural passes.** `inline` / `sroa` /
      `dae` / `drve` grow or union the live graph through their edits; no pass
      triggers a rebuild. Drives `rebuilds` toward 0.

      Finding (a `WADO_VERIFY_VG` harness — `partitions_agree` against a fresh
      build — earned this): a build-once attempt that let `licm` reuse `cse`'s
      graph across `const_fold` was prototyped and **reverted as unsound**.
      `const_fold` rewrites flow-sensitive local values (constant propagation), so
      it is **not graph-preserving** — `cse`'s graph is stale for `licm`. It was
      byte-identical on the full suite (the staleness happened to be conservative),
      but not rigorously sound, and a compiler ships only proven-sound passes. The
      retired `carry_vg_cache` relied on the same false assumption. So sound
      build-once requires **precise per-edit maintenance** — every value-changing
      pass (`const_fold` included) updates the graph as it edits, not just the
      structural ones — verified by the harness.

      Landed and validated (the maintenance primitive + the right oracle):
      `Engine::replace_expr_kind` now calls `maintain_value_after_edit`, which
      re-derives the edited node's pure value (`maintain_pure_value`) and
      propagates up its ancestor chain, dropping any entry it cannot re-derive
      (flow-sensitive `Local` / `FieldAccess`) rather than leaving it stale. The
      `WADO_VERIFY_VG` harness is wired live: on every graph query it rebuilds a
      fresh graph and checks the maintained one against it. The check is
      `partition_refines`, not strict `partitions_agree`: maintenance is sound by
      **refinement** (it may merge a pair only if a fresh build also merges it),
      not equality — dropping a flow value it cannot re-derive makes the graph
      *coarser*, a missed optimization, never a wrong merge. Empirically, across
      the full e2e fixture corpus at `-O2`, the maintained graph **never
      over-merges** (`partition_refines` clean; the strict `partitions_agree`
      flagged only the expected conservative-coarsening direction). The wiring is
      **byte-identical** on package-gale and **within timing noise** (~24s
      total), so it is sound, free groundwork — but it does not yet move
      `rebuilds` (still 5988): it only keeps the existing per-session graph
      current through `cse`'s edits, which were already value-preserving.

      Consequence that pins the next step: in the *side-table* model a coarser
      maintained graph is sound to consume but, reused across sessions, would
      **regress code quality** (the missed merges `cse` / `licm` would have
      found). Non-regressing reuse therefore needs *precise* flow-value
      preservation, and re-deriving flow values on each edit is the rejected
      incremental rebuild. The only model in which a pure-edit preserves flow
      values for free is operand promotion: flow is frozen into `ValueId`s in the
      operand slots at build, and a pure rewrite is a union that leaves them
      intact. So `rebuilds = 0` runs through Phase B.2/B.3, not through more
      side-table maintenance. `partitions_agree` (strict) stays the oracle for
      that precise design; `partition_refines` is the oracle for the conservative
      maintenance that exists today.

      Second concrete blocker, found while scaffolding env-gated reuse and then
      reverted: even setting code quality aside, side-table reuse does not reduce
      `rebuilds`. The two builds per function per iteration are `cse` and `licm`;
      `cse` is graph-preserving so `licm` could reuse its maintained graph — but
      `licm::hoist_invariant_arith` calls `invalidate_value_graph()` mid-pass to
      force a rebuild after each hoist round, because hoisting reorders the loop
      and staled its `loop_entry_values` snapshot. `loop_entry_values` is loop
      *recurrence* state the pure-operand maintenance does not update, and
      `partition_refines` does not cover it (it checks only the `value_of`
      partition), so reusing across a hoist would be both rebuild-defeating and
      unguarded. With `licm` still rebuilding, reuse saves nothing. The fix is
      the same operand-promotion model: loop recurrence frozen as a `LoopPhi`
      `ValueId` that a hoist references rather than re-derives.
- [ ] **Maintain the engine analysis.** Parent map, use index, and post-order are
      built once and updated through the edit API; `Engine::new`'s per-pass cost
      retires.
- [x] **Delete the cache.** Done above — all cache symbols at zero references.
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

### Decisive pivot: criterion 3 forces operand promotion; side-table persistence is a dead end

A build-once-via-persisted-side-table sub-path was started and instrumented this
session: the value graph (`ValueGraphBuild`) moved onto `Body` (commit
`685260a16`, behavior-neutral) and maintenance was hardened to drop a reassigned
local's downstream readers (commit `36fac4552`), which fixed the verified
`count_prime` over-merge (`Local m` ≡ a `Shl`). But running `WADO_VERIFY_VG`
across the benchmark corpus surfaced a **second, distinct** over-merge on zlib:

```
expr1713 = Local{index:333 "__v1"}  ≡  expr3564 = FieldAccess{.used}
maintained: both -> ValueId(935692)   fresh: 1840139 vs 1813397
```

A local that forwarded from `obj.used` (`let __v1 = obj.used`) and a later
`obj.used` read share a stale value after the heap changed. Maintaining this
_precisely_ means re-deriving per-`(receiver, field)` `HeapVersion`s on every
heap-affecting edit — i.e. re-implementing the builder's heap-version walk
incrementally. Each over-merge mechanism (reassigned-local readers, then
FieldAccess/heap, then `Select`/`LoopPhi` next) needs its own precise
maintenance; coarsening them instead regresses code quality, which fails 完勝
(no-compromise). So precise side-table maintenance is as hard as the builder and
still does not retire `value_of`.

Decisive point: **acceptance criterion 3 requires the `value_of` side-table to
retire**, which a _persisted side-table_ can never do by construction. Only
operand promotion — pure values frozen into `Operand::Value` slots, with a
`FieldAccess` frozen at its heap version — retires `value_of`, and it
**structurally eliminates both over-merge mechanisms**: a frozen operand value
cannot go stale (old reads keep their old `ValueId`; new reads get new ones; a
rewrite is a `union`, never a re-derivation). So the unified path to all three
criteria is to **complete operand promotion**, not to persist and maintain the
side-table. `36fac4552` (the over-merge fix) stands as a correctness fix on its
own; `685260a16` (graph on `Body`) is harmless and the pool already lives there.
The remaining work pivots to the promotion migration below.

### Extraction proven; the rebuild win is all-or-nothing

A late, in-pipeline freeze pass (`optimize/extract.rs::freeze_pure_arith`,
run after every other pass) was built to exercise the extractor end-to-end on
real programs. It redirects a pure-arith node (`Binary` / pure `Unary`) to its
`Operand::Value` when the value is re-emittable, and WIR build materialises it
via `extract_value`. It is full-e2e green (7397/0). What it established:

- The extractor works on real arithmetic: `extract_value` lowers
  `Binary` / `Unary` recursively (shared with the skeleton path via
  `emit_binary_wir` / `emit_unary_wir`) and re-emits an `Opaque` leaf as
  `local.get idx` (`OpaqueSource::Local`) or a scheduled skeleton expr
  (`OpaqueSource::Expr`).
- Soundness constraints the extractor needs, each now enforced and tested:
  - Only single-assignment locals freeze (`mut` / loop locals reject — a
    `local.get` at the use site must read the opaque's version).
  - `ValueKind` is type-erased and hash-consed, so a value shared between two
    differently-typed uses (`a+b` as `i32` and `i64`; `0.0` as `f32`/`f64`)
    can carry only one width — freeze skips on a width conflict; `Cast` is
    excluded (operand source type unrecoverable from the value tree).
  - The WEP's "main code-quality risk" is real: freezing a multi-use value
    duplicates its computation (`t*t` extracted twice loses `local.tee`). The
    freeze skips values whose extraction duplicates `Binary` / `Unary` work —
    a conservative stand-in for the share-vs-duplicate cost model.
  - The verify oracle compares only _live_ exprs (an orphaned node a freeze
    redirected away is never emitted and cannot miscompile).

The decisive finding: this freeze does **not** move `rebuilds` (measured 119,
unchanged). `rebuilds` is **all-or-nothing** — the per-pass `builder::build`
walks the body to derive every value not already in an operand slot, so it
runs in full until _every_ value (locals, calls, `FieldAccess`, `Select`,
`LoopPhi`) is frozen. Partial freezing, late or early, leaves the build intact.
A persisted derived `value_of` is not a path either: cross-pass reuse needs
precise per-edit maintenance, which for flow values is exactly operand
promotion (the side-table-reuse alternative coarsens — a quality regression —
and `licm::hoist_invariant_arith` still `invalidate_value_graph()`s because
`loop_entry_values` recurrence is unmaintained). So criteria 1/2 sit behind one
atomic change with no intermediate measurable win:

1. Relocate the builder's `current_value` flow-walk into `lower::translate`;
   build the graph once while lowering.
2. Freeze every read into its operand slot: single-assignment `Local` →
   `Opaque(Local)`; call result → scheduled skeleton `let` + `Opaque(Expr)`;
   merges → `Select`; loop recurrence → `LoopPhi`; pure `FieldAccess` at a
   heap version. Extend `extract_value` per kind (the arith/opaque half is
   proven above).
3. Migrate every value- and skeleton-walking pass to consume `Operand` /
   `Body::values`; structural passes (`inline` / `sroa`) grow the graph at
   splice points. Retire `value_of` and per-pass `builder::build`.

Done this round (prerequisites): scalars / `Null` / `String` / `Unit` promoted;
`ExprKind::Dead` tombstone split; the extraction machinery + `OpaqueSource` +
`value_fully_reemittable_locally` + the freeze, all full-e2e green.

### Per-pass attribution, and an incremental route under fix-while-advancing

Fresh `WADO_MEASURE_VG` on `benchmark/zlib/zlib_bench.wado -O2` (a stable
mid-size workload; absolute counts differ from package-gale but the structure is
invariant):

```
builds=1077 (first_builds=216, rebuilds=861)
  by pass cse:  builds=546 (first=216, rebuilds=330)
  by pass licm: builds=531 (first=0,   rebuilds=531)
total build node-walks = 573503
maintenance bound: new nodes 7389 + in-place edits 24012 = 31401 (5.5% of 573503)
```

So the graph is rebuilt once per _(pass × function × fixpoint-iteration)_: every
pass opens a fresh `Engine` per function and builds lazily on first query, and
the outer optimize fixpoint re-enters ~2.5×. `first_builds` (216) is one per
function; the 861 rebuilds are the redundant re-derivations. Pointwise
maintenance would touch ~5.5% of that node-walk cost — the ~18× prize behind
criterion 2.

The "all-or-nothing" claim above is precise only under a **zero code-quality
regression** bar. The maintenance primitives needed for an _incremental_ route
already exist and are proven: `Engine::maintain_value_after_edit` /
`maintain_pure_value` (re-derive a node's pure value, drop what they can't), and
the `partition_refines` oracle (`WADO_VERIFY_VG`) that certifies the maintained
graph never over-merges — coarsening (a dropped flow value) is sound, only a
missed optimization. Correction (Probe A, below): the route is _not_ a sequence of independently
shippable green steps. Step 2's coarsening alone **over-merges** — dropping the
edited node's ancestors leaves stale `value_of` on the **downstream readers** of
a reassigned local, which the next pure re-intern hash-conses wrongly (a
miscompile, masked today by the rebuild). So persistence (step 2) is sound only
once every flow value that a structural pass can stale is either coarsened
_with its downstream readers_ or **promoted into an operand slot** (a frozen
`Operand::Value` cannot stale). The blocker is soundness-coupling, not quality
regression — `WADO_VERIFY_VG` must be the development gate, and the persistence
flip lands atomically with the downstream-coarsening / promotion, not before it.
With that caveat, criterion 1 is still approached by these steps (green under the
verify harness), not one undifferentiated rewrite:

1. **Body-persistent graph.** Move `ValueGraphBuild` (`value_of` +
   `loop_entry_values`) onto `Body` next to `values: ValuePool`. Build once;
   never auto-drop. The build runs with a single **fixed conservative config**
   (alias/`mut_escaped` sets over-approximated up front), so there is no config
   to re-check on reuse — sidestepping the rejected "config-keyed reuse = cache."
   A coarser fixed config costs some forwarding precision (a recoverable
   regression), not soundness.
2. **Remove the rebuild triggers.** Drop the `value_graph = None` in
   `set_param_locals` / `set_alias_sets` / `set_value_graph_type_table` (config is
   fixed at build) and replace `invalidate_value_graph` at structural sites
   (`licm::hoist_invariant_arith`, `inline`, `sroa`) with **coarsening**: remove
   the edited subtree's `value_of` entries (and stale `loop_entry_values`) rather
   than re-deriving — `partition_refines` guards every step.
3. **Recover the regressions.** Tighten maintenance where the coarsening lost a
   real optimization (the field-forwarding `licm` relies on, the loop-entry
   snapshot a hoist staled), measuring byte size back toward parity. This is
   where loop recurrence eventually wants the `LoopPhi` operand model — but as an
   optimization-recovery step, not a soundness gate.

This is distinct from both rejected shortcuts: not a cache (no config/revision
check, no stored build to look up) and not an incremental _rebuild_ (no re-walk
of a disturbed suffix — maintenance is pointwise). It is the WEP's "live
maintenance to every pass," sequenced so each step is independently e2e-green and
moves `rebuilds` down monotonically.

#### Two empirical probes (run, measured, reverted) that scope the blockers

Step 1/2 above were probed with real code to find the precise obstacles before
committing the wide change. Both were reverted on correctness grounds; the
findings are the deliverable.

- **Probe A — drop `licm`'s `invalidate_value_graph`.** Byte-identical on zlib,
  correct on count_prime, full O0/O2 e2e green (2960/0) — but `WADO_VERIFY_VG`
  flagged a **new over-merge** in the licm session. Root cause:
  `maintain_value_after_edit` walks the edited node's _ancestors_ only, never the
  **downstream local readers** whose `value_of` a reassignment stales; the next
  pure re-intern then hash-conses two now-distinct trees to one `ValueId`. The
  invalidate was masking this by rebuilding. This is the same downstream-stale
  flaw the WEP names as the reason flow values must be _promoted into operand
  slots_ (a frozen `Operand::Value` cannot stale), not maintained as a side
  table. So step 2's coarsening must additionally **drop every downstream reader
  of a reassigned local**, or the value must be promoted — confirming operand
  promotion is the load-bearing piece, not optional.
- **Probe B — run the freeze (promotion) before the value passes.** Two failures:
  `licm` panics on a promoted operand (`licm.rs:559` assumes a skeleton `Expr` —
  a guard fix), and -O2 emits **invalid Wasm** (`i64`/`i32` type mismatch).
  Cause: `ValueKind` is type-erased and hash-consed, so after early promotion a
  later pass (`inline` / `const_fold`) creates a _new_ use of the shared value at
  a different width, and extraction picks the wrong one. The **late** freeze
  sidesteps this purely by timing (no pass runs after it to add a divergent-width
  use). So promoting early requires **per-use width preservation** —
  un-erasing the pool (carry width in `ValueKind`) or `alloc_unshared` per
  `(value, type)` — which is the precondition for moving promotion ahead of the
  passes.

  Partially fixed (commit `1f133e71a`): `ValueKind::Int` / `Float` now carry their
  source `TypeId` in the hash-cons key, so `7: i32` and `7: i64` are distinct
  values each recording its own width; `intern` stamps `type_of` at construction.
  All ~40 construction / match sites were converted atomically (the split-hash-cons
  hazard demands it), behavior-neutral (zlib -O2 byte-identical; 2960/0 e2e; a new
  width-distinctness unit test). The `licm.rs:559` guard (commit `6f158eba8`) lets
  `stmt_child_nodes` tolerate a promoted condition.

  Re-probed early promotion with both fixes in (freeze run once before the
  fixpoint loop): **still invalid Wasm** (`i64`/`i32` mismatch) on
  count_prime / mandelbrot / sieve / zlib. So the literal un-sharing was necessary
  but **not sufficient** — a second width path remains. `extract_value` derives a
  `Binary` / `Unary` / `Cast` operand's width from the single recorded
  `type_of(value)` (`translate.rs:1878-1903`), but `set_type` is last-write during
  the build, so a hash-consed value (a `Binary`, or an `Opaque` local read)
  reachable from two differently-typed uses keeps only one width — extraction then
  emits the wrong one. The late freeze dodges this by timing (nothing after it adds
  a divergent-width use). Real fix: width must be intrinsic to **every** value
  reached by extraction (carry the result type in `Binary` / `Unary` / `Cast` /
  `Opaque` hash-cons keys, or `alloc_unshared` per `(value, type)`), not a
  last-write side table. This is the precise next blocker for moving promotion
  ahead of the passes; the experiment was discarded (uncommitted, never landed).

  Root-caused (instrumented `extract_value`'s `Binary` arm, early-freeze build,
  count_prime): the wrong-width values are in the `u64` timing glue, and they are
  **internally inconsistent** — e.g. `ValueId(69) = Binary{Shr, Int(2,i32),
  Int(3,i32)}` with recorded `self_ty = u64`, and conversely a `Shr` with
  `self_ty = i32` whose `lhs = Int(1,u64)`. So a `u64`-typed binary expr carries
  `i32`-typed operand values (and vice versa). Two consequences:
  - Extraction reads the operand width from `type_of(lhs)`, not the binary's own
    `self_ty`, so it emits the operand width while the consuming context wants the
    result width → the `i64`/`i32` validation error.
  - `ValueKind::Binary` / `Unary` / `Cast` omit the result type from the hash-cons
    key, so a `u64` and an `i32` computation with structurally-equal operand ids
    collide onto one `ValueId` carrying a single (last-write) `self_ty`.

  The leaf fix (`1f133e71a`) was necessary but the composite kinds still erase
  width. The fix has two coupled parts, both real implementation: (1) carry the
  result `TypeId` in the `Binary` / `Unary` / `Cast` hash-cons key (intern records
  it, like `Int` / `Float`), so cross-width computations never share an id; (2)
  have `extract_value` derive each op's width from the value's own recorded type,
  and widen operands as needed. Only then is early promotion width-correct. The
  experiment + instrumentation were discarded (uncommitted); the branch stays
  green.

  Refinement (read `record_value_tree_types`, `extract.rs:92`): the freeze's
  width-conflict guard is actually **correct** — it recurses operands and returns
  `false` when an operand's recorded type differs from the stamped result type, so
  the inconsistent binaries above are **skipped, not promoted**. Therefore the
  early-promotion invalid Wasm is **not** a freeze-guard or hash-cons-key bug; it
  is the harder cross-pass class — a pass (`inline` / `const_fold` / `copy_prop`)
  substitutes a different-width value into a promoted operand slot _after_ the
  freeze, which the guard cannot see. That is fixable only by migrating those
  passes to respect promoted-operand types, confirming early promotion is coupled
  to the pass migration, not a bounded standalone fix.

  Composite-width landed (commit `adecff3ca`): `ValueKind::Binary` / `Unary` now
  carry the result `TypeId` in the key (part 1 above). Re-tested early promotion —
  **still invalid Wasm**, and instrumentation pinned a contradiction: the freeze
  redirects **0** width-mismatched binaries (`lhs_ty != ty`) directly (the guard
  works), yet early-freeze-**alone** (late freeze disabled) still emits the
  `i64`/`i32` mismatch on count_prime. So a mismatched `Shr` (`lhs = Int(2,i32)`,
  `ty = u64`) is extracted as a **descendant** of a redirected value via a path
  `record_value_tree_types` + `value_fully_reemittable_locally` should both
  reject (they recurse `Binary`/`Unary`/`Select`; `Cast`/`FieldAccess` children
  make a parent non-reemittable). The descendant reaching extraction contradicts
  that model — the next focused step is to instrument the **top** promoted operand
  and walk its value tree to see exactly how the `i32`-lhs `u64`-`Shr` is reached
  (and the upstream cause: a `u64` shift whose lhs literal is typed `i32` in NIR —
  a leaf-mistyping to fix at its source). Experiment + instrumentation discarded;
  branch green at `adecff3ca`.

  Deeper instrumentation (recurse **all** child kinds, `Cast`/`FieldAccess`
  included): **0** freeze-redirected source exprs have a width mismatch anywhere
  in their value tree _at freeze time_ — yet early-freeze-alone still emits the
  mismatch. So the inconsistency is **not present when the freeze promotes**; it
  arises **after**, when a later pass mutates the graph (a `value_union` from
  `cse` / `store_load_forward`, or a congruence rebuild) so that a promoted
  operand's `find()` later resolves to a different-width representative. Verdict:
  the freeze-before-passes **shortcut is the wrong vehicle** — it opens a
  freeze-then-mutate window that the WEP's actual design (`lower` emits
  `Operand::Value` directly and passes are migrated to consume operands, never
  re-deriving) does not have. The next promotion work should pursue the real
  design (operands born at lower, passes migrated) rather than the freeze-early
  shortcut, and separately fix the upstream `u64`-shift-with-`i32`-lhs-literal
  leaf-mistyping. Experiment + instrumentation discarded; branch green at
  `adecff3ca`.

### Pass migration begun: passes that mishandle promoted operands (commit `ebf1af821`)

The early-promotion experiment, though the wrong production vehicle, is the right
**diagnostic** for the pass migration the real design needs: with it on,
`WADO_SKIP_PASS` bisection pinpoints exactly which pass mishandles a promoted
`Operand::Value`. Two found and fixed:

- **`inline`** — `splice_operand` re-allocated a callee value into the caller pool
  by cloning its _kind_ and calling `alloc_unshared`, but a composite kind
  (`Binary` / `Cast` / `Select` / `FieldAccess` / `LoopPhi`) carries **child
  `ValueId`s scoped to the callee pool**; verbatim, they denote unrelated
  (often different-width) values in the caller. Fixed by a recursive `splice_value`
  that re-allocates the whole tree and remaps `Opaque` source locals via the
  inline `ctx`.
- **`dae`** — three gaps: (1) `find_dead_params` only scanned skeleton reads, so a
  param read **only through a promoted `Opaque(Local)`** looked dead (now unions
  `ValuePool::opaque_local_sources`); (2) local renumbering skipped
  `OpaqueSource::Local` indices (added `ValuePool::remap_opaque_locals`); (3) the
  dead-arg purity check `expect`ed a skeleton `Expr` (a promoted value is pure by
  construction).

With both fixed, all four benchmarks compile valid Wasm under early promotion and
count_prime runs correctly (`78498`). The fixes are **behavior-neutral in the
late-freeze flow** (no promoted composites reach `inline`/`dae` there): committed,
2960/0 e2e, 770 lib. Full e2e under early promotion still has **347 O2 failures**
(O0 skips the optimize loop, so it is unaffected) — the remaining pass-migration
surface (more passes to teach + `wir_expect` fixtures whose WIR shape changes
under promotion). These pass fixes are permanent and reused by the real
`lower`-emits-operands design; the next step is to keep bisecting the 347 for the
real-miscompile subset (vs `wir_expect` churn) and migrate those passes.

Next culprit bisected (`array_index_1`, `result: 0` vs `5` — a real miscompile,
not `wir_expect`): `WADO_SKIP_PASS` shows **`peephole`** is the pass; it runs every
iteration, so it likely accounts for a large share of the 347. Root cause is the
same class as the `dae` fix: `elide_local` decides a local is write-only via
`Engine::is_local_read`, which counts only **skeleton** `Local` mentions (the use
index) and misses a local read **only through a promoted `Opaque(Local)`** value —
so a still-read accumulator is elided and its value is lost. The fix needs care:
`ValuePool::opaque_local_sources` over-approximates (the builder seeds
`Opaque(Local)` for many locals that are never promoted into a live operand slot),
so consulting it unconditionally would keep nearly every local and **disable
`elide_local`**. The precise signal is the `Opaque(Local)` sources of values
referenced by a **live `Operand::Value` in the skeleton** — a skeleton walk, empty
in the late-freeze flow (so behavior-neutral there). That precise
live-promoted-reads query is the next brick (and the general primitive every
liveness-based pass needs under promotion); deferred rather than rushed because
the over-approximation trap makes a naive fix a silent quality regression.

Landed (commit `0cbb27fbf`): the live-promoted-reads primitive —
`Body::for_each_operand` (read-only mirror of `map_operands`) +
`Body::locals_read_via_promotion`, backed by `ValuePool::collect_opaque_locals` +
`find_imm` (a `&self` union-find root). `elide_local` now keeps a local read only
through a promoted `Opaque(Local)`. Precise (walks only values in live operand
slots, dodging the `opaque_local_sources` over-approximation), behavior-neutral
pre-promotion (770 lib green). Impact under early promotion: **e2e failures
347 → 111** — `peephole`/`elide_local` was the single dominant culprit (it runs
every iteration). Pass migration so far: `inline`, `dae`, `elide_local`.

Remaining 111 (early-promotion only; baseline green) by prefix: serde 12,
allocator 10, opt 7, match 6, array 6, wasm 5, newtype 5, closure 5, assert 5, …
Next culprit triaged — `coerce_float` (`f32 PI: 0.0000004` vs `3.1415927`):
`WADO_SKIP_PASS` implicates `copy_prop` + `inline`, but `extract_value`'s `Float`
arm is **correct** (instrumented: `bits` = f64 PI, `value_ty` = F32, emits
`F32Const(3.1415927)`). So the corruption is **downstream of extraction** — a
copy_prop/inline interaction on a promoted f32 value (a missing/!mismatched
f32↔f64 coercion or a cross-function value splice), not the extractor. A distinct
focused trace; deferred to the next brick. Tree restored green.

Width crux resolved (good news, verified): with composite-width (`adecff3ca`) and
the `elide_local` fix in, `count_prime` now **compiles valid Wasm under early
promotion** (was the original `i64`/`i32` invalid-Wasm), and a `VG_WMISMATCH`
probe (panic on any extracted `Binary` whose lhs width ≠ result width, comparisons
excluded) fires on **neither** count_prime nor coerce_float. So the
operand-width-erasure class that blocked early promotion is **closed** — the
remaining 111 are not width-extraction bugs. `coerce_float` specifically is a
copy_prop/inline value-substitution bug in the float-formatting path
(`short32`/`unpack32`'s u64 math producing a wrong digit _value_, not a wrong
width), `extract_value` and the binary widths being correct. Next brick: trace
that copy_prop/inline interaction (likely a promoted value spliced/propagated to a
wrong slot), which should clear the formatting-heavy fixtures (assert / inspect /
serde / coerce).

Pass-migration audit — two gap classes, mapped systematically:

- Local-elimination read-undercount (a pass counts skeleton `Local` reads, misses
  a local read only through a promoted `Opaque(Local)`, and eliminates/propagates
  it): found and fixed in `dae`, `elide_local`, `copy_prop` (the latter two via
  `Body::locals_read_via_promotion`). A sweep of every `optimize/*` pass for
  `is_local_read` / `read_count` / use-counting shows this class is now **closed**
  for locals: `dce` / `drve` / `sroa` do not count local reads; `ref_elim`'s
  `use_count` is over _reference_ bindings (`Ref` / `MutRef` / `Deref` are not
  promoted); `field_scalarize`'s `read_count` is over _fields_ (FieldAccess not
  promoted yet). Local renumbering that stales `Opaque(Local)` sources is handled
  in `dae` (`remap_opaque_locals`) and `inline` (`splice_value` via `ctx.local`);
  `licm` / `field_scalarize` / `container_sroa` append locals rather than
  renumber, so existing sources stay valid.
- Analysis-completeness (a per-expression read/write analysis misses a promoted
  read): `mod_ref`'s `local_reads` walks the skeleton subtree, so a local read via
  a promoted operand in that subtree is missed — currently unexercised (late
  freeze runs after `const_folding`, mod_ref's consumer), but a real gap once
  promotion precedes it. Deferred; flagged.

Future-gap (not yet exercised, will open when their kinds are promoted):
`field_scalarize` (FieldAccess promotion) and `mod_ref` (any promotion before
const_folding). Tracked so the keystone migration re-checks them.

Next high-leverage culprit: a `copy_prop` + `inline` interaction (skipping
_either_ fixes it) that miscompiles a _value_ — not a width — under early
promotion. Confirmed shared across batches: `coerce_float` (`f32 PI: 0.0000004`)
and `allocator_freelist_*` (a corrupted pointer `0xfffffff8` = -8 trapping in
`fl_unlink`), so it likely accounts for a large slice of the 111 (the formatting

- allocator families). Localization so far: `extract_value` and binary widths are
  correct; `copy_prop`'s `analyze_copy_binding` bails on a promoted-value _source_
  (`value.as_expr()?`), so the source is never promoted; the `read_count += 2`
  liveness fix (commit `5c0c2c31d`) does not resolve it. The `source_scope_stable`
  range check _should_ stay sound with a promoted `Opaque(Local target)` read
  present, so the mechanism resists static reasoning and wants empirical
  instrumentation (print when copy_prop propagates a binding whose target is in
  `locals_read_via_promotion`, on `allocator_freelist_align`, and diff the WIR
  with/without copy_prop).

  Resolved (commit `de72ba0af`): root cause is `apply_in_block` deleting the copy
  target's `let` (`dead_locals`) after `apply_in_expr` substituted only its
  _skeleton_ reads — a promoted `Opaque(Local target)` read then dangled on the
  deleted local. Fix: `can_propagate_copy` returns `false` when the target is in
  `locals_read_via_promotion`. This single guard cleared the bulk: early-promotion
  failures **111 → 10**. The `match_or_pattern_iflet_1` ICE
  (`labeled_block_fusion`'s `as_expr().expect` on a promoted condition) is also
  fixed (`9b139593f`, `as_expr()?`).

State of the early-promotion experiment: **347 → 10** (arith-freeze vehicle). The
final 10:

- ~5 not real bugs — `wir_expect` pattern / missed-opt tests whose expected WIR
  shape changes under promotion (`array_bounds_elim_*_wir`,
  `opt_licm_invariant_arith`, `tir_optimize_bool_identity`, likely `closure_2`):
  fixture-expectation churn, not miscompiles.
- **3 control-flow miscompiles** (`select_extended_arms`, `if_merged`,
  `labeled_block`) — all involve a promoted **`Select`** value. `WADO_SKIP_PASS`
  bisects to `peephole` (skipping it passes), so a peephole **rule corrupts the
  promoted `Select`** — it is _not_ the extraction-availability issue (that would
  manifest at WIR build regardless of peephole). Narrowing so far: removing
  `branch_prune` **or** `const_fold` individually does _not_ fix it, so the
  culprit is another peephole rule (`match_to_switch` / `value_copy_elide` /
  `ref_elim` / `elide_box` / `array_literal` / `labeled_block_fusion`) or the
  engine's value-graph maintenance over a promoted `Select` operand during the
  peephole session. `select_extended_arms` computes `decimal_pos = 2` where the
  source merge yields `-5`. Next step: continue the in-peephole rule bisection
  (remove rules one at a time) on `select_extended_arms`. (Separately, the
  extraction-availability concern — re-emitting `if cond {..} else {..}` at a use
  site vs a `local.get` of the merge value — remains a real keystone sub-problem,
  but it is not what these three fixtures hit.)

  Culprit isolated (per-peephole-rule `SKIP_PRULE` bisection): the rule is
  **`elide_local`** — skipping only it passes all three. It eliminates a local
  that is still read through a promoted operand, but `locals_read_via_promotion`
  (which `elide_local` already consults) **does not list that local**, so the
  guard does not fire. Verified: forcing `is_kept` to consult the pool-wide
  `opaque_local_sources` (over-conservative) fixes all three — so the needed
  local genuinely has an `Opaque(Local)` source, but it is not reached by
  `for_each_operand` + `collect_opaque_locals` from a live operand slot. A
  `find_imm`→raw-`kind` alignment in `collect_opaque_locals` (extraction reads the
  raw value, not the union-find rep) was tried and did **not** fix it, so the gap
  is subtler than a rep/raw mismatch — a promoted read this precise walk misses
  while `opaque_local_sources` catches. The over-conservative set is **not** a safe
  substitute (the builder seeds `Opaque(Local)` for many non-read locals, and
  `opaque_sources` persists on the body across passes, so it would disable
  `elide_local` broadly). Dormant in the committed late-freeze path
  (`locals_read_via_promotion` is empty there, e2e 2960/0), so the branch stays
  correct; the gap needs focused keystone-time work to make the precise walk
  complete. Reverted the speculative raw-`kind` change rather than ship an
  un-understood edit.

  Concrete next lead (derived, not yet validated against the fixture): a real
  asymmetry between extraction and the liveness walk. `extract_value`'s
  `Opaque` arm emits `OpaqueSource::Local(idx)` as `local.get idx` **and**
  `OpaqueSource::Expr(e)` as `translate_expr_inner(e)` — the scheduled skeleton
  expr, which reads whatever locals `e` reads. But `collect_opaque_locals` only
  handles `OpaqueSource::Local`; for `OpaqueSource::Expr(e)` it does nothing, so
  every local read by the scheduled expr behind an `Opaque(Expr)` is invisible to
  `locals_read_via_promotion`. Fix direction: `collect_opaque_locals` must, for an
  `OpaqueSource::Expr(e)`, also walk `e`'s skeleton subtree (and its nested
  operands) for `Local` reads. (Note `opaque_local_sources` also ignores `Expr`
  sources, so this specific gap is _not_ what the `ELIDE_OVERCONS` probe masked —
  meaning `select_extended_arms` has at least one _additional_ `Local`-source path
  the live-slot walk misses; both need closing.)
- 1 ICE (fixed) and `assert_fail_call_arg` (power-assert diagnostic formatting,
  uncharacterized).

Net: the systematic pass-migration is essentially done for arith promotion (five
passes migrated — `inline`/`dae`/`elide_local`/`copy_prop`×2 — two bug classes
closed, **97%** of the failure surface cleared); the residue is the
`Select`-availability soundness problem (a keystone sub-task) plus `wir_expect`
fixture churn that only matters once promotion is the real vehicle.

Standing pre-existing finding from Probe A's harness run: `WADO_VERIFY_VG` is
**not clean on count_prime even on the committed baseline** (a
`cse → store_load_forward` over-merge). Currently benign (e2e green), but it
falsifies the earlier "clean across the corpus" claim and is the same
downstream-stale flaw — it disappears under operand promotion.

Root-caused exactly (`-O2`, `verify_maintained_graph` now prints the pair): the
maintained graph merges `expr53 = Local{index:9 "m"}` with
`expr41 = Binary{Shl}`, both pinned to a stale `ValueId(1268)`; a fresh build
correctly splits them (`1644` vs `1650`). At the session's first build `m` and
the shift were legitimately equal (`let m = … << …`). A later
`store_load_forward` edit changed the shift's operand, so the two diverge — but
`maintain_value_after_edit` propagates only up the **expr-ancestor chain**, and a
`Let` / `Assign` **Stmt** boundary breaks that chain, so the local's _reader_
exprs (`expr53`) are never revisited and keep the stale id. A maintenance-only
fix would have to drop every reader of a local whose defining RHS is edited
(flow analysis on each edit); the frozen-`Operand::Value` model removes the
staleness by construction. This is the load-bearing case for promotion, now with
an exact repro.

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
