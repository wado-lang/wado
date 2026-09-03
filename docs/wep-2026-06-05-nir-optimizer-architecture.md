# WEP: NIR Optimizer Architecture

The NIR optimizer is a **two-tier IR** — a structured effect skeleton plus a
hash-consed graph of pure values — rewritten by a **worklist engine** over
per-function sessions, scheduled by a **per-function dirty-set gate**, and
**extracted once** into WIR.

This WEP is the design. The pass inventory lives in
[`docs/optimizer.md`](./optimizer.md); every implementation detail — node kinds,
rule predicates, pass ordering, which function holds what — is owned by the code
(`nir_arena.rs`, `nir_value_graph.rs`, `nir_engine.rs`, `optimize/`).

That division is what keeps this document true. A decision and the measurement
that settled it are dated facts: they do not go stale. A description of how the
code is arranged today goes stale the moment the code moves, and every such
sentence here is one more thing to keep in sync with a refactor that has no
reason to look. So this WEP states what was decided, why, what it obliges, and
what is still open — and names an identifier only where the decision is about
that identifier. A detail a reader needs in order to act correctly belongs in a
doc comment beside the code it describes, where it cannot drift.

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
A closure's call method and a non-trivial global initializer are bodies too, so
every pass reaches them without a special case.

- A node is addressed by a stable id, so a worklist entry survives an edit to
  the node it names — the handle the tree form could not offer. The id spaces
  are typed per node category rather than uniform, so a rule signature rejects a
  statement where an expression belongs.
- Only a node a rewrite can target independently bears an id. The records that
  are always rewritten through their parent live inline in it, and are
  transparent to the parent map.
- Spans are skeleton identity: they feed diagnostics, optimizer remarks, and
  DWARF. Layer 2 is the span-free layer, not this one.
- Parent edges and a per-local use index are derived once when a session opens
  and maintained incrementally by the edit API from there — never re-derived
  under a running session.
- Nodes are not freed mid-run. Liveness is reachability from the root, and the
  use index ignores orphans.

Document order is effect order. The skeleton carries statements, control flow,
and the expressions that are effectful or allocate; pure values have no place in
an order and live in Layer 2.

### Layer 2 — the live ValueGraph (`nir_value_graph.rs`)

Pure values are hash-consed into a per-function `ValuePool`, indexed by
`ValueId`: two structurally equivalent pure expressions share one id, and
equality is `==` on a `u32`. The graph is where pure values **live**, not a
side-table derived from the skeleton.

- The kinds cover what a pure value can be: constants, an opaque unknown,
  arithmetic, a structural merge, a loop recurrence, and a field read carried at
  a heap version.
- There is no kind naming a local. Flow is resolved into the graph **once, at
  build**: the builder threads a per-local current value, merges at joins,
  opens a recurrence at a loop, and bumps heap versions where the skeleton may
  write. A read's `ValueId` is fixed to the value dominant at that point, so
  reading a local is not an operation the graph can express — it is already
  resolved.
- Identity is structural, and a `ValueId` is stable once allocated: interning is
  pure hash-consing, with no e-class merges and so no representative lookup. A
  rewrite that proves `a ≡ b` points the operand slot at `b`; it never merges
  two ids. Rules apply eagerly and once per match, never searched to a fixed
  point.

Build once, rewrite eagerly, extract once — the shape Cranelift's aegraph
mid-end takes, minus its union-find.

A constant aggregate is one value however deep it is; materialising one is an
allocation, which is `const_object_globalization`'s decision, so an aggregate
constant is never frozen into an operand slot.

### The operand bridge

A skeleton child slot is `Operand = Expr(ExprId) | Value(ValueId)`. Pure operand
positions carry `Operand::Value`; effectful and control-flow operands stay
`Operand::Expr`.

Values are **born as operands**: lowering and the promotion passes freeze a pure
position into its `ValueId` directly, so a pass reads the value off the operand
rather than looking an expression up.

No expression-keyed value map is persisted. A scoped scratch walk may build one
to answer a query, but it dies with the query. An unpromoted skeleton leaf
therefore resolves to no value, which is sound because that is the finest
partition — a consumer skips the expression rather than over-merging.

