# WEP: NIR Optimizer Architecture

The NIR optimizer is a **two-tier IR** — a structured effect skeleton plus a
hash-consed graph of pure values — rewritten by a **worklist engine** over
per-function sessions, scheduled by a **per-function dirty-set gate**, and
**extracted once** into WIR.

This WEP is the design. The pass inventory lives in
[`docs/optimizer.md`](./optimizer.md); every implementation detail — node kinds,
rule predicates, pass ordering — is owned by the code
(`nir_arena.rs`, `nir_value_graph.rs`, `nir_engine.rs`, `optimize/`).

## Terms

- Skeleton — the tier that carries execution order: statements, control flow,
  calls, assignments, allocation. Document order is effect order. Pure values
  have no place in an order, so they are not stored here; what is left is the
  bare frame of the computation.
- Hash-consing — interning a value by its structure, so two structurally
  identical values are handed the same `ValueId` at construction. Equality
  becomes `==` on a `u32`, and a common subexpression is common before any pass
  looks at it.
- Promoted — a skeleton operand slot that holds a `ValueId` instead of pointing
  at a sub-expression node. The value moved out of the skeleton and into the
  pool; nothing in the arena spells it any more.

## Context

The optimizer was ~31 independent passes, each a full mutating walk over an
owned tree body, run inside a global fixed-point loop. Two costs followed from
that shape:

- No stable handles and no parent / use edges. `&mut NirExpr` recursion owns the
  path to a node, so nothing could hold a node across an edit — a worklist was
  not expressible, and CSE hand-rolled a structural `CseKey` because NIR offered
  nothing better.
- Every pass re-derived its own analysis. Pure-value identity was per-pass keys;
  reaching definitions were rebuilt per pass (`KnownValues`, `DefMap`,
  `env` / `field_env`). A native profile of `wado compile -O2` on `package-gale`
  put `optimize` at 52 – 65 % of the whole compile, with the per-function value
  graph rebuilt 2.67× per function and ~27 % of CPU in the allocation churn those
  rebuilds generate.

The second cost is structural, not a tuning problem: an analysis derived from a
mutable tree must be re-derived after every mutation. The fix is to stop deriving
it.

## Decision

### Layer 1 — the skeleton arena (`nir_arena.rs`)

A function body is a per-function arena, `Body`, and is the canonical post-lower
form: `lower::translate` builds it directly and `wir_build` reads it directly.
Closure `__call` methods and non-trivial global initializers are bodies too.

- Typed id spaces (`ExprId` / `StmtId` / `BlockId` / `PatId`) over
  `cranelift_entity` maps, with `NodeRef` as the uniform handle the worklist and
  parent map speak. Typed rather than uniform so a rule signature rejects a
  statement where an expression belongs.
- Records that are never independent rewrite targets — match arms, struct
  fields, call args — are not id-bearing. They live inline in their parent and
  are transparent to the parent map.
- `type_id` and `span` stay per node: `span` feeds diagnostics, optimizer
  remarks, and DWARF, and is part of skeleton identity. Layer 2 is the span-free
  layer, not this one.
- Parent edges (nearest id-bearing ancestor) and a per-local use index
  (`def` / `reads` / `writes`) are maintained incrementally by the edit API,
  never recomputed. Function-level alias facts (`address_taken_locals`,
  `stores_aliased_locals`) and the local table travel on `Body` beside the arena.
- Nodes are not freed mid-run. Liveness is reachability from `root`, and the use
  index ignores orphans.

Document order is effect order. The skeleton carries statements, control flow,
effectful and allocation-bearing expressions (`Call`, heap `Assign`,
`StructLiteral` / `TupleLiteral` / `ArrayLiteral`), and patterns.

### Layer 2 — the live ValueGraph (`nir_value_graph.rs`)

Pure values are hash-consed into a per-function `ValuePool`, indexed by
`ValueId`: two structurally equivalent pure expressions share one id, and
equality is `==` on a `u32`. The graph is where pure values **live**, not a
side-table derived from the skeleton.

- Kinds cover literals, `Opaque` (parameters and unknowns), `Binary` / `Unary` /
  `Cast`, `Select` (structural merge), `LoopPhi` (loop recurrence), and
  `FieldAccess` carrying a per-field `HeapVersion`.
- There is no `Local` kind. Flow is resolved into the graph **once, at build**:
  the builder threads a per-local current value, constructs `Select` at merge
  points and `LoopPhi` at loops, and bumps heap versions where the skeleton may
  write. A read's `ValueId` is fixed to the value dominant at that point.
- Identity is structural, and a `ValueId` is stable once allocated: interning is
  pure hash-consing, with no e-class merges and so no representative lookup. A
  rewrite that proves `a ≡ b` points the operand slot at `b`; it never merges
  two ids. Rules apply eagerly and once per match, never searched to a fixed
  point.

