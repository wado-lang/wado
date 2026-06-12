# WEP: Worklist-Driven NIR Rewrite Engine

This WEP sets the terminal architecture for the NIR optimizer: a two-tier IR
(structured effect skeleton + hash-consed pure-value graph) driven by a
single worklist engine, with optimizations expressed as declarative rewrite
rules under equality-saturation semantics, and a call-graph-driven
interprocedural driver replacing the global fixed-point loop.

The engine substrate, the arena NIR, the routing of every intra-procedural
pass through the engine edit API, and the ValueGraph foundation (kinds,
hash-cons pool, per-function builder) have all landed, along with the
Stage 3 (`cse`) and Stage 5 (`store_load_forward`) rule migrations onto
the ValueGraph. The remaining required-path migrations and the
interprocedural worklist are open. Full Layer-2 promotion (Skel
pure-`ExprKind` retirement and saturation-driven engine) stays in this
WEP as the terminal ideal but is gated behind measurement — see "Why
Layer 2 promotion is deferred" and the "Optional acceleration" entries
in the migration plan.

## Context

The optimizer was historically ~31 independent passes, each a full mutating
walk over every function, run inside a global fixed-point loop. The engine
substrate (`nir_engine.rs`) added a parent map, a local use index, a
mutating edit API, a `Rule` trait, and a per-function dirty-set gate
(`optimize/gate.rs`). Every intra-procedural pass has been migrated to run
as a `Rule` (or per-function standalone session of one) on it, and every
mutation goes through `Engine::*` so the parent map and use index stay
coherent.

What that gave us is the arena NIR, the parent map + use index, the
pooled `EngineBuffers`, and every intra-procedural rule running through
the engine edit API (see the "Completed substrate" checklist below for
the exact pass list).

What it has not yet given us is the architectural win the redesign exists
for:

- Pure-value identity is still expressed by ad-hoc per-pass keys
  (`CseKey`, copy-prop binding match, `condition_implication`'s bound
  shape). Structural equality is recomputed each time.
- Reaching definitions are still rebuilt per pass —
  `store_load_forward`'s `KnownValues`, `condition_implication`'s
  `DefMap`, `const_folding`-flow-sensitive's `env` / `field_env` all run
  their own walks.
- The global fixed-point loop and `OptConfig::iterations` are still
  present; intra-procedural saturation comes only from the loop re-running
  each rule.

A native profile of `wado compile` on `package-gale` (34.5k lines) pins
the surviving cost to per-pass analysis reconstruction: `optimize` is
~52 % of the whole compile, with the loop sweeping the program ~85 times
at `-O2`, each sweep rebuilding its own per-function dataflow.

## Decision

The terminal optimizer architecture is a two-tier NIR + equality-saturation
engine.

### Two-tier IR

- SkelTree (Layer 1) — the existing arena. Carries statements, control
  flow, effectful expressions, and patterns. Document order = effect
  order.
- ValueGraph (Layer 2) — a hash-consed DAG of pure values, indexed by
  `ValueId`. Two structurally equivalent pure expressions share one
  `ValueId`; equality is `==` on a `u32`.
- Operand bridge — a Skel node's child slot becomes
  `Operand = Skel(NodeRef) | Value(ValueId)`. Pure operand positions
  carry `Operand::Value(...)`; effectful and control-flow operands stay
  `Operand::Skel(...)`.

### ValueGraph kinds

```
ValueKind:
  Int / Float (bit pattern) / Bool / Char / String / Null / Unit   (literals)
  Opaque(OpaqueId)                                                 (params, unknown values)
  Binary(NirBinaryOp, ValueId, ValueId)
  Unary(NirUnaryOp, ValueId)
  Cast(ValueId, TypeId)
  Select(cond: ValueId, then: ValueId, else: ValueId)              (structural merge)
  LoopPhi(entry: ValueId, body_iter: ValueId)                      (loop recurrence; tagged opaque in MVP)
  FieldAccess(receiver: ValueId, field, heap_ver: HeapVersion)
```

