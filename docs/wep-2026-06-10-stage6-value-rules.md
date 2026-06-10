# WEP: Stage 6 Value Rules — const_folding / condition_implication / licm on the ValueGraph

Detailed design for Stage 6 of the
[Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md):
migrating `const_folding`, `condition_implication`, and `licm` onto the
engine-maintained ValueGraph, plus the builder upgrades (loop scopes,
induction tags, merge-join precision, field-store seeding) those
migrations need.

## Context

Stages 1 – 5 landed the `ValuePool` / hash-cons kinds, the per-function
builder with `current_value` local tracking and per-field heap versions,
the lazy `Engine::value(expr)` side-table, and the `cse` /
`store_load_forward` migrations. The three Stage 6 passes still carry
their own per-pass dataflow:

- `const_folding` runs the `niri::Interpreter` with `env` / `field_env` /
  `GlobalEnv` plus `alias.rs` annotations, and is the last optimizer pass
  that mutates `Body` directly instead of going through `Engine::*`
  (`reduce_local_a`, `reduce_local_block_a`, `rewrite_if_expr_a`,
  `rewrite_match_expr_a`).
- `condition_implication` rebuilds a `DefMap` (`Copy` / `AddConst` /
  `IntConst` / `BitAndConst` / `FieldAccess` / `StructLit`) and a
  whole-function `Taints` set per run, and compares guard and check
  operands by _syntactic_ identity (`resolves_to(i, i)` is
  unconditionally true).
- `licm` rebuilds `ModifiedVars` (full / per-field / alias pairs /
  written-field-types / clobbered-pointee-types), an immutable-ref
  binding map, and a structural `arith_exprs_equal` per run.

### Soundness finding: position-blind guard identity

Designing the `condition_implication` migration exposed a P0 soundness
bug in the current pass, since fixed ahead of this design (see
`tests/fixtures/array_bounds_elim_oob_guard_var_mutated.wado` and
`array_bounds_elim_oob_bound_shrunk.wado`): a guard fact `i < bound` was
applied to checks appearing _after_ a mutation of `i` or of `bound`'s
backing storage within the same iteration. `while i < arr.len() { i +=
1; arr[i] }` lost its bounds check to a raw wasm trap, and a `pop()`
that shrank `.used` before an access produced a silently wrong result
(the GC backing array retains the old capacity, so nothing traps).

The interim fix keeps the pass syntactic but makes guard facts
positional: a document-order `KillEvents` scan retires a guard once the
loop body may write its variable, its bound's field, or the heap; and
`Def::FieldAccess` equivalences are only recorded for never-written
`(local, field)` pairs.

This bug class is the strongest argument for the Stage 6 design: a
`ValueId` _is_ a flow-sensitive value identity. Two reads straddling a
write get different `ValueId`s by construction, so the migrated rule
cannot make this mistake — the kill machinery and the taint gate are
deleted together with the `DefMap`.

## Decision

Stage 6 keeps the required-path architecture: rules stay destructive,
apply at Skel positions (`ExprId` / `StmtId`) through the engine edit
API, and use the ValueGraph as a side-table for operand identity and
classification. Value-level normalization happens at intern time
(smart constructors on `ValuePool`), not via a saturation driver.

Two disciplines hold throughout:

- Snapshot-then-commit: edits do not invalidate the cached graph, so a
  rule collects every rewrite against one fresh graph, commits them all,
  then calls `Engine::invalidate_value_graph` if it iterates. Committing
  a value-preserving rewrite (replacing an expression with a literal of
  its own value) keeps the cached graph consistent for the remaining
  snapshot entries.
- Byte-identical staging: each increment lands first at parity with its
  predecessor (byte-identical WIR on the fixture + E2E suite and
  `package-gale`), then any strengthening that the ValueGraph enables
  lands as a separate commit whose diffs are reviewed as improvements
  (including `wir_expect` updates).

### Increment 6.0 — builder and engine prerequisites

Reachability-aware merge joins. Today every `If` / `Match` / `Switch`
endpoint ends with `heap_state.bump_all()`, and arm merges include
non-fall-through arms. Consequence: any loop whose first statement is
the `if !(cond) { break }` guard — i.e. every desugared loop — splits
the heap version of all later field reads, so `arr.used` read in the
guard and in a later bounds check never share a `ValueId`. The fix
mirrors what `const_folding`'s `Arm { reachable }` join already does on
its field snapshots:

- Per-arm: snapshot heap state, walk the arm, capture state +
  fall-through flag (`Break` / `Return` / `Continue` terminal stmt or a
  never-typed tail, the `block_falls_through` logic).