Promotion is staged against what the passes need to see, and the staging is the
design: a freeze is only sound where nothing later re-contextualizes the operand
it plants. Arithmetic freezes before the loop, on a clean graph; field reads
freeze once the structural passes have settled the shape they read; the last
arithmetic freeze runs after every pass that walks arithmetic.

### Extraction

One pass materialises each promoted operand back into concrete form, at WIR
build, reading each value's type from the pool and recursing on composites. The
optimizer can extract a constant on its own, which is what lets a pass read a
frozen value without waiting for the backend.

Extraction currently re-materialises a value at each use — always correct, and
for constants always cheaper than sharing. The share-vs-duplicate cost model for
multi-use non-constant values is open (see Remaining work).

### The engine (`nir_engine.rs`)

Genuinely-local rewrites run as `Rule`s on a worklist over one function's `Body`,
to a local fixed point: a node is revisited only when an edit may have made it
reducible, never by a whole-tree sweep.

- A session derives its indices and seeds its worklist in one linear pass, then
  maintains them.
- Rules never touch the arena directly; every mutation goes through the edit
  API, which is what keeps the indices and the promoted-read census coherent and
  re-enqueues exactly the affected neighbourhood. Replacement is in place, so an
  id — and the worklist entry naming it — survives the edit.
- At a popped node the engine tries the registered rules in order; the first that
  reports a change retries at the same node. Rules must be idempotent, and either
  confluent or priority-ordered — priority is rule order in the session.

The position-flexible local rules share **one session per function**, run
pre-inline and post-inline so each rule sees the instruction window the other
exposes. Which rules those are is the inventory's.

What stays outside the engine: flow-sensitive passes that need per-block dataflow
keep their own walkers over the arena, and the interprocedural stages run as
distinct steps.

### Per-function gating

Each function carries a monotonic revision and each pass a per-function
watermark; a gated pass visits a function only when the revision has passed the
watermark. A pass that changes a function bumps its revision and, conservatively,
that of its immediate call-graph neighbours in both directions. Interprocedural
passes pull their candidate set from the dirty set rather than scanning every
function, and re-mark affected callers when a callee shrinks; the loop's
iteration cap is the quiescence bound. A pass whose summary a call-graph change
invalidates cannot be gated on the callee's revision alone, and stays explicit.

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
- Heap-version monotonicity. A field read's heap version is the version before
  the read; a later write bumps to a fresh version, so any read after it gets a
  fresh `ValueId`.
- Single-version proof for a query-time leaf. The build is the primary source of
  a local's value, but a leaf the build did not reach may be resolved at query
  time instead, to a version-free value standing for that local. Version-free is
  the whole hazard: it is one value for every assignment of the local, so it is
  sound only under a proof that the local has exactly one. Absent the proof the
  query answers with no value, never with a guess. The anchor rule below is the
  same obligation seen from the freeze side.
- Pointwise maintenance. A structural edit keeps the graph coherent at the point
  of the edit — pruning a branch repoints the surviving operands past the dead
  merge arm; splicing an inlined body or an SROA split interns value nodes for
  the new skeleton subtree. Monotone growth, never a re-derivation of existing
  flow. This is the load-bearing claim of the design.
- Extraction equivalence. The extracted form computes, for every effectful
  position, the same values in the same effect order as graph + skeleton.
- The edit API is the only mutation path during a run. A pass that pokes the
  arena maps directly desynchronises the parent and use indices.

### Standing invariants — do not reintroduce

These are settled by measurement and re-derived at a cost.

- No mid-pipeline rebuild of the value graph. Build-once is structural, not a
  cache policy: nothing clears the graph. Maintain in place; never
  clear-and-rebuild.
- No expression-keyed value cache or side-table that outlives a query. A pass
  needing a value uses born-as-operands or a scoped scratch walk.
- A promoted read lives in the pool, not the skeleton. A pass that decides a
  local is unused, or rewrites every read of one, must count the pool's reads as
  well — scoped to the operands the skeleton still carries, since the pool is
  append-only and also holds reads that folded away.