Notably absent: no `Local` kind. Locals are resolved at ValueGraph build
time via a `current_value: HashMap<local_idx, ValueId>` the builder
threads through its walk. At `Let` / `Assign` the entry is updated; at
structural merges (If / Match / Switch endpoints) a fresh `Select` value
is constructed; at `Loop` entry the local becomes a `LoopPhi` (tagged
opaque in MVP, with simple-induction recognition added at the
flow-sensitive migration stage). Function parameters seed
`current_value` with `Opaque` values.

`StructLiteral` / `TupleLiteral` / `ArrayLiteral` stay Skel-side for the
MVP. They are pure but allocation-bearing; `const_object_globalization`
already covers the constant-sharing case, and Value-graph promotion
needs an extraction policy that decides when two identical literals may
share an allocation. Deferred until measurement justifies it.

### Heap modeling

`FieldAccess` carries a heap version the builder bumps when a Skel node
may write the heap (Assign-to-FieldAccess; non-pure Call / MethodCall /
IndirectCall; LoopPhi body-iter when the body may write). MVP uses
per-field granularity (a write to `obj.f` bumps the `f` slot only); a
later stage promotes to per-(receiver-root, field) using `mod_ref.rs`.
A global per-function heap counter is too coarse to make `FieldAccess`
CSE useful in practice and is skipped.

### Optimization as rewrite rules

```rust
trait Rule {
    fn apply_value(&self, e: &mut Engine, v: ValueId) -> bool { false }
    fn apply_skel(&self, e: &mut Engine, n: NodeRef) -> bool { false }
}
```

`apply_value` rules pattern-match on a `ValueKind` and either add a new
ValueId (an "equivalent representation" in e-graph terms) or replace the
kind. `apply_skel` rules reshape the skeleton (block flattening, statement
fusion, control-flow lowering).

CSE / GVN / copy propagation / constant folding / store-load forwarding
all reduce to one principle: same `ValueId` means same value. Two
expressions sharing a `ValueId` need not both be computed; an operand
reading the same `ValueId` as a known literal is itself that literal.

### Saturation and extraction (optional acceleration)

The terminal architecture runs all rules together until either no new
ValueIds are produced and no Skel rewrites fire, or a budget hits (node
count budget, per-ValueId rewrite count, outer iteration limit — all
three). After saturation, an extraction pass walks the SkelTree picking
a cost-minimal Skel form for each `Operand::Value(...)`. A multi-use
`ValueId` is materialised once with a hoisted `let __t = ...` only when
the cost model favours sharing over duplication — that is the "real CSE"
decision.

Equality saturation is optional acceleration: the required path runs
rules destructively as today, with the ValueGraph supplying hash-cons
equality and reaching-def-by-construction. Saturation (Stage 8) adds
phase-ordering dissolution and algebraic exploration on top, and is
deferred until measurement justifies it.

## Why two-tier (not classical SSA)

Wado emits structured Wasm, so SSA + relooper buys nothing the backend
doesn't already need. Two-tier keeps SkelTree as the effect-ordering
substrate; the ValueGraph is a side representation accessed via
`Operand::Value`, with local versioning expressed by structural `Select`
nodes at merges rather than explicit phis. Full SSA / sea-of-nodes
stays rejected, as does dropping the SkelTree (Wasm codegen needs the
effect ordering it carries).

## Why Layer 2 promotion (Stages 7 – 8) is deferred

The ValueGraph as an _engine side-table_ (Stages 1 – 6) delivers
hash-cons equality, reaching-def-by-construction, and analysis sharing.
Promoting it into the SkelTree (Stage 7: `Operand::Value` replaces pure
`ExprKind` variants) and switching the engine from destructive rewrites
to equality saturation (Stage 8) unlocks only **algebraic exploration**
on top of the side-table baseline: re-association, distributive law,
strength-reduction-per-use, cost-based share-vs-duplicate. The required
optimisations Wado actually carries (allocation elimination, structural
cleanup, dense-`Match` lowering, bounds-check elimination, inlining)
are all single-direction structural rewrites — saturation buys nothing
for them.

Wado's output target is **Wasm**, which a host runtime JITs again. Most
of the algebraic improvements saturation excels at (re-association,
strength reduction, De Morgan) are redone by the host JIT. Cranelift's
aegraph reports 5 – 10 % improvement on native-AOT code; the
corresponding number for JIT-target Wasm is much smaller because the
host JIT re-does the algebraic part.

