# WEP: Elaborator God-Object Dismantlement — Decl Signatures, Scope, and the Body Walker

> Companion to
> [`wep-2026-05-26-elaborator-rearchitecture.md`](./wep-2026-05-26-elaborator-rearchitecture.md).
> That WEP's Phase 1 (data decomposition: `TypeSystem` / `ModuleSemantics` /
> `Reify`) and most of Phase 2 (query migration onto `TypeSystem`) are done.
> This WEP owns the rest: the design that removes the remaining God-Object
> couplings from `Elaborator` and fixes its end state. It supersedes the old
> WEP's "Remaining" list; the design here was produced from a fresh survey of
> the code (2026-07), not by extrapolating the old plan.

## Context

`Elaborator` is down to 19 fields, each with a documented home. `Reify`,
`InferCtx`, `CtrlFlowCtx`, `TypeLookup`, and the `impl TypeSystem` query
clusters already stand alone, so the decomposition pattern is proven. What
still makes `Elaborator` a God Object is no longer data. It is four couplings.

### One receiver, three roles

About 47k lines under `elaborator/` extend the same struct. Walker code
(`expr` / `stmt` / `operators` / `item` / `module` / `handlers` / `closure` /
`assert` / the `resolve_call` / `resolve_method_call` trunks — the `resolve_*`
recursion that writes `sem` and emits diagnostics) and query code
(`method_lookup` / `trait_query` residue plus the callee-signature lookups in
`call.rs` / `method_call.rs`) share one `&mut self`. Nothing but review
discipline stops a query from mutating walk state or a walker arm from
open-coding a query.

### Queries re-resolve foreign declaration ASTs on demand

Roughly 40 `loaded_modules` reads survive outside reify. Every one exists to
fetch a declaration's AST and re-resolve its signature at the use site:

| Category                                                                          | Sites                                                                                                                       | Consumed                                                             |
| --------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| Free-function signatures                                                          | `call.rs` ×8, `expr.rs` ×2                                                                                                  | type params, param types, return, `is_mut`                           |
| Impl headers + impl-method signatures                                             | `method_lookup.rs`, `method_call.rs`, `call.rs`, `handlers.rs`, `expr.rs` (~24 via `get_impl_block` and whole-module scans) | impl `ty` / type params / assoc types / trait ref; method signatures |
| Trait-decl methods                                                                | `trait_query.rs` ×3                                                                                                         | signatures + `has_body`; bodies only for default-method synthesis    |
| Effect ops / resource statics                                                     | `call.rs`, `method_call.rs` ×3                                                                                              | op signatures, `#[cm]` attrs                                         |
| Globals / data section / assoc-type bounds / type-decl collection / import scopes | `expr.rs`, `module.rs`, `type_resolution.rs`, `orchestration.rs`, `elaborator.rs`                                           | decl types, bounds, module metadata                                  |
| Param-default expressions                                                         | `call.rs`, `method_call.rs` ×2                                                                                              | `ast::Expr` clones (irreducibly AST)                                 |

No site outside reify ever reads a method body except trait default-method
synthesis. Signatures are the whole coupling.

This on-demand re-resolution is also the root of three secondary structures:

- `resolve_type` is a four-way seam — it interns through `tysys`, records
  use→def edges into `sem.bindings`, reads `annotate_ctx`, and logs — so any
  query that calls it needs all of `Elaborator`, `&mut`.
- `suppress_reference_recording` / `with_reference_recording_suppressed`
  exist only so query-time re-resolution does not record non-authoritative
  edges over the owning module's.
- `with_module_perspective` swaps 10 fields so `resolve_type` can run under a
  foreign module's import context.

It is quadratic in places: `resolve_module`'s preamble rescans every loaded
module for associated constants per module (O(modules²), with `ast::Expr`
clones), and the driver recomputes `function_return_types` /
`imported_functions` per module from ASTs.

### Hand-rolled scope save/restore

The `TypeParamScope` RAII guard exists (~35 sites), but three clusters still
save/restore by hand: the `Item::Impl` arm of `resolve_module` (manual
`trait_ctx` clone with three restore exits), the `self_type` triple in
`method_lookup.rs` (manual save around `with_module_perspective_for`
closures), and the `current_effect_params` / `current_effect_param_decls`
`mem::take` pairs. Each is a panic-unsafe restore path the guard was built to
eliminate.

