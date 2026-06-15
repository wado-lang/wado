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

## Context

A sampling profile (samply, 1 kHz, dev/debug `wado`) of
`wado compile -O2 package-gale/src/main.wado` (~37k lines) pins the cost.
Percentages are inclusive shares of total CPU — machine-independent ratios;
absolute time varies by host. `optimize` is 69% of compile CPU, and the gated
intra passes (`run_gated` + `run_gated_cached`) are ~49%. Inside them the
dominant cost is per-function analysis rebuilt per pass and discarded:

- per-function `ValueGraph` build (the `walk_block` / `walk_expr` flow walk,
  reached via `Engine::value`) ~20%
- per-session engine setup (`Engine::new`: parent maps, use index, post-order
  seed) ~9%
- flow joins (`join_heap` / `flow_join_two` / `flow_join_n`) ~10%
- alias-set computation (`builder_alias_sets`) ~9%

Together ~40% of compile CPU is spent re-deriving per-function analysis the next
pass rebuilds from scratch. The build is compute-bound, not allocation-bound:
pooling the builder's output maps measured no improvement. The cost is the flow
walk, the hash-cons, and the joins.

This cost exists for one structural reason: the ValueGraph is a _derived_
analysis of a _mutable_ SkelTree. Every SkelTree edit can stale the graph, so
every pass that needs pure-value identity re-derives it. The dirty-set gate and
the revision-keyed `vg_cache` amortise this only for functions that did not
change; a function a pass actually rewrites pays a fresh re-derivation in the
next pass that visits it.

## Why source-of-truth, not incremental rebuild

The obvious patch — re-derive only the changed region of the side-table — was
prototyped, verified equivalent to a full rebuild under a `WADO_VERIFY_INCREMENTAL`
harness, and reverted: it fired on ~0.15% of builds on the large workload and 0%
on the small ones. Worse, it is the wrong shape of fix: it makes the
_re-derivation of a derived side-table_ cheaper, when the side-table only needs
re-deriving because it is derived. The direction WEP already reached this
conclusion — the side-table, and any machinery that rebuilds it, retire once the
graph becomes the source of truth.

So this WEP does not resurrect incremental rebuild. It removes the reason
re-derivation exists: the graph stops being a shadow of the SkelTree and becomes
where pure values _live_. Flow is resolved into the graph once, at build, and
frozen there; a rewrite is a union of two e-classes, which every user sees
through `find()` without any re-walk. There is no derived form to bring current.

This reverses the operand-promotion deferral from the previous draft of this WEP.
Promotion was mischaracterised there as a marginal representational cleanup; it
is the load-bearing change that lets flow be derived once and rewrites be
pointwise. It is in scope here.

## Decision

Promote the ValueGraph to the pure-value IR and drive it aegraph-style.

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
redesign keeps that build and runs it once per function per outer round. Two
additions make it an IR rather than a side-table:

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
at the splice point, not a re-derivation of existing flow). Interprocedural
passes that restructure a whole body (notably `inline`) run between outer rounds;
the next round rebuilds that function's graph once.

### Extraction

Before WIR build, one pass walks the SkelTree and lowers each pure `ValueId`
operand to a concrete skeleton/WIR form, choosing per multi-use value whether to
re-compute it at each use or materialise it once into a hoisted temp. This is the
one genuinely new analysis and the main regression risk: the cost model must not
emit worse code than today's CSE / hoisting heuristics. The migration de-risks it
by reproducing the current materialisation first (extract each value at the sites
and shapes the old passes produced), then improving the cost model behind
benchmarks.

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

- The per-pass re-derivation (~40% of compile CPU: build ~20%, setup ~9%, joins
  ~10%) collapses to one build per function per outer round; intra-procedural
  rewrites then mutate the live graph in place. The alias rebuild (~9%) is
  computed once per function per round.
- A cluster of dataflow passes (CSE, copy-prop, the bulk of const-fold,
  store-load-forward, loop-invariant hoisting) stops being passes and becomes
  graph structure, removing both their walks and their bespoke analysis caches.
- Target: package-gale optimise phase ~1.5× faster than the current baseline.
  Aspirational, not committed.

### Risks

- Extraction is the central risk: a weak cost model regresses code size or
  runtime. Mitigated by reproducing current materialisation first, then tuning.
- Operand-promotion breadth: a wide change across arena, lowering, WIR build, the
  unparser, and pure-`ExprKind`-matching passes. Mechanical but large.
- Congruence maintenance: deferred e-graph rebuild after unions has its own cost;
  it must stay below the re-derivation it replaces. Measured per step.
- Control-flow rewrites keeping the graph coherent (Select collapse, inlined-region
  build) is the subtle correctness surface; the verify harness is the guard.

### Trade-offs accepted

- The graph gains a union-find and a congruence rebuild; the optimizer gains an
  extractor. In exchange every pure-value pass's per-pass rebuild and bespoke
  analysis is deleted.
- Arena compaction (dead skeleton nodes from in-place rewrites) becomes more
  worthwhile once bodies are walked fewer times; tracked as the existing
  follow-up.

## Graph-preserving rewrites

