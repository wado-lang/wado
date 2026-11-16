# WEP: NIR Optimizer Architecture

Status: Complete. The architecture below is what the optimizer runs on. What it
does not do is a [known gap](#known-gaps), with no claim that it will be closed.

The NIR optimizer is a two-tier IR: a structured effect skeleton, plus a pool of
hash-consed pure values that some of its operands name. A worklist engine
rewrites it one function at a time, a per-function dirty-set gate schedules the
passes, and the result is extracted once into WIR.

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
  calls, assignments, allocation. Document order is effect order.
- Hash-consing: interning a value by its structure, so two structurally
  identical values get the same `ValueId` when built. Equality is `==` on a
  `u32`.
- Promoted: a skeleton operand slot that holds a `ValueId` instead of a
  sub-expression node. The value has moved out of the skeleton into the pool.
- Freeze: the pass step that promotes a pure operand.
- Scoped walk: a flow-sensitive walk of one region of a body, run to answer one
  query. What it derives dies with the query.

## Context

The optimizer was about 31 independent passes. Each was a full mutating walk over
an owned tree body, inside a global fixed-point loop. That shape had two costs.

- No stable handles and no parent or use edges. `&mut NirExpr` recursion owned
  the path to a node, so nothing could hold a node across an edit. A worklist
  could not be expressed.
- Every pass re-derived its own analysis. Reaching definitions were rebuilt per
  pass (`KnownValues`, `DefMap`, `env` / `field_env`). A native profile of
  `wado compile -O2` on `package-gale` put `optimize` at 52–65 % of the compile,
  with a per-function value graph rebuilt 2.67 times per function and about
  27 % of CPU in the allocation churn those rebuilds generate.

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

### Layer 2: the value pool (`nir_value_graph.rs`)

Pure values are hash-consed into a per-function `ValuePool`, indexed by
`ValueId`. Two structurally equivalent pure values share one id.

- Identity is structural, and a `ValueId` is stable once allocated. Interning is
  pure hash-consing, with no e-class merges and so no representative lookup. A
  rewrite that proves `a ≡ b` points the operand slot at `b`. It never merges
  two ids. Rules apply eagerly and once per match, never searched to a fixed
  point.
- The pool folds an operation on constants as it interns it.
- A value names a local only as a version-free leaf, sound under the
  single-version proof in [Soundness invariants](#soundness-invariants).
- A constant aggregate is one value however deep it is. Materialising it is an
  allocation, and where an allocation goes is `const_object_globalization`'s
  decision, so an aggregate constant is never frozen into an operand slot.

The pool holds values. It does not hold flow. No per-function structure records
which value a local has at a point, or what a field read sees at a heap
version: a scoped walk derives that for the region a query asks about, on a
scratch copy of the pool, and the answer is constants and field values, not
ids a later pass may hold. See
[Not a persistent value graph](#not-a-persistent-value-graph).

### The operand bridge

A skeleton child slot is `Operand = Expr(ExprId) | Value(ValueId)`. An
effectful or control-flow operand is always `Operand::Expr`. A pure one is
`Operand::Value` once a freeze has promoted it, and most are not promoted; see
[The value tier stays small](#the-value-tier-stays-small).

No expression-keyed value map is persisted. An unpromoted skeleton leaf
therefore resolves to no value. That is sound because "no value" is the finest
partition: a consumer skips the expression rather than over-merging.

A rule about a pure value is written against `Operand`, once. A promoted operand
is the same computation as the skeleton node it replaced, so it has to answer to
the same rules — and the two tiers spell that computation differently, so a rule
written once per tier drifts. The interpreter's arithmetic folding takes this
shape: one set of rules over a view that resolves the tier at the leaves. A rule
that reaches for `ExprKind` and `ValueKind` in two places is the shape to avoid.

Promotion is staged. A freeze is sound only where nothing later
re-contextualizes the operand it plants. Lowering interns all-literal
arithmetic, which names nothing outside the pool. Field reads freeze once the
structural passes have settled the shape they read. The arithmetic freeze runs
after every pass that walks arithmetic. No freeze runs before or inside the
loop.

### Extraction

One pass materialises each promoted operand back into concrete form, at WIR
build. It reads each value's type from the pool and recurses on composites. The
optimizer can extract a constant on its own, so a pass can read a frozen value
without waiting for the backend.

A scalar value is re-materialised at each use. That is always correct, and for a
constant always cheaper than sharing. A multi-use field read may be shared.

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

The heap-effect summaries are the exception. Several passes read them, so they
are held across the loop rather than rebuilt by each reader. They are keyed by
each function's write count, which every mutable access to its body bumps. A
reader re-solves only the call-graph components that hold a function written
since, or that call one whose summary moved. A stale summary is unsound rather
than imprecise, which is why the key is the access and not a pass's report of a
change. A debug build re-checks one summary on each read.

### The value tier stays small

The value pool is a constant pool and a post-loop extractor. It is not the home
of every pure value, and no work is committed to make it one.

The design this WEP started from was Cranelift's aegraph: every pure position
an operand, flow resolved into a graph once, and CSE, GVN, copy propagation and
store-load forwarding falling out of shared ids. That design was built as far
as the measurements below, and they say the remainder does not pay.

- What does the work is structural. Inside the loop, LICM hoists and
  deduplicates on structural keys, `niri` folds flow-sensitively over its own
  environment, and copy propagation runs on locals. None asks the pool for a
  local's value.
- The passes that plant or consume promoted operands are worth at most 1.1 %
  of fuel in any one region and 0.04–0.82 % of code size together; see
  [Where it stands](#where-it-stands).
- The leaves that block a value are not the pool's to resolve. Arithmetic left
  in the skeleton bottoms out on a call result (22–58 % of the blocked leaves),
  a reassigned local (19–36 %), a labeled block (11–24 %), or a field read
  (11–15 %).
- The host recompiles the output. binaryen's value-flow passes shrink Wado's
  `-O2` output by 0.6–2.4 % (`precompute-propagate`) and 0.3–1.5 % (`rse`), of
  the 12–18 % its `-O2` finds. The rest is local coalescing, branch cleanup,
  inlining and peepholes.
- The tier is not what the compile spends. It is 4–5 % of compile CPU, against
  15 % in the engine and 12 % in `niri`.

So the tier is kept at the size that earns its place: the pool, the two freezes
after the loop, post-scalarize store-load forwarding, and extraction. Growing it is a [known gap](#known-gaps).

## Soundness invariants

- Substitution soundness. An operand is repointed from `a` to `b` only when the
  two denote the same value in every execution reaching that point. Sharing an
  id is stronger and needs no justification: hash-consing gives one id only to
  structurally identical values.
- Single-version proof for a local leaf. A value naming a local is version-free:
  one value for every assignment of the local. It is sound only under a proof
  that the local has exactly one. Without the proof a query answers with no
  value, never with a guess.
- Availability is structural. A value naming a local may be materialised at a
  point only where the local is a parameter or its one definition dominates
  that point. The skeleton answers that, for extraction and for a hoist alike.
- Heap-version monotonicity. Within a scoped walk, a field read's heap version
  is the version before the read. A later write bumps to a fresh version, so
  any read after it gets a fresh value.
- Promoted operands follow structural edits. A pass that renumbers locals
  remaps the pool's local leaves beside the skeleton's, and a splice of one
  body into another re-interns the operands it carries in the receiving pool.
- Extraction equivalence. For every effectful position, the extracted form
  computes the same values in the same effect order as pool plus skeleton.
- The edit API is the only mutation path during a run. A pass that pokes the
  arena maps directly desynchronises the parent and use indices.
- One verdict per call. The straight-line walk and the loop summary classify a
  call from the same per-call verdicts. Two paths answering the same question
  with different predicates was the shape of every precision defect found in
  this layer.

### Standing invariants: do not reintroduce

These are settled by measurement and re-derived at a cost.

- No flow-resolved value structure outlives a query: no persisted graph, no
  expression-keyed value cache, no side-table. A pass needing a flow fact runs
  a scoped walk.
- A promoted read lives in the pool, not the skeleton. A pass that decides a
  local is unused, or rewrites every read of one, must count the pool's reads as
  well. The count is scoped to the operands the skeleton still carries, since the
  pool is append-only and also holds reads that folded away.
- A promoted operand names no skeleton node. Its leaves are constants and local
  reads, and extraction refuses an opaque that reads no local. The skeleton is
  therefore all a node walk reaches, and `for_each_reachable_node` is
  `for_each_node_under` from the root block.
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
- No walk buffer per node, and none thread-local. A read-only walk recurses
  inside the callback. A walk that mutates the engine's indices splits the
  borrow, since they sit beside `Body`. A walk that mutates the body threads
  one `Vec` down. Retiring the per-node buffers cut the SQLite parser's compile
  6.9 % and `json_twitter`'s 4.2 %; a pooled buffer measured the same as
  recursion.
- Siblings are visited in source order. The order reaches the output: it
  permutes `TypeId` and const-object numbering, and the order two
  specializations of one function are emitted in.

## Rejected

### Not sea-of-nodes

Sea-of-nodes, and dropping the skeleton, stay rejected.

### Not equality saturation

Saturation over the pool would unlock algebraic exploration: re-association,
the distributive law, strength reduction per use, cost-based
share-versus-duplicate. Wado's output is Wasm, which the host runtime JITs
again, and the host redoes most of that algebra. Cranelift's aegraph reports
5–10 % on native AOT, and the JIT-target number is much smaller. The
optimizations Wado carries are single-direction structural rewrites that
saturation does not help: allocation elimination, structural cleanup, dense
`Match` lowering, bounds-check elimination, inlining.

### Not a persistent value graph

A graph built once per function and maintained across every edit was the
centre of the original design: flow resolved at build, each read fixed to the
value dominant at its point, a snapshot of every local's value at each loop
entry.

It was built, and it was removed. Its goal was to halve the optimize phase, and
that was declared not met once the build had been made single. By then every
consumer had moved off it or been ablated to no diff: the bounds-check path
that read it was deleted, the union-find went, and an in-loop freeze under it
promoted nothing. What remained read it for one bit: whether a local held a
value at a loop's entry, which LICM's in-loop CSE asks before it shares an
expression.

The graph answered that bit by the history of the function. The snapshots were
taken at the first value query, before inlining, and never retaken. Inlining
cleared them after any splice of a call that was impure or carried a loop, and
loop versioning cleared them every time. The CSE therefore stopped reaching a
function once either had touched it. The skeleton now answers, the same for
every loop: the local is a parameter, or its `let` dominates the loop. Against the graph, `node benchmark/fuel.ts check` moves three
regions, and 133 of 3 496 WIR goldens change, 138 KB smaller and 5 KB larger.

| Region                  |    Fuel |
| ----------------------- | ------: |
| `codec/zlib/compress`   | −0.65 % |
| `gale/sqlite/parse`     | +1.19 % |
| `gale/sqlite/highlight` | +1.44 % |

Keeping it meant every structural pass maintaining it. Inlining ran a
whole-program immutability analysis per round to decide whether a splice
preserved the caller's values. That obligation is what bred defects: the
promoted tier and the graph account for 52 fix commits between June and
November 2026.

Folding the build into `lower` is rejected with it. The build's inputs, the
alias sets and the per-call purity verdicts, are derived over the whole lowered
package, and an empty alias set is the optimistic direction, so building
without them is unsound.

### Not a session index carried across passes

Opening a session rebuilds the parent map and use index, 18 % of the loop when
measured. Carrying the index across passes was built and measured. 91 % of
sessions reuse it and the derivation drops 68 %, but the loop total does not
move, and reuse changes the output by 3.3 KB, so the carried state is not
equivalent. Soundness would rest on every pass invalidating it by hand, each
omission a silent miscompile.

The prerequisite that would change this verdict is closing the arena's mutation
surface, so that the type system rather than a reviewer carries the invariant
that every edit goes through the API.

### Not incremental rebuild, and not a richer cache

Both were prototyped and measured, and both are the wrong shape. They make
re-deriving a derived analysis cheaper, when the analysis needs re-deriving only
because it is derived. See [Measured dead ends](#measured-dead-ends).

## Where it stands

Measured in November 2026 on a dev build, which is the compiler developer's
inner loop, so every figure includes `debug_assert!`s. The figures in this
section predate the removal of the persistent graph.

### Compile time

`wado compile -O2` under the span trace, warm.

| Share of the compile | `sqlite_parse` | `json_twitter` |
| -------------------- | -------------: | -------------: |
| `optimize`           |           82 % |           86 % |
| the fixed-point loop |           63 % |           65 % |

The loop converges in 8 or 9 iterations, with a long tail: iterations 4 to 6
visit functions of which over 90 % come out unchanged.

Self CPU on `json_twitter`, sampled with `samply`.

| Where                                                     | Share of the compile |
| --------------------------------------------------------- | -------------------: |
| `Engine`                                                  |               15.5 % |
| allocation                                                |               13.7 % |
| `const_fold` (`niri`)                                     |               12.4 % |
| `copy_prop`                                               |               10.2 % |
| the generic arena walks                                   |                9.6 % |
| `licm`                                                    |                9.3 % |
| `Engine::is_held_from_root`, a `debug_assert!`-only audit |                8.3 % |
| heap-effect summaries                                     |                4.8 % |
| the value tier                                            |                4–5 % |

### What the value tier buys

`WADO_SKIP_PASS=<pass> wado test -O2 --allocator freelist --report-fuel` over
twenty fuel regions of ten benchmarks, and the size of four compiled
benchmarks. A positive figure is a loss when the pass is skipped. Nine regions
move under none of them.

| Skipped                             | Fuel, the regions that move                        | Code size        |
| ----------------------------------- | -------------------------------------------------- | ---------------- |
| `promote_fields`                    | −0.02 to −0.19 % in nine regions                   | −8 to +493 B     |
| `freeze_pure_arith`                 | `json/canada/de` +0.12 %                           | +3 B             |
| `store_load_forward_post_scalarize` | `cbor/canada/de` +1.05 %, `json/canada/de` +0.36 % | +0.03 to +0.80 % |
| all three                           | at most +1.05 %                                    | +0.04 to +0.82 % |
| `value_pool/fold_const`             | none                                               | none             |

### What the pool reaches

`WADO_TRACE=arena_census`.

| At end-of-optimize                         | `sqlite_parse` | `json_twitter` |
| ------------------------------------------ | -------------: | -------------: |
| Pure kinds among reachable expressions     |           77 % |           70 % |
| Arena expressions per reachable expression |         2.15 × |         2.78 × |
| Arena statements per reachable statement   |         2.64 × |         3.51 × |

`WADO_TRACE=freeze_refusals`.

| Share of the pure kinds a freeze promotes | `sqlite_parse` | `json_twitter` |
| ----------------------------------------- | -------------: | -------------: |
| `promote_fields`                          |          9.9 % |         11.8 % |
| `freeze_pure_arith`                       |          4.7 % |          7.8 % |
| Local reads of a single-version local     |           75 % |           85 % |

## Known gaps

### The value tier

- Pure kinds stay in the skeleton, so every session walk covers them. They are
  61 % of the reachable expressions after lower, and the share climbs across
  the loop.
- A value query has no flow. A read of a multi-version local resolves to no
  value, and so does the arithmetic above it, a field read, and a call result.
- No pass inside the loop sees a promoted value naming a local, since every
  freeze that plants one runs after the loop.
- No freeze runs before the loop. One that planted values naming a local was
  correct on the corpus and cost 6.4 % of the SQLite parser's code, and the
  loop stopped converging inside its cap.
- Extraction re-materialises a multi-use non-constant scalar at each use. Two
  thirds of that 6.4 % is this.
- `scalar_forward` cannot forward into a promoted read. In
  `let c = <expr>; if !c { … }` the `!c` is promoted, the skeleton holds no
  read of `c`, and the bounds check the forward would have exposed stays.
- Copy propagation is on locals, not on value identity. Source-stability is not
  subsumed by value equality: a write-once `x` whose source `y` is later
  reassigned can read equal ids yet be unsafe to fold.
- No value carries an induction variable's base and step.
- `promote_fields` loses fuel wherever it changes it; see
  [What the value tier buys](#what-the-value-tier-buys).
- LICM's in-loop CSE costs fuel on the SQLite parser and saves it on zlib; see
  [Not a persistent value graph](#not-a-persistent-value-graph). What separates
  a sharing that pays from one that does not is unexamined.

### Compile time

- The arena and the pool are never compacted. In-place rewrites orphan nodes;
  `lower` arrives at 1.1 ×, so the growth in
  [What the pool reaches](#what-the-pool-reaches) is the loop's.
- `Engine::is_held_from_root` runs only under `debug_assert!` and is 8.3 % of
  a dev build's compile.
- Gate propagation marks both call-graph neighbours of a changed function.
  Marking only the function itself cut iterations 2 to 8 on `sqlite_parse` by
  16 % with the output the same size and not the same bytes. Whether the
  optimizations it skips are ever wanted is unmeasured.
- Opening a session re-derives the parent map and use index; see
  [Not a session index carried across passes](#not-a-session-index-carried-across-passes).

### Precision

- The escaped-set generation invalidates 66 % of local-rooted field reads on
  `sqlite_parse` and 43 % on `json_twitter`. No consumer is on that path: five
  conservatisms feeding it were removed, each measured to fire widely, and each
  left 1 921 goldens and every benchmark unchanged. LICM's structural path and
  `field_scalarize` already hoist the invariant reads and shadow the mutable
  field in a local.
- A reference to a local that flows only into callees which cannot retain it
  still marks the local aliased. The aliased set is seeded from the
  address-taken locals the elaborator records, so 3.7 % of ref-argument marks
  are the sole reason their local is aliased. The mutable-escape set is built
  by filtering the aliased set, so the two cannot be narrowed apart.
- The switch lowering is priced on table width, a proxy. A 40-arm synthetic of
  plain `i32` fields prefers the `else if` chain by 11 %, and `cbor-twitter`'s
  40-arm `User` prefers the table by 2 %.

## Measured dead ends

Each was built, verified, and reverted. Do not retry as-is.

- Incremental value-graph rebuild. Reuse a parked graph's unchanged prefix and
  re-walk only the disturbed region. It was correct, verified by a
  build-both-ways oracle, and fired on 0.15 % of builds on the large workload
  and none on the small ones.
- A richer graph cache. Caps low for the same reason: it rebuilds whenever a
  pass changes a function, which is the common case.
- Pooling the graph builder's output maps. The build is compute-bound, in the
  walk, the hash-consing, and the flow joins, not allocation-bound.
- An in-loop freeze. It promoted nothing on either benchmark, at 11.3 % of the
  loop. The leaves that matter are field loads and call results, and provably
  invariant in-loop field reads are 2.7 % and 7.1 % on the two benchmarks.
- Promoting induction-variable local reads to source-bearing opaques. That
  resolver keyed one `ValueId` per local, so the id spanned every version of
  the local, and an induction variable has one per iteration. Traps
  `closure_capture`.
- Refusing a local-naming value before the structural passes, as a _soundness_
  bar. The refusal was covering five miscompiles, each a place where the
  skeleton had an ability the pool did not: `scalar_forward` and `copy_prop`
  counted only the skeleton's reads, `dae` read "promoted" as "safe to delete"
  though `100 / zero` is pure and traps, `niri` stopped at the pool, and the
  pool folded no short-circuit identity. All five are fixed, and
  `e2e/opt_early_freeze_names_a_local` reduces the real failure: a dropped
  binding, not a merged value.
- A freeze before the loop. Refusing every value that names a local, it
  promoted nothing on either benchmark, and with the persistent graph gone
  skipping it left `sqlite_parse` byte-identical. It was removed.
- A query-time materialiser for a field read at function entry. Miscompiled
  about 165 fixtures. Reference and aggregate fields change copy and alias
  semantics.
- Keeping caller values across a loop-free but impure inline. Over-merges two
  reads of a mutable reference parameter.
- Dropping the promoted-read census memo on every edit. Measurably worse than
  no memo: a whole-body walk per rewrite.
- One `dyn FnMut` slot walk in place of the monomorphized ones. It measured
  6.5 % slower: the per-slot indirect call costs more than the instruction
  cache it buys.
- Directed gate propagation, callee-shrink to callers only. It drops the edges
  inlining adds to the call graph, for a gain that measured net-neutral then.

## Consequences

- The optimizer gained stable node ids, a worklist engine, a per-function gate,
  a hash-cons pool, and an extractor. It lost the per-pass analysis rebuilds
  and the cache machinery around them.
- CSE and loop-invariant hoisting stay structural, in LICM. Store-load
  forwarding stays a pass. The pool did not absorb them.
- The arena costs one mutation discipline. The edit API is mandatory, and dead
  nodes accumulate.
- The promoted tier costs each pass a second place to look. A pass that counts,
  rewrites, or deletes by local answers for the pool's reads as well, and
  `optimize::run_pass` audits that every local a promoted operand reads still
  has a definition.
- Measured wins along the way: the arena-direct engine cut a heavy module's
  optimize phase by about 40 % against the bridge baseline, and dirty-set gating
  cut it a further 50 % on the Gale-generated SQLite parser at `-O2`.

## See also

- [`docs/optimizer.md`](./optimizer.md), the pass inventory this architecture runs.
- [Normalized IR (NIR) Layer](./wep-2026-05-11-nir.md), the type boundary at `lower` that NIR is.
- [Wasm IR (WIR) Layer](./wep-2026-02-14-wir-layer.md), the separately-typed backend IR NIR extracts into.
- [Optimizer Remarks for Missed Optimizations](./wep-2026-06-03-optimizer-remarks.md), how a missed rewrite is reported.
- Cranelift's aegraph mid-end and `egg` (https://egraphs-good.github.io/), the model the value tier set out to adapt.
- `.claude/skills/profiling-wado-compiler`, the workflow behind the cost numbers.
