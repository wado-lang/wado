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

## Status and next step (handoff)

Operand promotion + build-once is the default (`b6bb675a9`); `rebuilds = 0` is
met (criterion 1). The **default `-O2` e2e is fully green** (3014/0), and so is
the whole suite — `mise run test` (all crates incl. `gale_cli`) and
`mise run test-wado` (3378/0 + 1478/0). The merge gate (all tests green, every
pre-existing failure fixed) is therefore met on this branch. What remains is
**criterion 3's side-table half**: retiring `value_of` via Phase B.3
(scheduling extraction for non-constant values); criterion 2 (optimize CPU
halved on package-gale) is not yet measured.

### Done — this branch (e2e 8 → 0, all green)

All eight remaining `-O2` fixtures and the pre-existing kiln panic are fixed;
`mise run test` (e2e 3014/0 + `gale_cli`) and `mise run test-wado` (3378/0 +
1478/0) are green, `oob` 14/14 and `WADO_VERIFY_VG` clean throughout. By
mechanism:

- BCE / guard cluster — the **re-seed (decision (a))** restores the operand
  values `licm`'s `drop_local_readers` wipes, without a rebuild (`rebuilds` stays
  0): loop-stable leaves to a `canonical_local`, field copies to a shared
  `field_access`, derived `let`s recomputed, the build's param opaque reused
  (`existing_local_opaque`). Plus `construction_field_value` recovering
  `arr.used == N` from `List::filled(N)`'s `used: N` for `le_guard` / `bitmask`.
  (`condition_implication`.)
- Store-to-load past SROA — `Engine::grow_bare_local_constants` regrows the bare
  scalars SROA mints after `inline` coarsened the graph (scratch `build_scoped`,
  not a counted rebuild). (`store_load_forward`.)
- **P0 miscompile** — a `&mut self` boxed receiver modelled as non-mutating
  (`x.bump(); assert x==2` → `if true { panic }` at O0 and O2). Boxing lowers both
  `&self` and `&mut self` to a `Box<T>` param, so the `MutRef`-only check misjudged
  `bump`; fixed by a per-callee call-graph fixpoint that marks methods writing
  through param 0 (`CallImmutability::method_writes_receiver`, `optimize/alias`).
- **Operand-promotion panic (P0)** — a promoted `Operand::Value` (constant-struct
  `FieldAccess` receiver) hit `as_expr().expect("skeleton operand")` in `ref_elim`
  / `licm` / `field_scalarize`, crashing the kiln generator; those three migrated
  to treat a promoted operand as a non-candidate (conservative, sound).
- Earlier: the 6 P0 miscompiles (`6bd41a28d`, the `cse_loop_body` clone-scope bug)
  and two drifted `wir_expect` refreshes (`opt_hfs_defer`, `brif_select`).

The minimal BCE reproducer, the exact `drop_local_readers` mechanism, and the
full re-seed design are kept under "Roadmap" / "ideal end state" below as
reference; they are resolved.

## Remaining work — start here

Criterion 1 (`rebuilds = 0`) and the cache half of criterion 3 are met; promote-
early and build-once are the default. What is left is the **`value_of`
side-table** (criterion 3's other half) and the CPU win it yields (criterion 2):
make the value passes read operands, then delete the map. Execute **Route B**
(full plan + confirmed touchpoints at "Route B execution plan" below):

### `value_of` deletion: the proven dependency chain (do not look for a shortcut)

Established by exhaustive consumer + producer analysis: deleting the `value_of`
map requires **born-as-operands for every flow-sensitive value** plus folding the
build into `lower`. There is **no incremental consumer migration** that deletes it
or even nets positive without the keystone — each was checked and rejected:

- The live `value_of` consumers are the **freeze** (`extract.rs` — the
  operand-promotion mechanism itself), **licm** (`value(leaf)` / `value(e)` /
  `cse_loop_body`), **store_load_forward** (constant field forwarding), and
  `condition_implication`'s two licm re-seed band-aids. (`cse` is skipped under
  `promote_active`; `const_fold` / `copy_prop` / `select_lowering` do not call
  `engine.value`.)
- **licm cannot be migrated structurally.** `cse_loop_body`'s `ValueId` is
  flow-sensitive and **correctness-load-bearing**: it distinguishes `x+y` before
  vs after `x = …` in the loop. A structural key would dedup them and miscompile;
  a conservative "don't dedup if any leaf is loop-modified" key is sound but
  **regresses perf** (loses CSE the value graph captures), which defeats the
  WEP's purpose. Correct + non-regressing requires loop locals frozen as
  **LoopPhi operands** — and `ValueKind::LoopPhi`'s `body_iter` is an unbuilt MVP
  `Opaque` (no induction recognition).
- **store_load_forward** is optimization-only (skipping it: full e2e, **1**
  `wir_not_expect`); its constant comes from the builder, so removing its
  `value()` needs constant fields born as operands.
- **Enabling scalar `FieldAccess` promotion by default** (which would let
  store_load_forward read operands) is itself blocked: materialising `arr.used`
  into `let _av = arr.used` defeats **WIR const-propagation** — the baseline folds
  `1 >= arr.used` because `arr` globalizes to a const struct and WIR folds the
  direct `StructGet`, but it does not propagate `_av = StructGet(const)` through
  the local (`niri_*_preserves_fields`, 3 `wir_not_expect`). The fix is WIR
  const-propagation through single-assignment local bindings (`const_forward` /
  `peephole`).
- The **freeze itself** queries `value_of` to decide promotions, so even with
  every other consumer migrated, `value_of` persists until the build is folded
  into `lower::translate` (born-at-lower) so values are born as operands without a
  query. That fold is blocked by a layering inversion (the builder needs
  `optimize::alias`, which depends on `lower`).

Sequenced plan (each step is substantial and separately validated; the order is
forced by the dependencies above):

1. [x] **WIR const-propagation through SSA local bindings** (`const_forward`,
       `2d21c0302`) — folds `LocalSet{x, scalar const}` reads, closing the
       value-graph/const_fold field_env divergence at WIR. Default e2e 3032/0.
2. [x] **Field-promotion-default** (`3a2eaffc2`) — scalar `FieldAccess` reads are
       born as operands in the default pipeline; default e2e 3032/0, no regressions.
       Reframe (decisive — makes deletion tractable): the deletion target is the
       `value_of` `ExprId → ValueId` **map**, separable from the `ValuePool`
       (`body.values`) and `loop_entry_values`. `find_imm` / `value_find` / `type_of` /
       `kind` / `collect_opaque_locals` / `loop_entry_value` read the **pool** /
       `loop_entry_values`, **not** `value_of`. So deleting `value_of` requires only
       eliminating the **`engine.value(expr)`** calls (6 live: freeze ×2,
       `cse_loop_body`, `store_load_forward`, re-seed ×2) and making the freeze's map
       **transient**. The builder and pool stay; born-at-lower is a separate later perf
       item, not a deletion prerequisite.

3. [x] **licm off `value_of`** (`df80213d5` + `fcebc7b3c`). Arith hoist: invariance via
       `!modified_vars.local_modified(leaf)` (exact — in-loop `let` counts as
       modified) and dedup via a commutative-normalised **structural key** (exact
       for invariant leaves). `cse_loop_body`: same structural key plus a "no leaf
       directly assigned across the occurrence span" run-split (`local_assigned_in_stmt`),
       address-taken leaves require loop-invariance. licm now makes **zero**
       `engine.value()` calls; default e2e 3032/0 (one minor `wir_optimize_brif_select`
       comparison-CSE boundary loss). The `find_imm` there reads the pool, fine.
4. [ ] **store_load_forward + the `condition_implication` re-seed band-aids off
       `value_of`** — **proven keystone-blocked, not independently achievable**
       (experiment, this branch). Both passes run _before_ the freeze (the last
       pass), so freeze-time extraction cannot feed them; their values must already
       be operands. Retiring store_load_forward entirely (full default e2e with the
       post-scalarize pass off) **regresses 2 fixtures**:
       `field_forward_snapshot_after_mutation` and `store_to_load_forwarding` —
       flow-sensitive field store→load forwards (`p2.a = 42; assert p2.a == 42`),
       exactly `value_of`'s content, which WIR const-prop (step 1) does **not**
       recover (those are `mut`-struct field stores, not SSA scalar-const locals).
       store_load_forward's reads are `Local`/`FieldAccess` with **no promoted
       operand**; `maintain_pure_node` only re-derives `Binary`/`Unary`/`Cast`, so
       `engine.value(expr)` there resolves purely from `value_of`. Same for the
       re-seed's leaf operands. Conclusion: steps 4 and 5 **collapse into the
       born-as-operands keystone (item 3 below)** — there is no reorder/WIR-const-prop
       route that removes these `value_of` reads without regression.

       Started, gated (`WADO_BORN_OPERANDS`, default off). Brick 1: store_load_forward's
       **bare-scalar** constant forwarding no longer round-trips through `value_of`.
       `grow_bare_local_constants` is split into `bare_local_constants` (the scratch
       re-walk, returns the `(read, value)` constants — no `value_of` write) and the
       thin `grow_*` wrapper that still inserts them on the default path. Under the
       gate store_load_forward promotes the bare-scalar constants **directly** from the
       walk (no `value_of` read/write); field reads still query the graph. Validated:
       gate-off behaviorally identical (the two store_load_forward fixtures pass, same
       pre-existing benign `WADO_VERIFY_VG` `expr3/expr0` over-merge the default path
       already reports — the scoped-walk constant is more precise than a fresh build,
       which is why the default marks it `analysis_only`); gate-on runtime-correct
       (4/4). Remaining for store_load_forward: field reads (needs invariant scalar
       field promotion on the maintained post-scalarize graph).
5. [ ] **Make `value_of` transient, then delete it.** Reachable only **after** the
       keystone (item 3): once every flow-sensitive read is born as an `Operand::Value`
       (so store_load_forward / condition_implication / inline read operands, never
       `value_of`), the map becomes a transient build byproduct and the persisted
       `Body::value_graph.value_of` side-table is deleted (criterion 3). The reframe's
       earlier "freeze-local transient, born-at-lower is a later perf step" was
       **over-optimistic**: the experiment shows the freeze (last pass) cannot supply
       the earlier passes, so born-as-operands is a **deletion prerequisite**, not a
       later perf item.

   Born-as-operands is a **post-lower** promotion (not a `lower::translate` fold):
   neither `nir_value_graph::builder` nor `optimize::alias` imports `lower`, so there
   is no Rust cycle — the only ordering constraint is that alias sets need
   whole-package info (`first_param_types` / `call_immutability`), available after the
   package is lowered. The hard part is the **correctness obligation**: a leaf promoted
   early (`Local`/`FieldAccess` → `Operand::Value(reaching_def)`) goes stale when a
   later structural pass (licm hoist, inline) changes its reaching def, so the
   per-edit maintenance must keep **operand slots** valid (today it maintains
   `value_of` + the re-seed band-aids). Making the operand slots the source of truth
   and `value_of` a transient index is the irreducible keystone.