Build once, rewrite eagerly, extract once — the shape Cranelift's aegraph
mid-end takes, minus its union-find.

Constant aggregates are named by a single `Const` kind; materialising one is an
allocation, which is `const_object_globalization`'s decision, so an aggregate
constant is never frozen into an operand slot.

### The operand bridge

A skeleton child slot is `Operand = Expr(ExprId) | Value(ValueId)`. Pure operand
positions carry `Operand::Value`; effectful and control-flow operands stay
`Operand::Expr`.

Values are **born as operands**: `lower::translate` and the promotion passes
(`optimize/extract.rs`) freeze a pure position into its `ValueId` directly, so a
pass reads the value off the operand rather than looking an `ExprId` up. There is
no `ExprId → ValueId` side-table; an unpromoted skeleton leaf resolves to `None`,
which is sound because `None` is the finest partition — a consumer skips the
expression rather than over-merging.

Promotion is staged against what the passes need to see: arithmetic freezes
before the loop (on each function's clean, un-restructured graph, which is what
makes freezing a constant leaf read sound), `FieldAccess` freezes after the SROA
passes have settled the struct shape, and the final arithmetic freeze runs last,
after every binary-walking pass.

### Extraction

One pass materialises each promoted operand back into concrete form. Constant
extraction is available to the optimizer itself (`extract::extract_const`); the
full extraction happens at WIR build (`wir_build::translate::extract_value`),
which lowers each kind from the pool using the type the builder recorded and
recurses on composite operands.

Extraction currently re-materialises a value at each use — always correct, and
for constants always cheaper than sharing. The share-vs-duplicate cost model for
multi-use non-constant values is open (see Remaining work).

### The engine (`nir_engine.rs`)

Genuinely-local rewrites run as `Rule`s on a worklist over one function's `Body`,
to a local fixed point: a node is revisited only when an edit may have made it
reducible, never by a whole-tree sweep.

- A session builds the parent maps, the use index, and a post-order-seeded
  worklist in one O(n) pass.
- Rules never touch the arena maps; every mutation goes through the edit API
  (`replace_expr_kind`, `become_expr`, `set_block_stmts`, `alloc_*`,
  `clone_expr`), which keeps the parent map and use index coherent and
  re-enqueues exactly the affected neighbourhood. In-place replacement keeps the
  id stable, so a worklist entry survives the edit.
- At a popped node the engine tries the registered rules in order; the first that
  reports a change retries at the same node. Rules must be idempotent, and either
  confluent or priority-ordered — priority is rule order in the session.

The position-flexible local rules share **one session per function** (the unified
peephole, `optimize/peephole.rs`), run pre-inline and post-inline so each rule
sees the instruction window the other exposes.

What stays outside the engine: flow-sensitive passes that need per-block dataflow
keep their own walkers over the arena, and the interprocedural stages (`inline`,
`dce`, `dae`, `drve`, globalization) run as distinct steps.

### Per-function gating (`optimize/gate.rs`)

Each function carries a monotonic revision and each pass a per-function
watermark; a gated pass visits a function only when `revision > watermark`. A
pass that changes a function bumps its revision and, conservatively, that of its
1-hop call-graph neighbours. Interprocedural passes pull their candidate set from
the dirty set rather than scanning every function, and re-mark affected callers
when a callee shrinks; `OptConfig::iterations` is the quiescence bound. Terminal
stages stay explicit.

Gating changes only **which** functions a pass visits, never the result of a
visit. Every loop pass is an optimization, so an imprecise gate costs
optimization quality, never correctness — the same one-sided argument covers the
interprocedural over-approximation.

## Soundness invariants

- Substitution soundness. An operand is repointed from `a` to `b` only when the
  two denote the same value in every execution reaching that point — the
  obligation CSE, copy propagation, and forwarding each discharged separately,
  now expressed once. Sharing an id is stronger than that and needs no
  justification at all: hash-consing only ever gives one id to structurally
  identical values.
- Flow-freeze validity. A control-flow rewrite that changes which value is
  dominant must repoint the affected operands; a read must never be left holding
  a value that no longer reaches it.
- Heap-version monotonicity. A `FieldAccess` value's `heap_ver` is the version
  before the read; a later write bumps to a fresh version, so any read after it
  gets a fresh `ValueId`.
- Pointwise maintenance. A structural edit keeps the graph coherent at the point
  of the edit — pruning a branch repoints the surviving operands past the dead
  `Select` arm; splicing an inlined body or an SROA split interns value nodes for
  the new skeleton subtree. Monotone growth, never a re-derivation of existing
  flow. This is the load-bearing claim of the design.
- Extraction equivalence. The extracted form computes, for every effectful
  position, the same values in the same effect order as graph + skeleton.
