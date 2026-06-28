# Canonical Function Identity (`FuncId`)

## Context

One function entity is addressed four different ways across the NIR pipeline:

- `nir::FunctionRef` — the structural reference a call site carries
  (`module_source` + bare `name` + `monomorph_info` + `method_info`).
- `FunctionRef::full_name()` — a mangled unique string _derived_ from the above,
  recomputed on demand.
- the bare `name` — used directly as a key in several optimizer maps.
- `optimize::gate::FunctionId` — a function's _index_ in `NirPackage::functions`.

Having four representations for one entity is the root smell. Three concrete
problems fall out of it:

1. Identity is a recomputed string. Resolving "which function does this call
   target" means materializing `full_name()` (for a method, `to_mangled_name()`;
   otherwise a `format!`) and hashing it — `wir_build/calls.rs` does this at
   codegen, and every whole-program optimizer analysis
   (`alias::CallImmutability`, `alias::first_param_types`,
   `const_folding`'s callee map, the gate's call graph) rebuilds a
   `(ModuleSource, String)`-keyed map per pass and re-derives the key per lookup.

2. Bare-name keying conflates distinct functions. `optimize::alias` keys
   receiver-mutation and first-param facts on the bare `name`, so two same-named
   methods on different types in one module (`String::len`, `List::len`) share a
   key. The analysis is conservative (it over-approximates mutation), so this is
   latent rather than miscompiling, but it is a correctness drift: the verdict
   for one method can be polluted by another.

3. Identity is tangled with storage position. `gate::FunctionId` _is_ the
   `Vec` index, and `dce::remove_unreachable_functions` compacts that `Vec`
   (`retain`), so the id is only stable within a single `run_optimization_passes`
   call. It cannot be stored on a node or carried across phases, and the gate
   must rebuild its call graph each run.

### Empirical finding (2026-06-28)

An attempt to key the alias analyses on `FunctionId` _while still resolving each
call's `FunctionRef` through `full_name()` at lookup time_ regressed
package-gale compilation ~40% (6.0s → 8.4s). A clean profile attributed the loss
to `FunctionRef::full_name` (1.46% self) and the per-call resolve inside
`CallImmutability::method_writes_receiver` (1.56%). The lesson is decisive:
**the win cannot come from resolving identity per lookup.** `full_name()` must be
computed once per call site, never per analysis pass. That forces the identity to
be _stored_, which forces a canonical id — this proposal.

## Decision

Introduce a single canonical function identity, `FuncId`, and resolve every call
to it exactly once.

### `FuncId` and the registry

`FuncId` is a `cranelift_entity` id (like the arena's `ExprId` / `BlockId`),
assigned once when the NIR package is finalized (after monomorphization) and
intrinsic to the function entity — **not** its storage position. `NirPackage`
owns a `FuncRegistry` mapping `FuncId → FunctionRecord` (the entity and its
metadata: `module_source`, `name`, `method_info`, `monomorph_info`). The mangled
`name` / `full_name` become _attributes_ of the record, materialized once and
looked up only when a name is actually needed (codegen, diagnostics) — never for
identity or keying.

External / builtin callees (not defined in the package) are interned into the
same id space with an `extern` marker, so a call site is always an integer.

### Call sites carry `FuncId`

`ExprKind::Call` / `ExprKind::MethodCall` reference their callee by `FuncId`. A
single resolution pass at NIR finalization stamps each call from its structural
reference (computing the canonical key once); nodes created later (inline copies,
synthesized calls) carry or are stamped with the id at creation. After that, no
analysis recomputes `full_name`.

### Identity decoupled from storage

`FuncId` is stable across DCE compaction and mid-loop function appends. Where an
analysis needs to index the function store, it goes through a `FuncId → index`
map — an integer→integer map, cheap to rebuild at the few DCE boundaries, unlike
the per-pass `full_name` maps it replaces. The gate's "index only stable within
one run" constraint and its call-graph rebuild disappear.

## Staging

Each phase is independently shippable and kept green; the structural payoff
("everything is integers") only fully lands at the end, so the phases are
sequenced to surface the perf win early (Phase 2) while deferring the widest
churn (Phase 4) to last.

1. Registry + stable `FuncId`, dual-carry. Add `FuncId` + `FuncRegistry` to
   `NirPackage`; assign ids at NIR build. Add `func_id: FuncId` to the call
   nodes _alongside_ the existing `func: FunctionRef`, stamped by a resolution
   pass at finalization and maintained for nodes created mid-optimization.
   Nothing reads `func_id` yet — green by construction.

2. Migrate optimizer analyses to `func_id`. The gate's call graph,
   `alias::CallImmutability` / `first_param_types`, and `const_folding`'s callee
   map read the stored `func_id` (integer) instead of resolving `full_name`.
   This is where the per-lookup `full_name` cost (the +40% landmine) is removed
   and the whole-program maps become integer-keyed. Also fixes the bare-name
   collision. Measure here.

3. Migrate WIR build / codegen to resolve names via the registry by `FuncId`,
   dropping the `func_ref.name` string path in `wir_build/calls.rs`.

4. Drop `FunctionRef` from the call nodes. The structural reference becomes
   registry metadata only; call nodes carry just `FuncId`. Updates the ~30
   nir-side `FunctionRef {…}` construction sites and `monomorphize`.

5. Simplify the gate. With `FuncId` stable across DCE, drop the rebuild and the
   "not a phase-stable id" caveat.

## Consequences

- Removes the four-representation smell: one identity, name as a lookup
  attribute.
- Performance: eliminates per-lookup `full_name()` and the per-pass
  `(ModuleSource, String)` map rebuilds, replacing them with integer keys. The
  gain is bounded (the fixpoint re-walk was already removed separately) but real,
  and it is the _precondition_ for any further integer-keyed whole-program
  analysis — the project only pays off when completed.
- Correctness: a single canonical id eliminates the bare-name conflation;
  identity/storage separation removes the gate's DCE-rebuild fragility.
- Blast radius: spans `lower`, `optimize`, `wir_build`, `codegen`, and
  `monomorphize`. Staged with dual-carry so every intermediate phase compiles and
  passes the suite.
- Risk: the dual-carry in Phase 1 briefly stores identity twice (the smell it
  removes), accepted as transient migration scaffolding.

## TODO

- [ ] Phase 1 — `FuncId` + `FuncRegistry` + dual-carry on call nodes.
- [ ] Phase 2 — optimizer analyses read `func_id`; measure.
- [ ] Phase 3 — WIR build / codegen resolve names via the registry.
- [ ] Phase 4 — drop `FunctionRef` from call nodes.
- [ ] Phase 5 — simplify the gate.