So Stages 7 – 8 stay in the WEP as the terminal ideal — they describe
the architecture this design points at — but they are not part of the
required path. They activate when measurement justifies their cost
(e.g., a future native-AOT backend lands, or Wasm output measurably
benefits from algebraic exploration the JIT cannot do).

## Migration plan

The plan has two parts: the **required path** (Stages 1 – 6, 9) that
delivers TODO② via engine-maintained side-tables and the
interprocedural worklist, and an **optional acceleration** (Stages 7 – 8)
that promotes the ValueGraph from a side-table into IR-level operands
and replaces destructive rules with equality saturation.

Each migrated rule must be byte-output-identical to the pass it replaces
on the full fixture + E2E suite and on `package-gale`, before the
predecessor is deleted. If the optional acceleration ever activates,
Stage 7 alters the NIR shape (pure `ExprKind` variants vanish) but the
WIR output stays byte-identical.

### Completed substrate

- [x] Engine substrate, edit API, `Rule` / `run`; arena-only NIR.
- [x] Per-function dirty-set gate over the existing loop.
- [x] `EngineBuffers` pooling, allocation-cost reduction.
- [x] All ~15 intra-procedural rules routed through the engine edit API:
      `ref_elim`, `elide_box_local`, `value_copy_elide`,
      `labeled_block_fusion` (shared peephole session); `sroa`,
      `container_sroa`, `store_load_forward`, `copy_prop`, `cse`, `licm`,
      `condition_implication`, `tmpl_hoist` (per-function standalone
      sessions); `match_to_switch`, `select_lowering`, env-free
      `const_folding` (peephole session). Mutations all go through
      `Engine::*`; parent map and use index invariants are upheld.

### Required path

#### Stage 1 — ValueGraph foundation

- [x] `ValueId`, `ValueKind`, `ValuePool` with hash-cons; the full
      `ValueKind` set above (heap-version-bearing kinds may be stubbed
      until Stage 5). Standalone module, exhaustive unit tests, no engine
      integration. SkelTree unchanged.

#### Stage 2 — Per-function builder

- [x] Walk the SkelTree assigning `ValueId` to every pure `ExprId`.
      Maintain `current_value: HashMap<local_idx, ValueId>`; build
      `Select` at If / Match / Switch endpoints; emit `Opaque` for loop
      locals; seed parameters with `Opaque`.
- [x] Side table `value_of: IndexMap<ExprId, ValueId>` populated per
      function. Engine exposes `engine.value(expr) -> Option<ValueId>`,
      built lazily on first call. Edits do not invalidate the cache;
      rules snapshot results before editing, or call
      `Engine::invalidate_value_graph` to force a rebuild.

#### Stage 3 — CSE migration

- [x] `CseRule` uses `engine.value(e1) == engine.value(e2)` for
      equality. The structural `CseKey` and its supporting walks are
      deleted after byte-identical confirmation. First end-to-end proof
      that Stage 1 + 2 are correctly wired.

#### Stage 4 — copy_prop migration _(deferred)_

Skipped on the required path: `copy_prop`'s source-stability check is
not subsumed by `ValueId` equality alone. A `let x = y` where the
target `x` is later observed with the same `ValueId` as a literal does
not imply the read is stable to substitute — write-once `x` whose
reassigned source `y` invalidates can still see equal `ValueId`s at the
two reads while being unsafe to fold. Revisit alongside Stage 6's
algebraic rules over `Select` and `Opaque` provenance.

#### Stage 5 — store_load_forward + heap-version activation

- [x] Activate the `FieldAccess` ValueKind with per-field heap versions.
      The builder bumps the appropriate field's version on each
      heap-write Skel node. `FieldAccess(r, f, v)` reads at the same
      `(r, f, v)` return the same `ValueId`, automatically forwarding
      stored literals.
- [x] `store_load_forward`'s `KnownValues` / `ModifiedLocalsCache`
      walker collapsed into a thin rule: walks `Local` mentions, consults
      `engine.value(read)`, replaces with the source literal when the
      `ValueKind` is `Int`/`Float`/`Bool`/`Char`. Locals in
      `NirFunction::address_taken_locals` or `stores_aliased_locals` are
      excluded — the builder models writes through neither.