- The edit API is the only mutation path during a run. A pass that pokes the
  arena maps directly desynchronises the parent and use indices.

### Standing invariants — do not reintroduce

These are settled by measurement and re-derived at a cost; `wado-compiler/AGENTS.md`
repeats them where a contributor will hit them.

- No mid-pipeline rebuild of the value graph. Build-once is structural: nothing
  clears `Body::value_graph`. Maintain in place; never clear-and-rebuild.
- No `ExprId`-keyed value cache or side-table. A pass needing a value uses
  born-as-operands or a scoped scratch walk.
- A promoted read lives in the pool, not the skeleton. A pass that decides a
  local is unused, or rewrites every read of one, must count the pool's reads
  (`arena_query`'s `promoted_*` queries) — scoped to the operands the skeleton
  still carries, since the pool is append-only and also holds reads that folded
  away.

## Rejected and deferred

### Not classical SSA

Wado emits structured Wasm, so SSA + relooper buys nothing the backend does not
already need. The skeleton stays the effect-and-control schedule; local
versioning is expressed by structural `Select` nodes at merges rather than
explicit phis. Sea-of-nodes, and dropping the skeleton, stay rejected.

### Not equality saturation

Saturation over the value graph would unlock **algebraic exploration** —
re-association, distributive law, strength-reduction-per-use, cost-based
share-vs-duplicate. Wado's output is Wasm, which the host runtime JITs again, and
the host redoes most of that algebra. Cranelift's aegraph reports 5 – 10 % on
native AOT; the JIT-target number is much smaller. Meanwhile the optimizations
Wado actually carries — allocation elimination, structural cleanup, dense-`Match`
lowering, bounds-check elimination, inlining — are single-direction structural
rewrites that saturation does not help.

So saturation stays the terminal ideal, activated only if measurement justifies
it (a native-AOT backend, or Wasm output measurably benefiting from algebra the
JIT cannot do). Its prerequisites are visualisation tooling
(`WADO_DUMP_VALUE_GRAPH`, `WADO_DUMP_AFTER_RULE`) and budget-bounded rule sets —
a budget hit must degrade gracefully to the partially saturated graph, never
panic and never produce worse output than the input.

### Not incremental rebuild, and not a richer cache

Both were prototyped and measured, and both are the wrong shape: they make
re-deriving a derived analysis cheaper, when the analysis only needs re-deriving
because it is derived. See "Measured dead ends".

## Remaining work

Compile speed. The build-once redesign met its structural goals but not its
2× CPU target: `package-gale`'s optimize phase measured ~15.7 s against a ~7.5 s
target. The premise was wrong — with build-once, the graph build is ~6 % of the
phase (was ~21 %); the cost is now the passes themselves.

- [ ] Pass-level cost: `peephole` (~4.6 s on package-gale) and the iteration
      count dominate. This is a separate track from the value-graph
      re-architecture, and the one with the remaining headroom.
- [ ] Build the engine session once per function and maintain it through the edit
      API, the way the graph already is. `Engine::new`'s parent map / use index /
      post-order seed is still paid per pass per function (~8 % of compile CPU at
      the time it was profiled).
- [ ] Function-level parallelism. The per-function build and walk are
      independent.
- [ ] Fold the graph build into `lower` (born-at-`lower`), retiring the lazy
      first-query build.
- [ ] Arena compaction. In-place rewrites orphan nodes that are never freed
      mid-run (~1.66× bloat measured at end-of-optimize on `package-gale`).

Precision.

- [ ] Widen local promotion past parameters. A promoted value may now read a
      local, but only a never-reassigned parameter qualifies, and only at the
      post-loop freezes: 37 – 146 such operands per benchmark, against zero
      before. The gate is the version question — one `ValueId` per local index
      denotes one value only where the local has one version, which a parameter
      has and an ordinary binding does not. Widening needs a def that provably
      dominates the use and a binding that does not re-execute — the flow fact
      the builder holds and a query-time resolver does not.

- [ ] Reach the in-loop consumers. Both freezes that may plant a local-naming
      value run after the fixed-point loop, so the passes inside it still see
      none: LICM's value hoist collected zero loop-entry locals in 10,900
      queries, and `loop_entry_values` still has no working consumer, which is
      why `inline` discarding a non-empty map 1,469 times costs nothing. An
      in-loop freeze cannot simply be added — the early one is bound by the
      context-free rule under "Measured dead ends". This is the born-at-`lower`
      item above: the builder can mint a versioned value as it walks, which is
      what this and the widening both want.

- [ ] Copy propagation on `ValueId`. Source-stability is not subsumed by value
      equality — a write-once `x` whose source `y` is later reassigned can read
      equal ids yet be unsafe to fold. Revisit with `Select` / `Opaque`
      provenance.
- [ ] Induction-variable recognition (`Opaque` tagged `{ base, step }`). Not
      needed yet — post-increment reads already appear as `Add(opaque_i, step)` —
      so it lands when a rule first wants it.
- [ ] Plumb a callee's `stores` annotation into `alias::AliasCollector`, so a
      `&` / `&mut` on a local flowing into a `stores`-free callee stops marking
      the local aliased. The unconditional mark over-approximates the common
      `(&self).field` patterns.
- [ ] Directed gate propagation (callee-shrink → callers only). Deferred: it
      drops the edges `inline` adds to the build-once call graph, and a
      per-iteration rebuild does not recover them — the staleness is
      intra-iteration. It would need incremental edge maintenance, for a
      measured net-neutral gain.

Terminal ideal, gated behind measurement (see "Not equality saturation").

- [ ] Retire the remaining pure `ExprKind` variants from the skeleton, so every
      pure position is an `Operand::Value`.
- [ ] Saturation driver plus cost-based extraction: run rules to a bounded
      saturation, then extract a cost-minimal form per operand, materialising a
      multi-use value only when sharing beats duplication. This subsumes the
      extraction cost model, the global fixed-point loop, per-rule `applied`
      guards, and the dirty-set gate.

## Measured dead ends

Each was built, verified, and reverted. Do not retry as-is.

- **Incremental ValueGraph rebuild.** Reuse a parked graph's unchanged prefix and
  re-walk only the disturbed region. Correct (verified by a build-both-ways
  oracle) but it fired on ~0.15 % of builds on the large workload and 0 % on the
  small ones: the only clean pass adjacency was spoiled by non-journaling passes
  between handoffs, and `inline` restructures bodies wholesale every iteration.
  Raising the fire rate needs the single-worklist architecture, which subsumes
  the whole mechanism.
- **A richer graph cache** (`vg_cache` / `carry_vg_cache`). Caps low for the same
  reason: it rebuilds whenever a pass changes a function, which is the common
  case. Deleted.
- **Pooling the graph builder's output maps.** The build is compute-bound (walk +
  hash-cons + flow joins), not allocation-bound; measured no improvement.