- That census is session-scoped, never per-application: it walks the whole body,
  so recomputing it per rule application is quadratic. It is memoized, and the
  memo is held across edits while it is empty — the case that decides the cost,
  since inside the loop nothing reachable names a local. Only a local-naming
  operand becoming reachable drops it, which the edit API is what reports; a
  rule therefore never writes such an operand into the body behind the API's
  back. A debug-only audit at session end is a backstop, not a proof.
- No whole-body walk per rule application, in an assertion either. A rewrite
  that deletes a binding records the local and the session audits the batch
  once; checking it at each rewrite cost more than half the loop. The audit
  reads the arena fresh rather than the indices the rewrite decided on —
  substituting those would make it agree with itself and catch nothing.

## Rejected and deferred

### Not sea-of-nodes

Sea-of-nodes, and dropping the skeleton, stay rejected.

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
JIT cannot do). Two things would have to exist first: tooling that makes a value
graph and a rule firing visible, since a saturating rule set is not debuggable by
reading output; and budget-bounded rule sets, where a budget hit degrades
gracefully to the partially saturated graph, never panicking and never producing
worse output than the input.

### Not a session index carried across passes

Opening a session rebuilds the parent map and use index — 1.03 s of a
~5.8 s loop over 36 000 sessions. Carrying it across passes was built and
measured: 91 % of sessions reuse it and the derivation drops 68 %, but the loop
total does not move, and three things argue against it.

- It optimizes the structure the terminal ideal retires (see the pure-node-kind
  item), and does nothing for the census the precision items load.
- Reuse changes the output (+3.3 KB), so the carried state is not equivalent.
- Soundness would rest on 36 `invalidate_engine_index` calls staying correct by
  hand, each omission a silent miscompile.

Closing the arena's mutation surface — so that the type system, rather than a
reviewer, carries the invariant that every edit goes through the API — is the
prerequisite that would change this verdict. Until then the surface is whatever
the current passes leave open, which is a thing to measure at the time, not a
number to record here.

### Not incremental rebuild, and not a richer cache

Both were prototyped and measured, and both are the wrong shape: they make
re-deriving a derived analysis cheaper, when the analysis only needs re-deriving
because it is derived. See "Measured dead ends".

## Remaining work

Compile speed here means the debug build — the compiler-developer inner loop —
so every timing in this WEP includes `debug_assert!`s.

The graph build is ~6 % of the phase (was ~21 % before build-once); what is left
to cut is the passes and their assertions. At `-O2` the loop costs ~6 s on each
benchmark, spread over `peephole` (23 %) and `copy_prop` / `const_fold` / `licm`
(13 % each), with no dominant iteration.

- [ ] Fold the graph build into `lower` (born-at-`lower`). Buys one body walk
      per function, not earlier availability: the early freeze already walks
      every body before the loop, so the lazy first-query path is eager in
      practice.

- [ ] Retire the pure node kinds still left in the skeleton, so every pure
      position is an operand. A compile-speed item as much as a saturation
      prerequisite: pure kinds were 52 % of the arena when last measured, and
      the local reads the use index is made of 34 %, so retiring them roughly
      halves what every session walk covers — and the other compile-speed items
      are sized against a skeleton this shrinks. Almost none of it is
      independently reachable, because a value query recurses through operands:
      arithmetic left in the skeleton bottoms out on a field load or a call
      result and yields no value at all, rather than being gated on something a
      flag could flip. It waits on Precision below.

- [ ] Arena compaction. In-place rewrites orphan nodes that are never freed
      mid-run (~1.66× bloat measured at end-of-optimize on `package-gale`).

- [ ] Price the switch lowering on something other than how wide the table is.
      The threshold is a count of the values the table covers, set where the
      `br_table` starts paying on the benchmarks, but the count is a proxy: a
      40-arm synthetic of plain `i32` fields prefers the `else if` chain by
      11 % where `cbor-twitter`'s 40-arm `User` prefers the table by 2 %.
      Whatever separates those two — arm body size, how well the scrutinee
      sequence predicts — is what the threshold should be reading. Arm body size
      is already summed, but as a blow-up guard on the emitted table, never as
      the pay-off predicate.

Precision. All of it waits on one rule.

### The anchor rule