#### Stage 6 — const_folding, condition_implication, licm

Builder prerequisites (landed):

- Reachability-aware heap joins: branch endpoints join field versions
  per fall-through arm instead of `bump_all`, so a loop guard and a
  later bounds check share field VNs. A labeled block whose `break`
  targets its own label falls through (fixture
  `labeled_block_self_break_field_write.wado`).
- Field store→load seeding: `obj.f = v` and struct-literal `let`s seed
  `(receiver, field, version) → value`, peering through block /
  labeled-block constructor wrappers with a sole-producer check.
  Seeding needs no alias exclusion: the heap version is global per
  `field_index`, so any write to field `f` — through any receiver or
  reference alias — bumps the `f` version and a read at the post-write
  version misses every prior `f` seed (calls / deref / global writes
  `bump_all`). Version monotonicity thus makes stale seeds unreachable
  even for reference-aliased receivers, so they seed like any other.
- Per-loop pre-header snapshots of `current_value`
  (`Engine::loop_entry_value`); parameters must be seeded up front
  (`Engine::set_param_locals`) to appear in them.

Migrations:

- [ ] Env-free `const_folding` as algebraic rules over Value kinds.
      Landed slice: `store_load_forward` substitutes any read whose VN
      is a literal (including seeded field reads). Remaining: the
      ValuePool's smart constructors (`binary` / `unary` / `cast`) fold
      literal operands into a literal `ValueId`. Correctness needs the
      operand width and signedness, so `ValueKind::Int` carries a numeric
      type (or the constructor takes the result `TypeId`); `i32 + i32`
      then wraps at 32 bits exactly as `niri` does. This is the last
      capability the builder lacks before `field_env` is deletable: a
      forwarded `max.low → -1` is only useful if the enclosing
      `-1 == -1` folds too. (Measured: with field forwarding complete,
      the only fixtures that still need the visitor's field path are the
      six whose asserts hinge on such a comparison fold — int128,
      static_1/2, coerce_int_3, match_literal_i128.)
- [x] Env-bound `const_folding` — niri commits via the engine. The
      const-fold visitor drives its bottom-up flow-sensitive walk over an
      `Engine` session and commits every niri rewrite through an `EditSink`
      (`replace_kind` / `become_expr` / `alloc_*` / `set_block_stmts`) instead
      of mutating `Body` in place, so the parent map / use index stay coherent
      through the walk. niri's rewrites are sink-generic (`reduce_local_via`,
      `reduce_local_block_via`, the short-circuit / if / match collapses); a
      `BodySink` backs the in-place CTFE scratch path, an `EngineSink` (in
      `optimize::const_folding`) backs the real walk. Goldens byte-identical;
      full e2e green.
- [x] ValueGraph reference look-through (the value-typed half of D). A
      `let r = &v` reference now forwards `r.f` from `v`'s field slot in the
      builder (`ref_targets`, cleared on reassignment; the pointee's live slot
      state is used as-is, so a stale forward is impossible). This closed the
      ValueGraph's reference-aliasing blind spot — `Ref(v) ≠ v` by receiver VN
      — that the WEP had kept Skel-side. Nine goldens improve (the
      `opt_licm_immut_ref*` cluster now const-folds immutable-ref fields).
- [x] ValueGraph call-survival for immutable-only locals (`mut_escaped`). The
      builder used to invalidate every reference-aliased local's fields on
      every call (`bump_call_effects` over the whole `aliased` set), so a field
      read never forwarded across a call once the local's address was taken —
      even when the only handle was an immutable `&v`. `optimize::alias` now
      derives a `mut_escaped` set _subtractively_ from the proven-conservative
      `aliased` set: a local is dropped only when it is provably immutable
      across calls — its type is transitively free of shared mutable state
      (`CallImmutability`: no `&mut`, `Box`/`List`, array, resource, reactive,
      or variant/generic/unknown shape) _and_ it has no syntactic mutable
      escape (`&mut v`, a mut-ref argument, a `&mut self` receiver read from the
      callee's declared first-param type à la `copy_prop`, or a `stores`
      stash). The result is closed over the reference alias groups so a `&mut`
      to a shared pointee still clobbers every alias. `bump_call_effects` bumps
      only `mut_escaped`; field-_write_ granularity still uses the full
      `aliased`. Effect: POD value structs whose only escape is an immutable
      `&self` receiver (e.g. `i128`/`u128`) keep their forwarded fields across
      the formatter calls, so the `high < 0` branch in the inlined `abs_u128`
      folds away — a CTFE-cluster fold the IR-level ValueGraph passes now do
      _without_ niri's evaluator. Five goldens improve; full e2e green. (A
      first attempt built `mut_escaped` additively from syntactic `&mut` sites
      and miscompiled `ref_2` — boxing erases the `&mut`, and a Box/List value
      passed by value is itself a mutable handle; the subtractive,
      type-driven model above is the fix.)
