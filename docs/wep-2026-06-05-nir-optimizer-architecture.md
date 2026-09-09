# WEP: NIR Optimizer Architecture

The NIR optimizer is a two-tier IR: a structured effect skeleton plus a
hash-consed graph of pure values. A worklist engine rewrites it one function at
a time, a per-function dirty-set gate schedules the passes, and the result is
extracted once into WIR.

This WEP records decisions, the measurements that settled them, the obligations
they impose, and what is still open. It names an identifier only when the
decision is about that identifier. Everything else is the code's to say: node
kinds, rule predicates, pass order, and which function holds what live in
`nir_arena.rs`, `nir_value_graph.rs`, `nir_engine.rs`, and `optimize/`, and the
pass inventory in [`docs/optimizer.md`](./optimizer.md). A decision does not go
stale. A description of the code does, the moment the code moves, so a detail a
reader needs in order to act belongs in a doc comment beside the code.

## Terms

- Skeleton: the tier that carries execution order. Statements, control flow,
  calls, assignments, allocation. Document order is effect order. Pure values
  have no place in an order, so they are not stored here.
- Hash-consing: interning a value by its structure, so two structurally
  identical values get the same `ValueId` when built. Equality is `==` on a
  `u32`, and a common subexpression is common before any pass looks at it.
- Promoted: a skeleton operand slot that holds a `ValueId` instead of a
  sub-expression node. The value has moved out of the skeleton into the pool.

## Context

The optimizer was about 31 independent passes. Each was a full mutating walk over
an owned tree body, inside a global fixed-point loop. That shape had two costs.

- No stable handles and no parent or use edges. `&mut NirExpr` recursion owned
  the path to a node, so nothing could hold a node across an edit. A worklist
  could not be expressed, and CSE hand-rolled a structural `CseKey` because NIR
  offered nothing better.
- Every pass re-derived its own analysis. Pure-value identity was a per-pass
  key. Reaching definitions were rebuilt per pass (`KnownValues`, `DefMap`,
  `env` / `field_env`). A native profile of `wado compile -O2` on `package-gale`
  put `optimize` at 52–65 % of the compile, with the per-function value graph
  rebuilt 2.67 times per function and about 27 % of CPU in the allocation churn
  those rebuilds generate.

The second cost is structural. An analysis derived from a mutable tree must be
re-derived after every mutation. The fix is to stop deriving it.

## Decision

### Layer 1: the skeleton arena (`nir_arena.rs`)

A function body is a per-function arena, `Body`. It is the canonical post-lower
form: `lower::translate` builds it and `wir_build` reads it. A closure's call
method and a non-trivial global initializer are bodies too, so every pass reaches
them without a special case.

- A node is addressed by a stable id, so a worklist entry survives an edit to
  the node it names. The tree form could not offer that handle. The id spaces
  are typed per node category, so a rule signature rejects a statement where an
  expression belongs.
- Only a node a rewrite can target on its own bears an id. A record that is
  always rewritten through its parent lives inline in that parent and is
  transparent to the parent map.
- Spans are skeleton identity. They feed diagnostics, optimizer remarks, and
  DWARF. Layer 2 is the span-free layer.
- Parent edges and a per-local use index are derived once when a session opens.
  The edit API maintains them from then on. They are never re-derived under a
  running session.
- Nodes are not freed mid-run. Liveness is reachability from the root, and the
  use index ignores orphans.

### Layer 2: the live ValueGraph (`nir_value_graph.rs`)

Pure values are hash-consed into a per-function `ValuePool`, indexed by
`ValueId`. Two structurally equivalent pure expressions share one id. The graph
is where pure values live. It is not a side-table derived from the skeleton.

- The kinds cover what a pure value can be: a constant, an opaque unknown,
  arithmetic, a structural merge, a loop recurrence, and a field read carried at
  a heap version.