- [~] **1. Availability-aware extraction — the one new analysis.** Materialise a
  shared / flow-dependent value (`Select` at a merge, a `FieldAccess` at its heap
  version, a value a later pass may drop) into a `let _av = <value>` at a point
  **dominating all uses**, keeping its leaves live. The single-use
  enclosing-statement materialiser is insufficient; placement is a dominance +
  liveness obligation, not an emission-point choice. Two shortcuts were built and
  reverted (see "ideal end state"): scalar-only is sound but recovers nothing, and
  a pinned aggregate/reference field changes value-copy semantics and trapped
  `array_index_1` (~165 fixtures). Build it gated/off-by-default; prove it on the
  BCE + i128 + `array_index_1` cases (runtime + `WADO_VERIFY_VG`) before flipping.

      Done — the placement analysis. `materialise_point` (`optimize/extract.rs`)
      computes the nearest-common-dominator insertion point from each use's
      structured-control path (`block_path`), replacing the single-block MVP
      `shared_field_materialise_point`; cross-block uses (one per `if` arm) now
      materialise before the common `if` rather than staying skeleton. Sound by
      structured control flow (a block runs its statements in order, so the deepest
      common enclosing block, taken at the earliest leading statement, dominates
      every use) and the shared `ValueId` (one `heap_ver` ⇒ field invariant across
      the span). Unit-tested (same-block / sibling-branch / outer-and-nested /
      shared-branch). Reached only on the field-promotion path, so the default O2
      pipeline is unchanged; the `WADO_PROMOTE_FIELDS` corpus is **net-neutral**
      (3030/2 before and after — the 2 are pre-existing benign `wir_expect`
      operand-spelling diffs, `arr.repr`→`_av`, validated on the default path).

      Done — sound non-param scalar `FieldAccess` promotion (two gates,
      `e985b0535`). Broadening the receiver beyond a **param** to a non-param local
      needed two independent gates, both now built:

      - **Field-value-type gate.** Materialise only a **scalar**
        (`is_primitive_like`) field: a primitive copy is value-independent, so
        pinning + sharing it is sound; an aggregate / reference field (`List.repr`,
        a `ref Array` into a mutable backing, a nested struct) aliases storage the
        `heap_ver` does not pin — the `array_index_1` `null reference` trap.

      - **Receiver-availability gate.** A non-param receiver is admitted only when
        owned / non-`mut` / non-address-taken / non-reference **and** its def
        dominates the `materialise_point` (`def_dominates` / `stmt_block_path` /
        `receiver_available_at`). The value's receiver `Opaque(Local i)` can differ
        from a use's syntactic local (copy-prop / value identity), so `i`'s def is
        checked against the actual placement, not assumed; a **param** is
        entry-defined. Without this, broadening trapped `http_base64_decode` /
        `newtype_operator_trait` (`null reference`).

      Result under `WADO_PROMOTE_FIELDS`: **3029/3, zero traps / miscompiles** —
      every `array_*` / receiver trap and the 2 prior `wir_expect` baseline
      failures are gone; the 3 remaining are `niri_*` `wir_not_expect` DCE-misses
      (materialisation across a prunable branch — a cost-model refinement, not a
      bug). Reached only under the flag, so the default pipeline is unchanged.

      Remaining: (a') a cost model so the materialiser does not span a prunable
      branch (the 3 DCE-misses); (b) extend the materialiser to `Select` (at the
      merge) and shared `Binary` (once); (c) the runtime + `WADO_VERIFY_VG` proof on
      BCE / i128 / `array_index_1` before flipping field promotion on by default —
      which is what makes scalar `FieldAccess` reads operands and lets
      `store_load_forward` drop its `value(expr)`.
- [~] **2. Migrate the value passes off `value_of`** to operand / pool queries.
  Survey of the live consumers (default pipeline): `cse` is **skipped** under
  `promote_active` (subsumed by hash-consing) — its sites are dead; `const_fold`
  (own niri / `field_env`), `copy_prop` (skeleton `unwrap_copy_value`), and
  `select_lowering` do **not** call `engine.value`. The real consumers are
  `condition_implication`, `licm`, `store_load_forward`.

      Done — `condition_implication`'s panic-elimination. `failure_ge_operands` /
      `is_bitmask_bounded` / `GuardFact::implies_false` /
      `ConditionEliminator::implied_false` take the if-condition `Operand` and read
      `engine.operand_value` instead of `engine.value(ExprId)`; the four eliminators
      (`GuardEliminator` / `ConditionEliminator` / `BitmaskEliminator`, over both
      `StmtKind::If` and `ExprKind::If`) no longer gate the whole elimination on
      `condition.as_expr()` — a promoted (`Operand::Value`) condition is read and, if
      proven false, rewritten via `force_condition_false` (skeleton conditions keep
      the graph-maintaining `set_false` redirect, so the default path is
      byte-identical: full e2e 3032/0, no golden churn).

      Done — the guard *extractors*. `extract_dominating_guard` (loop guard
      `Not(<comparison>)`) and `extract_early_exit_guard` (`var + k >= bound`) now
      value-kind-match the condition's `operand_value` instead of `as_expr()` +
      `ExprKind::Binary`, so a promoted guard condition is recognised
      (`from_comparison` / `from_values` already operate on values). Full default
      e2e green (3032/0).

      Remaining: the two `value(binding)` licm re-seed sites (derived-let reader
      recovery — entangled with build-once maintenance, not a clean operand swap).
      `licm` (leaf invariance over mut loop locals) and `store_load_forward`
      (`FieldAccess` / `Local` reads) query genuinely flow-sensitive values with
      **no promoted operand**, so they cannot be re-routed to `operand_value`; they
      stay on `value_of` until loop-phi / availability extraction (items 1 / 3)
      freezes those reads into operands.
- [ ] **3. Fold `current_value` into `lower::translate`** so pure values are born
      as `Operand::Value`; then **delete `value_of` and `nir_value_graph::builder`**
      (criterion 3) and measure `optimize` CPU on package-gale (criterion 2).

Lower-priority robustness, independent of the above:

- [ ] One shared promoted-operand primitive (a `walk_operand` / `operand_local`
      helper) for the ~100 remaining `expect("skeleton operand")` sites, so they
      convert mechanically rather than per-pass. They hold as invariants today (the
      full e2e under `WADO_PROMOTE_EARLY` raises zero of them).
- [ ] Retire `grow_bare_local_constants` and the re-seed once born-as-operands
      lands — they are the band-aids it replaces — and route `copy_prop` through
      `method_writes_receiver` instead of the `None` it passes today.

### Dead ends (tried, measured, reverted — do not retry as-is)

- **Entry-point `FieldAccess` materialiser** (insert `let _av = recv.field` at
  function entry). Miscompiles: a local receiver is not valid at entry. The
  sound placement is the source point (`shared_field_materialise_point`,
  single-block before the earliest use), already in `freeze_pure_arith`.
- **Dropping the `recv_param` gate** on source-point `FieldAccess`
  materialisation (admit any reemittable receiver). Adds 7 miscompiles, all
  reference / match-ergonomics / variant-payload receivers — a `&T` receiver
  aliases a mutation `heap_ver` does not pin. The gate is load-bearing; the
  sound broadening is a receiver-stability predicate (owned value, not
  address-taken, non-`mut`), still to validate.
- **`value()` query-time leaf re-derivation** (a `Local` / `FieldAccess`
  fallback in `maintain_pure_node`). A design smell that over-merges; removed.
- **Retiring `store_load_forward` / migrating it off `value_of` without the
  keystone.** Full default e2e with the post-scalarize pass disabled: **3030/2**,
  regressing `field_forward_snapshot_after_mutation` and `store_to_load_forwarding`
  (flow-sensitive `mut`-struct field store→load forwards). WIR const-prop (step 1)
  does not recover them; the reads have no promoted operand and `maintain_pure_node`
  re-derives only `Binary`/`Unary`/`Cast`. Do not retry as a reorder or const-prop
  fix — it needs born-as-operands (item 3).
- **Persisted/maintained side-table** as the build-once route. Cannot retire
  `value_of` and re-introduces over-merge mechanisms; only operand promotion
  retires it. (Detailed in the pivot sections below.)
- **Value-based `condition_implication` guard extraction + `visit_expr` panic
  elimination** (the two secondary issues above). Correct and necessary, but
  alone insufficient: the comparison operands resolve to `None` in licm's
  maintained graph (the decisive root cause above), so no value matches even
  with value-based extraction. Re-land together with the operand-value fix.
- **Stable `Opaque(Local N)` identity for licm hoist locals** (`set_value` on
  each rewritten read in `replace_hoisted_in_expr`, opaque allocated at
  `licm_loop` step 4). Aimed to restore the dropped hoist-local value. Did not
  fix BCE: `set_value` no-ops when the graph is unbuilt at hoist time, and the
  comparison's _left_ operand (the induction variable) is also value-less in the
  maintained graph — restoring only the bound is not enough. The induction
  variable's lost value must be addressed too (see fix direction above).
- **LICM modified-vars: marking `&mut x.field` as field-modified** (route
  `Unary{MutRef, FieldAccess}` through `mark_assignment_target_as_modified`).
  Sound but unnecessary — the sort miscompile was the `cse_loop_body` scope bug,
  not the field hoist (hoisting `self.repr` and redirecting `&mut self.repr`
  writes to the alias is correct, since the field is never reassigned). The
  change also suppressed the legitimate `_licm_repr_` hoist that
  `newtype_basic`'s `wir_expect:O2` pins, regressing it. Dropped.

## Goal

Make the NIR `optimize` phase **2× faster**. On the package-gale baseline (this
host, dev build) the phase is ~15s; the target is ~7.5s, comfortably under 10s.
Absolute time varies by host; the 2× ratio does not.

This is not a cache-tuning goal. The current optimizer already parks and carries
a _derived_ ValueGraph between passes (`vg_cache` / `carry_vg_cache`); that path
is measured and capped — it shaves a few percent and leaves the graph rebuilt
2.67× per function. It is removed by this WEP, not extended.

## Acceptance criteria

The work is done only when all three hold. None is satisfiable by a band-aid;
each is a static or measured fact, not a "byte-identical and X% faster" proxy.

- [x] **One build per function.** `WADO_MEASURE_VG` reports `rebuilds = 0` (only
      `first_builds`) under `WADO_PROMOTE_EARLY`. Baseline was `builds=4474`,
      `first_builds=1678`, `rebuilds=2796` — 2.67 builds per function. Achieved by
      persisting the per-function graph on `Body` across passes (the freeze builds
      it once; the value passes reuse it) and counting _actual_ `builder::build`
      calls rather than pass entries. zlib -O2 under promotion: `builds=139,
      first_builds=139, rebuilds=0`. See the P3 milestone below for the mechanism
      (persist gates + honest measurement + config-aware verify oracle).
- [ ] **`optimize` CPU halved** on package-gale (~15s → ~7.5s), measured by the
      sampling profile and wall time.
- [~] **The cache is deleted.** Cache half **done**: `vg_cache`,
  `carry_vg_cache`, `CachedAnalysis`, and `run_gated_cached` are at zero
  references (the graph lives on `Body::values`). Side-table half **pending**:
  the `value_of` `ExprId → ValueId` map still exists; retiring it needs Phase
  B.3 (a scheduler that extracts non-constant values from the graph), so the
  passes read `Operand::Value` end-to-end and no longer consult `value_of`.

The three are mutually reinforcing: deleting the cache (3) requires removing the
re-derivation it caches, which is the build-once change (1), which is what
produces the speedup (2). Any one unmet means the goal is unmet.

Merge gate (overrides incidental proxies): the full suite is green — `mise run
test` (every crate, all e2e fixtures at O0/O2) **and** `mise run test-wado`, with
no pre-existing failure left standing. The operand-promotion migration is part of
this: a promoted `Operand::Value` (e.g. a constant struct receiver of a
`FieldAccess`) must never hit an `as_expr().expect("skeleton operand")` in any
pass the suite exercises. `gale_cli` (kiln-generated code, which exercises
promoted shapes the mainline fixtures do not) is part of the gate. The remaining
`expect("skeleton operand")` sites in passes the suite does not yet drive with a
promoted operand are migrated test-first as Phase B.2 enables `WADO_PROMOTE_EARLY`
(which promotes more positions and so exercises them).

## Context

A sampling profile (samply, 1 kHz, dev/debug `wado`) of
`wado compile -O2 package-gale/src/main.wado` pins the cost. Percentages are
inclusive shares of total CPU — machine-independent ratios; absolute time varies
by host. `optimize` is **64.83%** of compile CPU. Inside it the dominant cost is
per-function analysis rebuilt per pass and discarded:

- per-function `ValueGraph` build (`builder::build`: the `walk_block` /
  `walk_expr` flow walk, the joins, the hash-cons) — **20.76%**
- allocation churn — `__rust_alloc` **13.94%** + `__rust_dealloc` **13.35%** —
  the maps and vectors each per-pass rebuild creates and drops
- per-session engine setup (`Engine::new`: parent maps, use index, post-order
  seed) — **8.48%**
- `IndexMap::swap_remove_full` — **7.73%** — worklist / use-index churn

By pass wall-time, `licm` (~3.7s), `cse` (~2.5s), and `const_fold` (~0.8s) — the
three value-graph passes — are together **~46% of the phase**.

This cost exists for one structural reason: the ValueGraph is a _derived_
analysis of a _mutable_ SkelTree, and so is the engine's analysis (parent map,
use index, post-order). Every SkelTree edit can stale them, so every pass that
needs pure-value identity re-derives the graph, and every pass re-derives the
engine. The dirty-set gate and the revision-keyed `vg_cache` amortise this only
for functions a pass did not change; a function a pass actually rewrites pays a
fresh re-derivation in the next pass that visits it. The result is 2.67 graph
builds per function and a comparable engine churn — and the ~27% allocation cost
that rebuilding those structures generates.

## Why source-of-truth, not incremental rebuild or a richer cache

Two cheaper-looking patches were considered and rejected.

Incremental rebuild — re-derive only the changed region of the side-table — was
prototyped, verified equivalent under a `WADO_VERIFY_INCREMENTAL` harness, and
reverted: it fired on ~0.15% of builds on the large workload and 0% on the small
ones, and it is the wrong shape — it makes the _re-derivation of a derived
side-table_ cheaper, when the side-table only needs re-deriving because it is
derived.

A richer cache — park the graph and reuse it whenever the body is unchanged —
caps low for the same reason: it still rebuilds whenever a pass changes the
function, which is exactly the common case, and it cannot reach 2×. It is the
current `vg_cache` / `carry_vg_cache` path, and this WEP deletes it.

So this WEP removes the reason re-derivation exists: the graph stops being a
shadow of the SkelTree and becomes where pure values _live_. Flow is resolved
into the graph once, at build, and frozen there; a rewrite is a union of two
e-classes, which every user sees through `find()` without any re-walk. There is
no derived form to bring current, and nothing to cache.

## Decision

Promote the ValueGraph to the pure-value IR and drive it aegraph-style; apply
the same build-once discipline to the engine's per-function analysis.

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
- The engine's parent map, use index, and post-order are built once per function
  and maintained through the edit API, not rebuilt per pass. `Engine::new`'s
  per-pass cost retires the same way the graph's does.
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
redesign runs that build once per function — at lower — and then maintains it.
Two additions make it an IR rather than a side-table:

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
at the splice point, not a re-derivation of existing flow).

This pointwise maintenance through structural edits is the load-bearing claim of
the whole design and its chief risk: `inline` and `sroa` must grow the graph at
the splice/decomposition point without re-walking the untouched remainder. The
roadmap measures this before the wide operand-promotion change is committed (see
Roadmap, de-risk step).

### Extraction

Before WIR build, one pass walks the SkelTree and lowers each pure `ValueId`
operand to a concrete skeleton/WIR form, choosing per multi-use value whether to
re-compute it at each use or materialise it once into a hoisted temp. This is the
one genuinely new analysis and the main code-quality risk: the cost model must
not emit worse code than today's CSE / hoisting heuristics. The migration
de-risks it by reproducing the current materialisation first (extract each value
at the sites and shapes the old passes produced), then improving the cost model
behind benchmarks.

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

- The per-pass re-derivation (build 20.76%, engine setup 8.48%, and the bulk of
  the ~27% allocation churn they generate) collapses to one build per function,
  maintained in place. The three value-graph passes (~46% of the phase by
  wall-time) stop carrying their own build.
- A cluster of dataflow passes (CSE, copy-prop, the bulk of const-fold,
  store-load-forward, loop-invariant hoisting) stops being passes and becomes
  graph structure, removing both their walks and their bespoke analysis caches.
- The cache machinery and the `value_of` side-table are deleted outright.

### Risks

- Pointwise maintenance through structural edits (`inline`, `sroa`) is the
  central correctness-and-cost surface: graph growth at the splice point must not
  degrade into a re-walk, or acceptance criterion 1 (`rebuilds = 0`) and the 2×
  both slip. De-risked: the measured maintenance work is 13.5% of the rebuild it
  replaces (see Roadmap), so the budget is wide — the risk is correctness of the
  splice-point growth, not its cost.
- Operand-promotion breadth: a wide change across arena, lowering, WIR build, the
  unparser, and pure-`ExprKind`-matching passes. Mechanical but large.
- Extraction cost model: a weak model regresses code size or runtime. Mitigated
  by reproducing current materialisation first, then tuning.
- Congruence maintenance: deferred e-graph rebuild after unions has its own cost;
  it must stay below the re-derivation it replaces. Measured per step.

### Trade-offs accepted

- The graph gains a union-find and a congruence rebuild; the optimizer gains an
  extractor. In exchange every pure-value pass's per-pass rebuild, the engine's
  per-pass setup, the cache machinery, and the `value_of` side-table are deleted.
- Arena compaction (dead skeleton nodes from in-place rewrites) becomes more
  worthwhile once bodies are walked fewer times; tracked as the existing
  follow-up.

## Current state and what it leaves to do

The primitives the build-once needs already exist, landed earlier on this branch:

- Union-find + deferred congruence rebuild on the `ValuePool` — equivalence by
  `find()`; constant kinds win the representative so a class containing a literal
  resolves to it. Exposed on the engine (`value_find` / `value_union` /
  `rebuild_value_congruence`).
- The extraction keystone — `extract::materialize_literal`, the graph→skeleton
  primitive that resolves through the representative. `store_load_forward` routes
  through it.

What is _not_ done, and is the actual subject of this WEP: build-once. The graph
is still derived and rebuilt 2.67× per function. The interim wins on this branch
— sharing one parked build across `cse`→`licm` and re-tagging it through
`const_fold` (`carry_vg_cache`) — are the cache path this WEP deletes, not
progress toward the goal. They are removed when operand promotion lands.

## Roadmap

Each step must not regress output (code size or runtime) on the full fixture +
E2E suite, on `wir_expect` / `wir_not_expect`, and on the benchmark set. Progress
is tracked against the three acceptance criteria, not against incidental speedups.

- [x] **De-risk maintenance cost — green light.** `WADO_MEASURE_VG` now counts,
      across the whole optimize, the genuinely-new skeleton nodes the edit API
      creates and the in-place structural edits it makes — the exact work
      pointwise maintenance does, with no re-walk of the untouched remainder. On
      package-gale: **33,672 new nodes + 94,808 in-place edits = 128,480 edits =
      13.5% of the 948,401 rebuild node-walks.** The current per-pass rebuild
      does ~7.4× more work than maintenance would, and under operand promotion
      those rebuilds leave the optimize phase entirely (the graph is built once
      at lower). This refutes the earlier "71.8% irreducible" figure, which was
      the incremental-_rebuild_ model (re-walk from the first divergent statement
      to the end) — pointwise maintenance never re-walks, so its cost tracks the
      edit count, not the disturbed suffix. `rebuilds = 0` is reachable far below
      the rebuild it replaces; the wide change is justified.