### Side channels and mode flags as struct fields

`pending_method_dispatch` and `pending_operator_ast_id` carry what should be
return values / parameters. `capture_tuple_overlays` is constructor-constant
`true` (dead flag). `suppress_reference_recording` is the query-suppression
gate above. Each is per-call-frame data living at struct scope.

## Decision

Elaborate's end state is four components. The boundary between them is
enforced by types, not review:

```
TypeSystem (+ Signatures)  — pipeline-wide queries; no AST, no sem writes, no logging
ModuleSemantics            — per-module facts (unchanged)
Annotator (today: Elaborator) — the per-module walker: AST in, facts out
Reify                      — facts in, TIR out (unchanged)
```

The one new load-bearing piece is `Signatures`.

### Signatures — every declaration signature becomes a decl-pass fact

Rule: after `annotate_decls`, no phase re-resolves a declaration signature
from AST. Each signature is resolved exactly once, by the decl pass, in its
own declaration frame, and stored as `TypeId`-level facts:

- Free functions — type params, param types, return type, `is_mut`,
  param-default `ast::Expr`s.
- Impl methods — per-method canonical signatures (params / return /
  `self_kind` / `is_mut` / type params / defaults), plus the owning impl's
  `associated_types` bindings and `is_synthesize_request` flag.