- There is no kind naming a local. Flow is resolved into the graph once, at
  build. The builder threads a per-local current value, merges at joins, opens a
  recurrence at a loop, and bumps heap versions where the skeleton may write. A
  read's `ValueId` is fixed to the value dominant at that point, so the graph
  cannot express "read a local"; that is already resolved.
- Identity is structural, and a `ValueId` is stable once allocated. Interning is
  pure hash-consing, with no e-class merges and so no representative lookup. A
  rewrite that proves `a ≡ b` points the operand slot at `b`. It never merges
  two ids. Rules apply eagerly and once per match, never searched to a fixed
  point.

Build once, rewrite eagerly, extract once. This is the shape of Cranelift's
aegraph mid-end, minus its union-find.

A constant aggregate is one value however deep it is. Materialising it is an
allocation, and where an allocation goes is `const_object_globalization`'s
decision, so an aggregate constant is never frozen into an operand slot.

### The operand bridge

A skeleton child slot is `Operand = Expr(ExprId) | Value(ValueId)`. A pure
operand position carries `Operand::Value`. An effectful or control-flow operand
stays `Operand::Expr`.

Values are born as operands. Lowering and the promotion passes freeze a pure
position into its `ValueId` directly, so a pass reads the value off the operand
rather than looking an expression up.

No expression-keyed value map is persisted. A scoped scratch walk may build one
to answer a query, and it dies with the query. An unpromoted skeleton leaf
therefore resolves to no value. That is sound because "no value" is the finest
partition: a consumer skips the expression rather than over-merging.

Promotion is staged, and the staging is the design. A freeze is sound only where
nothing later re-contextualizes the operand it plants. Arithmetic freezes before
the loop, on a clean graph. Field reads freeze once the structural passes have
settled the shape they read. The last arithmetic freeze runs after every pass
that walks arithmetic.

### Extraction

One pass materialises each promoted operand back into concrete form, at WIR
build. It reads each value's type from the pool and recurses on composites. The
optimizer can extract a constant on its own, so a pass can read a frozen value
without waiting for the backend.

A scalar value is re-materialised at each use. That is always correct, and for a
constant always cheaper than sharing. A multi-use field read may be shared. The
cost model for sharing a multi-use non-constant scalar is open; see Remaining
work.

### The engine (`nir_engine.rs`)

Local rewrites run as `Rule`s on a worklist over one function's `Body`, to a
local fixed point. A node is revisited only when an edit may have made it
reducible, never by a whole-tree sweep.

- A session derives its indices and seeds its worklist in one linear pass, then
  maintains them.
- Rules never touch the arena directly. Every mutation goes through the edit
  API. That is what keeps the indices and the promoted-read census coherent, and
  re-enqueues exactly the affected neighbourhood. Replacement is in place, so an
  id survives the edit, and with it the worklist entry naming it.
- At a popped node the engine tries the registered rules in order. The first
  that reports a change retries at the same node. Rules must be idempotent, and
  either confluent or priority-ordered. Priority is rule order in the session.

The position-flexible local rules share one session per function. It runs
pre-inline and post-inline, so each rule sees the instruction window the other
exposes. Which rules those are is the inventory's.

Two things stay outside the engine. A flow-sensitive pass that needs per-block
dataflow keeps its own walker over the arena. The interprocedural stages run as
distinct steps.

### Per-function gating

Each function carries a monotonic revision and each pass a per-function
watermark. A gated pass visits a function only when its revision has passed the
watermark. A pass that changes a function bumps its revision and, conservatively,
that of its immediate call-graph neighbours in both directions. An
interprocedural pass pulls its candidates from the dirty set rather than scanning
every function, and re-marks the callers of a callee it shrinks. The loop's
iteration cap is the quiescence bound. A pass whose summary a call-graph change
invalidates cannot be gated on the callee's revision alone, and stays explicit.

An empty column skips the pass before its whole-program setup rather than after.
A pass builds its catalogs, callee maps, effect summaries and call-site censuses
before it reaches its first function, so a late round that visits nothing would
otherwise spend all its time there.