- [x] **Phase A — representation.** The arena's operand slots carry
      `Operand { Value(ValueId), Expr(ExprId) }`; `lower` emits `Operand`; every
      consumer (engine, builder, passes, WIR build, niri, unparser) handles it;
      the extraction seams (`translate_operand` / `splice_operand` /
      `walk_operand`) are in place. All operands are `Operand::Expr`, so behavior
      is unchanged — the full suite (769 lib + 2958 e2e) is green. This is the
      safe scaffold; it meets no acceptance criterion yet.
- [x] **Phase B.1 — extraction proven (constants), end-to-end.** `ValuePool`
      records a per-value source type (`set_type` / `type_of`) and allocates each
      promoted literal un-shared (`alloc_unshared`) so a type-erased `7: i32` and
      `7: i64` keep distinct widths. `extract_value` materialises constant value
      kinds back to WIR; the builder seeds its pool from `Body::values` so a
      promoted id resolves. The whole WIR-build dispatch now takes `Operand`
      (`translate_operand` / `operand_type_id`; index / cast / match / switch /
      tuple+array literals / variant-construct payload / indirect-call args /
      SIMD lane / canonical-ABI). `promote_literals` is wired as the last optimize
      step (promote-late). Result: package-gale -O2 is **byte-identical**
      (740267 bytes) and the **full e2e suite (2982 fixtures) is green** — the
      WEP's flagged "main regression risk" (extraction) is de-risked on a real
      workload. This is necessary groundwork; promote-late runs after every pass,
      so it moves no acceptance criterion yet (`rebuilds` unchanged at 2796).
- [~] **Phase B.2 — promote early + migrate the literal-matching passes.**
  Promote-early is now the production default (`promote_early_enabled() ->
      true`), and the literal-matching passes the suite exercises read the
  promoted constant from the pool — the full suite is green with it. The
  remaining work is robustness: ~100 `as_expr().expect("skeleton operand")`
  sites in passes that no current test reaches with a promoted operand. The
  kiln-path subset (`ref_elim` / `licm` / `field_scalarize`) is migrated; the
  rest hold as documented invariants. Measured: the full `-O2` e2e under
  `WADO_PROMOTE_EARLY=1` (the materialise sub-feature, which promotes the most
  positions) raises **zero** skeleton-operand panics, so the remaining sites
  are not test-drivable today — they are migrated opportunistically when a
  program (e.g. a future kiln grammar) exercises them.
- [ ] **Phase B.3 — scheduling extraction for non-constant values.** Materialising
      a `Binary(Opaque, 1)` needs the skeleton computation behind the `Opaque`
      operand (a `Local` read, a `Call` result). The graph alone cannot re-emit
      it, so the effectful sub-results stay scheduled in the skeleton and the
      extractor reads them. Constants extract without this; non-constants need the
      scheduler. Required to fully retire `value_of`.
- [x] **Cache deleted (criterion 3, cache half) — graph relocated to the body.**
      `build` now writes into `Body::values` (the one persistent pool; ids stay
      stable across builds), and `ValueGraphBuild` drops its own pool. The
      revision-keyed cross-pass cache and all its machinery are gone:
      `vg_cache`, `carry_vg_cache`, `CachedAnalysis`, `run_gated_cached`,
      `Engine::with_analysis` / `into_analysis` reach zero references. cse / licm /
      store_load_forward build their session fresh via `run_gated`. Verified e2e
      (2982 fixtures green); package-gale byte-identical. The cache was masking
      rebuilds: `WADO_MEASURE_VG` rebuilds 2796 → 5988 (cse and licm no longer
      reuse one build across const_fold). That increase is intentional and honest.
      `value_of` retirement (the side-table half of criterion 3) still needs full
      promotion (Phase B.2/B.3).

      Architectural finding that fixes the next step: cross-session graph reuse
      (cse → licm sharing one build) requires storing the build-config (alias
      sets, param seeding) *with* the graph — structurally the just-deleted
      `CachedAnalysis`. So a body-owned, config-keyed reuse is the cache by
      another name and is rejected. The only no-cache route to `rebuilds = 0` is
      maintenance: build once and keep current through every edit, so there is no
      config to check and nothing to cache. The two maintenance items below are
      therefore the whole of criteria 1 and 2.
- [ ] **Maintain the graph through structural passes.** `inline` / `sroa` /
      `dae` / `drve` grow or union the live graph through their edits; no pass
      triggers a rebuild. Drives `rebuilds` toward 0.

      Finding (a `WADO_VERIFY_VG` harness — `partitions_agree` against a fresh
      build — earned this): a build-once attempt that let `licm` reuse `cse`'s
      graph across `const_fold` was prototyped and **reverted as unsound**.
      `const_fold` rewrites flow-sensitive local values (constant propagation), so
      it is **not graph-preserving** — `cse`'s graph is stale for `licm`. It was
      byte-identical on the full suite (the staleness happened to be conservative),
      but not rigorously sound, and a compiler ships only proven-sound passes. The
      retired `carry_vg_cache` relied on the same false assumption. So sound
      build-once requires **precise per-edit maintenance** — every value-changing
      pass (`const_fold` included) updates the graph as it edits, not just the
      structural ones — verified by the harness.

      Landed and validated (the maintenance primitive + the right oracle):
      `Engine::replace_expr_kind` now calls `maintain_value_after_edit`, which
      re-derives the edited node's pure value (`maintain_pure_value`) and
      propagates up its ancestor chain, dropping any entry it cannot re-derive
      (flow-sensitive `Local` / `FieldAccess`) rather than leaving it stale. The
      `WADO_VERIFY_VG` harness is wired live: on every graph query it rebuilds a
      fresh graph and checks the maintained one against it. The check is
      `partition_refines`, not strict `partitions_agree`: maintenance is sound by
      **refinement** (it may merge a pair only if a fresh build also merges it),
      not equality — dropping a flow value it cannot re-derive makes the graph
      *coarser*, a missed optimization, never a wrong merge. Empirically, across
      the full e2e fixture corpus at `-O2`, the maintained graph **never
      over-merges** (`partition_refines` clean; the strict `partitions_agree`
      flagged only the expected conservative-coarsening direction). The wiring is
      **byte-identical** on package-gale and **within timing noise** (~24s
      total), so it is sound, free groundwork — but it does not yet move
      `rebuilds` (still 5988): it only keeps the existing per-session graph
      current through `cse`'s edits, which were already value-preserving.

      Consequence that pins the next step: in the *side-table* model a coarser
      maintained graph is sound to consume but, reused across sessions, would
      **regress code quality** (the missed merges `cse` / `licm` would have
      found). Non-regressing reuse therefore needs *precise* flow-value
      preservation, and re-deriving flow values on each edit is the rejected
      incremental rebuild. The only model in which a pure-edit preserves flow
      values for free is operand promotion: flow is frozen into `ValueId`s in the
      operand slots at build, and a pure rewrite is a union that leaves them
      intact. So `rebuilds = 0` runs through Phase B.2/B.3, not through more
      side-table maintenance. `partitions_agree` (strict) stays the oracle for
      that precise design; `partition_refines` is the oracle for the conservative
      maintenance that exists today.

      Second concrete blocker, found while scaffolding env-gated reuse and then
      reverted: even setting code quality aside, side-table reuse does not reduce
      `rebuilds`. The two builds per function per iteration are `cse` and `licm`;
      `cse` is graph-preserving so `licm` could reuse its maintained graph — but
      `licm::hoist_invariant_arith` calls `invalidate_value_graph()` mid-pass to
      force a rebuild after each hoist round, because hoisting reorders the loop
      and staled its `loop_entry_values` snapshot. `loop_entry_values` is loop
      *recurrence* state the pure-operand maintenance does not update, and
      `partition_refines` does not cover it (it checks only the `value_of`
      partition), so reusing across a hoist would be both rebuild-defeating and
      unguarded. With `licm` still rebuilding, reuse saves nothing. The fix is
      the same operand-promotion model: loop recurrence frozen as a `LoopPhi`
      `ValueId` that a hoist references rather than re-derives.
- [ ] **Maintain the engine analysis.** Parent map, use index, and post-order are
      built once and updated through the edit API; `Engine::new`'s per-pass cost
      retires.
- [x] **Delete the cache.** Done above — all cache symbols at zero references.
- [ ] **Subsume the dataflow passes into graph structure.** CSE, copy-prop, and
      store-load-forward become hash-cons / `find()` results; delete the passes.
      Then flow-sensitive `const_fold` and `condition_implication` as graph
      rewrites (const_fold keeps niri for pure CTFE calls). Then loop-invariant
      hoisting as the extractor's placement decision; retire `licm`'s
      pure-arithmetic hoisting.
- [ ] **Cost-based extraction.** Replace straight extraction with a share-vs-
      duplicate cost model; tune against benchmarks.
- [ ] **Retire the intra-procedural iteration count.** The graph and worklist
      self-converge; keep an outer round count only for the interprocedural cycle.

### Decisive pivot: criterion 3 forces operand promotion; side-table persistence is a dead end

A build-once-via-persisted-side-table sub-path was started and instrumented this
session: the value graph (`ValueGraphBuild`) moved onto `Body` (commit
`685260a16`, behavior-neutral) and maintenance was hardened to drop a reassigned
local's downstream readers (commit `36fac4552`), which fixed the verified
`count_prime` over-merge (`Local m` ≡ a `Shl`). But running `WADO_VERIFY_VG`
across the benchmark corpus surfaced a **second, distinct** over-merge on zlib:

```
expr1713 = Local{index:333 "__v1"}  ≡  expr3564 = FieldAccess{.used}
maintained: both -> ValueId(935692)   fresh: 1840139 vs 1813397
```

A local that forwarded from `obj.used` (`let __v1 = obj.used`) and a later
`obj.used` read share a stale value after the heap changed. Maintaining this
_precisely_ means re-deriving per-`(receiver, field)` `HeapVersion`s on every
heap-affecting edit — i.e. re-implementing the builder's heap-version walk
incrementally. Each over-merge mechanism (reassigned-local readers, then
FieldAccess/heap, then `Select`/`LoopPhi` next) needs its own precise
maintenance; coarsening them instead regresses code quality, which fails 完勝
(no-compromise). So precise side-table maintenance is as hard as the builder and
still does not retire `value_of`.

Decisive point: **acceptance criterion 3 requires the `value_of` side-table to
retire**, which a _persisted side-table_ can never do by construction. Only
operand promotion — pure values frozen into `Operand::Value` slots, with a
`FieldAccess` frozen at its heap version — retires `value_of`, and it
**structurally eliminates both over-merge mechanisms**: a frozen operand value
cannot go stale (old reads keep their old `ValueId`; new reads get new ones; a
rewrite is a `union`, never a re-derivation). So the unified path to all three
criteria is to **complete operand promotion**, not to persist and maintain the
side-table. `36fac4552` (the over-merge fix) stands as a correctness fix on its
own; `685260a16` (graph on `Body`) is harmless and the pool already lives there.
The remaining work pivots to the promotion migration below.

### Extraction proven; the rebuild win is all-or-nothing

A late, in-pipeline freeze pass (`optimize/extract.rs::freeze_pure_arith`,
run after every other pass) was built to exercise the extractor end-to-end on
real programs. It redirects a pure-arith node (`Binary` / pure `Unary`) to its
`Operand::Value` when the value is re-emittable, and WIR build materialises it
via `extract_value`. It is full-e2e green (7397/0). What it established:

- The extractor works on real arithmetic: `extract_value` lowers
  `Binary` / `Unary` recursively (shared with the skeleton path via
  `emit_binary_wir` / `emit_unary_wir`) and re-emits an `Opaque` leaf as
  `local.get idx` (`OpaqueSource::Local`) or a scheduled skeleton expr
  (`OpaqueSource::Expr`).
