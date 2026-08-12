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
  `clone_expr`), which keeps the parent map, the use index, and the
  promoted-read census coherent and re-enqueues exactly the affected
  neighbourhood. In-place replacement keeps the id stable, so a worklist entry
  survives the edit.
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
- That census is session-scoped, never per-application: it walks the whole body,
  so recomputing it per rule application is quadratic. `Engine` memoizes it and
  holds an _empty_ memo across edits — the case that decides the cost, since
  inside the loop the early freeze plants context-free values only and no
  reachable operand names a local. Only a local-naming operand becoming
  reachable drops it, which the edit API reports; a rule therefore never writes
  an operand past it into `Body`, and an operand-slot sweep goes through
  `Engine::map_operands`. `Engine::run` audits the memo against a fresh walk
  under debug assertions — a backstop covering a session that asked, at its end
  only, not a proof.
- No whole-body walk per rule application, in an assertion either. A rewrite
  that deletes a binding reports the local through `Engine::note_elided_local`
  and `Engine::run` audits the batch once; checking it at the rewrite put
  `peephole` at 5.1 s of a 9.5 s loop, against 1.4 s of 6.2 s once
  session-scoped. The audit reads the arena fresh rather than the use index and
  census memo the rewrite decided on — substituting those would make it agree
  with itself and catch nothing.

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
JIT cannot do). Its prerequisites are visualisation tooling
(`WADO_DUMP_VALUE_GRAPH`, `WADO_DUMP_AFTER_RULE`) and budget-bounded rule sets —
a budget hit must degrade gracefully to the partially saturated graph, never
panic and never produce worse output than the input.

### Not a session index carried across passes

`Engine::new` derives the parent map, use index, and post-order seed per pass
per function — 1.03 s of a ~5.8 s loop, over 36 000 sessions. Carrying it was
built and measured: 91 % of sessions reuse it and the derivation drops to
0.33 s (−68 %). It still does not pay.

- It optimizes the structure the terminal ideal retires. Pure `ExprKind`s are
  52 % of the arena, and the `Local` nodes the use index is made of are 34 %;
  moving them into the pool shrinks `build_indices` without a new invariant.
- It does nothing for where the precision items move the load. A promoted read
  lives in the pool, so widening local promotion shifts reads off the use index
  and onto the promoted-read census, which a carried index does not touch.
- Reuse changes the output (+3.3 KB), so the carried state is not equivalent to
  a fresh one. `def` is carried and never re-derived, and `extract`'s dominance
  gate reads it.
- Soundness would rest on 36 `Body::invalidate_engine_index` calls plus every
  future direct arena write — unenforced, and failing as a silent miscompile
  rather than a panic, since a stale index reads to `elide_local` as "unused".

Closing the arena's mutation surface (226 mutating sites over 57 files) so the
type system carries that invariant is the prerequisite that would change this
verdict.

### Not incremental rebuild, and not a richer cache

Both were prototyped and measured, and both are the wrong shape: they make
re-deriving a derived analysis cheaper, when the analysis only needs re-deriving
because it is derived. See "Measured dead ends".

## Remaining work

Compile speed here means the debug build (`cargo build`, the dev profile) — the
compiler-developer inner loop, not a release binary. Every timing in this
section and in "Measured dead ends" is a debug-build number, so
`wado-compiler`'s `debug_assert!`s are inside the measured cost, the
promoted-read census audit at the end of `Engine::run` among them.

The graph build is ~6 % of the phase (was ~21 % before build-once), so what is
left to cut is the passes and the assertions that guard them. At `-O2` the loop
costs 6.2 s on `benchmark/sqlite_parse` and 6.0 s on
`benchmark/syntax_highlight`, spread over `peephole` (23 %) and `copy_prop` /
`const_fold` / `licm` (13 % each). No iteration dominates: under the gate,
iterations 5 - 8 cost 0.2 - 0.4 s each, so the iteration count is not the
lever it looks like.

- [ ] Function-level parallelism. The per-function build and walk are
      independent.
- [ ] Fold the graph build into `lower` (born-at-`lower`). What this buys is one
      body walk per function, not earlier availability: the graph is already
      built for every function before the loop, because
      `extract::freeze_pure_arith`'s early run walks every expression of every
      body with the alias sets supplied, and the first of those queries builds
      it. So the lazy first-query path is eager in practice, and the value it
      hands the precision items below is not timing — see them for what is
      actually in their way.
      This item also retires `Engine::ensure_value_graph`'s
      `VG_MAX_EXPRS` size gate. That gate skips the graph for a body over 5000
      expressions, citing the build plus `build_scoped`'s scratch clone OOMing
      under `wado test`'s parallel compilation. Measured, the memory it saves is
      not there: on `driver_cst_sqlite_oracle_test` (the ANTLR4-driver shape the
      gate names) raising it covers the one over-threshold body — 113 823 of the
      program's 274 143 expressions — for +24 479 pool nodes, +3 MB peak RSS and
      +6 - 10 % compile time over three runs, and emits a byte-identical module.
      So the gate costs no optimization quality today and buys no headroom;
      born-at-`lower` inherits a memory question far smaller than the comment
      implies. Retire it with this item and not before: on its own the removal
      is a pure regression, because the graph it would then build for that body
      has no consumer until the in-loop freeze lands, leaving only the
      slowdown. The parallel-`wado test` OOM the comment claims is still
      untested — that is this item's entry check, not a settled fact.
