# Spike: per-`(receiver-root, field)` heap modeling in the ValueGraph

Status: **design + scaffolding only** (spike branch `spike/valuegraph-field-heap`).
Not merged. The verified byte-identical alternative (Option A — engine-routed
const-fold visitor) is the safe fallback for the main branch.

## Goal

Make the ValueGraph builder the single source of field/local value identity,
so the standalone flow-sensitive const-fold walker (`ConstFoldVisitor` +
niri `field_env`) can be deleted. This is the WEP's stated heap-model
evolution: "MVP uses per-field granularity …; a later stage promotes to
per-`(receiver-root, field)` using `mod_ref.rs`"
(`docs/wep-2026-06-05-worklist-rewrite-engine.md`, "Heap modeling").

## Why this is needed (measured)

Deleting the visitor outright (Option C) regressed **58 golden fixtures**.
Root cause: the builder suppresses field store→load forwarding for
reference-aliased receivers (`alias_unsafe` guard in `Builder::walk_expr`
Assign / `seed_struct_literal_fields`), and its heap version is
per-`field_index` (global). The visitor forwarded aliased single-level
`local.field` reads with per-`(local, field)` precision and alias-aware
invalidation. Example (`httpbin_get`): base folds `table.used → 256` at the
use site, so the bound is inline `256` before LICM; without it LICM hoists
`table.used` into extra `_licm_used` locals + empty `block {}` cruft — same
semantics, worse code.

`store_load_forward` already forwards `FieldAccess` reads via `engine.value`
(`forward_at_root`), so **the only change needed is in the builder**: give
aliased-receiver single-level field reads a literal `ValueId`.

## Design

### HeapState → per-`(receiver-root, field)`

Replace `per_field: IndexMap<u32, HeapVersion>` with:

```rust
struct HeapState {
    next: HeapVersion,
    /// Version of an exact (receiver-root local, field_index) slot, bumped by
    /// a direct `local.field = …` store.
    per_slot: IndexMap<(u32, u32), HeapVersion>,
    /// Per-root-local generation, bumped when ALL fields of a local may have
    /// changed: the local reassigned, or a call/opaque write while the local
    /// is reference-aliased. `version_of` maxes this with `per_slot`.
    per_local: IndexMap<u32, HeapVersion>,
    /// Version for slots in neither map. `bump_all` advances it (truly opaque
    /// writes: deref store, global set, indirect call).
    default_version: HeapVersion,
}
```

`version_of(root: Option<u32>, field) = max(default, per_slot[(root,field)],
per_local[root])` (root present only for `Local`-rooted receivers).
`HeapVersion` now derives `Ord` (done in this spike) so `max` works.

`HeapSnapshot` mirrors the three fields; `version_of` likewise.

### receiver-root helper

```rust
fn receiver_root(&self, recv_expr: ExprId) -> Option<u32> {
    match &self.body.exprs[recv_expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr, .. } => self.receiver_root(*expr),
        _ => None,
    }
}
```

The visitor only forwarded **single-level** `local.field` (its arm required a
bare `Local` receiver), so seed only when the receiver is a bare `Local`.
Use the root for invalidation granularity.

### AliasInfo (relocate from `optimize/alias.rs` + niri)

The builder needs the full `AliasInfo { aliased, untrackable, alias_groups }`,
not the current `alias_unsafe` union:

- `aliased` — receivers whose fields ARE recorded but dropped at side-effect
  boundaries (calls). Seed them; a call bumps `per_local` for each.
- `untrackable` (`stores`-aliased) — never seed (their aliasing escapes).
- `alias_groups` — ref-copy groups (`let r = &x`, `Box`, `List`, `&T`,
  `&mut T`); a write to one member's field bumps the group's `per_local`.

Move `LocalSet`, `AliasInfo`, and `build_alias_info` (+ helpers
`walk_all`, `collect_aliased_node`, `same_pointee_reference_edges`,
`collect_alias_groups`, `type_creates_alias`, `collect_alias_edges_node`,
`reference_pointee_struct_key`) into a new `nir_value_graph/alias.rs` (it
only needs `Body`, `NirLocal`, `TypeTable`, `IndexSet/Map` — all below the
`optimize` layer). `build_value_copy_helpers` / `recognize_value_copy_a`
move too (value-copy field transfer, below).

### Wiring AliasInfo to the builder

`Engine` currently carries `alias_unsafe_locals` + a lazy `body_address_taken`
scan, passed to `builder::build(body, param_locals, alias_unsafe)`. Replace
with `AliasInfo`:

