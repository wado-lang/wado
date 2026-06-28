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

## Decision

Introduce `FuncId`, a `cranelift_entity` id (like the arena's `ExprId`), as the
one canonical function identity. It is **minted in `lower`** — the phase that
creates every NIR function and call node — over the post-monomorphization
function set, and is intrinsic to the entity, not its storage position.

Calls are **born resolved**. `lower` already holds each callee's identity when it
builds a call node, so it stamps the node's `FuncId` at construction. There is no
separate resolution pass and no `full_name()` recomputation downstream; the
"resolve once" happens at the earliest possible point, reusing work `lower`
already does. Concretely `lower` runs in two sub-steps: (i) assign a `FuncId` to
each function and build the canonical `key → FuncId` map; (ii) translate bodies,
stamping each call with its callee's id.

`NirPackage` owns a `FuncRegistry` (`FuncId ↔ function`, plus the resolver),
populated by `lower`. The mangled `name` becomes a registry _attribute_, looked
up only when a name is actually emitted (codegen, diagnostics) — never for
identity or keying. External / builtin callees are interned into the same id
space with an `extern` marker, so a call site is always an integer.

`FuncId` is stable across `dce` compaction and mid-loop appends. Functions the
optimizer adds (`value_copy_demote`) draw fresh ids from the registry; copied
calls (`inline`) carry their id; retargeted calls (`container_sroa`) are stamped
by the pass that rewrites them. Indexing the store goes through a `FuncId → index`
map — integer→integer, cheap to rebuild at the few `dce` boundaries. The gate's
"index stable only within one run" constraint and its call-graph rebuild
disappear.

## Staging

Dual-carry keeps every phase green; the payoff lands when complete.

1. `FuncId` + `FuncRegistry` on `NirPackage`, minted and stamped in `lower`,
   carried on call nodes _alongside_ the existing `FunctionRef`. Unread — green by
   construction.
2. Optimizer analyses (gate call graph, `alias`, `const_folding` callee map) read
   the stamped `func_id` instead of resolving `full_name`. Removes the +40%
   per-lookup cost, integer-keys the whole-program maps, fixes the bare-name
   collision. Measure here.
3. WIR build / codegen resolve names via the registry by `FuncId`, dropping the
   `func_ref.name` path in `wir_build/calls.rs`.
4. Drop `FunctionRef` from call nodes — it becomes registry metadata; `lower`
   stops materializing call-site name strings. Updates the ~30 nir-side
   construction sites and `monomorphize`.
5. Simplify the gate: with `FuncId` DCE-stable, drop the rebuild and the
   "not phase-stable" caveat.

## Consequences

- One identity; name demoted to a lookup attribute. The four-representation smell
  is gone.
- Performance: eliminates per-lookup `full_name()` and per-pass
  `(ModuleSource, String)` map rebuilds in favour of integer keys. Bounded but
  real, and the precondition for any further integer-keyed analysis — the project
  only pays off complete.
- Correctness: a single canonical id removes the bare-name conflation;
  identity/storage separation removes the gate's DCE-rebuild fragility.
- Blast radius spans `lower`, `optimize`, `wir_build`, `codegen`, `monomorphize`.
  Phase 1 transiently stores identity twice (dual-carry scaffolding).

## TODO

- [ ] Phase 1 — `FuncId` + `FuncRegistry`, minted and stamped in `lower`,
      dual-carried on call nodes.
- [ ] Phase 2 — optimizer analyses read `func_id`; measure.
- [ ] Phase 3 — WIR build / codegen resolve names via the registry.
- [ ] Phase 4 — drop `FunctionRef` from call nodes.
- [ ] Phase 5 — simplify the gate.
