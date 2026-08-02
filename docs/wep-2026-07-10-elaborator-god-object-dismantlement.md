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

Placement: `Signatures` is one struct (`elaborator/sig.rs`, next to the
`DeclSig` / `MethodSig` shapes it stores), a field on `TypeSystem` (`Rc`,
assembled once from the per-module `ModuleDecls` digests between the decl and
body passes), keyed by the declaring node's globally-unique `AstId` with
name-keyed indices layered on top.

Membership rule: one entry per source declaration, holding what that
declaration says — its signature, or the declaration-level datum it is.
Nothing computed from a use site, and nothing a later phase recomputes. AST
survives inside an entry only where the value is irreducibly AST and the
consumer is the walker or reify, never a query.

`Signatures` deliberately does _not_ extend `TraitEnv`. The two are built in
different phases over different alphabets: `TraitEnv::build` runs before any
decl pass and indexes _names_ ("which impls exist, on what receiver, for what
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
positionally and is how a use site reads one; inference, which solves _for_
the arguments, is the one consumer that reads the canonical types directly.
`MethodInfo` stops being independently computed and becomes exactly
`instantiate(impl_method_sig, receiver_args)`.

The two genuinely AST-level helpers are
`method_lookup::resolve_type_with_param_mapping` and the
`trait_query::build_type_param_mapping` that exists only to feed it. Their
count is the sharper completion metric: `loaded_modules` measures what was
unplugged, AST-level substitution measures what was actually lowered.

Of their nine call sites only one resolved a method's parameter type; the
other eight resolved an impl block's **associated-type bindings**
(`type Item = …`) and the type arguments of its trait reference. Those are
declaration facts too, so they became the impl's own digest entry
([`ImplSig`], S5c) rather than method signatures, and both helpers are gone.

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
re-resolution (the reify Stage-7 precedent). The impl-method digest needs no separate
completeness test: the body walk visits every impl block in every module and
`.expect`s the entry, so the suite already fails deterministically at the
declaration rather than at whichever use site reaches it first.

- [x] S1 `Scope` + guards (subsumes the parent WEP's Track B Stage E).
- [x] S2 Side channels: `resolve_method_call_with` returns
      `MethodCallOutcome`; the operator dispatcher takes `origin`;
      `capture_tuple_overlays` deleted.
- [x] S3 Decl work → decl pass: `resolve_module` split into
      `annotate_module_decls` / `annotate_module_bodies`, every decl pass
      running before any body walk.
- [x] S4 Signatures stage A: `FunctionSig`, globals, effect-op signatures,
      data sections. `clone_digests_from` is the one snapshot-seeding field
      list, and `Signatures` is the one program-wide struct they assemble
      into. Associated-type _bounds_ stayed on `TraitEnv`: they are
      `ast::TraitBound`s indexed by name, which is `TraitEnv`'s alphabet,
      not a resolved declaration fact.
- [x] S4.5 Header-only consumers → `ImplHeader`. `impl_header` borrows
      through an `Arc<TraitEnv>` handle, so a header outlives the
      `&mut self` calls a lookup makes.
- [x] S4.6 One frame exit: `DeclSig` (`elaborator/sig.rs`) and
      `DeclSig::instantiate`, the single positional substitution.
- [x] S5a Impl-method signatures resolved in the decl pass, keyed by the
      method's `AstId`. `enter_impl_method_frame` is the one definition of
      the frame and its slot numbering.

      Trait default methods are excluded *in the type*: `resolve_method`
      takes the recorded signature as an argument and the synthesis path
      passes `None`. The same trait method `AstId` resolves in a different
      frame for every impl that inherits it, so a declaration-keyed digest
      cannot represent it — that is S6's job, not a licence to re-resolve.
- [x] S5b-1/2 Dispatch instantiates the recorded signature and
      `get_impl_block` is deleted, so it cannot reach an impl AST.
      `instantiate_slots` fills slots by index because generic, ref,
      blanket and variadic-tuple impls number them differently and a
      positional list cannot express a non-contiguous subset.
- [x] S5b-3 `StaticMethodEntry` carries the method's own `AstId`; four of
      five consumers collapse.
- [x] S5b-4 Resource operations move to the decl pass and merge across
      modules. Their signatures were already digested as `effect_ops`;
      building a second digest would have added the duplicate mechanism
      this WEP exists to remove.
- [x] S5b-5 The canonically-keyed current-module scans are removed. The
      rest are not redundant: `impl_index` was keyed by bare name, so the
      scan _was_ the disambiguator.
- [x] S5d Identity convergence. `impl_index` is re-keyed by
      `ImplTargetKey`; `canonical_decl_key` is the single place a module is
      decided; a blanket impl's bare type parameter gets its own variant
      instead of a fabricated `DeclKey`. Builtin types are declared in
      `primitive.wado` / `array.wado` / `types.wado` and `type_decl_key`
      delegates to the name path rather than declining.

      The measured shape of the defect: three functions answered "which
      type is this name?" and disagreed. `is_primitive_type_name` listed 12
      of the 15 names `resolve_named_type` treats as builtin, so
      `impl Inspect for v128` in `core:simd` keyed to `core:simd` while
      lookups asked `core:prelude/primitive`.

      Adding `module_source` to `ResolvedType::Primitive`/`Unit` was
      considered and rejected: it adds a *second* place that decides a
      builtin's module. Instrumenting the fallback showed the fix was four
      lines of delegation, not ~200 match sites.

      Making `type_decl_key` answer for builtins exposed a latent bug in
      operator dispatch: it peeled the receiver to its newtype base and
      keyed the call off that. While a primitive base had no key the peel
      was masked by a by-name fallback; once it had one, `impl Add for
      Counter` on `type Counter = i32` routed the call to
      `core:prelude/primitive` while the body stayed in the newtype's
      module. Dispatch now keys off the chain link the impl was found on
      (`Elaborator::impl_target_decl_key`), which is what "the module that
      owns this impl" always meant.
- [x] S5e Numbering convergence. A method's own type parameters start past
      the impl's slots, and five places derived where that is. They
      disagreed whenever a concrete argument sat among the free ones.
      `TypeSystem::is_impl_target_param` is now the one predicate for which
      target arguments are slots; associated-type bindings resolve in the
      method's own frame rather than one scope out; `MethodInfo` carries
      the offset the digest used instead of letting consumers count
      receiver arguments.
- [x] S5b-6a One `MethodSig` for both kinds of method declaration.
      `resolve_effect_ops` already resolved an `interface` / `resource`
      operation in its canonical frame — the declaration's type parameters
      in slots 0.., `Self` built over them, the receiver synthesised as
      parameter 0 — but recorded only a `TirEffectOp`, which drops
      `self_kind`, per-parameter defaults, and the method's own `AstId`.
      Dispatch therefore re-resolved the same declaration from AST under a
      scope fabricated from the receiver's arguments.

      Now the one resolution records both: `MethodSig` (grown a `cm_name`
      and an `is_async`) keyed by the operation's `AstId`, plus the
      `TirEffectOp` the effect system reads. `impl_method_sigs` becomes
      `method_sigs` because both kinds live in it, and
      `resource_method_ids` is the name-keyed index over it for callers
      holding an operation name.

      The `has_self` check the old path used — does any parameter's written
      type name `Self` or the resource — is gone with it. The parser
      rejects `self: T`, so a receiver is always `self` / `&self` /
      `&mut self` and `self_kind` is the whole answer.
- [x] S5b-6b `StaticMethodSig`, `find_static_method_def` and
      `find_resource_decl` are deleted: a static-method lookup goes
      index → `MethodSig` for both kinds of declaration.

      A call site spells the declaring block's type arguments and the
      method's own separately (`Type<A>::method<B>()`), so `MethodSig`
      records `declaring_slot_count` — where the first list stops and the
      second begins — and `instantiate_call` is the one operation that
      fills both. It fills each slot by the slot's *own* index rather than
      by position, because a generic, `&`-target, blanket or
      variadic-tuple impl numbers its slots differently and a
      partially-concrete target leaves gaps.

      Inference reads the canonical types directly, as the WEP reserves for
      it: `infer_static_method_type_args` no longer rebuilds the slot scope
      and re-resolves the parameter AST inside it, it feeds
      `decl.param_types` to `InferCtx` and splits the solution at
      `declaring_slot_count`.

      `StaticMethodEntry` loses `module` and `impl_id`. They existed to
      re-resolve the method under the impl's module; the signature already
      is that resolution, so the method's own `AstId` is the whole entry.
- [x] S5b-6c Inherent-method dispatch instantiates the recorded signature.
      Its two branches — receiver module known, and the prelude case where
      it is not — each fetched the impl AST, rebuilt the block's slot scope
      from the receiver's type arguments, seeded the method's own slots
      after it, and re-resolved the signature under the impl module's
      perspective. They now share `inherent_method_info`, which reads the
      digest, and `inherent_impl_applies`, which asks `ImplHeader` the two
      applicability questions.

      With that, `method_lookup.rs` holds no `loaded_modules` read at all —
      the file that was the God Object's whole reason for carrying the
      module map.

      The branches also numbered the method's own slots from
      `receiver_type_args.len()`. `MethodSig` numbers them from
      `method_param_offset`, the single predicate S5e converged on, which
      differs precisely when a concrete argument sits among the free ones.

      The `MethodInfo` producers that remain are not signature lookups: the
      tuple builtins (`len` / `zip`) synthesise a signature no declaration
      has, and the trait-bound path is S6's.

**What the remaining `loaded_modules` reads are waiting on.** The
trait-bound path (`find_method_in_trait_bounds`) is the one place a
declaration-keyed digest genuinely cannot answer alone, and it splits three
ways:

- The trait method's `DeclSig`, with `Self` as slot 0 and the method's own
  parameters after it. A declaration fact — S6.
- The associated-type projections' `assoc_type_bindings`, computed from the
  _caller's_ where clause (`I: IntoIterator<Item = u8>` gives
  `[("Item", u8)]`). Use-site data, so it becomes an explicit substitution
  input, never a re-resolution.
- The `ast::TraitBound` lists those projections are built from. Declaration
  facts, but name-keyed and AST-shaped — `TraitEnv`'s alphabet, where
  `assoc_type_bound_index` already keeps them.

Binding `Self` to a slot needs one fix first:
`TypeTable::substitute_type_params` returns an `AssocTypeProjection`
unchanged when the substituted base is still a type parameter, so
`Self::Item` with `Self := I` stays `Self::Item` instead of becoming
`I::Item`. It must re-intern the projection over the new base.

- [x] S5c `ImplSig` — a block's target and trait type arguments and its
      associated-type bindings, resolved once in the block's own frame and
      keyed by the block's `AstId`. `ImplSig::instantiate` is its one frame
      exit, mirroring `DeclSig::instantiate`.
      `resolve_type_with_param_mapping` / `build_type_param_mapping` are
      deleted, so no query resolves an impl's AST.

      `enter_impl_frame` splits out of `enter_impl_method_frame`: the impl's
      slot numbering had no name of its own, and the bindings are numbered
      against it, not against any one method's frame.

      Two things fell out of removing the name-keyed mapping. A binding
      naming `Self` needs no substitution key — the impl frame resolved
      `Self` to the target, so instantiation yields the receiver. And the
      mapping's "no declared params, so treat every `Named` argument as a
      parameter" fallback is gone: whether a target position is a slot is
      decided once, by the decl pass.

      `is_synthesize_request` stays on `ImplHeader`. It is a syntactic
      property of the block, readable before any type is interned, which is
      exactly `TraitEnv`'s alphabet. An `ImplSig` *is* recorded for such a
      block, so the digest is total over `impl_headers` and dispatch reads
      it with `.expect` rather than a fallback.
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
independent of everything. S4.6 gates S5. S5a–S5c build one digest each and
can land per consumer category, but S5b follows S5a directly: between them
both passes resolve method parameter types, and that duplication is exactly
what the digest exists to remove. S7 requires S4–S6, and converts one query at
a time rather than as a single cut. S8–S9 are last.

Progress metric:

| Metric                                           | S4 | S5b-6 | Target |
| ------------------------------------------------ | -- | ----- | ------ |
| `loaded_modules` reads outside reify / decl pass | 25 | 8     | 0      |
| AST-level type-param substitution helpers        | 2  | 0     | 0      |
| `get_impl_block` + callers                       | 15 | 0     | 0      |
| `with_module_perspective` call sites             | 16 | 5     | 1      |
| `suppress_reference_recording` call sites        | 7  | 2     | 0      |
| Manual scope save/restore clusters               | 0  | 0     | 0      |
| `Elaborator` fields                              | 13 | 13    | 6      |

The eight that remain are three `method_call.rs` name-scan fallbacks, the
three trait-declaration sites S6 takes, `expr.rs`'s scan for the module
declaring a `From` impl, and `find_impl_method_ast_id`.

## Consequences

### Benefits

- Boundaries by type. Queries are callable — and testable — without a walker;
  a walker arm cannot open-code a foreign-AST lookup because the field is
  gone.
- Each signature is resolved once, not once per use site (the old
  `method_info_cache` was removed, so the dispatch path re-resolves today).
  Associated-const collection drops from O(N²) to O(N).
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