Gating changes only which functions a pass visits, never the result of a visit.
Every loop pass is an optimization, so an imprecise gate costs optimization
quality, never correctness. The same one-sided argument covers the
interprocedural over-approximation.

## Soundness invariants

- Substitution soundness. An operand is repointed from `a` to `b` only when the
  two denote the same value in every execution reaching that point. CSE, copy
  propagation, and forwarding each discharged that obligation separately. It is
  now expressed once. Sharing an id is stronger and needs no justification:
  hash-consing gives one id only to structurally identical values.
- Flow-freeze validity. A control-flow rewrite that changes which value is
  dominant must repoint the affected operands. A read must never be left holding
  a value that no longer reaches it.
- Heap-version monotonicity. A field read's heap version is the version before
  the read. A later write bumps to a fresh version, so any read after it gets a
  fresh `ValueId`.
- Single-version proof for a query-time leaf. The build is the primary source of
  a local's value. A leaf the build did not reach may be resolved at query time
  instead, to a version-free value standing for that local. Version-free is the
  hazard: it is one value for every assignment of the local, so it is sound only
  under a proof that the local has exactly one. Without the proof the query
  answers with no value, never with a guess.
- Pointwise maintenance. A structural edit keeps the graph coherent at the point
  of the edit. Pruning a branch repoints the surviving operands past the dead
  merge arm. Splicing an inlined body or an SROA split interns value nodes for
  the new skeleton subtree. Growth is monotone. Existing flow is never
  re-derived. This is the load-bearing claim of the design.
- Extraction equivalence. For every effectful position, the extracted form
  computes the same values in the same effect order as graph plus skeleton.
- The edit API is the only mutation path during a run. A pass that pokes the
  arena maps directly desynchronises the parent and use indices.
- One verdict per call. The straight-line walk and the loop summary classify a
  call from the same per-call verdicts. Two paths answering the same question
  with different predicates was the shape of every precision defect found in
  this layer.

### Standing invariants: do not reintroduce

These are settled by measurement and re-derived at a cost.

- No mid-pipeline rebuild of the value graph. Build-once is structural, not a
  cache policy. Nothing clears the graph. Maintain in place.
- No expression-keyed value cache or side-table that outlives a query. A pass
  needing a value uses born-as-operands or a scoped scratch walk.
- A promoted read lives in the pool, not the skeleton. A pass that decides a
  local is unused, or rewrites every read of one, must count the pool's reads as
  well. The count is scoped to the operands the skeleton still carries, since the
  pool is append-only and also holds reads that folded away.
- That census is session-scoped, never per-application. It walks the whole
  body, so recomputing it per rule application is quadratic. It is memoized, and
  the memo is maintained across edits rather than dropped: an operand becoming
  reachable adds its locals, and one leaving is never subtracted, so the memo is
  an upper bound. Over-counting keeps a binding alive, costing an elision and
  never correctness. The edit API is what reports an arrival, so a rule never
  writes an operand into the body behind its back. A debug-only audit at session
  end checks the bound, not equality — a backstop, not a proof.
- No whole-body walk per rule application, in an assertion either. A rewrite
  that deletes a binding records the local, and the session audits the batch
  once. Checking at each rewrite cost more than half the loop. The audit reads
  the arena fresh rather than the indices the rewrite decided on. Reading those
  would make it agree with itself and catch nothing.
- Per-loop heap summarization is a union over the whole body. It is sound and
  necessary, not a conservatism: the body runs many times, so every write in it
  must be assumed to precede every read. Its granularity is the ceiling on what
  a loop can know about its heap.

## Rejected and deferred

### Not sea-of-nodes

Sea-of-nodes, and dropping the skeleton, stay rejected.

### Not equality saturation

Saturation over the value graph would unlock algebraic exploration:
re-association, the distributive law, strength reduction per use, cost-based
share-versus-duplicate. Wado's output is Wasm, which the host runtime JITs
again, and the host redoes most of that algebra. Cranelift's aegraph reports
5–10 % on native AOT, and the JIT-target number is much smaller. The
optimizations Wado carries are single-direction structural rewrites that
saturation does not help: allocation elimination, structural cleanup, dense
`Match` lowering, bounds-check elimination, inlining.

