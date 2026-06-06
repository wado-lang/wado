# WEP: Worklist-Driven NIR Rewrite Engine

This WEP is direction-setting only: it fixes the shape of the solution, not the
detailed design. A follow-up WEP will specify the engine, the worklist
discipline, and the migration steps before any code lands.

## Context

The NIR optimizer is structured as ~31 independent passes, each a full mutating
walk over every function, run inside a global fixed-point loop that repeats
until no pass reports a change. Profiling `wado test --no-cache
example/wado_syntax_highlight.wado` (debug build) shows the cost is structural,
not local to any one pass.

Redundant traversal: the visitor walk machinery (`walk_expr` / `walk_stmt` /
`opt_walk_*`) is ~7.5% of total CPU on its own. With ~31 passes × ~4 iterations,
each function body is walked on the order of 80 times.

Wasted late iterations: the fixed point converges on a global `changed` flag,
with no per-function dirty tracking. Tracing the loop, the last iteration
changes nothing (a full ~31-pass sweep of every function purely to confirm
convergence), and the second-to-last changes only 2–4 passes — the back half of
the loop mostly re-walks already-converged code.

Fragile phase ordering: correct ordering between passes is maintained by hand
and documented in scattered comments ("Container SROA must run before inline in
each iteration…"). The ordering is load-bearing and easy to break.

These are properties of the architecture — many whole-tree rewrite passes in a
global fixed point — so incremental tuning of individual passes hits a ceiling.

## Decision

Replace "N passes × global fixed point" with a single worklist-driven rewrite
engine for the local (intra-procedural, peephole-style) rewrites — the large
majority of the current passes (const-fold, copy-prop, branch-prune, ref-elim,
select-lowering, array-literal, string-push, match-to-switch, value-copy,
elide, labeled-block-fusion, condition-implication, sroa, cse, …).

The engine visits a node only when it might be reducible: rewriting a node
re-enqueues its parent / uses / affected neighbours, and the process runs to a
local fixed point. There is no repeated whole-tree sweep and no global
convergence sweep.

Interprocedural passes (inline, DCE, DAE, globalization) stay as distinct stages
around the engine. Gating them by a per-function dirty set, so the engine only
re-runs on functions those stages actually touched, is in scope for the
follow-up design but not mandated here.

One change, three wins:

- Speed: eliminates the ~7.5% repeated-traversal cost and the wasted back-half
  iterations; nodes are visited roughly when they change, not ~80× each.
- Maintainability: one engine with one set of rewrite rules replaces ~31
  separately-driven passes and their hand-tuned ordering; adding a rewrite means
  adding a rule, not threading another pass through the loop.
- Correctness: a single, explicit worklist discipline (what re-enqueues what) is
  easier to reason about than emergent interactions between independent
  whole-tree passes and a global fixed point; phase-ordering hazards mostly
  disappear because the rules co-exist rather than run in a fixed sequence.

## IR substrate

The engine needs what the current owned `Box<NirExpr>` tree lacks: stable node
handles, parent edges, and a per-local use index to drive the worklist. The
substrate is two layers, split on the only distinction rewriting cares about —
whether a node is a referentially-transparent value or an ordered effect.

- Layer 1 — effect skeleton: a flat per-function arena (`NodeId` + parent +
  use index) for statements, control flow, places, memory writes, and
  effectful expressions. Parent is unique; the worklist is the classic tree
  walk. This is the only layer the engine strictly requires.
- Layer 2 — pure-value e-graph: a hash-consed acyclic e-graph for
  referentially-transparent expressions, whose value ids the skeleton's leaf
  operands reference. Rewrites are non-destructive (add an equivalence), CSE /
  GVN fall out for free, and "rewrite to canonical" is O(1); a final extraction
  picks the best form. This is the acyclic e-graph / equality-saturation step
  the engine does not depend on.

Keeping them as two layers rather than one uniform arena is deliberate:
identity differs (skeleton = ordered position keyed by `NodeId`, carrying
`span`; e-graph = structure keyed by hash-cons, span-free) and so does the
re-enqueue discipline (skeleton: a node's unique parent; e-graph: the
congruence parents of a merged class). Wado's aliasing, places, and value-copy
semantics keep the pure partition small — a `FieldAccess` read is not pure
across an intervening write — so the boundary is gated by the existing
`optimize/mod_ref.rs` referential-transparency check, not by node kind alone.
The reference design is Cranelift's aegraph mid-end (side-effecting skeleton +
pure e-graph + elaboration). SSA / sea-of-nodes is rejected for the same reason
NIR keeps its tree shape (see [NIR Layer](./wep-2026-05-11-nir.md)): it
dissolves the structured control flow Wasm re-emits directly.

Layer 1 is the NIR representation itself, not a conversion boundary wrapped
around the tree — a boundary is overhead the engine cannot see across,
justified only by saved work, which this WEP does not trade for. Sequencing:

- [ ] Layer 1 — land the effect-skeleton arena as the NIR representation.
- [ ] The rewrite engine on top of Layer 1.
- [ ] Layer 2 (the e-graph) — revisited and re-decided only after the engine
      lands and its effect is measured; not a prerequisite.

## Consequences

Deferred, to avoid prejudging the detailed design:

- The Layer 1 arena's field-level layout (`NodeId` space, parent / use-edge
  storage) and where the Layer 1 / Layer 2 boundary sits in the type system.
  The substrate's shape and sequencing are settled in
  [IR substrate](#ir-substrate); only the detailed layout is open.
- The exact worklist data structure, fairness, and rule-conflict policy.
- The migration order, and how long the old pass loop and the new engine
  co-exist.

Out of scope: the resolver, monomorphizer, lowering, and the WIR optimizer.

Codegen must not regress: the engine has to reach at least the current fixed
point's result on existing fixtures.

## See also

- `docs/optimizer.md` — current optimization passes.
- The profiling workflow that produced the numbers above:
  `.claude/skills/profile-compiler`.
