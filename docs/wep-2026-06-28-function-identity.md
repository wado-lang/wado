# Canonical Function Identity (`FuncId`)

## Context

One function entity is addressed four ways: `nir::FunctionRef` (the structural
call-site reference), its derived `full_name()` mangled string, the bare `name`,
and `optimize::gate::FunctionId` (a `NirPackage::functions` index). Four
representations for one entity is the root smell, and three problems fall out:

1. Identity is a recomputed string. Resolving a call's target means
   materializing `full_name()` and hashing it — `wir_build/calls.rs` does this at
   codegen, and every whole-program optimizer analysis (`alias::CallImmutability`,
   `alias::first_param_types`, `const_folding`'s callee map, the gate's call
   graph) rebuilds a `(ModuleSource, String)`-keyed map per pass and re-derives
   the key per lookup.
2. Bare-name keying conflates functions. `optimize::alias` keys receiver-mutation
   facts on the bare `name`, so two same-named methods on different types in one
   module (`String::len`, `List::len`) share a key. The analysis is conservative,
   so this is latent rather than miscompiling, but it is a correctness drift.
3. Identity is tangled with storage. `gate::FunctionId` _is_ the `Vec` index, and
   `dce` compacts that `Vec`, so the id is stable only within one
   `run_optimization_passes` call — it cannot be stored on a node.

Empirical finding (2026-06-28): keying the alias analyses on `FunctionId` _while
still resolving each call through `full_name()` at lookup time_ regressed
package-gale ~40% (6.0s → 8.4s), the cost landing in `FunctionRef::full_name`.
The lesson: `full_name()` must be computed once per call site, never per lookup.
That forces the identity to be **stored**, which forces a canonical id.

## Decision — the end state

Functions become an entity arena, exactly like the expression arena. `FuncId` is
a `cranelift_entity` id (like `ExprId`), minted in `lower` over the
post-monomorphization function set and **permanent**: the function store is a
`PrimaryMap<FuncId, NirFunction>`, append-only, and `dce` marks a function dead
with a liveness bit rather than removing/renumbering it. So a `FuncId` never
moves — it can be stored on a node and carried across every phase.

A call node carries its callee as a `FuncId`; there is no `FunctionRef` on the
call. `lower` stamps it at construction ("born resolved"), reusing the callee
identity it already holds — `full_name()` is materialized once, in `lower`, and
never recomputed downstream. The mangled `name` lives only in the function's
arena record, read solely when a name is emitted (codegen, diagnostics). Externs
/ builtins are interned into the same `FuncId` space (with an `extern` marker),
so a call site is always an integer.

There is no `FuncId → index` map, ever. `FuncId` is the arena key: `store[id]` is
direct, and any per-function fact is a `SecondaryMap<FuncId, T>` (sparse-tolerant,
sized to the id space). Analyses iterate the store — which yields `(FuncId, &fn)`
pairs — and key everything by `FuncId`; callees come straight off the call node's
id. Nothing is keyed by storage position, so there is nothing to remap when a
function dies. This is the same entity-id + `SecondaryMap` discipline the arena
and `cranelift` already use; the only state of record is the arena itself.

The gate's dirty set becomes `SecondaryMap<FuncId, _>`; its "index stable only
within one run" constraint and its per-run call-graph rebuild disappear.

## Staging

Toward the end state without throwaway: every step adds code that survives to the
end; the only removals (Phase 4–5) delete the _old_ `FunctionRef` / `Vec` store,
never code a prior phase introduced.

1. Mint `FuncId` in `lower` and stamp it onto the call node (the permanent home),
   alongside the existing `FunctionRef` (kept only until codegen migrates).
   `NirFunction` carries its `FuncId`. Unread — green by construction.
2. Optimizer analyses read the call node's `FuncId` and key facts by
   `SecondaryMap<FuncId, _>`. Removes the +40% per-lookup `full_name`, fixes the
   bare-name collision. Measure here.
3. WIR build / codegen resolve names by `FuncId` from the function record,
   dropping the `func_ref.name` path in `wir_build/calls.rs`.
4. Make the store stable under `dce`: mark a function dead with a liveness bit
   instead of `retain`-removing it, so `FuncId` _is_ the store position for the
   whole pipeline (`store[id]` is valid post-`dce`). Fold the gate onto `FuncId`.
   The position/id duality is gone. **Prerequisite for Phase 5** — see below.
5. Drop `FunctionRef` from the call node; intern externs so the callee `FuncId`
   is non-optional. Codegen reads the callee descriptor (`module_source`, `name`,
   `monomorph_info`, `method_info`) by `store[id]` instead of off the node.
   Updates the ~30 nir-side construction sites and `monomorphize`.

Staging-order finding (2026-06-28): Phase 5's `FunctionRef` drop requires reading
the callee descriptor by `FuncId` at codegen, i.e. `store[id]`. Today `dce`'s
`remove_unreachable_functions` `retain`s by position, renumbering the `Vec`, so
post-`dce` `FuncId != position` and `store[id]` is invalid — only the
`funcid_map` rebuilt from the stored `func.id` resolves a call. Hence the
liveness-bit `dce` (originally Phase 5) must precede the `FunctionRef` drop
(originally Phase 4); the two were swapped. The alternative — a parallel
`SecondaryMap<FuncId, FunctionRef>` descriptor maintained across optimizer
mutations — was rejected: it duplicates the arena record and risks drift, against
"the name lives only in the arena record."

## Consequences

- One identity; name demoted to an arena-record attribute. The
  four-representation smell is gone.
- Performance: per-lookup `full_name()` and per-pass `(ModuleSource, String)` map
  rebuilds become integer `SecondaryMap` keys; the store needs no remap on `dce`.
- Correctness: a single canonical id removes the bare-name conflation; identity
  no longer rides on storage position, so the gate's DCE-rebuild fragility is
  designed out. A `lower`-time `assert` guards `full_name` uniqueness (the
  load-bearing minting invariant; the check is O(1)).
- Blast radius spans `lower`, `optimize`, `wir_build`, `codegen`, `monomorphize`,
  and the function store. Staged so each phase compiles and passes the suite.

## TODO

- [x] Phase 1 — mint `FuncId` in `lower`; stamp the call node; `NirFunction.id`.
- [x] Phase 2 — analyses read the call-node `FuncId`, keyed by `SecondaryMap`.
- [x] Phase 3 — codegen resolves in-package calls by `FuncId` (`funcid_map`).
- [x] Phase 4a — liveness-bit `dce` (no renumber): `FuncId == position` holds
      end-to-end, asserted at the `wir_build` entry.
- [ ] Phase 4b — fold the gate onto `FuncId` (drop `gate::FunctionId` = index).
- [ ] Phase 5 — drop `FunctionRef` from call nodes; intern externs; codegen reads
      the callee descriptor by `store[id]`.