- Trait-decl methods — signatures + `has_body`, and `Rc<ast::Function>` for
  default-method bodies (the walker's synthesis input).
- Effect ops, resource static methods (including resolved `#[cm]` names),
  global types, associated-type bounds, the newtype / generic-struct decl
  index, per-module data sections.

The canonical frame: a signature is resolved under its declaring module's
import scope, with `Self` bound to the impl target, impl and method type
params as `TypeParam` slots, and associated types as the impl's own bindings.
Use sites substitute into that frame — the same substitution `MethodInfo`
consumers already perform — and never re-resolve. A signature whose meaning
would depend on the use site cannot exist under this rule; if migration finds
one, that is a design bug to surface fail-loud, not a licence to re-resolve.

AST inside the digest is allowed only where the value is irreducibly AST and
the consumer is the walker or reify, never a query: param-default exprs
(resolved per call site under the callee's scope, per WEP 2026-04-11),
associated-const value exprs (already digested this way on `ModuleDecls`),
trait default-method bodies.

What this deletes, structurally:

- Every `loaded_modules` read outside reify, `get_impl_block` and its 14
  callers, and the whole-module scans in `call.rs` / `method_call.rs` /
  `expr.rs`.
- `suppress_reference_recording` and `with_reference_recording_suppressed`:
  the owning module's decl pass records the authoritative use→def edges once;
  queries no longer resolve anything, so there is nothing to suppress.
- `with_module_perspective` on the query paths. It survives only for the
  walker's callee-scope work (default-argument resolution at call sites).
- The partial digests that grew ad hoc because this one didn't exist:
  `ModuleDecls::function_return_types`, `imported_functions`,
  `generic_function_*`, `generic_method_*`,
  `precompute_generic_function_cache`, and the driver preamble that rebuilds
  the first two per module.
- `&mut self` on the dispatch-query cluster: `lookup_method_info` and its
  callees become `impl TypeSystem` operations over `(ctx, scope, ids)`.

Placement: `Signatures` is one struct, a field on `TypeSystem` (`Rc`, seeded
from the stdlib snapshot like the `all_*` tables), keyed by the declaring
node's globally-unique `AstId` with name-keyed indices layered on top.
Membership rule: "the `TypeId`-level signature of a source declaration" —
anything else is rejected. The existing flat `all_function_sigs` /
`all_effect_op_sigs` / `all_globals` / `all_associated_constants` tables move
into it, so the rule lives in one place instead of four.

`Signatures` deliberately does *not* extend `TraitEnv`. The two are built in
different phases over different alphabets: `TraitEnv::build` runs before any
decl pass and indexes *names* ("which impls exist, on what receiver, for what
trait"), then freezes behind `Arc`; signatures are `TypeId`-level and can only
exist after the decl pass has interned types. Hanging signatures off
`ImplHeader` would make `TraitEnv` a two-phase build-then-backfill structure
and cost it the immutability its consumers rely on. Two maps under the same
`(ModuleSource, AstId)` key compose just as well at the use site.

### One canonical frame implies one way to leave it

A signature's canonical frame is only enforceable if there is exactly one
operation that instantiates it. The `TypeId`-level primitive already exists
and is canonical (`TypeTable::substitute_type_params`, keyed by slot index),
but no type pairs a signature with the slots it is abstract over, so each
consumer open-codes "clone the param types, substitute each, substitute the
return". Migrating consumers onto the digest without first naming that
operation would mint one more copy of it per converted site.

So a signature is a [`DeclSig`]: the slots plus the parameter and return
types resolved against them. `DeclSig::instantiate` fills the slots
positionally and is how a use site reads one; inference, which solves *for*
the arguments, is the one consumer that reads the canonical types directly.
`MethodInfo` stops being independently computed and becomes exactly
`instantiate(impl_method_sig, receiver_args)`.

The two genuinely AST-level helpers —
`method_lookup::resolve_type_with_param_mapping` and the
`trait_query::build_type_param_mapping` that exists only to feed it — survive
only because impl-method signatures are still AST when consumed. S5 deletes
both, which makes their count the sharper completion metric: `loaded_modules`
measures what was unplugged, AST-level substitution measures what was actually
lowered.

### Scope — transient walk state with RAII-only mutation

One `Scope` struct (`elaborator/scope.rs`) absorbs `annotate_ctx` and
`default_scope_module`. Effect parameters move into `TraitContext` itself:
they are declared in a signature's `type_params` list, so they are
generic-scope state and the `TypeParamScope` guard restores them with the
rest of the context. All mutation goes through guards — `TypeParamScope`,
`with_self_type` / `with_self_type_if_known`, `with_default_scope_module`
(one shared field-restore guard behind the `with_*` helpers) — and every
manual save/restore is deleted. Enforceable by inspection: no
`mem::replace` / manual clone-restore of scope fields outside `scope.rs`.

### TypeSystem — completed query surface, and the no-logging rule

The remaining queries move: the `lookup_method_info` cluster,
trait-method-for-type, arithmetic / indexing / static-method lookups, and the
callee-signature lookups in `call.rs`. Signature shape:
`fn query(&self, ctx: &Scope, scope: &TypeLookup, …) -> …`.

Three rules define the boundary: `TypeSystem` never sees AST, never mutates
`ModuleSemantics`, never logs. Queries return data — including reason chains
(WEP 2026-06-02) — and the walker turns them into diagnostics. This is
already the pattern for the migrated `trait_query` half; it becomes the rule
for all of it.

### Annotator — the walker Elaborator honestly is

End-state shape (6 fields, from 19):

```rust
pub struct Annotator<'a, H: CompilerHost> {
    env: ElabEnv<'a, H>,   // symbols, logger, interner, invocations, entry module
    tysys: TypeSystem,     // shared handle (+ Signatures)
    sem: ModuleSemantics,  // owned; driver swaps per module
    module: ModuleCx<'a>,  // current module source + items, set at entry
    scope: Scope,          // guard-managed transient state
    infer_holes: InferHoleTable,
}
```

- `loaded_modules` leaves the struct. The walker's last AST needs are covered
  by `Signatures` (fallback-module idents, data sections, defining `AstId`s
  for `find_impl_method_ast_id`) and the `Rc`'d trait default bodies.
- Side channels become data flow: `resolve_method_call_with` returns its
  dispatch outcome; the operator's source `AstId` becomes a parameter;
  `capture_tuple_overlays` is deleted.
- `resolve_module` sheds its decl preamble (globals, imported globals,
  associated constants, effect sources, use-specifier edges, generic-cache
  precompute — all move to `annotate_decls`) and becomes the body walk it
  claims to be.
- `resolve_type` stays on the walker by design. Inside the walk it is honest:
  interning, authoritative edge recording, and diagnostics are the walker's
  job. What was wrong was queries calling it; that path is gone.
- The struct is renamed `Annotator` at the end, matching the phase names
  (`annotate` / `reify`). `elaborator/` stays as the directory and umbrella
  term, per the parent WEP.

### Driver

```
annotate_decls   — types → TraitEnv + Signatures → per-module decl facts
annotate_bodies  — ×N, the walker
liveness         — unchanged
reify            — ×N, unchanged
```

`AnnotateState` dissolves (its own doc predicts this): `tysys` and
`module_semantics` land on `Semantics`, the rest are driver locals. The
per-module construction site collapses from 19 fields (two of them
placeholders) to `Annotator::new(&env, tysys.clone(), sem)`.

### Rejected alternative

Passing a narrow "resolution context" (`type_table` + reference sink + logger

- scope) into the query layer, keeping on-demand foreign-signature
  resolution. Rejected: it re-creates the God Object as a parameter bundle. The
  suppression gate, the perspective swaps, and the per-use-site re-resolution
  cost all stay — it treats the symptom, and the query layer still cannot be
  tested without a walker. The digest removes the cause.

## Implementation

Slices land independently, each keeping `mise run test`, the WIR goldens, and
the LSP query tests green. Converted consumers read the digest via
`.expect(…)` — a missing entry is a loud panic, never a fallback to AST
re-resolution (the reify Stage-7 precedent). A completeness test asserts that
after `annotate_decls` every free function and impl method in every loaded
module has an entry, so a gap fails one deterministic test instead of
panicking at whichever use site happens to reach it first.

- [x] S1 `Scope` + guards (subsumes the parent WEP's Track B Stage E).
      Refinements: effect params live on `TraitContext` (restored by
      `TypeParamScope`); `AnnotateCtx` renamed to `Scope`.
- [x] S2 Side channels: `resolve_method_call_with` returns
      `MethodCallOutcome`; the operator dispatcher takes
      `origin: Option<AstId>`; `capture_tuple_overlays` deleted.
- [x] S3 Decl work → decl pass: `resolve_module` split into
      `annotate_module_decls` / `annotate_module_bodies`, all decl passes
      running before any body walk. Associated constants are keyed by
      canonical identity `(type-declaring module, "Type::CONST")` —
      record and lookup both canonicalize the type prefix, so the merged
      map is collision-free and needs no shadowing or namespace-alias
      registration.
- [x] S4 `Signatures` stage A: `FunctionSig` (canonical frame incl.
      resolved `with` effects, the one signature resolution per function
      in the decl pass), `all_globals`, `all_effect_op_sigs`,
      `data_sections`, `TraitEnv::assoc_type_bound_index`; per-module
      halves live on `ModuleDecls` (`clone_digests_from` is the one
      snapshot-seeding field list). Deviations: resource statics ride the
      S5 impl/static digest; the imported-globals decl loop still reads
      the source module's AST (S5, needs visibility on the globals
      digest).
- [x] S4.5 Header-only consumers → `ImplHeader`, which gained the full
      `trait_type` and `is_synthesize_request`. `get_impl_block`'s 14
      callers are down to the 3 that read method signatures — the exact
      surface S5 replaces. `impl_header` borrows through an `Arc<TraitEnv>`
      handle, not `&self`, so a header outlives the `&mut self` calls a
      lookup makes.
- [x] S4.6 One frame exit: [`DeclSig`] (`elaborator/sig.rs`) — slots,
      param types, return type — and `DeclSig::instantiate`, the single
      positional substitution. `FunctionSig` embeds it, separating the
      canonical frame from the per-declaration extras (names, `is_mut`,
      defaults, effects). `instantiate` takes the `TypeTable` rather than
      the `TypeSystem`, keeping `sig` a leaf module.
- [ ] S5 `Signatures` stage B — impl methods: per-method canonical
      signatures as `DeclSig`s keyed by the method's `AstId`, plus
      `associated_types` and `is_synthesize_request` on the impl entry; convert
      `method_lookup` / `method_call` / `trait_query` consumers;
      `MethodInfo` becomes `instantiate(sig, receiver_args)`; delete
      `get_impl_block`.
- [ ] S6 `Signatures` stage C — trait decls: method signatures + `has_body` +
      `Rc` bodies; convert `find_trait_decl_methods_with_module` /
      `find_method_in_trait_bounds`; reify reads default bodies from the
      digest; the walker drops `loaded_modules`.
- [ ] S7 Query migration: `lookup_method_info` cluster and remaining
      callee-signature queries → `impl TypeSystem (ctx, scope)`; delete
      `suppress_reference_recording` / `with_reference_recording_suppressed`;
      `with_module_perspective` shrinks to the walker's default-argument use.
- [ ] S8 Walker slim-down: `ElabEnv` / `ModuleCx` bundles; `AnnotateState`
      dissolves; the construction site collapses.
- [ ] S9 Rename `Elaborator` → `Annotator`; update `docs/compiler.md` and
      `wado-compiler/AGENTS.md`.

Ordering: S1–S3 are mutually independent and independent of S4–S6. S4.5 is
independent of everything. S4.6 gates S5. S4–S6 build one digest each and can
land per consumer category. S7 requires S4–S6, and converts one query at a
time rather than as a single cut. S8–S9 are last.

Progress metric, measured at S4's completion:

| Metric                                           | S4  | Target |
| ------------------------------------------------ | --- | ------ |
| `loaded_modules` reads outside reify / decl pass | 25  | 0      |
| AST-level type-param substitution helpers        | 2   | 0      |
| `get_impl_block` + callers                       | 15  | 0      |
| `with_module_perspective` sites                  | 16  | 1      |
| `suppress_reference_recording` sites             | 7   | 0      |
| Manual scope save/restore clusters               | 0   | 0      |
| `Elaborator` fields                              | 13  | 6      |

Compile-time is a stated benefit (one signature resolution per declaration
instead of per use site), so the baseline is captured before S5 rather than
inferred after it.

## Consequences

### Benefits

- Boundaries by type. Queries are callable — and testable — without a walker;
  a walker arm cannot open-code a foreign-AST lookup because the field is
  gone.
- Each signature is resolved once, not once per use site (the old
  `method_info_cache` was removed, so the dispatch path re-resolves today).
  Associated-const collection drops from O(N²) to O(N). Expected
  compile-time win; measure after S5.
- One recorded truth for use→def edges — the suppression machinery is deleted
  rather than maintained, removing the "query clobbers the owning module's
  edge" bug class at the root.
- The construction site, the placeholder fields, and the dead flag disappear;
  every surviving field has a membership rule.

### Trade-offs

- Eager signature resolution interns types that may never be used. Bounded by
  declaration count, not use count; joins the stdlib snapshot like the
  existing decl tables.
- Digests clone AST fragments (default exprs, trait default bodies). `Rc`
  where a body is heavy; the clones replace per-use-site clones that exist
  today.
- Diagnostic timing shifts: a broken signature errors once at its
  declaration, not at each use. Same-or-better, but golden fixtures need
  review during S4–S5.

### Risks and mitigations

- A signature whose current meaning secretly depends on use-site context
  (e.g. caller-side associated-type bindings). Surfaces as an `.expect` panic
  or a golden diff during S4–S6; the fix is to widen the canonical frame or
  add an explicit substitution input — never use-site re-resolution.
- `Signatures` becomes the new dumping ground. The membership rule is one
  sentence; reviews reject anything that is not a declaration's signature.
- Stdlib-snapshot compatibility: `Signatures` is built in the same pass and
  seeded the same way as the `all_*` tables; the snapshot round-trip tests
  cover it.

## See Also

- [`wep-2026-05-26-elaborator-rearchitecture.md`](./wep-2026-05-26-elaborator-rearchitecture.md)
  — Phase 1 / Phase 2 history and the annotate / reify contract this
  completes.
- [`wep-2026-04-11-default-arguments.md`](./wep-2026-04-11-default-arguments.md)
  — the callee-scope contract that keeps param-default exprs AST-shaped.
- [`wep-2026-06-02-diagnostic-reason-chains.md`](./wep-2026-06-02-diagnostic-reason-chains.md)
  — the data-not-diagnostics shape `TypeSystem` queries return.