- **Promoting induction-variable `Local` reads to source-bearing opaques.** One
  `ValueId` per local index spans every version of that local, and an induction
  variable has one per iteration. Traps `closure_for_loop_mutation`.
- **Freezing a local-naming value before the structural passes.** The early
  freeze is sound because a frozen value survives `inline` and `sroa` copying the
  operand around — true of a constant, which means the same thing wherever it
  lands, false of a value naming a local, because those passes renumber locals
  and splice a callee body into a caller, re-contextualizing the slot underneath
  the value. `String::substr_bytes`'s parameters, frozen early and inlined into
  `trim_start`'s loop, read back as an iteration's worth of values and trap with
  "allocation size too large". The invariant is now explicit at the freeze
  decision: early plants only context-free values.
- **A query-time entry-`FieldAccess` materialiser.** Miscompiled ~165 fixtures:
  reference and aggregate fields change copy / alias semantics.
- **Keeping caller values across a loop-free-but-impure inline.** Over-merges two
  reads of a `&mut` parameter.

The throughline: a value's identity is sound only when carried by an operand the
edits maintain, never re-derived from a side-table at query time.

## Consequences

- A cluster of former passes became graph structure and stopped existing: CSE and
  GVN fall out of hash-consing, pure copy propagation out of shared ids,
  store-load forwarding out of `(receiver, field, heap_ver)` identity. Their
  walks and bespoke analysis caches went with them.
- The optimizer gained a hash-cons pool, a builder, and an extractor; it lost
  the `value_of` side-table, the cache machinery, the engine's per-pass value
  rebuild, and every per-pass structural key.
- The arena costs one mutation discipline: the edit API is mandatory, and dead
  nodes accumulate until compaction lands.
- Measured wins along the way: the arena-direct engine cut a heavy module's
  optimize phase ~40 % against the bridge baseline; dirty-set gating cut it a
  further ~50 % on the Gale-generated SQLite parser at `-O2`.

## See also

- [`docs/optimizer.md`](./optimizer.md) — the pass inventory this architecture runs.
- [Normalized IR (NIR) Layer](./wep-2026-05-11-nir.md) — the type boundary at `lower` that NIR is.
- [Wasm IR (WIR) Layer](./wep-2026-02-14-wir-layer.md) — the separately-typed backend IR NIR extracts into.
- [Optimizer Remarks for Missed Optimizations](./wep-2026-06-03-optimizer-remarks.md) — how a missed rewrite is reported.
- Cranelift's aegraph mid-end and `egg` (https://egraphs-good.github.io/) — the build-once, eager-rewrite, single-extraction model this adapts (without their union-find over e-classes).
- `.claude/skills/profiling-wado-compiler` — the workflow behind the cost numbers.