So saturation stays the terminal ideal, activated only if measurement justifies
it, through a native-AOT backend or Wasm output that measurably benefits from
algebra the JIT cannot do. Two things would have to exist first. One is tooling
that makes a value graph and a rule firing visible, since a saturating rule set
cannot be debugged from its output. The other is budget-bounded rule sets, where
a budget hit degrades to the partially saturated graph, never panics, and never
produces worse output than the input.

### Not a session index carried across passes

Opening a session rebuilds the parent map and use index: 1.03 s of a 5.8 s loop
over 36 000 sessions. Carrying the index across passes was built and measured.
91 % of sessions reuse it and the derivation drops 68 %, but the loop total does
not move. Three things argue against it.

- It optimizes the structure the terminal ideal retires (the pure-node-kind
  item), and does nothing for the census the precision items load.
- Reuse changes the output by 3.3 KB, so the carried state is not equivalent.
- Soundness would rest on 36 `invalidate_engine_index` calls staying correct by
  hand, each omission a silent miscompile.

The prerequisite that would change this verdict is closing the arena's mutation
surface, so that the type system rather than a reviewer carries the invariant
that every edit goes through the API. Until then the surface is whatever the
current passes leave open. Measure it at the time; it is not a number to record
here.

### Not incremental rebuild, and not a richer cache

Both were prototyped and measured, and both are the wrong shape. They make
re-deriving a derived analysis cheaper, when the analysis needs re-deriving only
because it is derived. See Measured dead ends.

## Remaining work

Compile speed here means the debug build, the compiler-developer inner loop, so
every timing in this WEP includes `debug_assert!`s.

`optimize` is 71 % of a warm `-O2` compile of the Gale-generated SQLite parser
and 78 % of `json_twitter`. The fixed-point loop is about a third of that, spread
over `peephole` at 21 %, `const_fold` at 16 %, and `inline`, `copy_prop` and
`licm` at about 12 % each, with no dominant iteration.

Self CPU says something the per-pass spans do not, and it is what the items below
are sized against. The generic arena walks — `for_each_child`,
`for_each_reachable_node`, `for_each_operand` — are 14 % of the whole compile, and
allocation another 15 %. `Engine::new`'s index derivation is 4.7 %. The graph
build is about 6 % of the phase, down from 21 % before build-once, while the whole
`nir_value_graph` module is 1.2 % of self CPU: what the build costs is the walk it
shares with every other pass, not its own arithmetic.
`WADO_TRACE=arena_census` reports the node-mix and bloat numbers these items
quote.

Two things price a walk. One is how many nodes it covers: every step runs the
inlined slot match, and 61 to 77 % of the nodes are pure kinds carrying no
children. The other is the allocator, and it was the larger of the two: the
walks allocated a buffer per node, and retiring those cut the parser's compile
6.9 % and `json_twitter`'s 4.2 %.

A buffer exists to break `for_each_child`'s borrow, which a walk cannot hold
while it mutates. Three cases, and only the last needs one.

- The walk only reads. It recurses inside the callback and needs no buffer. This
  covers `walk_nodes_under`, so the arena's own traversal has no explicit stack:
  the arena is a tree and the call stack is the traversal stack.
- The walk mutates something other than the body. `Engine`'s parent map and
  worklist are fields beside `Body`, not inside it, so splitting the borrow lets
  the callback write them.
- The walk mutates the body as it recurses. It threads one `Vec` down: each
  level appends its children at the tail, indexes them out by value, and
  truncates back, so the peak is the deepest path and the cost is one
  allocation for the walk.

No walk buffer is thread-local. A pooled one was tried and measured the same as
recursion, which leaves nothing to pay for the hidden state.