- [x] HFS shadow-init forwarding via a post-`field_scalarize` store→load
      forward. `field_scalarize` runs once after the fixed-point loop and emits
      scalarization shadow inits (`__hfs_x = obj.f`); previously the only
      post-scalarize fold was `field_env`-based `const_fold_post_global`. An
      ungated `forward_stores_to_loads_all` (factored from the gated pass via a
      shared `forward_one`) now runs once after `field_scalarize`, before
      `const_object_globalization`. It folds a shadow read whose field the
      ValueGraph still knows, and lets DCE drop a wholly-constant scalar chain.
      This closed the entire HFS cluster: disabling `field_env` now regresses 5
      fixtures, down from 26. (`hfs_break_from_scalar_state_commits` was
      rewritten to keep its scalar non-constant — `self.base + k` through an
      `#[inline(never)]` method — so the commit-on-break machinery it guards
      survives the improved optimizer instead of folding to one constant store.)
- [ ] `field_env` not yet deletable: disabling it regresses 5 remaining
      fixtures, in two harder clusters that the ValueGraph passes do not yet
      reproduce:
  - int128 const-object dedup (`coerce_int_3`, `int128_cast_to_primitives`):
    `field_env` lets `const_object_globalization` recognise a repeated
    `u128 { 1000, 0 }` and hoist it into one shared global; without it each
    site re-emits the struct. Closing this means the globalization recogniser
    matching repeated constant structs via the ValueGraph rather than
    `field_env`'s global field map. Code-size only; runtime identical.
  - Mutable-reference field forwarding (`mut_param`, `mut_param_merged`,
    `ref_1`): `field_env` folds `q.x == 0` (eliding an assert) and the
    `n.value = 1; n.value + 1 → 2` accumulation where `q` / `n` are `&mut` /
    `Box`. The ValueGraph keeps these conservative — `mut_escaped` deliberately
    excludes mutable-reference types — so closing this is the soundness-
    sensitive step: forward a `Box` / `&mut` field read only across
    straight-line code with no intervening mutation of that slot.
    Deletion path: reproduce the const-object dedup and the mutable-reference
    field reads in the ValueGraph, then delete `field_env`. The store→load
    duplication the WEP set out to remove is gone, and the larger HFS and
    CTFE-cluster folds now happen IR-side; these 5 are the remainder.
- [x] `condition_implication`: all guard kinds (loop, dominating,
      early-exit, short-circuit, bitmask) unified into one
      `GuardFact { var_vn, max_offset, bound_vn, is_strict }` with
      multi-hop `Add` decomposition. The `DefMap` / taint / kill
      machinery is deleted (1,940 → ~730 lines) — a mutation between
      guard and check changes the check operand's `ValueId`, so facts
      go stale by construction (fixtures
      `array_bounds_elim_oob_guard_var_mutated.wado`,
      `array_bounds_elim_oob_bound_shrunk.wado`; the migration fixed a
      position-blindness miscompile in the syntactic predecessor).
- [x] `licm` arithmetic hoisting. Design refinement: clone-to-pre-header
      hoisting needs _pre-header stability_ — each `Local` leaf's
      use-site `ValueId` equals the loop-entry snapshot value — not
      cross-iteration invariance (`loop { x = 5; … x+n … }` has an
      invariant use value that differs from the pre-header `x`). An
      invariance predicate built first on that wrong premise was
      deleted unconsumed. Dedup is by `ValueId` (copies share one
      temp); the field-hoist half keeps `ModifiedVars` until
      per-`(receiver-root, field)` heap precision lands via
      `mod_ref.rs`. `&x` look-through stays Skel-side — `Ref` /
      `MutRef` are deliberately not pure values.
