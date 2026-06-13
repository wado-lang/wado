# WEP: Worklist-Driven NIR Rewrite Engine

This WEP sets the terminal architecture for the NIR optimizer: a two-tier IR
(structured effect skeleton + hash-consed pure-value graph) driven by a
single worklist engine, with optimizations expressed as declarative rewrite
rules under equality-saturation semantics, and a call-graph-driven
interprocedural driver replacing the global fixed-point loop.

The engine substrate, the arena NIR, the routing of every intra-procedural
pass through the engine edit API, and the ValueGraph foundation (kinds,
hash-cons pool, per-function builder) have all landed, along with the
Stage 3 – 6 rule migrations onto the ValueGraph — culminating in the
retirement of niri's per-local `field_env` (the ValueGraph now owns all
field reaching-def; one `int128_cast` reinterpret assert is a +36-byte
code-size residual deferred to `mod_ref.rs`). The interprocedural worklist
(Stage 9) is the remaining required-path item. Full Layer-2 promotion (Skel
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

### Done (required path)

- [x] **Substrate.** Engine (edit API, `Rule` / `run`, arena-only NIR),
      per-function dirty-set gate, `EngineBuffers` pooling, and all ~15
      intra-procedural passes routed through the engine edit API.
- [x] **Stage 1 – 2 — ValueGraph + builder.** Hash-cons `ValuePool`
      (`ValueId` / `ValueKind`); a per-function builder assigns a `ValueId` to
      every pure `ExprId` (`current_value` threading, `Select` at merges,
      `Opaque` for loop locals / params), exposed lazily as `engine.value(expr)`.
- [x] **Stage 3 — CSE.** Equality is `engine.value(a) == engine.value(b)`; the
      structural `CseKey` is gone.
- [x] **Stage 5 — store_load_forward.** Per-field heap versions on
      `FieldAccess`; the rule forwards a read whose VN is a literal.
      (Address-taken / `stores`-aliased locals are excluded — the builder
      models writes through neither.)
- [x] **Stage 6 — const_folding / condition_implication / licm.**
  - Env-free and env-bound `const_folding` fold via the ValueGraph (niri's
    CTFE reused; niri commits through an `EditSink`).
  - Reference look-through (`let r = &v` forwards `r.f`), call-survival for
    immutable-only locals (`mut_escaped`, derived subtractively from `aliased`
    so a POD value struct escaping only by `&self` keeps its forwarded fields),
    and HFS shadow-init forwarding.
  - `field_env` retired: niri's per-local field map and all its machinery are
    deleted; the ValueGraph owns all field reaching-def. Three precision fixes
    closed the gap — `copy_local_field_slots` (forward through a `Local`
    producer), short-circuit precise heap invalidation, reflexive equality
    (`x == x`). Residual: `int128_cast_to_primitives` is +36 bytes (one
    reinterpret assert whose seed and consumer never coexist in one per-build
    ValueGraph — needs the Phase-2 `mod_ref.rs` heap model).
  - `condition_implication` unified into one `GuardFact` (facts go stale by
    construction — a mutation changes the operand's `ValueId`); `licm` hoists by
    pre-header `ValueId` stability.

### Open

- [ ] **Stage 4 — copy_prop (deferred).** Source-stability is not subsumed by
      `ValueId` equality (a write-once `x` whose source `y` is later reassigned
      can read equal VNs yet be unsafe to fold). Revisit with Stage 6's
      `Select` / `Opaque` provenance.
- [ ] **Stage 6 — induction-variable recognition** (`Opaque` tagged
      `{ base, step }`). Not needed yet — post-increment reads already appear
      as `Add(opaque_i, step)` — so it lands when a rule first wants it.
- [ ] **Stage 9 — interprocedural worklist.** Move `inline` / `dae` / `drve` /
      `sroa_param` / `value_copy_demote` onto a call-graph worklist that re-runs
      affected callers when a callee shrinks; `OptConfig::iterations` becomes the
      worklist convergence bound. Terminal stages (`multi_value_return`,
      `field_scalarize`, `const_object_globalization`, `dce`) stay explicit. The
      stub per-pass walkers are then deleted in bulk.

### Optional acceleration (measured-deferred)

Stages 7 – 8 promote the ValueGraph from a side-table to an IR-level substrate
and replace destructive rules with equality saturation. They unlock algebraic
exploration (re-association, strength-reduction-per-use, share-vs-duplicate)
the required path can't — but Wasm output is re-JITted by the host, which
recovers most of it, so they wait on measurement. See "Why Layer 2 promotion
is deferred".

- [ ] **Stage 7 — retire pure `ExprKind` from the SkelTree.** Pure literals /
      `Binary` / `Unary` / `Cast` slots become `Operand::Value(ValueId)`;
      `lower::translate` builds the values directly and `wir_build` follows
      `Operand` (WIR output unchanged). The `value_of` side-table is removed.
- [ ] **Stage 8 — saturation driver + cost-based extraction.** Run all rules to
      a budget-bounded saturation, then extract a cost-minimal Skel form per
      `Operand::Value` (materialising a multi-use VN only when sharing wins).
      The global fixed-point loop, per-rule `applied` guards, `peephole.rs`, and
      the dirty-set gate go away.

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