An implementation finding that shapes the build-once work: a rewrite that
replaces an expression with a value the graph already holds — store-load
forwarding a stored literal, materialising a condition the guard proved
constant, folding a constant arithmetic node — does not change any _other_
expression's ValueGraph value. Its only stale effect is on the rewritten node's
own `value_of` entry, which now reads as a literal anyway. Such a rewrite is
**graph-preserving**: a later pass can keep using the same build without a
rebuild.

This is the lever for build-once: passes whose rewrites are graph-preserving
share one build per function per round. The combined session therefore carries,
per migrated pass, a graph-preservation obligation — a `set_block_stmts` that
drops an effectful statement (e.g. a panic check) can bump heap versions and is
_not_ graph-preserving, so it must either rebuild or be expressed as a union.
The `WADO_VERIFY_INCREMENTAL`-style cross-check guards each migration.

## Roadmap

Each step must not regress output (code size or runtime) on the full fixture +
E2E suite, on `wir_expect` / `wir_not_expect`, and on the benchmark set before
the predecessor is deleted.

- [x] Union-find + congruence rebuild on the `ValuePool` — equivalence by
      `find()`, deferred re-canonicalisation after unions; constant kinds win the
      representative so a class containing a literal resolves to it. Exposed on
      the engine (`value_find` / `value_union` / `rebuild_value_congruence`).
- [x] Extraction keystone — `extract::materialize_literal`, the graph→skeleton
      primitive that resolves through the representative. `store_load_forward`
      now routes through it (the first production pass on the e-class).
- [~] Build-once-per-round — one engine session holds the graph across the
  adjacent graph-preserving passes; share one build, dropping the second pass's
  separate engine setup, alias-set computation, and `ValueGraph` rebuild. Both
  clean adjacencies landed, each byte-identical on package-gale + full E2E:
  - `cse` + `store_load_forward` share one session (cse replaces matching
    subexpressions in place, so values are preserved).
  - `licm` + `condition_implication` share one session (licm hoists only
    invariant, move-safe code, so values are preserved; cond-impl runs after
    licm in document order — no reorder — and is invariant to licm's param
    seeding since it tests only `ValueId` equality). ~2–4% faster compile on
    package-gale (median 23.69s → 23.14s).

  An _earlier_ adjacency does not work: moving `condition_implication` ahead of
  `const_fold` / `licm` (into the cse session) regresses output (+58 bytes), as
  it then misses folded / hoisted guards.

- [~] Keep the graph live across `const_fold`. The cse session now seeds
  params, making its parked graph config-identical to licm's, and the licm
  session reuses it (via `run_gated_cached`) for every function `const_fold`
  leaves unchanged between them — so one build serves both value-graph sessions
  instead of two. `const_fold` is niri-based and touches the graph through no
  re-walk (it only `mark_changed`s); a function it _does_ change falls back to a
  fresh build at licm. No incremental rebuild, no re-derivation — the graph is
  simply not invalidated by an intervening pass that did not disturb the
  function. Byte-identical, full E2E green, ~7.6% faster compile on package-gale
  (median 23.21s → 21.45s).

  Remaining: the first build at `cse` (per function the early structural passes
  changed) and the licm rebuild for `const_fold`-changed functions. Removing
  these needs the structural passes and `const_fold` to _maintain_ the graph
  through their edits via graph operations (union / `set_value` / new-node
  construction for genuinely-new code) — never a re-walk of existing code — i.e.
  the operand-promotion / graph-as-source-of-truth work. Incremental rebuild of
  the derived side-table (re-walking disturbed regions) is explicitly _not_ the
  path: it was prototyped, reverted, and is the wrong shape (see "Why
  source-of-truth, not incremental rebuild").
- [ ] Subsume CSE, copy-prop, and store-load-forward into graph structure; delete
      the passes and their analysis. (copy-prop / CSE need extraction beyond the
      literal case — a flow-valid source for a shared non-constant value.)
- [ ] Subsume flow-sensitive `const_fold` and `condition_implication` as graph
      rewrites; delete their per-pass dataflow. (`const_fold` keeps niri for pure
      CTFE calls, which the graph does not fold.)
- [ ] Subsume loop-invariant hoisting into the extractor's placement decision;
      retire `licm`'s pure-arithmetic hoisting.
- [ ] Cost-based extraction — replace the straight extraction with a share-vs-
      duplicate cost model; tune against benchmarks.
- [ ] Operand promotion — pure literal / `Binary` / `Unary` / `Cast` slots carry
      `ValueId`s; `lower::translate` builds the graph; the `value_of` side-table
      retires. The widest change (arena / lowering / WIR build / unparser /
      pure-`ExprKind`-matching passes); deferred until the graph-driven passes
      prove the model and the win.
- [ ] Retire the intra-procedural iteration count — the graph and worklist
      self-converge; keep an outer round count only for the interprocedural cycle.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the direction; equality saturation stays deferred there.
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the engine substrate, edit API, and gate this builds on.
- [`docs/optimizer.md`](./optimizer.md) — the pass inventory the graph absorbs.
- Cranelift's aegraph mid-end and `egg` (https://egraphs-good.github.io/) — the build-once, eager-rewrite, single-extraction model this adapts.