- [ ] Simple induction-variable recognition (`Opaque` tagged with
      `{ base, step }` in a side table). Not needed by the landed
      consumers — post-increment reads already appear as
      `Add(opaque_i, step)` — so this lands when a rule first wants it.

#### Stage 9 — Interprocedural worklist

Stages 7 – 8 belong to the optional acceleration path below; the required
path jumps from Stage 6 directly to Stage 9.

- [ ] Old per-pass walkers (stub-only after Stages 3 – 6) are deleted in
      bulk.
- [ ] `inline` / `dae` / `drve` / `sroa_param` / `value_copy_demote` move
      to a call-graph worklist driver that re-runs the per-function
      engine session for affected callers when a callee's signature or
      body shrinks. `OptConfig::iterations` shrinks to the convergence
      bound of the worklist, not a fixed-pass count.
- [ ] Terminal / once-only stages stay explicit pre- or post-saturation,
      not loop members: `multi_value_return`, `field_scalarize`,
      `const_object_globalization`, `dce`.

### Optional acceleration (measured-deferred)

Stages 7 – 8 promote the ValueGraph from a side-table to an IR-level
substrate and replace destructive rule application with equality
saturation. They unlock algebraic exploration (re-association,
distributive law, strength-reduction-per-use, cost-based
share-vs-duplicate) that the required path cannot. The current Wado
target — Wasm output JITted by the host — recovers most of those gains
through the JIT anyway, so the optional path is deferred until
measurement shows it justifies its cost. See "Why Layer 2 promotion is
deferred" above.

#### Stage 7 — Skel pure-ExprKind retirement _(optional)_

- [ ] Remove pure `ExprKind` variants (`IntLiteral`, `FloatLiteral`,
      `BoolLiteral`, `CharLiteral`, `StringLiteral`, `Null`, `Unit`,
      `Binary`, `Unary`, `Cast`) from the SkelTree. Skel child slots
      that previously held an `ExprId` to a pure expression hold
      `Operand::Value(ValueId)`.
- [ ] `lower::translate` builds pure values directly in the ValueGraph
      and stores `Operand::Value` on the parent's slot. `wir_build`'s
      reads follow `Operand`; the WIR output shape is unchanged.
- [ ] The `value_of: IndexMap<ExprId, ValueId>` side-table is removed.
      Stage 3 – 6 rules that queried `engine.value(expr_id)` shift to
      following `Operand` directly — rule logic unchanged, API surface
      adjusted.

#### Stage 8 — Saturation driver + cost-based extraction _(optional)_

- [ ] Per-function session runs all rules together to saturation,
      bounded by node count budget + per-ValueId rewrite count + outer
      iteration limit (defaults loose, env-var overridable).
- [ ] Extraction walks the SkelTree picking a cost-minimal Skel form for
      each `Operand::Value`. Multi-use ValueIds are materialised via a
      hoisted `let __t = ...` only when the cost model favours sharing.
- [ ] The global fixed-point loop, per-rule `applied: Cell<bool>`
      guards, `peephole.rs` (the rule list lives on the engine), and
      the per-pass dirty-set in `gate.rs` are removed in favour of the
      saturation driver.

## Soundness invariants

- Byte-identical co-existence per stage. Each Stage 3 – 6 rule migration
  must reproduce its standalone predecessor's output on the full suite
  before the predecessor is deleted. If Stage 7 ever activates, it is
  allowed to alter the NIR shape but the WIR output stays
  byte-identical.
- Rules are idempotent (re-running on the same input does not regress)
  and either confluent or priority-ordered. Under the required path's
  destructive driver, priorities are encoded by rule order in the
  engine session, the way `peephole.rs` already does.
- The ValueGraph builder classifies expressions as pure or impure at
  build time. Impure expressions get no `ValueId` (they stay Skel-side);
  pure expressions get a hash-cons-deduplicated `ValueId`. The
  `Operand::Value(v)` form (when Stage 7 activates) is restricted to
  pure values by construction.