Siblings are visited in source order, matching `for_each_child`. The stack walk
this replaced visited them in reverse, which nothing documented. The order does
reach the output: it permutes `TypeId` and const-object numbering, and the order
two specializations of one function are emitted in. Code size is unchanged on
both benchmarks.

- [ ] Fold the graph build into `lower`. The build's inputs do not exist during
      lowering. `builder::build` takes the alias sets and the per-call purity
      verdicts, and both are derived over the whole lowered package. An empty
      alias set is the _optimistic_ direction, not a conservative one, so
      building without them is unsound rather than imprecise. A call's verdict is
      keyed by `FuncId` besides, and `translate` finalizes callee ids only at its
      end. Keeping the facts therefore means building once every body exists,
      which is the separate walk this item wanted to remove. What it buys is that
      one body walk per function, not earlier availability: the early freeze
      already walks every body before the loop, so the lazy first-query path is
      eager in practice.

- [ ] Retire the pure node kinds still left in the skeleton, so every pure
      position is an operand. This is a compile-speed item as much as a
      saturation prerequisite. Pure kinds are 61 % of the reachable expressions
      after lower and 77 % at end-of-optimize on the SQLite parser, 61 % and 70 %
      on `json_twitter`. The local reads the use index is made of are 31–39 % of
      them. Retiring them therefore cuts what every session walk covers by about
      two thirds rather than half. The share climbs across the loop because the
      freeze reaches so little: only 17 % of the reachable operand slots carry a
      value, so a pure kind stays in the skeleton for want of a freeze. The other
      compile-speed items are sized against a skeleton this shrinks. Almost none
      of it is reachable on its own. A value query recurses through operands, so
      arithmetic left in the skeleton bottoms out on a field load or a call
      result and yields no value at all. It waits on a consumer; see Precision.

- [ ] Arena compaction. In-place rewrites orphan nodes that are never freed
      mid-run. At end-of-optimize the arena holds 1.9× the reachable expressions
      and 2.4× the reachable statements on the SQLite parser, and 2.5× / 3.5× on
      `json_twitter`. `lower` alone arrives at 1.1×, so the growth is the loop's.

- [ ] Price the switch lowering on something other than table width. The
      threshold is a count of the values the table covers, set where the
      `br_table` starts paying on the benchmarks. The count is a proxy. A
      40-arm synthetic of plain `i32` fields prefers the `else if` chain by
      11 %, and `cbor-twitter`'s 40-arm `User` prefers the table by 2 %. What
      separates those two is what the threshold should read: arm body size, or
      how well the scrutinee sequence predicts. Arm body size is already summed,
      but only as a blow-up guard on the emitted table, never as the pay-off
      predicate.

### Precision

Every precision item below waits on a consumer, not the other way round. That
is the measured finding of this section.

The value graph's heap precision is not on the critical path of any consumer
that exists. The escaped-set generation invalidates 66 % of local-rooted field
reads on `sqlite_parse` and 43 % on `json_twitter`. Five conservatisms feeding
that generation were removed, each measured to fire widely, and every one left
the corpus WIR-identical over 1 921 goldens and every benchmark.

The reason is visible on the shape they targeted: a mutable receiver whose
fields are read repeatedly in a loop around calls on it. The emitted loop
already carries the invariant reads hoisted and the mutable field shadowed in a
local. LICM's structural path and `field_scalarize` do that, and neither asks
the value graph. What the escaped generation invalidates is either a read the
structural passes have already retired, or a read between calls that genuinely
mutate. No further precision at the call boundary changes that until a consumer
wants what the structural passes do not already take; field-granular mod/ref
was designed on the way and is priced out by the same fact. Each removed
conservatism stands on the principle that an unjustified conservatism is dropped
for being unjustified. None is a performance change.

### Naming a local, before and after

The early freeze plants no value naming a local. That refusal was a soundness
rule. It is now a cost decision, and the two reasons are worth keeping apart.