- Soundness constraints the extractor needs, each now enforced and tested:
  - Only single-assignment locals freeze (`mut` / loop locals reject — a
    `local.get` at the use site must read the opaque's version).
  - `ValueKind` is type-erased and hash-consed, so a value shared between two
    differently-typed uses (`a+b` as `i32` and `i64`; `0.0` as `f32`/`f64`)
    can carry only one width — freeze skips on a width conflict; `Cast` is
    excluded (operand source type unrecoverable from the value tree).
  - The WEP's "main code-quality risk" is real: freezing a multi-use value
    duplicates its computation (`t*t` extracted twice loses `local.tee`). The
    freeze skips values whose extraction duplicates `Binary` / `Unary` work —
    a conservative stand-in for the share-vs-duplicate cost model.
  - The verify oracle compares only _live_ exprs (an orphaned node a freeze
    redirected away is never emitted and cannot miscompile).

The decisive finding: this freeze does **not** move `rebuilds` (measured 119,
unchanged). `rebuilds` is **all-or-nothing** — the per-pass `builder::build`
walks the body to derive every value not already in an operand slot, so it
runs in full until _every_ value (locals, calls, `FieldAccess`, `Select`,
`LoopPhi`) is frozen. Partial freezing, late or early, leaves the build intact.
A persisted derived `value_of` is not a path either: cross-pass reuse needs
precise per-edit maintenance, which for flow values is exactly operand
promotion (the side-table-reuse alternative coarsens — a quality regression —
and `licm::hoist_invariant_arith` still `invalidate_value_graph()`s because
`loop_entry_values` recurrence is unmaintained). So criteria 1/2 sit behind one
atomic change with no intermediate measurable win:

1. Relocate the builder's `current_value` flow-walk into `lower::translate`;
   build the graph once while lowering.
2. Freeze every read into its operand slot: single-assignment `Local` →
   `Opaque(Local)`; call result → scheduled skeleton `let` + `Opaque(Expr)`;
   merges → `Select`; loop recurrence → `LoopPhi`; pure `FieldAccess` at a
   heap version. Extend `extract_value` per kind (the arith/opaque half is
   proven above).
3. Migrate every value- and skeleton-walking pass to consume `Operand` /
   `Body::values`; structural passes (`inline` / `sroa`) grow the graph at
   splice points. Retire `value_of` and per-pass `builder::build`.

Done this round (prerequisites): scalars / `Null` / `String` / `Unit` promoted;
`ExprKind::Dead` tombstone split; the extraction machinery + `OpaqueSource` +
`value_fully_reemittable_locally` + the freeze, all full-e2e green.

### Keystone execution plan (multi-session, behind `WADO_PROMOTE_EARLY`)

The rewrite is developed behind the `WADO_PROMOTE_EARLY` flag (commit `4bb79d30d`):
flag-on freezes pure values _before_ the value passes and skips the late freeze;
default-off keeps the committed late-freeze behavior, so the branch stays green
and continuously validated while the keystone is built up. Flip the default when
flag-on is fully sound (e2e green) and measured (`rebuilds = 0`). Phases:

- [x] **P1 — flag-on correctness complete** (the pass migration). Flag-on e2e is
      **2956 / 2960** (347 → 10 → 5 → 4), and **every real miscompile is fixed**.
      The class was a single liveness gap: a still-read local invisible to the
      precise `locals_read_via_promotion` walk, which let `elide_local` /
      `copy_prop` drop it. Both now use the over-conservative pool-wide
      `opaque_local_sources` under early promotion (`7b5a8dd59`, `9a3f43482`) —
      sound (keeps more locals, never drops a read one); flag-off keeps the precise
      walk, so the default is byte-identical. This closed the 3 `Select` miscompiles
      and `assert_fail_call_arg` (a promoted inlined call-arg binding whose only
      read was a power-assert's promoted temp). The remaining **4 are `wir_expect`
      WIR-shape tests** (`array_bounds_elim_le_guard_wir`,
      `array_bounds_elim_offset_chain`, `opt_licm_invariant_arith`,
      `tir_optimize_bool_identity`) that _should_ change under promotion — fixture
      updates done at the P4 default-flip, not bugs. Still measurement-neutral (the
      builder still runs); promotion now threads correctly through the whole pass
      pipeline, which was P1's goal. Recoverable later: the precise-walk gap itself
      (over-conservation costs a few elisions/copies) — a code-quality item, not
      correctness.
- [ ] **P2 — availability-aware extraction.** Extend promotion past the
      re-emittable subset to _every_ pure value, materialising a shared or
      flow-dependent value (`Select`, shared `Binary`, `FieldAccess`) into a `let`
      at an available point and pointing uses at it (`local.get`), instead of
      re-emitting an `if`/recompute. This is the one genuinely new analysis and the
      main code-quality risk; it is also what makes full promotion sound. Gates
      `Select` promotion on it.

      Done so far: the materialisation primitive for the base case — a shared value
      whose leaves are all non-`mut` params, materialised at function entry
      (commit `b71ecb8ec`, flag-on), plus `promoted_value_use_counts` for the
      share decision.

      Concrete implementation map for the rest (scouted this session):
      - **General source-point materialisation.** Entry insertion only covers
        param leaves. For a value over a non-param local (or a `FieldAccess` /
        `Select`), insert the `let` at a point that **dominates all uses** and
        where every input is available — its source/definition point, not entry.
        Needs a dominance check (MVP: all uses in one block, insert before the
        first; general: nearest common dominator). Done: `materialise_point`
        (`optimize/extract.rs`) computes the nearest-common-dominator placement
        from each use's structured-control path (`block_path`: the outermost-first
        `(block, stmt_index)` chain), generalising the former single-block
        `shared_field_materialise_point`. In structured control flow a block runs
        its statements in order, so the deepest enclosing block common to every
        use, taken at the earliest leading statement, dominates them all; the
        shared `ValueId` (one `heap_ver`) means the field is invariant across the
        span. Unit-tested for same-block, sibling-branch (placed before the `if`),
        outer-and-nested, and shared-branch cases. The `recv_param` gate still
        bounds leaf availability (a param is defined at every point), so the
        broadened cross-block firing is sound; the receiver-stability predicate
        below is the next step to admit non-param leaves (which need the placement
        after the leaf's def).
      - **`FieldAccess` extraction** (`extract_value` has no arm yet). Soundness:
        the value's `heap_ver` already guarantees the field is unchanged across all
        uses that share the value (same `(receiver, field, heap_ver)` key), so
        sharing is sound — but re-emitting the load *inline* is not, because a pass
        can move the operand to a different heap point; it must be pinned in a
        materialised `let` at the source point. Mechanics: `ValueKind::FieldAccess`
        deliberately carries only `field_index` (the receiver `ValueId` pins the
        type), so extraction must derive the field **name** from the receiver
        value's type (`type_of(receiver)` → struct def → `fields[field_index].name`,
        `tir.rs` `TirField`) and emit the `StructGet` via `struct_field_wir_type`,
        reusing the `translate_expr_inner` `FieldAccess` path. The single builder
        call site (`builder.rs:1007`) already has the name in hand if threading it
        proves simpler than deriving.
      - **`Select` materialisation** at the merge point closes the 3 fixtures the
        over-conservative `elide_local` fix currently carries (it keeps them
        correct but is a stand-in).

      Done so far for FieldAccess: `extract_value` handles it (`ad2e1f7ba`), and
      the extraction half is complete for **every** value kind. Scouting promotion
      (briefly enabled flag-on) showed it opens a **pass-migration surface** like
      P1's: the 122 `as_expr().expect("skeleton operand")` sites across 20 optimize
      passes. Most are structural invariants (a `Ref`/`Deref` inner is always a
      skeleton place — never promoted), but FieldAccess reaches **value-read /
      receiver** positions, so a subset needs per-site judgment: guard with
      `as_expr()?` where a promoted value can now appear (`string_push:141`,
      `copy_prop:175` done — behavior-neutral), and semantic handling where the
      site reasons about a place vs value (`copy_prop:372`, a receiver-mutation
      check). `is_ref_place` (extract.rs) already excludes `&mut obj.field` places
      from promotion at the source, shrinking the surface. The grind is bounded
      (a handful of receiver-position sites, found by re-enabling promotion and
      fixing each benchmark/e2e crash) and is the FieldAccess analogue of P1.

      Strategy refinement (do this first): the **materialiser largely avoids the
      grind**. If a `FieldAccess` is promoted by materialising `let _av =
      obj.field` and rewriting its uses to **skeleton `Local _av` reads** (not
      `Operand::Value(Opaque)`), passes at receiver/operand positions see an
      ordinary skeleton local — already handled — and the only promoted operand is
      the `let`-value slot. Two such lets over the same field share a value
      (hash-cons), so `cse` / store-load-forward stay subsumed. So implement the
      source-point materialiser *before* widening inline promotion: it makes
      `FieldAccess` sound (load pinned at its source heap version) **and** keeps the
      receiver-position migration surface from opening. The same shape covers
      `Select` (materialise at the merge) and shared `Binary` (materialise once).

      Receiver-broadening probe (run, measured, reverted). The source-point
      materialiser currently gates `FieldAccess` promotion on a **parameter**
      receiver. The obvious next step — drop that gate, keeping only the
      `value_fully_reemittable_locally(recv)` check (single-assignment leaf
      locals) and the single-block placement — was implemented and measured under
      `WADO_PROMOTE_FIELDS`. Baseline (param-only) is **38 failing** e2e fixtures
      (the flag is itself WIP: `inspect_*` / `httpbin_*` hit the unmigrated
      `skeleton operand` sites; `closure_3` already traps). Dropping the gate adds
      **7 new miscompiles** — `match_ergonomics`, `nested_variant_match_ref_test`,
      the three `serde_*` reference-receiver tests, `newtype_string_coercion` (a
      `wir_build` panic), `template_string_precision`. Every new failure is a
      **reference / match-ergonomics-bound / variant-payload receiver**. So the
      `recv_param` gate is **load-bearing, not merely conservative**: a local
      `FieldAccess` receiver can be a `&T` (or an ergonomics ref-binding) whose
      pointee aliases a mutation the value graph's `heap_ver` does not pin to that
      `let`, so the shared-version assumption breaks and a stale field is read.
      A param is by-value (deeply copied at the call boundary), so it has no such
      alias. The sound broadening is therefore not "drop the gate" but a
      **receiver-stability predicate**: admit a non-param local receiver only when
      its root is an **owned value** (non-reference type), **not address-taken**
      (`address_taken_locals`), and **non-`mut`** — i.e. no `&`/`&mut` alias and no
      reassignment can change the pointee across the span. That predicate, plus the
      existing single-block placement, is the next concrete P2 step; it must be
      re-validated against the full `WADO_PROMOTE_FIELDS` corpus, since
      `newtype_string_coercion` shows at least one value-type receiver also needs
      an extraction fix, not just the alias gate.
- [ ] **P3 — migrate the value passes to operands.** `cse` / `licm` /
      `const_fold` / `condition_implication` / `store_load_forward` read
      `engine.operand_value(operand)` instead of `engine.value(expr)`; structural
      passes grow the graph at splice points. Once no pass calls `engine.value`,
      `builder::build` stops running inside `optimize`.

      Started — cse subsumption (commit `ef5646034`): under operand promotion,
      pure-value CSE is just hash-consing (identical values already share a
      `ValueId`), so the `cse` pass (and its `store_load_forward` session) is
      **skipped** when promotion is active. `WADO_MEASURE_VG` on zlib -O2:
      **`rebuilds` 861 → 283** (`builds` 1077 → 499) under `WADO_PROMOTE_EARLY` —
      the first time criterion 1 moves; flag-off unchanged (861, cse runs). The
      remaining 283 are **all `licm`** (`by pass licm: rebuilds=283`): licm
      re-enters each fixpoint iteration with a fresh `Engine` and rebuilds.
      cse-skip is correctness-safe (an optimization): the 29 e2e diffs under
      EARLY-only are all `wir_expect` WIR-pattern / optimization-loss
      (`array_bounds_elim_*_wir`, `opt_*`, `tir_*`, `tmpl_*`) — no miscompiles.
      The EARLY-only loss (FieldAccess CSE / store-load-forward) is recovered once
      FieldAccess promotes **in the loop** (its `heap_ver` values subsume
      store-load-forward), which also makes the cse-skip regression-free.

      Remaining for `rebuilds = 0`: eliminate licm's per-iteration rebuilds. Two
      routes — (a) subsume licm's loop-invariant motion into the extractor's
      placement (hoist an invariant value to the pre-header once), or (b) the
      build-once persist+maintain: keep `Body::value_graph` across passes/iterations
      (drop the `Engine::new` reset + config-drops + licm `invalidate`), maintained
      through every edit, `WADO_VERIFY_VG`-checked. With cse skipped, the only
      maintainer left in the value-pass set is licm + the structural passes, a
      smaller surface than before.

      Route (b) **landed (commit `8bcdfffb2`): `rebuilds = 0`.** Gating the five
      drops (`Engine::new` reset, the three `set_*` config-drops, `invalidate`) on
      `promote_active` persists the graph across passes; under promotion the freeze
      builds each function's graph once and the value passes reuse it. zlib -O2:
      `builds=139, first_builds=139, rebuilds=0`.

      Two things made it land where the earlier probe stalled:

      - **Honest measurement.** `record_build` was at the pass-entry sites, so it
        counted pass *entries*, not builds — a persisted graph reused at a later
        pass still counted. It now fires inside `Engine::ensure_value_graph` exactly
        when `builder::build` runs, attributed to the active pass via a `BuildScope`
        guard (freeze / cse / licm). Reuse records nothing; only real builds count.
        This also makes the meter measure what the criterion means (one build per
        function across the phase), not a per-pass proxy.
      - **Config-aware verify oracle (the over-merge was a false positive).** The
        probed `expr5 ≡ expr17` over-merge was **not** a stale maintained merge: a
        persisted graph is read by sessions that configure their engine differently
        (e.g. `select_lowering` does not seed params), and `verify_maintained_graph`
        was rebuilding the comparison graph with the *consuming* session's config —
        an unseeded fresh build splits two param reads the seeded build legitimately
        merges, read as a spurious over-merge. `ValueGraphBuild` now retains the
        `BuildConfig` (param seeding + alias sets) it was built with, and the oracle
        rebuilds with *that* config. count_prime / mandelbrot / sieve / zlib / fts
        are `WADO_VERIFY_VG`-clean under promotion; the maintained graph is sound.
        (The config is for the verify oracle only — never a reuse key, so it is not
        the rejected config-keyed cache.)

      Flag-off is byte-identical (the gates reduce to the original drops under
      `!promote_active()`); lib 770/770, flag-off e2e 2960/0.
- [ ] **P4 — retire `value_of` + per-pass build; flip the default.** The graph is
      built once (at lower / first promotion) and never re-derived. Verify
      `WADO_MEASURE_VG` reports `rebuilds = 0`, measure `optimize` CPU halved,
      delete the `value_of` side-table — the three acceptance criteria — then make
      `WADO_PROMOTE_EARLY` the default and remove the flag + the late freeze.

Each phase is `WADO_VERIFY_VG`- and e2e-checkable with the flag on while the
default stays green; the criteria flip together at P4.

(The default-path regression set and the gating P0 are in "Status and next
step" at the top.)

### Per-pass attribution, and an incremental route under fix-while-advancing

Fresh `WADO_MEASURE_VG` on `benchmark/zlib/zlib_bench.wado -O2` (a stable
mid-size workload; absolute counts differ from package-gale but the structure is
invariant):

```
builds=1077 (first_builds=216, rebuilds=861)
  by pass cse:  builds=546 (first=216, rebuilds=330)
  by pass licm: builds=531 (first=0,   rebuilds=531)
total build node-walks = 573503
maintenance bound: new nodes 7389 + in-place edits 24012 = 31401 (5.5% of 573503)
```

So the graph is rebuilt once per _(pass × function × fixpoint-iteration)_: every
pass opens a fresh `Engine` per function and builds lazily on first query, and
the outer optimize fixpoint re-enters ~2.5×. `first_builds` (216) is one per
function; the 861 rebuilds are the redundant re-derivations. Pointwise
maintenance would touch ~5.5% of that node-walk cost — the ~18× prize behind
criterion 2.

The "all-or-nothing" claim above is precise only under a **zero code-quality
regression** bar. The maintenance primitives needed for an _incremental_ route
already exist and are proven: `Engine::maintain_value_after_edit` /
`maintain_pure_value` (re-derive a node's pure value, drop what they can't), and
the `partition_refines` oracle (`WADO_VERIFY_VG`) that certifies the maintained
graph never over-merges — coarsening (a dropped flow value) is sound, only a
missed optimization. Correction (Probe A, below): the route is _not_ a sequence of independently
shippable green steps. Step 2's coarsening alone **over-merges** — dropping the
edited node's ancestors leaves stale `value_of` on the **downstream readers** of
a reassigned local, which the next pure re-intern hash-conses wrongly (a
miscompile, masked today by the rebuild). So persistence (step 2) is sound only
once every flow value that a structural pass can stale is either coarsened
_with its downstream readers_ or **promoted into an operand slot** (a frozen
`Operand::Value` cannot stale). The blocker is soundness-coupling, not quality
regression — `WADO_VERIFY_VG` must be the development gate, and the persistence
flip lands atomically with the downstream-coarsening / promotion, not before it.
With that caveat, criterion 1 is still approached by these steps (green under the
verify harness), not one undifferentiated rewrite:

1. **Body-persistent graph.** Move `ValueGraphBuild` (`value_of` +
   `loop_entry_values`) onto `Body` next to `values: ValuePool`. Build once;
   never auto-drop. The build runs with a single **fixed conservative config**
   (alias/`mut_escaped` sets over-approximated up front), so there is no config
   to re-check on reuse — sidestepping the rejected "config-keyed reuse = cache."
   A coarser fixed config costs some forwarding precision (a recoverable
   regression), not soundness.
2. **Remove the rebuild triggers.** Drop the `value_graph = None` in
   `set_param_locals` / `set_alias_sets` / `set_value_graph_type_table` (config is
   fixed at build) and replace `invalidate_value_graph` at structural sites
   (`licm::hoist_invariant_arith`, `inline`, `sroa`) with **coarsening**: remove
   the edited subtree's `value_of` entries (and stale `loop_entry_values`) rather
   than re-deriving — `partition_refines` guards every step.
3. **Recover the regressions.** Tighten maintenance where the coarsening lost a
   real optimization (the field-forwarding `licm` relies on, the loop-entry
   snapshot a hoist staled), measuring byte size back toward parity. This is
   where loop recurrence eventually wants the `LoopPhi` operand model — but as an
   optimization-recovery step, not a soundness gate.

This is distinct from both rejected shortcuts: not a cache (no config/revision
check, no stored build to look up) and not an incremental _rebuild_ (no re-walk
of a disturbed suffix — maintenance is pointwise). It is the WEP's "live
maintenance to every pass," sequenced so each step is independently e2e-green and
moves `rebuilds` down monotonically.

#### Two empirical probes (run, measured, reverted) that scope the blockers

Step 1/2 above were probed with real code to find the precise obstacles before
committing the wide change. Both were reverted on correctness grounds; the
findings are the deliverable.

- **Probe A — drop `licm`'s `invalidate_value_graph`.** Byte-identical on zlib,
  correct on count_prime, full O0/O2 e2e green (2960/0) — but `WADO_VERIFY_VG`
  flagged a **new over-merge** in the licm session. Root cause:
  `maintain_value_after_edit` walks the edited node's _ancestors_ only, never the
  **downstream local readers** whose `value_of` a reassignment stales; the next
  pure re-intern then hash-conses two now-distinct trees to one `ValueId`. The
  invalidate was masking this by rebuilding. This is the same downstream-stale
  flaw the WEP names as the reason flow values must be _promoted into operand
  slots_ (a frozen `Operand::Value` cannot stale), not maintained as a side
  table. So step 2's coarsening must additionally **drop every downstream reader
  of a reassigned local**, or the value must be promoted — confirming operand
  promotion is the load-bearing piece, not optional.
- **Probe B — run the freeze (promotion) before the value passes.** Two failures:
  `licm` panics on a promoted operand (`licm.rs:559` assumes a skeleton `Expr` —
  a guard fix), and -O2 emits **invalid Wasm** (`i64`/`i32` type mismatch).
  Cause: `ValueKind` is type-erased and hash-consed, so after early promotion a
  later pass (`inline` / `const_fold`) creates a _new_ use of the shared value at
  a different width, and extraction picks the wrong one. The **late** freeze
  sidesteps this purely by timing (no pass runs after it to add a divergent-width
  use). So promoting early requires **per-use width preservation** —
  un-erasing the pool (carry width in `ValueKind`) or `alloc_unshared` per
  `(value, type)` — which is the precondition for moving promotion ahead of the
  passes.

  Partially fixed (commit `1f133e71a`): `ValueKind::Int` / `Float` now carry their
  source `TypeId` in the hash-cons key, so `7: i32` and `7: i64` are distinct
  values each recording its own width; `intern` stamps `type_of` at construction.
  All ~40 construction / match sites were converted atomically (the split-hash-cons
  hazard demands it), behavior-neutral (zlib -O2 byte-identical; 2960/0 e2e; a new
  width-distinctness unit test). The `licm.rs:559` guard (commit `6f158eba8`) lets
  `stmt_child_nodes` tolerate a promoted condition.

  Re-probed early promotion with both fixes in (freeze run once before the
  fixpoint loop): **still invalid Wasm** (`i64`/`i32` mismatch) on
  count_prime / mandelbrot / sieve / zlib. So the literal un-sharing was necessary
  but **not sufficient** — a second width path remains. `extract_value` derives a
  `Binary` / `Unary` / `Cast` operand's width from the single recorded
  `type_of(value)` (`translate.rs:1878-1903`), but `set_type` is last-write during
  the build, so a hash-consed value (a `Binary`, or an `Opaque` local read)
  reachable from two differently-typed uses keeps only one width — extraction then
  emits the wrong one. The late freeze dodges this by timing (nothing after it adds
  a divergent-width use). Real fix: width must be intrinsic to **every** value
  reached by extraction (carry the result type in `Binary` / `Unary` / `Cast` /
  `Opaque` hash-cons keys, or `alloc_unshared` per `(value, type)`), not a
  last-write side table. This is the precise next blocker for moving promotion
  ahead of the passes; the experiment was discarded (uncommitted, never landed).

  Root-caused (instrumented `extract_value`'s `Binary` arm, early-freeze build,
  count_prime): the wrong-width values are in the `u64` timing glue, and they are
  **internally inconsistent** — e.g. `ValueId(69) = Binary{Shr, Int(2,i32),
  Int(3,i32)}` with recorded `self_ty = u64`, and conversely a `Shr` with
  `self_ty = i32` whose `lhs = Int(1,u64)`. So a `u64`-typed binary expr carries
  `i32`-typed operand values (and vice versa). Two consequences:
  - Extraction reads the operand width from `type_of(lhs)`, not the binary's own
    `self_ty`, so it emits the operand width while the consuming context wants the
    result width → the `i64`/`i32` validation error.
  - `ValueKind::Binary` / `Unary` / `Cast` omit the result type from the hash-cons
    key, so a `u64` and an `i32` computation with structurally-equal operand ids
    collide onto one `ValueId` carrying a single (last-write) `self_ty`.

  The leaf fix (`1f133e71a`) was necessary but the composite kinds still erase
  width. The fix has two coupled parts, both real implementation: (1) carry the
  result `TypeId` in the `Binary` / `Unary` / `Cast` hash-cons key (intern records
  it, like `Int` / `Float`), so cross-width computations never share an id; (2)
  have `extract_value` derive each op's width from the value's own recorded type,
  and widen operands as needed. Only then is early promotion width-correct. The
  experiment + instrumentation were discarded (uncommitted); the branch stays
  green.

  Refinement (read `record_value_tree_types`, `extract.rs:92`): the freeze's
  width-conflict guard is actually **correct** — it recurses operands and returns
  `false` when an operand's recorded type differs from the stamped result type, so
  the inconsistent binaries above are **skipped, not promoted**. Therefore the
  early-promotion invalid Wasm is **not** a freeze-guard or hash-cons-key bug; it
  is the harder cross-pass class — a pass (`inline` / `const_fold` / `copy_prop`)
  substitutes a different-width value into a promoted operand slot _after_ the
  freeze, which the guard cannot see. That is fixable only by migrating those
  passes to respect promoted-operand types, confirming early promotion is coupled
  to the pass migration, not a bounded standalone fix.

  Composite-width landed (commit `adecff3ca`): `ValueKind::Binary` / `Unary` now
  carry the result `TypeId` in the key (part 1 above). Re-tested early promotion —
  **still invalid Wasm**, and instrumentation pinned a contradiction: the freeze
  redirects **0** width-mismatched binaries (`lhs_ty != ty`) directly (the guard
  works), yet early-freeze-**alone** (late freeze disabled) still emits the
  `i64`/`i32` mismatch on count_prime. So a mismatched `Shr` (`lhs = Int(2,i32)`,
  `ty = u64`) is extracted as a **descendant** of a redirected value via a path
  `record_value_tree_types` + `value_fully_reemittable_locally` should both
  reject (they recurse `Binary`/`Unary`/`Select`; `Cast`/`FieldAccess` children
  make a parent non-reemittable). The descendant reaching extraction contradicts
  that model — the next focused step is to instrument the **top** promoted operand
  and walk its value tree to see exactly how the `i32`-lhs `u64`-`Shr` is reached
  (and the upstream cause: a `u64` shift whose lhs literal is typed `i32` in NIR —
  a leaf-mistyping to fix at its source). Experiment + instrumentation discarded;
  branch green at `adecff3ca`.

  Deeper instrumentation (recurse **all** child kinds, `Cast`/`FieldAccess`
  included): **0** freeze-redirected source exprs have a width mismatch anywhere
  in their value tree _at freeze time_ — yet early-freeze-alone still emits the
  mismatch. So the inconsistency is **not present when the freeze promotes**; it
  arises **after**, when a later pass mutates the graph (a `value_union` from
  `cse` / `store_load_forward`, or a congruence rebuild) so that a promoted
  operand's `find()` later resolves to a different-width representative. Verdict:
  the freeze-before-passes **shortcut is the wrong vehicle** — it opens a
  freeze-then-mutate window that the WEP's actual design (`lower` emits
  `Operand::Value` directly and passes are migrated to consume operands, never
  re-deriving) does not have. The next promotion work should pursue the real
  design (operands born at lower, passes migrated) rather than the freeze-early
  shortcut, and separately fix the upstream `u64`-shift-with-`i32`-lhs-literal
  leaf-mistyping. Experiment + instrumentation discarded; branch green at
  `adecff3ca`.

### Pass migration begun: passes that mishandle promoted operands (commit `ebf1af821`)

The early-promotion experiment, though the wrong production vehicle, is the right
**diagnostic** for the pass migration the real design needs: with it on,
`WADO_SKIP_PASS` bisection pinpoints exactly which pass mishandles a promoted
`Operand::Value`. Two found and fixed:

- **`inline`** — `splice_operand` re-allocated a callee value into the caller pool
  by cloning its _kind_ and calling `alloc_unshared`, but a composite kind
  (`Binary` / `Cast` / `Select` / `FieldAccess` / `LoopPhi`) carries **child
  `ValueId`s scoped to the callee pool**; verbatim, they denote unrelated
  (often different-width) values in the caller. Fixed by a recursive `splice_value`
  that re-allocates the whole tree and remaps `Opaque` source locals via the
  inline `ctx`.
- **`dae`** — three gaps: (1) `find_dead_params` only scanned skeleton reads, so a
  param read **only through a promoted `Opaque(Local)`** looked dead (now unions
  `ValuePool::opaque_local_sources`); (2) local renumbering skipped
  `OpaqueSource::Local` indices (added `ValuePool::remap_opaque_locals`); (3) the
  dead-arg purity check `expect`ed a skeleton `Expr` (a promoted value is pure by
  construction).

With both fixed, all four benchmarks compile valid Wasm under early promotion and
count_prime runs correctly (`78498`). The fixes are **behavior-neutral in the
late-freeze flow** (no promoted composites reach `inline`/`dae` there): committed,
2960/0 e2e, 770 lib. Full e2e under early promotion still has **347 O2 failures**
(O0 skips the optimize loop, so it is unaffected) — the remaining pass-migration
surface (more passes to teach + `wir_expect` fixtures whose WIR shape changes
under promotion). These pass fixes are permanent and reused by the real
`lower`-emits-operands design; the next step is to keep bisecting the 347 for the
real-miscompile subset (vs `wir_expect` churn) and migrate those passes.

Next culprit bisected (`array_index_1`, `result: 0` vs `5` — a real miscompile,
not `wir_expect`): `WADO_SKIP_PASS` shows **`peephole`** is the pass; it runs every
iteration, so it likely accounts for a large share of the 347. Root cause is the
same class as the `dae` fix: `elide_local` decides a local is write-only via
`Engine::is_local_read`, which counts only **skeleton** `Local` mentions (the use
index) and misses a local read **only through a promoted `Opaque(Local)`** value —
so a still-read accumulator is elided and its value is lost. The fix needs care:
`ValuePool::opaque_local_sources` over-approximates (the builder seeds
`Opaque(Local)` for many locals that are never promoted into a live operand slot),
so consulting it unconditionally would keep nearly every local and **disable
`elide_local`**. The precise signal is the `Opaque(Local)` sources of values
referenced by a **live `Operand::Value` in the skeleton** — a skeleton walk, empty
in the late-freeze flow (so behavior-neutral there). That precise
live-promoted-reads query is the next brick (and the general primitive every
liveness-based pass needs under promotion); deferred rather than rushed because
the over-approximation trap makes a naive fix a silent quality regression.

Landed (commit `0cbb27fbf`): the live-promoted-reads primitive —
`Body::for_each_operand` (read-only mirror of `map_operands`) +
`Body::locals_read_via_promotion`, backed by `ValuePool::collect_opaque_locals` +
`find_imm` (a `&self` union-find root). `elide_local` now keeps a local read only
through a promoted `Opaque(Local)`. Precise (walks only values in live operand
slots, dodging the `opaque_local_sources` over-approximation), behavior-neutral
pre-promotion (770 lib green). Impact under early promotion: **e2e failures
347 → 111** — `peephole`/`elide_local` was the single dominant culprit (it runs
every iteration). Pass migration so far: `inline`, `dae`, `elide_local`.

Remaining 111 (early-promotion only; baseline green) by prefix: serde 12,
allocator 10, opt 7, match 6, array 6, wasm 5, newtype 5, closure 5, assert 5, …
Next culprit triaged — `coerce_float` (`f32 PI: 0.0000004` vs `3.1415927`):
`WADO_SKIP_PASS` implicates `copy_prop` + `inline`, but `extract_value`'s `Float`
arm is **correct** (instrumented: `bits` = f64 PI, `value_ty` = F32, emits
`F32Const(3.1415927)`). So the corruption is **downstream of extraction** — a
copy_prop/inline interaction on a promoted f32 value (a missing/!mismatched
f32↔f64 coercion or a cross-function value splice), not the extractor. A distinct
focused trace; deferred to the next brick. Tree restored green.

Width crux resolved (good news, verified): with composite-width (`adecff3ca`) and
the `elide_local` fix in, `count_prime` now **compiles valid Wasm under early
promotion** (was the original `i64`/`i32` invalid-Wasm), and a `VG_WMISMATCH`
probe (panic on any extracted `Binary` whose lhs width ≠ result width, comparisons
excluded) fires on **neither** count_prime nor coerce_float. So the
operand-width-erasure class that blocked early promotion is **closed** — the
remaining 111 are not width-extraction bugs. `coerce_float` specifically is a
copy_prop/inline value-substitution bug in the float-formatting path
(`short32`/`unpack32`'s u64 math producing a wrong digit _value_, not a wrong
width), `extract_value` and the binary widths being correct. Next brick: trace
that copy_prop/inline interaction (likely a promoted value spliced/propagated to a
wrong slot), which should clear the formatting-heavy fixtures (assert / inspect /
serde / coerce).

Pass-migration audit — two gap classes, mapped systematically:

- Local-elimination read-undercount (a pass counts skeleton `Local` reads, misses
  a local read only through a promoted `Opaque(Local)`, and eliminates/propagates
  it): found and fixed in `dae`, `elide_local`, `copy_prop` (the latter two via
  `Body::locals_read_via_promotion`). A sweep of every `optimize/*` pass for
  `is_local_read` / `read_count` / use-counting shows this class is now **closed**
  for locals: `dce` / `drve` / `sroa` do not count local reads; `ref_elim`'s
  `use_count` is over _reference_ bindings (`Ref` / `MutRef` / `Deref` are not
  promoted); `field_scalarize`'s `read_count` is over _fields_ (FieldAccess not
  promoted yet). Local renumbering that stales `Opaque(Local)` sources is handled
  in `dae` (`remap_opaque_locals`) and `inline` (`splice_value` via `ctx.local`);
  `licm` / `field_scalarize` / `container_sroa` append locals rather than
  renumber, so existing sources stay valid.