A frozen operand may name a local only when that local's defining statement
travels with the operand.

A source local fails that. A parameter is defined by the function entry, which
is not a statement that can travel, so inlining splices the operand into a
caller where the same slot is assigned once per call — and since a query-time
local value is version-free, two reads that now denote different values share an
id. That is the over-merge behind `String::substr_bytes` under "Measured dead
ends". The guard used to be "every leaf is a parameter", a proxy for "this local
has one version" that inlining invalidates, parameter-ness not surviving being
inlined; it is now that predicate itself, plus a dominance check at the
placement.

A freeze-minted temp satisfies the rule by construction: a fresh immutable
binding, in the same body, assigned once, so any pass that copies the operand
copies the definition with it and the one-value-per-binding property holds in
every context it lands in. So: never freeze a value naming a source local —
materialise it into such a temp and name that. The freeze already mints one for
the multi-use case; the rule makes it the only way a local may be named, and
replaces the parameter restriction.

The rule is necessary and not sufficient. There is no in-loop freeze: one was
built under it and promoted nothing on either benchmark, at 11.3 % of the loop,
so it was removed, phase and all — reviving it means adding the phase back under
the rule, not flipping a flag. Two things have to hold and neither did.

The material has to exist. Resolving every provably single-version local, not
just parameters, changes no output on its own, because the leaves that matter
are field loads and call results — and a field read is keyed on the heap version
at its point, which no query-time re-derivation supplies. The builder is the
only source, through the graph build or a scratch re-walk.

A consumer has to want it. The candidate was hoisting a loop-invariant field
load, and no value-graph consumer does it: LICM's value path excludes field
reads, and its structural path, which does hoist them, is deliberately
value-graph-free — so the one pass that wants the fact derives it without
asking. Surveyed, in-loop field reads that are provably invariant are 2.7 % and
7.1 % on the two benchmarks — too few to pay for maintaining promoted operands
across inlining and SROA, which an in-loop freeze would need.

One note for whoever revives this. The invariance test is free: every slot a
loop may write is bumped before the body walk and versions are monotonic, so a
read below the loop-entry watermark is invariant.

What used to hold that 2.7 % down was upstream, and is fixed: the loop
heap-effect collection marked a call's receiver mutably borrowed whether or not
the callee could mutate it, disagreeing with the alias analysis beside it, which
asks that very question before aliasing the same receiver. Both now read one
verdict. It bought no output: the fixture corpus is WIR-identical over 1 921
goldens, and so is every benchmark. The reason is this section's other half —
still no consumer. A field read of a reference parameter across an immutable
call in a loop is already hoisted by LICM's structural path, which never asks
the value graph, and the value-side consumers do not reach the shape at all,
because a local struct is scalarized by SROA and a small callee is gone to the
inliner before either matters. So the precision is real and unclaimed, and the
consumer, not the precision, is what to build next.

- [ ] Reach the in-loop consumers. Every freeze that may plant a local-naming
      value runs after the fixed-point loop, so the passes inside it still see
      none: LICM reads the loop-entry values and its value hoist collected zero
      loop-entry locals in 10,900 queries, which is why keeping that map across
      the loop costs nothing either way. An in-loop freeze cannot simply be
      added — one was, and is recorded under "The anchor rule"; the early freeze
      is separately bound by the context-free rule under "Measured dead ends".
      Moving the build to lowering does not lift that bound: it is the
      extraction that is point-dependent, not the build. This and the widening
      above share one prerequisite, the anchor rule opening this section, which
      also gates nearly all of retiring the pure node kinds — not only the local
      reads, since the arithmetic above a local read resolves to no value
      either.

- [ ] Copy propagation on value identity rather than on locals. Source-stability
      is not subsumed by value equality — a write-once `x` whose source `y` is
      later reassigned can read equal ids yet be unsafe to fold. Revisit with
      provenance on the merge and opaque kinds.
- [ ] Induction-variable recognition, as a tagged opaque carrying base and step.
      Not needed yet — a post-increment read already appears as the addition it
      is — so it lands when a rule first wants it.