It was covering five miscompiles, each one a place where the skeleton had an
ability the pool did not. `scalar_forward` forwarded a scalar temp and dropped
its binding while counting only the skeleton's reads, so a read living in the
value pool was left naming a slot that no longer existed and the extractor
emitted a `local.get` of it. `copy_prop` forwarded a promoted binding that was
not constant. `dae` read "promoted" as "safe to delete", but purity is not the
question it asks: `100 / zero` is pure and still traps. `niri` stopped at the
pool instead of evaluating the operand behind it. The pool folded no
short-circuit identity, so `x || false` survived in a promoted operand that the
skeleton would have collapsed.

Counting pooled reads is each pass's own obligation, and `optimize::run_pass`
audits the invariant — every local a promoted operand reads still has a
definition — at end-of-optimize, or at every pass boundary under
`WADO_TRACE=promoted_reads`, which is what named `scalar_forward`. All five are
fixed: with the refusal removed the corpus is correct, 4 977 of 4 982 e2e
fixtures passing and every one of the five failures a `wir_expect` shape
assertion rather than a miscompile.

The rule's own account of itself was wrong, and the mistake is worth recording.
It read the failure as an over-merge, one version-free id standing for two
runtime values. The reductions that account predicts do not reproduce.
Arithmetic over a parameter, inlined into a loop, is correct. Identity per read
rather than per local changes nothing. `e2e/opt_early_freeze_names_a_local`
reduces the real failure, and it is a dropped binding, not a merged value.

What stays unpaid is cost. Removing the refusal and keeping everything else
measures three things.

- ~~The compile took 80 % longer, the promoted-read census re-walking at 14 % of
  self CPU.~~ Paid. The census adds to its memo rather than dropping it, so it
  is an upper bound maintained per edit instead of an equality re-derived per
  query. Over-counting keeps a binding alive, which costs an elision and never
  correctness; the session-end audit checks the bound, not equality. With the
  refusal removed the compile is back to its baseline.
- The parser's code grows 6.4 %. Two thirds of that is the extraction cost
  model, one third is `scalar_forward` going blind; see below.
- The fixed-point loop stops converging inside its cap, with `copy_prop` and
  `licm` still changing at 15 iterations.

The refusal stands until those two are closed, and they are not next in line.
Removing it buys no compile time on its own, since the census fix already paid
that back. What it buys is a prerequisite for retiring the pure node kinds, and
that item is held up by something larger anyway: a value query has no flow, so a
read of a multi-version local resolves to nothing whether the refusal is there
or not. Taking the walk buffers off the allocator was worth comparable compile
time for mechanical work and byte-identical output, so it went first. Coming
back here is a matter of when, not whether.

Skipping `scalar_forward` separates the two, on `sqlite_parse` at `-O2`:

| bytes            | `scalar_forward` on | skipped |
| ---------------- | ------------------: | ------: |
| refusal in place |             393 702 | 412 659 |
| refusal removed  |             418 914 | 429 640 |

The pass is worth 19.0 KB with the refusal in place and 10.7 KB without it, so
8.2 KB of the 25.2 KB growth is forwarding it can no longer do. The other
17.0 KB is re-materialisation, and it is there whether the pass runs or not.

What blinds the pass is the shape `let c = <expr>; if !c { … }`. The `!c` read
is pure, so it is promoted, and the skeleton is then left with zero reads of
`c` — `sole_value_use` finds nothing to forward into. `cond_impl_post_promote`
then never prunes the bounds check the forward would have exposed, which is
where `wir_optimize_branchless_increment` and
`wir_optimize_copy_prop_destructure` fail; `array_bounds_elim_version_fill_wir`
and `match_switch_large_range_body_cost` read the same unpruned shape.
`tuple_literal_projection_wir` is not yet reduced.

Substituting into the pool does not reach it. Forwarding means replacing
`Opaque(Local c)` with the value of `i < len`, and that does not intern: `i` is
a loop counter, so `local_has_one_version` fails and the tree has no `ValueId`.
The operand has to go back to the skeleton instead — a NIR-level twin of
`wir_build`'s `extract_value`, rebuilding expressions from a value tree. The
same primitive is what the extraction cost model needs, to bind a
multiply-used local-naming value once rather than re-materialise it per use, so
one mechanism closes both open items. That is the shape of the work whenever it
is picked up.