- Analysis-completeness (a per-expression read/write analysis misses a promoted
  read): `mod_ref`'s `local_reads` walks the skeleton subtree, so a local read via
  a promoted operand in that subtree is missed — currently unexercised (late
  freeze runs after `const_folding`, mod_ref's consumer), but a real gap once
  promotion precedes it. Deferred; flagged.

Future-gap (not yet exercised, will open when their kinds are promoted):
`field_scalarize` (FieldAccess promotion) and `mod_ref` (any promotion before
const_folding). Tracked so the keystone migration re-checks them.

Next high-leverage culprit: a `copy_prop` + `inline` interaction (skipping
_either_ fixes it) that miscompiles a _value_ — not a width — under early
promotion. Confirmed shared across batches: `coerce_float` (`f32 PI: 0.0000004`)
and `allocator_freelist_*` (a corrupted pointer `0xfffffff8` = -8 trapping in
`fl_unlink`), so it likely accounts for a large slice of the 111 (the formatting

- allocator families). Localization so far: `extract_value` and binary widths are
  correct; `copy_prop`'s `analyze_copy_binding` bails on a promoted-value _source_
  (`value.as_expr()?`), so the source is never promoted; the `read_count += 2`
  liveness fix (commit `5c0c2c31d`) does not resolve it. The `source_scope_stable`
  range check _should_ stay sound with a promoted `Opaque(Local target)` read
  present, so the mechanism resists static reasoning and wants empirical
  instrumentation (print when copy_prop propagates a binding whose target is in
  `locals_read_via_promotion`, on `allocator_freelist_align`, and diff the WIR
  with/without copy_prop).

  Resolved (commit `de72ba0af`): root cause is `apply_in_block` deleting the copy
  target's `let` (`dead_locals`) after `apply_in_expr` substituted only its
  _skeleton_ reads — a promoted `Opaque(Local target)` read then dangled on the
  deleted local. Fix: `can_propagate_copy` returns `false` when the target is in
  `locals_read_via_promotion`. This single guard cleared the bulk: early-promotion
  failures **111 → 10**. The `match_or_pattern_iflet_1` ICE
  (`labeled_block_fusion`'s `as_expr().expect` on a promoted condition) is also
  fixed (`9b139593f`, `as_expr()?`).

State of the early-promotion experiment: **347 → 10** (arith-freeze vehicle). The
final 10:

- ~5 not real bugs — `wir_expect` pattern / missed-opt tests whose expected WIR
  shape changes under promotion (`array_bounds_elim_*_wir`,
  `opt_licm_invariant_arith`, `tir_optimize_bool_identity`, likely `closure_2`):
  fixture-expectation churn, not miscompiles.
- **3 control-flow miscompiles** (`select_extended_arms`, `if_merged`,
  `labeled_block`) — all involve a promoted **`Select`** value. `WADO_SKIP_PASS`
  bisects to `peephole` (skipping it passes), so a peephole **rule corrupts the
  promoted `Select`** — it is _not_ the extraction-availability issue (that would
  manifest at WIR build regardless of peephole). Narrowing so far: removing
  `branch_prune` **or** `const_fold` individually does _not_ fix it, so the
  culprit is another peephole rule (`match_to_switch` / `value_copy_elide` /
  `ref_elim` / `elide_box` / `array_literal` / `labeled_block_fusion`) or the
  engine's value-graph maintenance over a promoted `Select` operand during the
  peephole session. `select_extended_arms` computes `decimal_pos = 2` where the
  source merge yields `-5`. Next step: continue the in-peephole rule bisection
  (remove rules one at a time) on `select_extended_arms`. (Separately, the
  extraction-availability concern — re-emitting `if cond {..} else {..}` at a use
  site vs a `local.get` of the merge value — remains a real keystone sub-problem,
  but it is not what these three fixtures hit.)

  Culprit isolated (per-peephole-rule `SKIP_PRULE` bisection): the rule is
  **`elide_local`** — skipping only it passes all three. It eliminates a local
  that is still read through a promoted operand, but `locals_read_via_promotion`
  (which `elide_local` already consults) **does not list that local**, so the
  guard does not fire. Verified: forcing `is_kept` to consult the pool-wide
  `opaque_local_sources` (over-conservative) fixes all three — so the needed
  local genuinely has an `Opaque(Local)` source, but it is not reached by
  `for_each_operand` + `collect_opaque_locals` from a live operand slot. A
  `find_imm`→raw-`kind` alignment in `collect_opaque_locals` (extraction reads the
  raw value, not the union-find rep) was tried and did **not** fix it, so the gap
  is subtler than a rep/raw mismatch — a promoted read this precise walk misses
  while `opaque_local_sources` catches. The over-conservative set is **not** a safe
  substitute (the builder seeds `Opaque(Local)` for many non-read locals, and
  `opaque_sources` persists on the body across passes, so it would disable
  `elide_local` broadly). Dormant in the committed late-freeze path
  (`locals_read_via_promotion` is empty there, e2e 2960/0), so the branch stays
  correct; the gap needs focused keystone-time work to make the precise walk
  complete. Reverted the speculative raw-`kind` change rather than ship an
  un-understood edit.

  Concrete next lead (derived, not yet validated against the fixture): a real
  asymmetry between extraction and the liveness walk. `extract_value`'s
  `Opaque` arm emits `OpaqueSource::Local(idx)` as `local.get idx` **and**
  `OpaqueSource::Expr(e)` as `translate_expr_inner(e)` — the scheduled skeleton
  expr, which reads whatever locals `e` reads. But `collect_opaque_locals` only
  handles `OpaqueSource::Local`; for `OpaqueSource::Expr(e)` it does nothing, so
  every local read by the scheduled expr behind an `Opaque(Expr)` is invisible to
  `locals_read_via_promotion`. Fix direction: `collect_opaque_locals` must, for an
  `OpaqueSource::Expr(e)`, also walk `e`'s skeleton subtree (and its nested
  operands) for `Local` reads. (Note `opaque_local_sources` also ignores `Expr`
  sources, so this specific gap is _not_ what the `ELIDE_OVERCONS` probe masked —
  meaning `select_extended_arms` has at least one _additional_ `Local`-source path
  the live-slot walk misses; both need closing.)

  Update (implemented + tested, reverted): the `OpaqueSource::Expr` walk was
  built (`collect_opaque_reads` collecting `Expr` sources, `locals_read_via_
  promotion` walking those skeleton exprs) and **did not** fix the three —
  because `value_fully_reemittable_locally` rejects `Opaque(Expr)`, so the freeze
  never promotes an `Expr`-source value in the first place; the walk is dead code
  for the current freeze. An `ELIDE_DIFF` probe showed the locals the precise walk
  misses (`[1,2]`, `[3,5]`, `[3]`, `[7,8]` across the fixture's functions) are in
  `opaque_local_sources` (so they have a _build-time_ `Opaque(Local)` seed from
  `seed_params` / `read_local`) but are **not reachable from any live operand
  slot** — i.e. they are not actually read through a promoted operand. So
  `ELIDE_OVERCONS` fixes the fixtures by _over-conserving_ (keeping locals with a
  build-time opaque seed), masking a root that is **not** a promoted-read gap.
  The true cause is therefore elsewhere — `elide_local` (in `peephole`, before
  `select_lowering`) dropping a local that some later step still needs, plausibly
  via the engine value-graph / `select_lowering` interaction rather than a missed
  promoted read. Three targeted fixes (`find_imm`→raw, `Opaque(Expr)` walk, and
  the over-conservative set) each disproved a hypothesis; the bug is dormant in
  the committed path (e2e 2960/0) and needs keystone-time investigation with the
  actual promote-at-lower machinery, not the late-freeze experiment, where the
  `Select` values arise in their real form.
- 1 ICE (fixed) and `assert_fail_call_arg` (power-assert diagnostic formatting,
  uncharacterized).

Net: the systematic pass-migration is essentially done for arith promotion (five
passes migrated — `inline`/`dae`/`elide_local`/`copy_prop`×2 — two bug classes
closed, **97%** of the failure surface cleared); the residue is the
`Select`-availability soundness problem (a keystone sub-task) plus `wir_expect`
fixture churn that only matters once promotion is the real vehicle.

### A criterion-1 lever independent of full promotion: licm's `loop_entry_values`

Of the 861 zlib rebuilds, **531 are `licm`** — `hoist_invariant_arith` calls
`invalidate_value_graph()` once per hoist round. The earlier conclusion ("needs
the `LoopPhi` operand model") may be stronger than necessary for the _per-round_
rebuilds: an arith hoist appends `let t = <invariant>` to the pre-header and
rewrites in-loop occurrences to read `t`. That edit **does not reassign any
existing local**, so every existing local's `loop_entry_values` entry stays
valid; only the new temp `t` needs an entry, and licm already knows `t`'s value
(the invariant it just hoisted). So the per-round invalidate can be replaced by
**targeted maintenance**: add `Engine::set_loop_entry_value(loop_body, t_local,
inv_value)` and let the occurrence rewrites flow through the maintaining edit API
(`replace_expr_kind`), dropping licm's rebuilds toward one-per-function (≈216) —
no promotion, no cache, no coarsening, `WADO_VERIFY_VG`-checkable. Caveat: the
_first_ arith-hoist invalidate per loop also absorbs the staleness from this
iteration's earlier _field_-hoisting (which does restructure the loop), so closing
that one too needs field-hoist maintenance; the per-round arith invalidates are
the bounded, immediately-actionable slice. This is the most direct next lever on
criterion 1 that does not wait for the whole keystone, and pairs with the
`cse`-side reuse (the other 330) once cross-pass maintenance is sound.

Correction after reading `hoist_invariant_arith`: the lever is more entangled than
a one-line `set_loop_entry_value`. The hoisted `let t` is **deferred** — appended
to `all_hoist_stmts` and prepended before the loop only _after the whole
`licm_loop` finishes_ — while the in-loop occurrences are rewritten to `Local t`
_during_ the round. So between rounds the body holds `Local t` reads with **no
`t` definition in the pre-header yet**; the value graph would see `t` as a
fallback `Opaque`, and `loop_entry_values` has no entry to set. That transient
inconsistency is the real reason for the per-round `invalidate`. Making it
maintainable therefore means restructuring licm to **insert each `let t` into the
pre-header immediately** (not defer to the caller), then maintain `value_of` +
`loop_entry_values` across that concrete edit (`replace_expr_kind` already
maintains the occurrence rewrites; the new `let t` adds one entry whose value is
the hoisted invariant). Bounded and self-contained, but a real refactor of licm's
insertion order, not a drop-in — flagged accurately so the keystone session does
not under-scope it.

Standing pre-existing finding from Probe A's harness run: `WADO_VERIFY_VG` is
**not clean on count_prime even on the committed baseline** (a
`cse → store_load_forward` over-merge). Currently benign (e2e green), but it
falsifies the earlier "clean across the corpus" claim and is the same
downstream-stale flaw — it disappears under operand promotion.

Root-caused exactly (`-O2`, `verify_maintained_graph` now prints the pair): the
maintained graph merges `expr53 = Local{index:9 "m"}` with
`expr41 = Binary{Shl}`, both pinned to a stale `ValueId(1268)`; a fresh build
correctly splits them (`1644` vs `1650`). At the session's first build `m` and
the shift were legitimately equal (`let m = … << …`). A later
`store_load_forward` edit changed the shift's operand, so the two diverge — but
`maintain_value_after_edit` propagates only up the **expr-ancestor chain**, and a
`Let` / `Assign` **Stmt** boundary breaks that chain, so the local's _reader_
exprs (`expr53`) are never revisited and keep the stale id. A maintenance-only
fix would have to drop every reader of a local whose defining RHS is edited
(flow analysis on each edit); the frozen-`Operand::Value` model removes the
staleness by construction. This is the load-bearing case for promotion, now with
an exact repro.

## Operand-promotion migration order

The representation change is wide and lands red across the pipeline. The order
below keeps each layer's intent clear so the work is resumable mid-change. The
verify harness (`partitions_agree`) and `maintain_pure_value` are in place.

- [x] **IR foundation.** `Operand { Value(ValueId), Expr(ExprId) }`;
      `Body::values: ValuePool` (function-owned graph). Compiles; nothing
      consumes them yet.
- [ ] **Arena.** Remove the pure `ExprKind` variants (`IntLiteral`, `FloatLiteral`,
      `BoolLiteral`, `CharLiteral`, `StringLiteral`, `Null`, `Unit`, `Binary`,
      `Cast`, and the pure `Unary` ops `Neg` / `Not` / `BitNot` — `Ref` / `MutRef`
      / `Deref` stay). Operand-bearing fields (`Assign.value`, `Let.value`,
      `Return.value`, call args, receivers, conditions, scrutinees, literal
      elements, …) become `Operand`. Update `for_each_child` / `clone_*` to recurse
      only into `Operand::Expr`. A skeleton placeholder replaces `ExprKind::Unit`'s
      dead-node role (`become_expr`).
- [ ] **lower::translate.** Build the graph while lowering: a pure expression
      interns into `Body::values` and yields `Operand::Value`; an effectful one
      stays an `ExprId`. Thread `current_value` / heap versions as the old
      `builder` did — this merges the value-graph build into lowering. `value_of`
      and `nir_value_graph::builder` retire (the builder's flow walk moves here).
- [ ] **Engine.** `value()` resolves an `Operand`; the edit API maintains
      `Body::values` in place (`maintain_pure_value` for new pure nodes, `set_value`
      for flow-sensitive ones); drop the lazy `value_graph` cache and the
      `CachedAnalysis` / `vg_cache` / `run_gated_cached` plumbing.
- [ ] **Passes.** Each pass matching a pure `ExprKind` switches to `Operand` /
      `Body::values` queries; structural passes maintain the graph at splice
      points. cse / copy-prop / slf collapse into graph identity.
- [ ] **WIR build + unparser + niri + type-repr.** Consume `Operand`: a `Value`
      extracts from the graph (the extractor), an `Expr` lowers the subtree.

## Build-once made invariant; promotion is the default; route B chosen

This session committed to operand promotion as the production path and removed
the rebuild machinery outright, then root-caused why the remaining redness is
the keystone, not a patch.

### What landed

- `promote_early_enabled()` / `promote_active()` are unconditionally `true`
  (commit `b6bb675a9`): promotion + build-once is the default; the
  `WADO_PROMOTE_EARLY` flag and the flag-off dual path are retired.
- The rebuild path is **gone** (commit `c5de23321`): `Engine::new` no longer
  resets `value_graph`; `set_param_locals` / `set_alias_sets` /
  `set_value_graph_type_table` no longer drop it; `invalidate_value_graph` is
  removed (its address-taken rescan survives as `invalidate_address_taken`,
  which licm calls). `rebuilds = 0` is now a structural invariant — there is no
  code that drops a built graph. zlib -O2: `builds=139 first_builds=139
  rebuilds=0`.
- The config-aware verify oracle (`ValueGraphBuild::config`, commit `8bcdfffb2`)
  stands: a persisted graph is read by sessions that configure differently, so
  `WADO_VERIFY_VG` rebuilds the comparison graph with the _build_ config.

### Acceptance-criteria status

- Criterion 1 (`rebuilds = 0`): met, structurally, under promotion-default.
- Criterion 2 (`optimize` CPU halved): on package-gale the optimize phase is
  13.6s → 8.3s (~1.65×), short of 2×, and with a +12% code-size regression
  (740254 → 828871 B) from the cse-skip + FieldAccess-not-yet-in-loop loss.
- Criterion 3 (`value_of` retired): not yet — the side-table still backs the
  per-function build.

### Promotion-default redness (fix-forward, accepted)

50 O2 e2e fixtures regressed at the default flip; the inline coarsening fix
(below) cleared 13, leaving 37: ~24 lost-optimization `wir_expect` + 11
`tmpl_hoist` + misc — all optimization-recovery, no miscompiles. The 3 Select
miscompiles the WEP tracked are gone (the over-conservative `elide_local` path
is always active now).

### The i128 miscompile: root cause and four ruled-out cheap fixes

`coerce_int_3`, `static_1`, `match_literal_i128_guarded` trapped: a u128 const
compared equal to itself read `false`. Root cause is the freeze-as-a-pass
**freeze-then-mutate window** (the shortcut flaw this WEP already named):

1. the early freeze promotes a pure value in a callee where its leaf is a
   parameter (valid, available);
2. `inline` splices that callee into the caller, where the param becomes a
   local bound to the argument; the promoted value now reads that local;
3. a downstream pass (const-object globalization / the late freeze) consumes a
   stale `value_of` / produces a body where the leaf's def is dropped while a
   read survives → WIR build emits an uninitialized read → trap.

Confirmed by `WADO_SKIP_PASS`: skipping `nir/inline` or
`nir/promote_pure_values_early` fixes it; a value-pool NIR diff shows the const
field read as non-constant under promotion. **Four cheap fixes were tried and
ruled out** — record them so they are not re-attempted:

- Inline dropping the graph (`value_graph = None`, commit `f9871c624`): fixed
  it, but reintroduced rebuilds (0 → 50) — rejected (build-once invariant).
- Inline coarsening (`value_of.clear()`): keeps rebuilds=0 but does **not** fix
  it — the corruption is at WIR-build extraction, not in `value_of`.
- Conservative freeze (only promote param-leaf values): does not fix it —
  param-ness is judged in the callee where the leaf _is_ a param.
- Materialiser enable + single-use source-point materialise for all kinds:
  does not fix it — materialising at the use's enclosing statement reads the
  leaves at the same point; the leaf's _def_ is already gone.

Conclusion: there is no cheap fix. Correctness here requires either precise
splice-point graph maintenance (route A, which the WEP already judged a
dead-end that never retires `value_of`) or **operand promotion born at lower so
there is no `value_of` side-table to go stale** (route B). Route B is chosen.

Exact mechanism (pinned to WIR build, not any pass — skipping every `wir/*`
pass still traps): the final NIR of `coerce_int_literal_comparison_i128` is
correct — `let cond = { let other = &{ let value = 1000; u128 { low: value } };
(obj0.low == other.low) && … }; if %96 { panic }` where `%96 = !cond`. But the
emitted WIR drops the RHS field's source:

```
value_2 = 1000;                                      // LHS const: emitted
global:__const_obj_0 = struct.new u128 { low: value_2 };
cond_5 = (if global:__const_obj_0.low == value_9 …); // RHS: reads value_9
…                                                    // value_9 = 1000 NEVER emitted
```

Two coupled causes, both downstream of the lost value identity:

1. Under promotion const-fold leaves the RHS field as `low: value` (a local)
   instead of folding it to `low: 1000` (it can no longer see `value ≡ 1000`),
   so a `value` local survives that folding would have removed.
2. WIR build flattens `(&{ block returning a struct literal }).low` — forwarding
   the field access to the literal's source `value` (→ `value_9`) but dropping
   the `let value = 1000` def when the enclosing block/struct is not
   materialised. The skeleton (`if !cond`) path keeps `cond` live through a real
   read so the block materialises and the def survives; the promoted (`if %96`)
   path reads `cond` only through `Opaque(Local cond)`, and the flatten loses it.

Route B fixes this structurally: with pure values born as operands and
`value_of` retired, const-fold reads `value`'s operand directly (always
`1000`), folds it, and no orphan `value` local reaches WIR build.

Resolution (commit `7377aad2d`): the trap was actually a **bounded WIR bug**,
not a value-graph one — the "no cheap fix" conclusion above was wrong. The RHS
const struct materialises as `drop(struct.new { low: local.tee value_9(1000),
high: 0 })`; the `local.tee` assigns `value_9` (read by the comparison) as a
side effect. `elide_multi_field_struct_locals` elided that struct because
`is_pure_for_elision` only recursed into children and judged `local.tee` /
`local.set` / heap writes pure — so the elision dropped the `tee` while the read
survived. Fix: treat `LocalSet` / `LocalTee` / `GlobalSet` / `StructSet` /
`ArraySet` as impure there. All three i128 fixtures pass; promotion exposed the
latent WIR bug (it left the field as a local the skeleton path would have
folded), but the root and fix are in WIR elision, independent of route B. Route
B remains the durable direction for criteria 2/3.

### Route B execution plan (build-once-and-promote, then fold into lower)

Touchpoints confirmed this session: `lower::translate::convert_operand`
(translate.rs:874–912) already births scalar literals as `Operand::Value` and
holds `arena: RefCell<Body>` (so the pool is reachable at lower); the builder's
flow walk (`compute_value`, builder.rs:759–1106; `current_value` / heap state /
`Select` at merges / `LoopPhi` at loops) is the piece to relocate; WIR build
(`translate_operand` / `extract_value`) and niri (`operand_to_lattice_a`)
already consume `Operand::Value`.

Recommended sequencing (reuse the tested builder first to limit risk):

1. Run the builder once after lower and **fully promote** every re-emittable
   pure value to `Operand::Value`, before the optimize loop.
2. **Availability-aware extraction (the one new analysis, where i128 is fixed):**
   materialise shared / flow-dependent values (`Select` at the merge,
   `FieldAccess` at its heap version, a value whose leaf a later pass may drop)
   into a `let _av = <value>` at a point dominating all uses, and keep leaves
   live (`locals_read_via_promotion`). The single-use enclosing-statement
   materialiser is insufficient — the leaf's def must be guaranteed, which is a
   liveness + placement obligation, not just an emission-point choice.
3. Migrate the value passes (`const_fold` / `cse` / `copy_prop` / `licm` /
   `condition_implication` / `select_lowering`) off `value_of` to operand /
   pool queries.
4. Fold `current_value` into `lower::translate` so pure values are born as
   `Operand::Value` directly; delete `value_of` and `nir_value_graph::builder`
   (criterion 3).
5. Recover the +12% / 37 `wir_expect` by promoting `FieldAccess` in-loop (its
   `heap_ver` values subsume store-load-forward) and measure criterion 2.

### Step 5 progress: immutable-ref field forwarding recovers the licm cluster

Landed (commit `b1e07c8ea`). The `opt_licm_immut_ref` cluster (4 fixtures, the
goal's licm half) is recovered, `WADO_VERIFY_VG`-clean, 0 newly broken — and the
fix retired `ref_1`'s prior verify false positive. Two parts:

- A builder gap: `apply_loop_heap_effects` invalidated _every reference-aliased_
  local's fields on an external write (a non-builtin call in a loop), while the
  per-call `bump_call_effects` invalidated only `mut_escaped`. The loop path now
  matches — an immutably-`&`-escaped local (`&config` to `fn process(&Config)`)
  cannot be written through, so its field constant survives the loop call. Before,
  `config.threshold` versioned past its seed and read an opaque `FieldAccess`
  (`heap_ver 3`) instead of `100`.
- The constant-leaf promotion now covers `FieldAccess` reads whose projection root
  is not `mut_escaped` (relaxed from address-taken: an immutable `&` escape is
  stable), freezing the now-forwarded `config_ref.threshold → 100`.

The BCE cluster is harder and **not** a constant case: `let input_len =
input.len()` is a _call_ at the build-once freeze, so `input_len` is valued as the
call result, not `input.used`. Only when `len()` inlines (`return self.used`) does
`input_len ≡ input.used` hold — an equivalence the build-once graph never captures
(inline coarsens, and the inlined `input.used` is itself an unvalued post-freeze
node). BCE then cannot prove `i < input_len` ⟹ `i < input.used`.

Empirical finding (this session): inline maintenance **alone is insufficient**.
`build_scoped` was extended to keep _splice-safe_ values — not just constants, but
`Binary`/`FieldAccess` over seeded caller values, so the inlined `pos < self.used`
re-interns to `Binary(V_i, Ge, FieldAccess(V_arr, used, INITIAL))` and the two
inlined `self.used` reads hash-cons. It recovered **zero** BCE fixtures and was
reverted. The reason: `condition_implication`'s actual operands are not the
inline-created nodes but `input_len` and `_licm_used_12` — bindings **`licm` and
`copy_prop` mint after inline**, themselves unvalued post-freeze nodes. So BCE
needs node-creation maintenance through `inline` **and** `licm` **and**
`copy_prop` — the full "maintain the graph through every structural pass," the
WEP's largest deferred core — not an inline-only `build_scoped` change. (Contrast
the recovered `licm_immut_ref` cluster, whose `config.threshold = 100` is forwarded
_at the freeze_ — once the loop-heap-effects fix stops bumping the immutable
receiver — and frozen by promotion _before_ any pass restructures it.)

### The 21 `-O2` regressions are node-creation, not coarsening (this session)

The remaining 21 `fixture_test_o2` regressions under build-once (`compound_assign_basic`,
`store_to_load_forwarding`, the `array_bounds_elim_*` / `optimize_bce_*` /
`opt_licm_immut_ref_*` clusters) share **one** root cause, and it is **not** the
inline-coarsening half of "maintain through structural passes."

Diagnosis: a structural pass (`sroa` / `container_sroa` / `field_scalarize`)
creates **new scalar locals** after the graph is frozen — e.g. `p2.a = 42`
becomes `__sroa_p2_a = 42; … read __sroa_p2_a`. Under build-once there is no
rebuild, so the new read's value must come from maintenance. But
`Engine::maintain_pure_value` re-derives only `Binary` / `Unary` / `Cast`; a
`Local` read needs its **reaching def**, which is flow state the frozen graph
discards. So the read carries no `ValueId`, `engine.value()` returns `None`, and
`store_load_forward` / BCE / `condition_implication` cannot fold it — the assert,
the bounds check, the `{x}` format stay live. `compound_assign_basic` is the same
shape one inline deep: `{x}` lowers to `Box<i32>{value: x}.Display::fmt(…)`, which
`inline` splices and `sroa` then scalarizes into a fresh local.

What this rules out: the inline-coarsening lever does **not** move these, and no
cheap retain is sound. Measured on the full `-O2` corpus under `WADO_VERIFY_VG`:

- `value_of.clear()` (clear-all) — sound, 21 failures.
- keep-every-caller-entry (keep-all) — **over-merges** (`serde_json_*`,
  `variant_qualified_pattern`, `opt_container_sroa_nondup_idx`,
  `parser_synth_id_collision_test`): it retains opaque call-result classes that
  inlining turns into distinct allocations a fresh build splits.
- keep-only-constant-valued entries (constant-retain) — **also over-merges** (31
  cases: `closure_*`, `opt_container_sroa_*`, `mut_param`, …). The hole: a
  `Local` read pointing at a constant is sound only while that constant is its
  reaching def. Inlining can restructure control flow (introduce a loop
  back-edge), so a fresh build makes the read loop-variant; two distinct locals
  sharing a constant init (`index = 0`, `init = 0`) then both keep the
  hash-consed constant and over-merge. Only a literal _expr node_ is truly
  inline-invariant, and keeping just those recovers nothing. All three retains
  yield the same 21 (constant store→load forwarding across inline is exactly the
  unsound `Local`-read case), so coarsening cannot recover them. clear-all stays
  the sound coarsening.

Two decisive experiments pin it to the node-creation half and rule out Method A:

- **Fresh rebuild at the inline splice** (force `value_graph = None` so the next
  query rebuilds) recovers them: `compound_assign_basic` folds to
  `fmt_decimal(15/12/24/6/2)`, `store_to_load_forwarding` drops all panics. So
  the values _are_ derivable — build-once plus incomplete maintenance loses them.
  But this is a rebuild (what the WEP forbids), and it works only because the
  rebuilt graph is taken _after_ `sroa` has scalarized: the recovery is downstream
  of the splice, not at it.
- **Completing Method A's seeding** (seed `build_scoped`'s scratch heap with the
  call-site struct-literal field values, so a `param.f` read in the spliced body
  forwards — `compound_assign`'s `Box<i32>{value: x}.Display::fmt` makes
  `self.value = 15` a constant in the inlined region) lands the value in
  `value_of` but recovers **0** of the 21: `inline` runs _before_ `sroa`, only
  `peephole` sits between them and does not forward field reads, so `sroa`
  restructures `self.value` into a fresh scalar local and the entry is lost before
  any forwarder runs. Reverted — correct but inert against these regressions.
- **Seeding at the node-creating pass itself** (`sroa` transfers a forwarded field
  read's constant to the scalar read it mints, via `Engine::seed_const_value` —
  the WEP's "have the creating pass seed `value_of`"). Recovers **0**: a debug
  trace shows `engine.value()` is `None` for _every_ `obj.f` read `sroa` rewrites.
  The field reads themselves postdate the build-once graph (assert / template
  desugaring mints them after the freeze), so they were never valued — there is
  nothing to transfer. This is the gap one level up, and it generalizes: under
  build-once, **every node created after the freeze is unvalued**, and only a
  rebuild over the current body values them. No localized seed reaches them; that
  is precisely why the fresh rebuild is the only thing that recovers, and why the
  fix must be _born-frozen values_ (Route B), not post-hoc maintenance. Reverted.
- **The combination** (precise inline coarsening — drop the spliced _region's_
  exprs plus every _non_-constant caller entry, keeping caller constants so
  `p2.a = 42` survives the inline that wiped `run()`'s graph — _plus_ the `sroa`
  const-transfer above, now with a value to transfer) **does** light up `__sroa_*`
  reads (`p2.a → 42`), but is **unsound**: 20 over-merges, 22 fixtures miscompiled.
  `WADO_VERIFY_VG` pins it: `__sroa_p_x` maintained→`Int(0)` but fresh→`ValueId(45)`.
  The forwarded constant a field read carried is **context-dependent** — it is the
  value at one heap version / reaching def, not a property of the scalar's def. A
  fresh post-`sroa` build re-derives the scalar from _its_ def and splits, so
  transferring the constant onto the new node over-merges. This is the same lesson
  as constant-retain (a constant on a `Local`/scalar read is only as stable as its
  reaching def) and the deepest reason the value cannot be _maintained_ across
  restructuring — it must be _born_ in the operand slot, which is Route B.

Remaining piece (the node-creation half): when a structural pass creates a local
def with a known value and reads of it, maintenance must propagate the def's
value to those reads — either by re-deriving the affected local's reaching defs
pointwise (which is the rejected incremental rebuild), or by having the value be
born frozen so a read of it cannot stale. This is the precise obligation Route B
discharges by construction (flow frozen into `Operand::Value` at the def, a read
of a promoted local is the value itself), so the durable fix is Route B — not a
`Local`-read case in `maintain_pure_value`, not field-seeding the regrow, not a
retain at the inline splice, not seeding at `sroa`, not the two combined. Seven
localized-maintenance variants were measured under `WADO_VERIFY_VG`:

1. clear-all — sound, 21.
2. keep-all caller entries — unsound (opaque call results inlining splits).
3. constant-retain (all constant entries) — unsound (inlined-region loop vars).
4. field-seed-regrow (`build_scoped` seeds call-site struct fields) — sound, 0.
5. sroa-const-seed (transfer field read's constant to the scalar) — sound, 0
   (the field reads postdate the freeze; nothing to transfer).
6. precise-retain + sroa-seed — unsound (20 over-merges, 22 miscompiled).
7. precise inline retain alone (drop spliced-region exprs, keep _caller_
   constants) — **still unsound**: `opt_container_sroa_nondup_idx` over-merges
   `__v3`/`__v1`, two caller locals both kept a constant that a fresh build splits.
8. pre-`sroa` field forward (promote `obj.f` constants to frozen operands before
   `sroa`) — ineffective: the _field_ read `p2.a` does not forward at the value
   graph (it gets an opaque `FieldAccess(recv, field, ver)`, a heap-version/root
   gap), so there is no constant to freeze; the recovery the fresh rebuild gets is
   the post-`sroa` _scalar_ (`__sroa_p2_a = 42; read → 42`), not the field.
9. pre-`inline` constant forward (freeze constants before inline empties the
   graph) — **unsound and net-negative**: recovers 1 (`inline_cold_path_cost`) but
   14 over-merges and 13 newly broken. Building the graph early and then letting
   inline/`sroa` restructure re-introduces the same stale-context over-merge.

Variants 8–9 confirm the recovery is the post-`sroa` _scalar_ local read, which
build-once leaves unvalued (a fresh rebuild over the scalarized body is the only
thing that values it), and that pre-emptively building/forwarding to dodge that
just relocates the same stale-context over-merge.

Variant 7 is the clinching proof. The hope was that a _caller_ constant is safe
because its reaching def is caller-side; it is not, because `inline` restructures
the caller's own control flow (a spliced loop/branch puts the read on a back-edge),
so a fresh build re-derives the caller read as loop-variant. The invariant under
all seven: **a value the graph forwarded is bound to the flow context it was
derived in, and any structural pass can change that context for any read** — so no
retained or transferred value is safe, and `clear-all` is the _unique_ sound
coarsening. The one thing that recovers — a fresh rebuild over the post-`sroa`
body — is the rebuild build-once forbids. Only a value _born_ in the operand slot
(frozen at the def, never re-derived) is context-free. Recovery is therefore Route
B or nothing — this is now demonstrated, not argued.

The `WADO_VERIFY_VG` oracle (`verify_maintained_graph` / `partition_refines`,
config-aware fresh rebuild per query, off by default) is reinstated as the guard
for this maintenance work — it is what proved both keep-all and constant-retain
unsound, redirecting the effort to Route B.

### Variant 10: born-frozen constant promotion is sound and recovers the values

The one variant that is _not_ maintenance: promote each constant-valued leaf read
(`Local` / `FieldAccess`) to its literal `Operand::Value` at the **early freeze**
(`freeze_pure_arith` before the optimize loop), where the build-once graph still
values it. A literal is context-free, so this is sound by construction — `0`
over-merges across the whole `-O2` corpus under `WADO_VERIFY_VG` — and it survives
`inline` and `sroa` because both copy operands rather than re-derive them.
Measured: the value _is_ recovered (`compound_assign`'s `i32::fmt_decimal` arg
becomes the constant `15`/`12`/`24`/`6`/`2`; `store_to_load`'s asserts fold). This
is the first lever that both stays sound and carries the value across the
structural passes — the operand-promotion keystone, applied to constants.

End-to-end **recovery was demonstrated** this round: with the three pieces below
applied, `compound_assign_basic` and `inline_cold_path_cost` both pass (the first
actual `-O2` recoveries of the session). Three pieces were needed, and the third
exposes the real soundness boundary:

- Pass migration. A promoted `Operand::Value` where a pass assumed a skeleton
  `Expr` panics on `as_expr().expect("skeleton operand")` (139 such sites total).
  Constant-leaf promotion reaches ~14; each is the same uniform guard (a promoted
  constant is the benign case — no projection root, no ref target, trivially
  speculatable / uniquely-owned). The 12 already committed as groundwork plus
  `ref_elim`, `string_push`, a second `elide_box_local`, and `builder`'s effect
  walk cover the reachable set; it converges per fixture round.
- The residual fold. `inline` binds the spliced callee param as
  `let self = Operand::Value(15)`, but the read `self` did not fold: `const_fold`'s
  `flow_fold_value_a` calls `try_fold_a`, which only folds `Binary`/`Unary`/`Cast`
  and **never consults the local env for a bare `Local` read** — even though its
  own doc promises "env-bound locals". Routing a `Local` read through
  `expr_to_lattice_a` (which does read the env) folds `let x = <const>; … x …`
  that store→load forwarding missed (a post-`inline` binding the build-once graph
  never valued). This is a real, localized completeness gap.
- The soundness boundary (why this is not committed). A constant read is only safe
  to freeze in a place that is **not** a lvalue (the `&mut x` / `.field` / receiver
  positions — guarded by `is_place_read`) **and** whose storage a fresh build will
  still agree is that constant. The early freeze is **mid-pipeline**: a later
  `inline` / `sroa` can re-contextualize a promoted read's neighbours, so a fresh
  rebuild splits a pair the maintained graph still merges — `ref_1`'s
  `let mut c = true; &mut c; … c …` over-merges (`WADO_VERIFY_VG` flags it). The
  hazard is exactly the reference-aliased / mutable locals; but `compound_assign`'s
  recovery itself depends on promoting a mutable, aliased `x` read, so excluding
  the unsound case (`!aliased` / `!mut`) also removes the recovery. There is no
  sound subset of _mid-pipeline_ freeze promotion that recovers these.

Landed (commits `556e3e4bd`, `4c2f90a9b`). The mid-pipeline hazard above is
resolved by restricting the freeze to the **early** (pre-loop) call, on each
function's clean freshly-built graph, plus three guards that make the promotion
sound by construction:

- `early`-only: the late (post-loop) freeze never leaf-promotes — only the
  pre-`inline`/`sroa` one does, so the frozen literal is born before any
  re-contextualization.
- `is_place_read`: never freeze an lvalue read (`&mut x`, a `.field`/method/index
  receiver, an assign target) — the storage, not the value, is used there.
- Address-taken / `Local`-only: a `&`/`&mut`-escaped local's constant is
  point-specific, and a `FieldAccess` constant can be a reference field whose
  pointee changes; both are excluded.

A real builder gap surfaced and is fixed alongside: `bump_call_effects` now drops
a `mut_escaped` local's scalar `current_value` (not only its heap fields), since a
`&mut` call (`set_bool(&mut c, false)`) overwrites the scalar — coarsening-only,
sound. With the `const_fold` env-local fold (`flow_fold_value_a` now routes a
`Local` read through `expr_to_lattice_a`), this recovers `compound_assign_basic`
and `inline_cold_path_cost`: **21 → 19** on the `-O2` corpus, **0 newly broken**,
lib green, runtime-correct. The width-preservation blocker was already fixed
(`extract_value` takes a constant's width from the literal's own carried `TypeId`).

The earlier "no sound subset of mid-pipeline promotion" applied to the broad,
unguarded variant; the `early`-only + guarded form is the sound one. Under
`WADO_VERIFY_VG`, `ref_1` reports one **false positive**: maintained correctly
forwards `Box{value:c}.value → c` (a valid equality at that read — the box just
captured `c`), but the post-promotion _fresh_ build fails to re-derive that
field-forward, so the oracle flags a merge the runtime confirms correct (`ref_1`
prints `true`/`false`). The durable resolution is the lower-phase graph (step 4
below), where there is no separate build-once graph to diverge; widening
promotion past constants to in-loop `FieldAccess` is the remaining recovery lever
for the BCE / `licm_immut_ref` clusters (non-constant forwarded field values).

## The ideal end state, and the one root cause to remove

Two recovery walls remain — the BCE field bound (`arr.len()` inlines to
`arr.used`, whose promotion needs the correct heap version) and the loop
induction variable (which `cse`/`copy_prop` fragments into a temporary, so a
naive `let _iv = i` materialiser does not unify guard and check and miscompiled
`array_bounds_elim_oob_guard_var_mutated`). Both have one root cause:

> `inline`'s `value_of.clear()` destroys the flow context (heap versions,
> reaching defs) that the values were correct under.

Every workaround tried — a query-time `value()` fallback over leaves, a
fresh-heap `build_scoped`, a per-pass loop-var materialiser — is an attempt to
reconstruct that lost context after the fact, and each is unsound or fragile.
The fallback is a smell: it signals the side-table `value_of` is the wrong home
for pure values under build-once, because the one pass that cannot maintain it
(`inline`, which splices through the arena and reshapes control flow) tears it
down.

The ideal removes the root cause rather than patching symptoms:

- [x] Pure-node maintenance through the clear (`Binary`/`Unary`/`Cast` recomputed
      from operands; never a leaf). The legitimate half of the fallback.
- [x] Same-block shared `FieldAccess` materialisation (one load for many uses).
- [ ] Born-as-operands: pure values live in the skeleton as `Operand::Value`, not
      in a side-table. `inline`'s arena splice then carries them for free —
      there is nothing to clear.
- [ ] `inline` carries and **remaps** spliced operands instead of clear+regrow:
      callee opaque locals → caller arg values (`seed` already has this), callee
      heap versions → the caller's version at the call site.
- [x] The caller's version at the call site, without a rebuild: persist the
      `HeapSnapshot` at each `Call` expr (`ValueGraphBuild::call_site_heap`) and
      seed `build_scoped` with it instead of a fresh `INITIAL` heap. `build_scoped`
      now also keeps every caller-rooted re-emittable value (not just constants),
      re-interned into the live pool at its true version (`reintern_live_rooted`).
      Sound: the per-call snapshots distinguish versions a fresh `INITIAL` would
      collapse, so `array_bounds_elim_oob_bound_shrunk` still traps and the oracle
      is clean.
- [x] Call-immutability-aware `bump_call_effects`. `alias::pure_calls` flags every
      call that mutates no caller local (no `&mut`/by-value-reference arg, an
      immutable receiver per `method_mutates_receiver`); the build (config-carried,
      so the oracle rebuilds consistently) skips such a call's `mut_escaped` bump,
      bumping only any `untrackable` stash. A `mut_escaped` receiver's field
      version is now stable across a pure accessor (`arr.len()` no longer splits
      `arr.used`). Sound: a mutating call (`pop()`) is impure and still bumps, so
      all `array_bounds_elim_oob_*` fixtures trap and the oracle is clean; no
      regressions on the optimization corpus.
- [ ] The remaining link, pinned exactly: at `condition_implication` for
      `safe_get`, **both** operands of the guard `pos >= arr.used` resolve to
      `None` — not just the field, but the **parameter** `pos`. `inline` clears
      `value_of` for the _whole function_, while `build_scoped` only re-grows the
      _inlined blocks_, so the caller's own code (the guard, the `pos` read) is
      orphaned, and `arr.used`'s build_scoped value is also lost when `peephole`
      collapses the inlined `len()` `LabeledBlock`. The fix is born-as-operands for
      the caller's entry-stable leaves: the early freeze (pre-`inline`) promotes a
      value-position immutable-parameter read to `Operand::Value(Opaque(Local p))`
      — a re-emittable `local.get p` that survives the clear, so `pos` resolves at
      the consumer (sound promotion, not the query-time leaf derivation that was
      the smell). With `pos` promoted and the inlined field surfaced as an operand
      the `peephole` collapse carries, the guard and check resolve and the bound
      folds. Measured (this session): promoting every immutable-parameter read at
      the early freeze is sound (the verify oracle stays clean, no over-merges) but
      **not** byte-neutral — it is the WEP's P4 "born-as-operands default flip" and
      churns ~40 `wir_expect` fixtures (the promoted `Operand::Value(Opaque param)`
      re-emits the same `local.get` but reshapes the WIR the goldens pin). So leaf
      promotion must land as a coordinated unit (promote leaves + surface the
      inlined field + refresh the goldens), not piecemeal: promoting `pos` alone
      churns broadly without recovering, since `arr.used` is still orphaned. The
      full chain was traced end to end this session (param promotion + an entry
      cross-block field materialiser, both built, validated sound, then reverted):
      with `pos` promoted, the guard's left operand resolves, but the field still
      does not, because the **inlined receiver** `arr` is itself unvalued after the
      clear — it is a _receiver-position_ read (a place, excluded from leaf
      promotion), so `operand_value_in` returns `None` for the inline `seed`, and
      `build_scoped` mints a _fresh_ opaque for `len()`'s `self` and another for
      `index_value()`'s `self`. The two `arr.used` then carry different receiver
      opaques and never share a value to group on. So the coordinated unit also
      needs the inlined call's receiver value seeded (promote receiver reads, or
      thread the receiver's `ValueId` into the binding) — the same born-as-operands
      obligation, one level up.

      An entry-`FieldAccess` materialiser was implemented and reverted; the
      attempt **disproved the simple model** and pins the real obstacle. The
      materialiser computes `field_access(entry_value(root), field, INITIAL)` for a
      `recv.field` whose receiver `resolve_param_root`s to a parameter, pins one
      `let _av = param.field` at entry, and promotes the reads. It miscompiled ~165
      fixtures — a pinned **reference/aggregate** field (`Array`/`List`/struct)
      changes value-copy / aliasing semantics and *traps* `array_index_1` — and it
      re-fired per optimize iteration, minting duplicate `_av`. Restricting to
      scalar fields (`prim_of`) is sound but **recovers nothing**, because tracing
      `safe_get` at the materialiser shows its only candidates are a `MutRef` and a
      `BuiltinArray` field — **the `.used` bound is not a bare `param.field` read at
      all** after `len()` inlines (it sits inside the `len()` `LabeledBlock`, its
      receiver a wrapped/inlined `self`, not the parameter). So the earlier
      "`safe_get` recovers correctly" was a *false positive*: the correct output
      came from the unsound reference-field materialisation that happened not to
      trap there, not from a sound i32-bound promotion. Conclusion: a sound
      field-bound recovery is **not** a entry-materialiser over `param.field`; the
      bound's post-inline shape (LabeledBlock-wrapped, receiver an inlined `self`)
      must be normalised first — i.e. it really does require maintaining the value
      through the inline splice and the `peephole` collapse (the born-as-operands /
      all-pass-maintenance core), not a post-hoc materialiser. The loop-guard
      cluster additionally needs the induction variable resolved.
- [ ] Migrate the value passes off `value_of` to operand / pool queries, then
      delete `value_of` and `nir_value_graph::builder` (criterion 3). The
      loop-var wall dissolves too: with the graph never cleared, the induction
      variable keeps its `LoopPhi` identity across `cse`'s copies, so guard and
      check resolve to it without a materialiser.

Build redness during the migration is accepted: the side-table's removal is the
point, not its preservation. The persisted-snapshot seed is the next concrete
step and is independent of the rest (inert until `build_scoped` consumes it).

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md) — the direction; equality saturation stays deferred there.
- [NIR Rewrite Engine — Detailed Design](./wep-2026-06-05-nir-rewrite-engine-design.md) — the engine substrate, edit API, and gate this builds on.
- [`docs/optimizer.md`](./optimizer.md) — the pass inventory the graph absorbs.
- Cranelift's aegraph mid-end and `egg` (https://egraphs-good.github.io/) — the build-once, eager-rewrite, single-extraction model this adapts.