- [ ] Stop a borrow of a local that flows only into callees which cannot retain
      it from marking the local aliased. Plumbing the callee's escape summary
      into the ref-argument check is not the way: the aliased set is seeded from
      the address-taken locals the elaborator records for every borrow of a
      local, so the `(&self).field` shape this was written for is already
      aliased before the call site is looked at. Measured, 3.7 % of ref-arg
      marks are the sole reason their local is aliased — borrows of a
      projection, which the elaborator does not record. Narrowing the rest means
      narrowing the seed, which is a whole-function question (do _all_ uses of
      the borrow flow into callees that cannot retain it?) over an annotation
      several other passes also read. Note too that the mutable-escape set is
      built by filtering the aliased set, so dropping a local from one silently
      drops it from the other — and a callee that cannot retain a reference can
      still mutate through it during the call, so the two have to be decoupled
      first.
- [ ] Directed gate propagation, callee-shrink to callers only. Deferred: it
      drops the edges inlining adds to the build-once call graph, and a
      per-iteration rebuild does not recover them — the staleness is
      intra-iteration. It would need incremental edge maintenance, for a
      measured net-neutral gain.

Terminal ideal, gated behind measurement (see "Not equality saturation").

- [ ] Saturation driver plus cost-based extraction: run rules to a bounded
      saturation, then extract a cost-minimal form per operand, materialising a
      multi-use value only when sharing beats duplication. This subsumes the
      extraction cost model, the global fixed-point loop, the per-rule
      did-anything-change guards, and the dirty-set gate.

## Measured dead ends

Each was built, verified, and reverted. Do not retry as-is.

- **Incremental ValueGraph rebuild.** Reuse a parked graph's unchanged prefix and
  re-walk only the disturbed region. Correct (verified by a build-both-ways
  oracle) but it fired on ~0.15 % of builds on the large workload and 0 % on the
  small ones: the only clean pass adjacency was spoiled by non-journaling passes
  between handoffs, and `inline` restructures bodies wholesale every iteration.
  Raising the fire rate needs the single-worklist architecture, which subsumes
  the whole mechanism.
- **A richer graph cache.** Caps low for the same reason: it rebuilds whenever a
  pass changes a function, which is the common case. Deleted.
- **Pooling the graph builder's output maps.** The build is compute-bound (walk +
  hash-cons + flow joins), not allocation-bound; measured no improvement.
- **Promoting induction-variable local reads to source-bearing opaques.** That
  resolver keyed one `ValueId` per local, where the builder mints one per
  assignment, so the id spanned every version of the local — and an induction
  variable has one per iteration. Traps `closure_for_loop_mutation`.
- **Freezing a local-naming value before the structural passes.** The early
  freeze is sound because a frozen value survives inlining and SROA copying the
  operand around — true of a constant, which means the same thing wherever it
  lands, false of a value naming a local, because those passes renumber locals
  and splice a callee body into a caller, re-contextualizing the slot underneath
  the value. `String::substr_bytes`'s parameters, frozen early and inlined into
  `trim_start`'s loop, read back as an iteration's worth of values and trap with
  "allocation size too large". The invariant is now explicit at the freeze
  decision: early plants only context-free values.
- **A query-time materialiser for a field read at function entry.** Miscompiled
  ~165 fixtures: reference and aggregate fields change copy / alias semantics.
- **Keeping caller values across a loop-free-but-impure inline.** Over-merges two
  reads of a mutable reference parameter.
- **Dropping the promoted-read census memo on every edit.** The obvious
  invalidation rule, and measurably worse than no memo: a whole-body walk per
  rewrite where the per-block recomputation it replaced at least amortised over
  a block. Only holding an empty memo across edits pays.

The throughline: a value's identity is sound only when carried by an operand the
edits maintain, or by a leaf whose single version the query itself proves — never
re-derived from a side-table at query time.

## Consequences

- A cluster of former passes became graph structure and stopped existing: CSE and
  GVN fall out of hash-consing, pure copy propagation out of shared ids,
  store-load forwarding out of receiver-field-version identity. Their walks and
  bespoke analysis caches went with them.
- The optimizer gained a hash-cons pool, a builder, and an extractor; it lost
  the persisted value side-table, the cache machinery, the per-pass value
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