There is no in-loop freeze. One was
built under the rule and promoted nothing on either benchmark, at 11.3 % of the
loop, so it was removed with its phase. Reviving it means adding the phase back
under the rule, not flipping a flag. Two things have to hold and neither did.

The material has to exist. Resolving every provably single-version local, not
just parameters, changes no output on its own, because the leaves that matter
are field loads and call results. A field read is keyed on the heap version at
its point, and no query-time re-derivation supplies that. The builder is the
only source, through the graph build or a scratch re-walk.

A consumer has to want it. The candidate was hoisting a loop-invariant field
load, and no value-graph consumer does it. LICM's value path excludes field
reads. Its structural path, which does hoist them, is deliberately
value-graph-free, so the one pass that wants the fact derives it without asking.
Surveyed, provably invariant in-loop field reads are 2.7 % and 7.1 % on the two
benchmarks, too few to pay for maintaining promoted operands across inlining and
SROA, which an in-loop freeze would need. Whoever revives this should know the
invariance test is free: every slot a loop may write is bumped before the body
walk and versions are monotonic, so a read below the loop-entry watermark is
invariant.

- [ ] Reach the in-loop consumers. Every freeze that may plant a local-naming
      value runs after the fixed-point loop, so the passes inside it still see
      none: LICM's value hoist collected zero loop-entry locals in 10 900
      queries. An in-loop freeze cannot simply be added; see Naming a local,
      before and after. Moving the build to lowering
      does not lift that bound: the extraction is point-dependent, not the
      build. This and resolving every single-version local share the anchor
      rule as prerequisite. It also gates nearly all of retiring the pure node
      kinds, not only the
      local reads, since the arithmetic above a local read resolves to no value
      either.

- [ ] Copy propagation on value identity rather than on locals.
      Source-stability is not subsumed by value equality. A write-once `x` whose
      source `y` is later reassigned can read equal ids yet be unsafe to fold.
      Revisit with provenance on the merge and opaque kinds.
- [ ] Induction-variable recognition, as a tagged opaque carrying base and step.
      Not needed yet. A post-increment read already appears as the addition it
      is, so this lands when a rule first wants it.
- [ ] Stop a borrow of a local that flows only into callees which cannot retain
      it from marking the local aliased. Plumbing the callee's escape summary
      into the ref-argument check is not the way. The aliased set is seeded from
      the address-taken locals the elaborator records for every borrow of a
      local, so the `(&self).field` shape this was written for is already
      aliased before the call site is looked at. Measured, 3.7 % of ref-arg
      marks are the sole reason their local is aliased: borrows of a projection,
      which the elaborator does not record. Narrowing the rest means narrowing
      the seed. That is a whole-function question, whether all uses of the
      borrow flow into callees that cannot retain it, over an annotation several
      other passes also read. The mutable-escape set is built by filtering the
      aliased set, so dropping a local from one silently drops it from the
      other, and a callee that cannot retain a reference can still mutate
      through it during the call. The two have to be decoupled first.
- [ ] Directed gate propagation, callee-shrink to callers only. Deferred. It
      drops the edges inlining adds to the build-once call graph, and a
      per-iteration rebuild does not recover them because the staleness is
      intra-iteration. It would need incremental edge maintenance, for a
      measured net-neutral gain.

### Terminal ideal

Gated behind measurement; see Not equality saturation.

- [ ] Saturation driver plus cost-based extraction. Run rules to a bounded
      saturation, then extract a cost-minimal form per operand, materialising a
      multi-use value only when sharing beats duplication. This subsumes the
      extraction cost model, the global fixed-point loop, the per-rule
      did-anything-change guards, and the dirty-set gate.

## Measured dead ends

Each was built, verified, and reverted. Do not retry as-is.