- `Engine::set_alias_info(AliasInfo)` (replaces `set_alias_unsafe_locals`).
- `build(body, param_locals, &AliasInfo)`.
- **Every pass that builds a ValueGraph session and queries field VNs must
  supply `AliasInfo`**: `store_load_forward`, `cse`, `licm`,
  `condition_implication`, the const-fold rule. They have `func`
  (locals, `address_taken_locals`, `stores_aliased_locals`) and
  `project.type_table`; call `build_alias_info(...)` and `set_alias_info`.
  This is the cross-cutting part — get it wrong and every field-VN pass is
  affected.

### Builder walk changes (mirror the visitor, `optimize/const_folding.rs`)

- `Assign` to `local.field` (bare `Local` root `r`, `r ∉ untrackable`):
  `bump_slot(r, field)`; seed `field_store[(recv_vn, field, version_of(Some(r),
  field))] = value_vn`; also `bump_slot` for `alias_groups[r]` members.
  Non-bare-`Local` field target (`a.b.f`, deref, index): bump aliased
  locals' `per_local` (mirrors `invalidate_aliased_fields`), or `bump_all`
  for deref/global.
- `Assign` to bare `Local r`: `current_value[r]` already changes (auto-drops
  `r`'s field slots via recv-VN change); additionally copy field knowledge
  for ref-typed `let r = src` (value-copy / Local→Local — `copy_fields_from`).
- `Call` / `MethodCall` / `IndirectCall`: `for l in aliased { bump_local(l) }`
  (NOT `bump_all` — non-aliased fields survive a call, matching the visitor).
  `IndirectCall` / `CmRawCall` stay `bump_all` (unknown capture). Builtin
  intrinsics that don't touch user struct fields (`is_field_env_pure_call`)
  skip the aliased bump (mirror `collect_loop_writes`).
- `FieldAccess` read: `root = receiver_root(inner)`; `ver = version_of(root,
  field)`; existing `field_store` lookup + `field_access` VN.
- `seed_struct_literal_fields`: seed for all receivers (drop the
  `alias_unsafe` guard; keep the `untrackable` guard).
- Branch (`If`/`Match`/`Switch`): the existing `join_heap` already joins
  `per_field` per fall-through arm — extend to join `per_slot` + `per_local`
  the same way (lattice meet = keep a slot's version only where all
  fall-through arms agree; differ ⇒ fresh). This is the
  `FieldSnapshot::join_arms` analog.
- `Loop`: pre-scan body writes (mirror `collect_loop_write_effects`):
  `bump_slot`/`bump_local` for every `(local, field)` / aliased local the
  body may write, before snapshotting `loop_entry_values`; restore after.

### Delete the duplicate

Once forwarding lands in the builder and goldens are reviewed:

- Delete `ConstFoldVisitor` + branch-fork/loop-write/`AssignTarget`/
  `ExprShape` machinery in `optimize/const_folding.rs`; const-fold becomes
  the engine `ConstFoldRule` (literal arithmetic + CTFE + globals +
  constant-branch collapse) — see the Option-C shape, but **keep** field
  forwarding in the builder, not deleted.
- Delete niri `field_env` / `bind_field` / `AliasInfo` / `FieldSnapshot`
  (now in `nir_value_graph/alias.rs`); keep `env` (CTFE) and the
  `Local → env` arm.

## Verification gate

Regenerate ALL golden WIR fixtures (`mise run update-golden-fixtures`) and
diff vs base. Expect: the 58 regressions recover (aliased fields forwarded
again), plus a set of CSE-precision changes from per-`(root, field)` (a.f
now stable across a b.f write). **Review every changed golden by hand** to
confirm equal-or-better (never a real regression). Then full e2e (all opt
levels) + `test-wado` + `package-gale` compile.

## Risks

- Heap version is load-bearing for `store_load_forward` / `cse` / `licm` /
  `condition_implication` correctness — a per-`(root, field)` invalidation
  bug is a **miscompile**, not a quality regression. Test exhaustively.
- The aliased-vs-`bump_all` call change makes the builder forward more across
  calls; ensure `aliased` is complete (the visitor's `same_pointee` ref-param
  edges + body `&`/`&mut`/capture scan must all feed `aliased`).
- `alias_groups` precision: under-grouping ⇒ stale forward (miscompile);
  over-grouping ⇒ lost fold (quality). Match `build_alias_info` exactly.

## Scaffolding landed in this spike

- `HeapVersion` derives `Ord` (`nir_value_graph.rs`).
- This spec.