- Heap-version monotonicity: a `FieldAccess` value's `heap_ver` is the
  version before the read. A write Skel node that follows bumps to a
  fresh version; any later read at that field gets a fresh `ValueId`.
- Interprocedural over-approximation only costs a redundant re-combine;
  under-approximation drops an optimization — the same one-sided safety
  argument as the dirty-set gate (every loop pass is optional, so
  imprecision costs quality, never correctness).
- Under the optional Stage 8 driver: saturation has bounded budgets
  (node count + per-node rewrite + outer iterations). A budget hit is a
  graceful fallback to the partially saturated graph, never a panic;
  the fallback is monotone (never produces worse output than the
  pre-saturation IR).

## Consequences

### Expected effect — required path

- Per-pass analysis reconstruction disappears: CSE keys,
  source-stability walks, modified-locals caches, def maps,
  env / field_env all collapse into one ValueGraph that every rule
  shares.
- Reaching-def queries become O(1) (the side-table already encodes the
  current value for every local at every program point).
- Code reduction: the `optimize/` directory shrinks from ~13K lines to
  an estimated ~8K (each migrated pass becomes a thinner rule querying
  the ValueGraph; the structural rules and Skel-side logic still need
  their walks).
- Compile-time target: package-gale optimise phase ~1.5× faster than
  the current substrate-only baseline. Aspirational, not committed.

### Additional effect — if optional acceleration ever activates

- Phase-ordering hazards dissolve: rules co-exist under saturation; the
  hand-tuned ordering in `run_optimization_passes` becomes irrelevant.
- Algebraic exploration enables re-association, distributive law,
  strength-reduction-per-use, and cost-based share-vs-duplicate.
- Code reduction further to an estimated ~3 – 4K (each pass becomes a
  50 – 150 line declarative rule set; the global fixed-point loop and
  per-pass dirty-set go away).
- Compile-time impact is uncertain (saturation has tuning overhead).

### Risks

- Heap modeling precision: MVP per-field can be too coarse around calls.
  `mod_ref.rs` integration in Phase 2.
- Loop induction recognition: pattern-matched at Stage 6; insufficient
  coverage costs `condition_implication` and `licm` quality. Measured.
- `niri.rs` refactor: the in-place `reduce_*_a` cluster is rewritten to
  return Value descriptions. CTFE step budgeting and effect handling
  carry over but the API surface changes.
- (Optional Stage 8) Equality-saturation tuning: rule sets can explode
  without confluence. Budget-bounded; investigative — visualisation
  tooling (`WADO_DUMP_VALUE_GRAPH`, `WADO_DUMP_AFTER_RULE=...`) is a
  hard prerequisite.
- (Optional Stage 8) Extraction cost tuning: picking when to share vs
  duplicate a multi-use `ValueId` is heuristic. Tuned with benchmarks.

### Trade-offs accepted

- The ValueGraph adds a hash-cons table, an explicit kind enum, and a
  builder. Under the required path it is reachable as an
  engine-maintained side-table (`engine.value(expr_id)`); under the
  optional acceleration it promotes to IR-level operands. The same
  data structures and builder algorithm carry over either way.
- If the optional acceleration ever activates: the Stage 2 – 6
  scaffolding (the `value_of` side-table; the `engine.value(expr_id)`
  API surface) retires at Stage 7. Real throwaway code budget:
  ~200 – 300 lines of bridge logic; the version-tracking algorithm, the
  rules, and the ValueGraph itself survive the transition.
- Arena compaction (dead nodes from in-place rewrites are not freed
  mid-run; ~1.66× bloat measured at end-of-optimize on `package-gale`)
  becomes more worthwhile once the engine walks bodies fewer times;
  tracked as a follow-up.

## See also

- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the landed engine substrate, edit API, and gate.
- [NIR Skeleton Arena (Layer 1)](./wep-2026-06-05-nir-skeleton-arena.md) — the SkelTree substrate.
- [`docs/optimizer.md`](./optimizer.md) — the current pass inventory the two-tier engine absorbs.
- Cranelift's aegraph mid-end and `egg` (https://egraphs-good.github.io/) — the e-graph mechanics this design adapts.
- The profiling workflow behind the cost numbers above: `.claude/skills/profiling-wado-compiler`.