- Incremental ValueGraph rebuild. Reuse a parked graph's unchanged prefix and
  re-walk only the disturbed region. It was correct, verified by a
  build-both-ways oracle, and fired on 0.15 % of builds on the large workload
  and none on the small ones. The only clean pass adjacency was spoiled by
  non-journaling passes between handoffs, and `inline` restructures bodies
  wholesale every iteration. Raising the fire rate needs the single-worklist
  architecture, which subsumes the whole mechanism.
- A richer graph cache. Caps low for the same reason: it rebuilds whenever a
  pass changes a function, which is the common case. Deleted.
- Pooling the graph builder's output maps. The build is compute-bound, in the
  walk, the hash-consing, and the flow joins, not allocation-bound. No
  improvement measured.
- Promoting induction-variable local reads to source-bearing opaques. That
  resolver keyed one `ValueId` per local, where the builder mints one per
  assignment, so the id spanned every version of the local, and an induction
  variable has one per iteration. Traps `closure_capture`.
- Freezing a local-naming value before the structural passes, as a _soundness_
  bar. It was read as a re-contextualization: those passes renumber locals and
  splice a callee into a caller, so a value naming a slot was thought to survive
  into a frame where the slot means something else. Both halves of that are
  maintained — `dae` remaps the pool's recipes beside the skeleton's, `inline`
  mints a fresh caller opaque per splice — and the reductions the account
  predicts do not reproduce. The real failure was a dropped binding; see Naming
  a local, before and after. The refusal stands on cost now, not on this.
- A query-time materialiser for a field read at function entry. Miscompiled
  about 165 fixtures. Reference and aggregate fields change copy and alias
  semantics.
- Keeping caller values across a loop-free but impure inline. Over-merges two
  reads of a mutable reference parameter.
- Dropping the promoted-read census memo on every edit. The obvious invalidation
  rule, and measurably worse than no memo: a whole-body walk per rewrite, where
  the per-block recomputation it replaced at least amortised over a block.
  Maintaining the memo as an upper bound is what pays.
- One `dyn FnMut` slot walk in place of the monomorphized ones. The slot match is
  inlined into every instantiation of `for_each_child`, and a dev build folds 125
  of them onto one address, so a single indirect call promised that code size
  back. It measured 6.5 % slower: the per-slot indirect call costs more than the
  instruction cache it buys.

A value's identity is sound only when an operand the edits maintain carries it,
or when the query itself proves the leaf has a single version. It is never
re-derived from a side-table at query time.

## Consequences

- A cluster of former passes became graph structure and stopped existing. CSE
  and GVN fall out of hash-consing, pure copy propagation out of shared ids,
  store-load forwarding out of receiver-field-version identity. Their walks and
  bespoke analysis caches went with them.
- The optimizer gained a hash-cons pool, a builder, and an extractor. It lost the
  persisted value side-table, the cache machinery, the per-pass value rebuild,
  and every per-pass structural key.
- The arena costs one mutation discipline. The edit API is mandatory, and dead
  nodes accumulate until compaction lands.
- Measured wins along the way: the arena-direct engine cut a heavy module's
  optimize phase by about 40 % against the bridge baseline, and dirty-set gating
  cut it a further 50 % on the Gale-generated SQLite parser at `-O2`.

## See also

- [`docs/optimizer.md`](./optimizer.md), the pass inventory this architecture runs.
- [Normalized IR (NIR) Layer](./wep-2026-05-11-nir.md), the type boundary at `lower` that NIR is.
- [Wasm IR (WIR) Layer](./wep-2026-02-14-wir-layer.md), the separately-typed backend IR NIR extracts into.
- [Optimizer Remarks for Missed Optimizations](./wep-2026-06-03-optimizer-remarks.md), how a missed rewrite is reported.
- Cranelift's aegraph mid-end and `egg` (https://egraphs-good.github.io/), the build-once, eager-rewrite, single-extraction model this adapts, without their union-find over e-classes.
- `.claude/skills/profiling-wado-compiler`, the workflow behind the cost numbers.