- [ ] Retire the remaining pure `ExprKind` variants from the skeleton, so every
      pure position is an `Operand::Value`. This is a compile-speed item, not
      only the saturation prerequisite it also is: pure kinds are 52 % of the
      arena on `benchmark/sqlite_parse` and `benchmark/syntax_highlight`, and
      the `Local` nodes the engine's use index is made of are 34 %. Retiring
      them halves what every session walk and every arena scan covers, and the
      use index largely empties as reads move into the pool — the same saving
      carrying a session index chased, taken structurally instead of behind a
      new invariant (see "Not a session index carried across passes"). Measure
      `Engine::new` (1.03 s, ~17 % of the loop) again afterwards: the remaining
      compile-speed items are all sized against a skeleton this shrinks.
      Over reachable nodes (the arena is only 62 % live, the rest is
      compaction's job) the target is 71 775 of 169 976 — 42 % of the live
      skeleton — after setting aside what must stay: an assign place (5 521) and
      `Ref` / `MutRef` / `Deref`, which are not pure (13 916).
      Almost none of it is independently reachable, which a kind-by-kind reading
      of the target hides. `Engine::value` is `maintain_pure_node`, and that
      recurses through operands: a `Binary` / `Unary` / `Cast` resolves only if
      its leaves do. A leaf `Local` now resolves for any provably single-version
      local, which was not enough — the arithmetic left in the skeleton is still
      not gated, it has no value at all, because it bottoms out on a field load
      or a call result. `FieldAccess` has no arm there and never resolves, which
      is the query-time materialiser under "Measured dead ends". The gates
      themselves reject almost nothing: `not-reemittable` 64,
      `extraction-duplicates-work` 0.
      What that leaves is one independent piece — 19 % of the unresolved
      arithmetic (5 894 of 31 730 `Binary` rejections) is a body the
      `VG_MAX_EXPRS` gate skipped entirely, so retiring that gate is worth doing
      for this item and not only for born-at-`lower`. The other 81 % waits on
      the prerequisite under Precision below.
- [ ] Arena compaction. In-place rewrites orphan nodes that are never freed
      mid-run (~1.66× bloat measured at end-of-optimize on `package-gale`).

Precision. All of it waits on one rule.

### The anchor rule

A frozen operand may name a local only when that local's defining statement
travels with the operand.

A source local fails that. A parameter's def is the function entry, which is not
a statement, so `inline` splices the operand into a caller where the same index
is assigned once per call — and because `ValuePool::canonical_local` hands out
one `ValueId` per local index, two reads that now denote different values share
an id. That is the over-merge behind `String::substr_bytes` under
"Measured dead ends". The guard used to be "every leaf is a parameter", a proxy
for "this local has one version" that inlining invalidates, since
parameter-ness does not survive being inlined; it is now the predicate itself
(`Engine::local_has_one_version`) plus a dominance check at the placement.

A freeze-minted temp satisfies it by construction. `let _av = <value>` is a
statement in the same body, non-`mut`, and assigned once, so any pass that
copies the operand copies the def with it, and one `ValueId` per `_av` stays
true in every context it lands in. So: never freeze a value naming a source
local — materialise it into an `_av` and name that.
`apply_value_freeze` already mints `_av` this way for the multi-use case; the
rule makes it the only way a local may be named, and drops the parameter
restriction, which the anchor replaces.

The rule is necessary and not sufficient. Freezing inside the loop under it was
built and measured, and promoted nothing on either benchmark — zero candidates
reached `apply_value_freeze`, both modules came out byte-identical, and the pass
cost 12 % of the loop. The reason is upstream of the rule: too few leaves
resolve, and `Binary` / `Unary` / `Cast` propagate that `None` through `?`, so
there is no value to freeze in the first place. Widening the `Local` arm to
every provably single-version local (from non-`mut` parameters only) resolves
16 661 more reads on `benchmark/sqlite_parse` and still changes no output,
because the leaves that matter are field loads and call results, which no
query-time re-derivation can supply — a `FieldAccess` value is keyed on the
`heap_ver` at its point, and a version-free id per local index is the recorded
dead end. The _builder_ mints both correctly, which is where the material has
to come from.

So the convergence point has two halves, and both are needed:

- The builder's versioned values have to reach every pure position, which means
  freezing them at birth in `lower` rather than asking a query-time resolver
  later. That is what born-at-`lower` and retiring the pure `ExprKind`s amount
  to, and it is what supplies the material.
- A frozen local-naming value has to be anchored, which is the rule here, and is
  what makes planting that material sound while `inline` and `sroa` still run.

Two things have to be fixed for the rule to hold.

- `materialise_point` is the nearest common dominator, computed as the longest
  common prefix of `block_path`, and `block_path` has no notion of a loop
  boundary. With one use inside a loop and one outside, the `let` lands outside
  and the in-loop use reads a value from before the loop. Placement has to be
  clamped to the innermost loop enclosing any use.
- A `FieldAccess` value carries the `heap_ver` it was read at, which the anchor
  does not supply, so `_av` has to be placed where that version still holds.
  Until that is expressed, the rule unlocks arithmetic and local reads but not
  `FieldAccess` — the query-time materialiser stays a dead end.

The cost is a local and a store per frozen value, today gated on
`ids.len() > 1 && worth_materialising`. Widening that gate to the single-use
case buys a promotion for a store; measure module size and benchmark runtime
before doing it. `value_may_trap` keeps gating regardless — hoisting a trapping
value to a dominator changes when it traps.

#### What the freeze funnel actually rejects

Counted end to end on `benchmark/sqlite_parse` at `-O2`, per phase, before
deciding anything else about the gates. The material is what is scarce; no gate
is the binding constraint.

|                                         |  Early |    InLoop | Terminal |
| --------------------------------------- | -----: | --------: | -------: |
| exprs seen                              | 81 067 | 2 126 679 |  549 525 |
| pure-arith candidates                   | 10 393 |    96 205 |   34 642 |
| … with a value at all                   |    698 |     9 582 |    2 633 |
| admitted (reemittable, non-duplicating) |     24 |     8 570 |    2 417 |
| distinct representatives                |     17 |     8 399 |    2 375 |
| **materialised into an `_av`**          |  **0** |     **0** |    **0** |
| redirected inline                       |     17 |       375 |      682 |

Three readings, in order of how much they cost:

- **`apply_value_freeze`'s materialisation path has never fired.** Every
  representative is single-use — 8 570 candidates over 8 399 representatives is
  1.02 uses each — so `ids.len() > 1` refuses nearly all of them and
  `worth_materialising` the rest. The gates before it reject far less than
  expected: leaf availability rejects **0** once it is a dominance check rather
  than a parameter test, `value_may_trap` 22, no common point 383. Widening a
  gate therefore cannot produce a promotion, which is why unifying the
  single-version predicate, adding the in-loop freeze, and widening the `Local`
  arm each emitted a byte-identical module. Anything aimed at this path must
  first make values shared.
- **Values are scarce because leaves do not resolve**: 7.6 % of pure-arith nodes
  at Terminal have a value at all. Two arithmetic nodes can only share a
  representative if both resolve, so the sharing rate is roughly the square of
  this. That is the born-at-`lower` prerequisite above, restated as a number.
- **`promote_fields` is dead.** It is the only caller passing
  `include_fields = true`, and `field-group = 0` — no `FieldAccess`
  representative reached the apply phase in the whole compile, so
  `apply_field_materialise`, `receiver_available_at`, and the
  `cond_impl_post_promote` re-run that exists to consume its output are all
  unreachable. The cause is the missing arm: `maintain_pure_node` returns `None`
  for `ExprKind::FieldAccess`, because a field value carries the `heap_ver` at
  its program point and a point-free re-derivation has no version to supply.
  The value it needs does exist — the builder mints it, and
  `Engine::scoped_const_reads` already recovers `(read, value)` pairs by scratch
  re-walk for exactly this reason (SROA's post-build stores). Routing the field
  candidates through that, rather than through `Engine::value`, is what would
  revive the pass — and field loads are where real sharing lives, so it is also
  the shortest path to a non-zero materialisation count.

- [x] Widen local promotion past parameters. Landed in two halves, both keyed on
      one predicate — `Engine::local_has_one_version`, read off the use index
      (never reassigned, address never taken, bound by exactly one `let`) rather
      than the `NirLocal::is_mut` flag, which rejects every `let mut` a body
      leaves alone and admits a temp a pass minted with the flag clear.
      `maintain_pure_node` resolves a `Local` read under it, and the freeze
      refuses to name a local failing it (`multi_version_locals`); the two agree
      by construction. Leaf availability at the materialisation point is now the
      same dominance check the `FieldAccess` materialiser used
      (`leaf_available_at`), so parameter-ness no longer gates anything.
      The gate was never value identity. The builder already mints a fresh
      `Opaque` per assignment, so two versions of a local are two `ValueId`s.
      What is per-index is the extraction: `OpaqueSource::Local(idx)` means
      "emit `local.get idx`", which is only right at a point where the local
      still holds that version — hence the two conditions, one body-wide and one
      positional.
      No effect on either benchmark's output: the freeze is starved of shared
      values, not of admissible locals. See the funnel table below.

- [ ] Revive `promote_fields`. It is dead today (funnel table above), and it is
      the only freeze path whose representatives would be genuinely shared — one
      `(receiver, field, heap_ver)` triple per field, read at every use — so it
      is the first thing that could make `apply_value_freeze`'s materialisation
      count non-zero. The two soundness gates (scalar-field,
      receiver-availability) are written and unreachable, not missing.
      The material has to come from a scratch re-walk, since
      `maintain_pure_node` has no version to supply, but
      `Engine::scoped_const_reads` cannot be widened to hand it over as-is: a
      whole-function re-walk is unseeded, so every receiver is a walk-local
      `Opaque` (`reintern_live_rooted` drops those) and every version numbers
      from a fresh heap that would over-merge against the live pool's. Both are
      why the constant filter is there — it is the design, not vestigial. The
      seeded inline path escapes both by construction and is not a precedent.
      What does work is to treat the re-walk as an *equivalence oracle* rather
      than a value source: group the field reads by their scratch `ValueId`, and
      mint one live triple per group over `canonical_local(i)` — sound exactly
      when `Engine::local_has_one_version(i)` — at a version above every version
      already in the live pool. That reproduces the sharing the walk proved
      while making a collision with an existing live triple impossible.
      Entry check: the pass and its `cond_impl_post_promote` follow-up cost loop
      time today for nothing, so measure both before and after.

- [ ] Reach the in-loop consumers. Both freezes that may plant a local-naming
      value run after the fixed-point loop, so the passes inside it still see
      none: LICM's value hoist collected zero loop-entry locals in 10,900
      queries, and `loop_entry_values` still has no working consumer, which is
      why `inline` discarding a non-empty map 1,469 times costs nothing. An
      in-loop freeze cannot simply be added — the early one is bound by the
      context-free rule under "Measured dead ends". Moving the build to `lower`
      does not lift that bound: it is the extraction that is point-dependent,
      not the build. This and the widening above share one prerequisite, the
      anchor rule below, which also gates nearly all of retiring the pure
      `ExprKind`s — not only its `Local` half, since the arithmetic above a
      local read resolves to no value either.

- [ ] Copy propagation on `ValueId`. Source-stability is not subsumed by value
      equality — a write-once `x` whose source `y` is later reassigned can read
      equal ids yet be unsafe to fold. Revisit with `Select` / `Opaque`
      provenance.
- [ ] Induction-variable recognition (`Opaque` tagged `{ base, step }`). Not
      needed yet — post-increment reads already appear as `Add(opaque_i, step)` —
      so it lands when a rule first wants it.
- [ ] Stop a `&` / `&mut` on a local that flows only into `stores`-free callees
      from marking the local aliased. Plumbing the callee's `stores` into
      `alias::escape_ref_arg` is not the way: `build_alias_info` seeds `aliased`
      from `address_taken_locals`, and the elaborator marks that for every
      `&x` / `&mut x` on a local, so the `(&self).field` shape this was written
      for is already aliased before the call site is looked at. Measured, 3.7 %
      of ref-arg marks (2 099 of 56 269 on `benchmark/sqlite_parse`) are the
      sole reason their local is aliased — refs to a projection (`f(&x.field)`),
      which the elaborator does not record. Narrowing the rest means narrowing
      the seed, which is a whole-function question (do _all_ uses of `&x` flow
      into `stores`-free callees?) over an annotation `boxing`, `sroa`, and
      `elide_local` also read. Also note `mut_escaped` is built by filtering
      `aliased`, so dropping a local from `aliased` silently drops it from
      `mut_escaped` too — a callee that cannot retain a reference can still
      mutate through it during the call, so the two have to be decoupled first.
- [ ] Directed gate propagation (callee-shrink → callers only). Deferred: it
      drops the edges `inline` adds to the build-once call graph, and a
      per-iteration rebuild does not recover them — the staleness is
      intra-iteration. It would need incremental edge maintenance, for a
      measured net-neutral gain.

Terminal ideal, gated behind measurement (see "Not equality saturation").

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
- **Promoting induction-variable `Local` reads to source-bearing opaques.** That
  resolver keyed one `ValueId` per local index (the builder instead mints one
  per assignment), so the id spanned every version of the local, and an
  induction variable has one per iteration. Traps `closure_for_loop_mutation`.
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
- **Dropping the promoted-read census memo on every edit.** The obvious
  invalidation rule, and measurably worse than no memo: a whole-body walk per
  rewrite where the per-block recomputation it replaced at least amortised over
  a block. Only holding an empty memo across edits pays.

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