- Join over fall-through arms only: a field keeps its version when all
  fall-through arms agree on it; otherwise that field bumps. The
  trailing unconditional `bump_all` is removed. Local merges
  (`merge_two_arms` / `merge_n_arms`) get the same reachability filter.

A panic-only bounds-check arm or a `break`-only guard arm then
contributes nothing, and field reads across the guard share VNs. This
also strengthens `cse` / `store_load_forward`; it lands as its own
commit with the byte-identity check.

Per-loop scopes and invariance. The builder maintains a loop-scope
stack. For each `Loop`, it records into `ValueGraphBuild`:

- the fresh `Opaque`s minted for loop-written locals at entry (the
  "loop-phi" stand-ins; `ValueKind::LoopPhi` stays dormant — no cyclic
  graph),
- the heap activity of the body (fields bumped, or bumped-all),
- a `variant: bitset over ValueId` computed by one forward pass over the
  pool slice interned during the body walk: a value is loop-variant iff
  it is one of the entry opaques, an opaque minted inside the body, a
  `FieldAccess` whose field the body bumps (or `bump_all`), or any of
  its operands is variant.

Engine API: `engine.is_loop_invariant(loop_body: BlockId, v: ValueId)`.

Induction tags. During `walk_loop`'s pre-scan, a written local `i`
whose only writes in the body are exactly one `i = i + C` (C an integer
literal; subsumes the `+=` desugar) gets its entry opaque recorded in a
side table `induction_of: IndexMap<ValueId, Induction { base: ValueId,
step: i64 }>` (`base` = the local's value before the loop). The
`ValueKind::Opaque` shape is unchanged — the tag is a side table, so
hash-consing semantics stay put. Consumers pattern-match via
`engine.induction(v)`; reads after the in-body increment naturally
appear as `Binary(Add, opaque_i, Int(step))`, so no implicit identity
between pre- and post-increment reads can arise.

Field-store seeding. The builder currently bumps a field's version on
`Assign`-to-`FieldAccess` but does not remember the stored value, and a
`let x = S { f: 16 }` binds `x` to a bare `Opaque`. Add a builder-side
map `(receiver ValueId, field_index, HeapVersion) → ValueId` populated
at:

- `Assign(FieldAccess(pure recv, f), pure value)` — after the bump,
  seed the new version with the stored value,
- `Let` of a `StructLiteral` — bind the fresh receiver opaque and seed
  each pure field value at the current version (struct literals stay
  Skel-side; only the field projections enter the graph).

`compute_value` consults the map before interning a `FieldAccess` kind,
so a read of a seeded field returns the stored value's `ValueId`
directly. This completes store→load forwarding for fields (Stage 5
only dedups _reads_) and gives `condition_implication` its
`StructLit`-field numeric facts.

Alias-safety prerequisite (found while prototyping). A first attempt at
this seeding (the `field_store` map exactly as above) forwarded
`p.a` to the `assert p.a == 42` in `stores_optimize_with_stores_no_forward`
and `stores_optimize_mixed_calls`, where a `with stores[p]` callee was
inlined into a `Holder { pair: &p }` shape that captures `&p` without
an opaque call to bump the heap. Runtime output stayed correct (the
field _was_ still 42 there), but those fixtures deliberately lock in
the conservative non-forwarding that `store_load_forward`'s
`stores_aliased_locals` / `address_taken_locals` exclusion provides,
and the builder has neither set — `builder::build` takes only `body`
and `params`. So field-store seeding must thread those two sets in and
skip seeding a receiver that is (or derives from) an aliased /
address-taken local. That threading lands together with this seeding,
not before; the prototype is reverted until then so the heap-join
increment (6.0, landed) stays independently shippable.

Unsafe locals stay a rule-layer concern. Address-taken locals are boxed
by lower (`&mut x` turns `x` into `Box<i32>` with `.value` field
accesses), so bare-local staleness from reference writes does not arise
in well-formed NIR; rules keep excluding
`address_taken_locals` / `stores_aliased_locals` reads defensively, the
way `store_load_forward` does, against transient post-inline shapes.

### Increment 6.1 — value_fold (env-free + env-bound constant folding, store_load_forward)

ValuePool smart constructors. `binary` / `unary` / `cast` / `select`
gain folding variants that the builder calls with the result type's
`PrimitiveType` (from the Skel node's `type_id`):

- Literal evaluation delegates to `niri`'s `eval_binary` / `eval_unary`
  / `eval_cast` (already pure over `Value`); results truncate per the
  supplied primitive. Trapping shapes (`Div` / `Mod` by zero,
  out-of-range casts) intern un-folded — the runtime trap must survive.
- Integer identities: `x+0`, `0+x`, `x-0`, `x*1`, `1*x`, `x*0 → 0`
  (operand is pure by construction), `x&0`, `x|0`, `x^0`, `x<<0`,
  `x>>0`; `And` / `Or` with a literal side; `Not(Not(x)) → x`,
  `Not(lit)`; `Select(true, a, b) → a`, `Select(false, a, b) → b`,
  `Select(c, a, a) → a`.
- Floats fold only literal×literal (bit-exact, via `niri`); no float
  identities (`x+0.0` is wrong for `-0.0`, `x*0` for NaN).

Because the builder works bottom-up, the graph is in normal form after
one walk: `let z = 0; x + z` yields `value_of[x + z] == value_of[x]`,
and constants propagate through locals, `Select`-free merges, and
seeded fields without an `env`.

The Skel rule. A per-function standalone session (`ValueFoldRule`)
replaces every pure expression whose `ValueKind` is a literal — and
whose `ExprKind` is not already that literal — with the literal
`ExprKind` (`literal_source` preserves `repr`), excluding assign
targets and unsafe-local reads. Bounded fixpoint: commit a batch,
`invalidate_value_graph`, rebuild, repeat (bound ~4).

Subsumptions, in deletion order after byte-identical confirmation:

- `store_load_forward` — its substitution is the `Local`-read special
  case of the rule.
- The env-free `ConstFoldRule`'s `try_fold_a` half. The CTFE half
  (`try_call_fold_a`) stays a peephole rule — calls are Skel-side; a
  later strengthening can classify argument literal-ness via
  `engine.value` instead of requiring literal `ExprKind` children.
- The flow-sensitive walker's env-bound local-constant rewrites (its
  `env` lattice for non-mut locals) become redundant; the walker keeps
  branch collapsing, the alias-aware `field_env` cases the seeding does
  not cover, and `GlobalEnv` reads until measurement shows what is left.

Parity protocol: the first commit limits smart constructors to literal
evaluation (exactly what `try_fold_a` + `env` folding achieve at
fixpoint today); identity rules land separately with reviewed diffs.

### Increment 6.2 — niri purity refactor (engine-routed const_folding)

`niri.rs` stops mutating the optimized `Body`:

- `reduce_local_a` becomes `reduce_kind_a(&mut self, body: &Body, e) ->
  Option<ExprKind>` — every rewrite it performs (literal substitution,
  `GlobalVarGet` / `field_env` folds, CTFE results, short-circuit
  identities, constant-`if` → `Block(arm)` / `Unit`, constant-`match` →
  chosen arm) is already a kind replacement at `e`.
- `reduce_local_block_a` (constant-`if` statement splice) becomes a
  pure `spliced_stmts(body, block) -> Option<Vec<StmtId>>`; the caller
  commits via `engine.set_block_stmts`.
- `ConstFoldVisitor` becomes a per-function standalone engine session
  (same shape as `licm` / `condition_implication`); commits go through
  `replace_expr_kind` / `set_block_stmts`. CTFE's `reduce_in_place_a`
  on the scratch callee clone stays niri-internal — the clone is not
  engine-managed state.

This is a pure refactor (byte-identical), and removes the last
non-engine mutation path in the optimizer.

### Increment 6.3 — condition_implication on ValueIds

A guard fact becomes a pair of `ValueId`s captured at the guard
position: `Guard { var: ValueId, bound: ValueId, strict: bool }` (loop
guards, dominating guards with `max_offset`, short-circuit guards).
Check sites compare `engine.value(check_lhs)` / `engine.value(check_bound)`:

- identity regime: `ValueId` equality — one comparison replaces
  `resolves_to`, copy chains, field-chain matching, and the
  `Def::FieldAccess` equivalence; flow-precision comes from the graph
  (a write to `i` or `arr.used` between guard and check changes the
  check operand's `ValueId`, so the implication simply fails — the
  Stage-6-interim `KillEvents` scan and the taint gate are deleted).
- plus-one (`<=` guards): `kind(check_bound) == Binary(Add, bound,
  Int(1))`.
- offset regime: `kind(check_lhs) == Binary(Add, var, Int(k))` with
  `0 <= k <= max_offset`; post-increment reads of a tagged induction
  variable appear as `Add(opaque_i, step)` and participate only through
  the explicit offset arithmetic.
- numeric regime: both bounds' kinds are `Int(_)`.
- bitmask regime: `kind(check_lhs)` is `Binary(BitAnd, _, Int(mask))`
  (or resolves to it), bound kind `Int(b)`, `b > mask >= 0`.

Skel-side survivors: loop-guard shape extraction (`if !(i < b) {
break }` as first statement), dominating-guard scoping over `if`
then-blocks, `is_panic_block` / early-exit detection, and the single
rewrite (`set_false` via `replace_expr_kind`). The `DefMap`, `Taints`,
`Bound`, and all `resolve_*` chain walkers (~700 lines) are deleted.

Prerequisite: 6.0's reachability-aware joins — without them the guard
`if` ends with `bump_all` and no field-chain bound ever matches.
Queries are snapshotted before the first `set_false` (value graph
stale-cache discipline); `set_false` replaces a `Binary` condition with
`BoolLiteral(false)`, which is not value-preserving for the node, but
no further queries read it.

Acceptance: the two OOB fixtures plus the `array_bounds_elim_*` family
must hold; byte-identical otherwise. Expected precision gains (e.g.
guards surviving across statements the kill scan conservatively
retires) land as a follow-up diff-reviewed commit.

### Increment 6.4 — licm

Invariant arithmetic on the graph. `is_invariant_arith` +
`arith_exprs_equal` collapse onto `engine.is_loop_invariant(loop, v)` +
`ValueId`-keyed dedup of maximal invariant subtrees. The trap-safety
filter stays a Skel-shape check (`Div` / `Mod` / `Cast` excluded, the
existing `is_hoistable_binop` list), since hoisting moves the _Skel
computation_ into the pre-header.

Field hoisting keeps its Skel-side legality analysis for now.
`ModifiedVars` (alias sets, written-field-types, clobbered pointees,
immutable-ref look-through) is _more precise around calls_ than the
MVP heap model: the builder bumps all fields at every non-builtin call,
so "is this `FieldAccess` ValueId loop-invariant" would deny hoisting
in any loop that calls a user function — a parity regression licm
cannot accept. The ValueGraph still takes over candidate identity
(dedup by the read's `ValueId` where one exists). Full collapse of
`ModifiedVars` is deferred behind per-`(receiver-root, field)` heap
versions via `mod_ref.rs` — the heap-precision follow-up the parent
WEP already tracks. Note `&x` reference look-through cannot ride on
`ValueId`s either: `Ref` / `MutRef` are deliberately not pure values.

This refines the parent WEP's Stage 6 wording ("licm recognises
loop-invariance as a ValueId property"): true for the scalar/arithmetic
half on the required path; the field half joins when heap precision
lands.

### Increment 6.5 — deletions and WEP bookkeeping

- Delete `store_load_forward.rs`, the env-free `ConstFoldRule`
  arithmetic half, the `DefMap` / `Taints` / kill machinery in
  `condition_implication.rs`, and licm's arithmetic-equality walkers,
  each only after its replacement has held byte-identity on the full
  suite and `package-gale`.
- Update the parent WEP's Stage 6 checklist; `OptConfig::iterations`
  and pass ordering are untouched (Stage 9's scope).

## Sequencing

1. 6.0 joins (own byte-identity check; strengthens cse/slf as a
   reviewed diff)
2. 6.0 loop scopes + induction tags + field seeding (additive; no
   consumer yet, unit-tested on the builder)
3. 6.1 value_fold parity commit → delete `store_load_forward` → identity
   rules commit
4. 6.2 niri purity refactor (byte-identical)
5. 6.3 condition_implication migration → delete DefMap/kill machinery
6. 6.4 licm arithmetic migration
7. 6.5 cleanup, WEP checklist, `docs/optimizer.md` refresh

Each step runs the full fixture + E2E suite at O0/O2 (O1/O3/Os in CI),
`mise run test-wado`, and a `package-gale` WIR byte-comparison against
the previous commit.

## Consequences

- The remaining per-pass dataflow reconstructions collapse into the one
  per-function graph build: `DefMap` / `Taints` (~700 lines), the
  env-bound local lattice, `store_load_forward` (~170 lines), licm's
  structural-equality walkers. `optimize/` moves materially toward the
  parent WEP's ~8K-line target.
- `condition_implication` becomes sound by construction against
  guard-position staleness; the interim positional-kill fix and its
  conservatism (stmt-granularity, receiver-insensitive field kills,
  whole-function `Def::FieldAccess` gating) disappear with it.
- Risks carried forward from the parent WEP: heap-model precision
  around calls (deferred to `mod_ref.rs` integration; bounded here by
  keeping licm's field legality Skel-side), induction-recognition
  coverage (single-increment pattern only; measured), and byte-diff
  churn from stronger VNs (contained by the parity-then-strengthen
  commit protocol).
- The builder gains state (loop scopes, variance bitsets, field-value
  map, induction table). Graph build stays one walk per function per
  session; the added memory is per-function and pooled with
  `EngineBuffers`.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the parent plan; Stage 6 checklist.
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — engine substrate and session model.
- [`docs/optimizer.md`](./optimizer.md) — current pass inventory.
