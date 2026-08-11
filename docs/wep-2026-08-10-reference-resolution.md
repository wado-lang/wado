# WEP 2026-08-10: Reference resolution — one answer per reference site

## Context

A type name in Wado source is module-relative. `Greet` written in `entry.wado`
and `Greet` written in `sub/other.wado` are two declarations; which one a
spelling means is a fact about the module that wrote it — its `use` list, its
aliases, its own declarations, the prelude behind them.

The compiler answers that question at **consumption** time, repeatedly, from
whatever module happens to be current. Every consumer that needs an identity
re-derives one, and a consumer holding another module's syntax has the wrong
module in hand. That is the defect generator, and it has produced a steady
stream at every layer it reaches:

| issue | layer                      | symptom                                                        |
| ----- | -------------------------- | -------------------------------------------------------------- |
| #1298 | default-method synthesis   | trait resolved by global name                                  |
| #1348 | cross-module impl dispatch | keyed on a simple name                                         |
| #1769 | inherent-impl coherence    | collision bucket keyed on the written head                     |
| #1785 | trait-impl lookup          | aliased bound unsatisfiable; same-named foreign trait accepted |

`tests/fixtures/cross_module_same_name_*` is 21 fixtures, one per occurrence
found by hand.

### Why fixing sites does not converge

Two measurements say it:

- **The interfaces speak names.** 114 parameters are spelled `trait_name: &str`
  and 93 `struct_name` / `type_name: &str`. Each is a place where two frames'
  spellings can meet and compare equal. `type_implements_trait` threads
  `trait_name: &str` down a recursion guarded by `(TypeId, String)`.
- **One reference is resolved many times.** `find_trait_impl_for_type_with_args`
  compares the impl block's spelling against the query's; each was resolved (or
  not) by a different consumer, from a different module.

Constraining the _derivation_ — requiring the writing module to turn an
`ast::Type` into a head — narrows producers while consumers still accept names,
so it moves the defect rather than removing it. The currency has to change.

### What the AST already provides

Both halves of a resolution are already addressable:

- `NamedType`, `GenericType` and `NamespacedGenericType` each carry a
  globally-unique `AstId`, so every type reference site has a name to be called
  by.
- `Symbol` is keyed by the `AstId` of the **declaring** node and records that
  node's module, so every declaration already has an identity that is not a
  spelling.

`TraitBound` is the exception: `name: String`, no id. It is also where #1785
breaks — `T: Greet` has nowhere to record _which_ `Greet`, so the name is
threaded to the comparison instead. `AssocTypeBound` is the same shape.

`NamedType::source_interface: Option<String>` is the intended answer in partial
form: a resolved fact stored on the reference site — but as a string, only for
CM interfaces, and optional, so no consumer can rely on it.

## Decision

Resolve each reference site once, where it is written; make the answer the only
currency of identity; leave names to syntax and to diagnostics.

### 1. Every reference site carries an id

`TraitBound` and `AssocTypeBound` carry an `AstId`, and the invariant is: a node
that names a declaration carries one. `ast.rs`'s
`every_reference_bearing_node_carries_an_ast_id` scans its own source and fails
on a name-bearing node with no id unless `NAMED_WITHOUT_ID` registers it with
the reason it needs none — an attribute name, a WIT interface id, a field of an
already-known struct type, or a `use` item, which builds the module scope rather
than consulting it.

The ids must also be reachable: `walk_generic_params` descends into each
binder's bounds, so an id-collecting walk sees every reference site rather than
stopping at the binder.

### 2. One pass, one answer: `Resolutions: AstId -> DeclRef`

`SymbolTable` is already most of the producer. It keys every declaration by its
declaring node's `AstId`, and its per-module `imports` map already sends a
module's local — possibly aliased — name to that `AstId`. So an imported name's
`DeclRef` is a lookup, not a re-derivation, and the alias handling that
`ModuleImportScope::original_names` does by string is already done there.

```rust
pub enum DeclRef {
    /// The declaring node's `AstId` — the key `SymbolTable` is already built on.
    Decl(AstId),
    /// A type parameter of an enclosing item.
    Binder(BinderId),
    /// A shape no module declares: a primitive, `()`, a tuple, `&`, `fn`.
    Builtin(BuiltinTy),
    /// Names nothing. Diagnosed here, once.
    Unresolved,
}
```

A declaration's identity is the `AstId` of its declaration site, so there is no
new id space to intern, no `(ModuleSource, String)` pair, and nothing to render
or parse. Equality is `AstId == AstId`.

`BuiltinShape` covers only the shapes written without naming anything — a tuple,
a reference, a function type, a pack, a placeholder. The named ones are not
special: `i32`, `()` and `!` are `internal type` declarations in
`core:prelude/primitive.wado`, so they resolve through the same layers as
`List`. One rule, rather than a primitive short-circuit beside it.

The pass runs after module loading and before elaboration, walking each module
with that module's scope. It is the only place a name becomes an identity, and
it holds the site's module by construction.

The scope is layered and ordered, and the order is the design rather than a
lookup's incidental fallbacks: the enclosing item's binders, then the builtin
shapes, then the module's explicit imports, then its own declarations, then the
prelude — including the prelude's implementation modules, so a compiler item
declared `internal` there (`ReflectStruct`, `Member`, `Ref`) still resolves for
the module that writes its name and can be diagnosed as sealed. A name reaching
none of the layers is `Unresolved`.

A synthesized reference — the `Self: <this trait>` bound the elaborator mints
for a trait's own body — knows its referent, so it is recorded directly rather
than spelled and re-resolved.

This is what removes the declaring-side / call-site duality. A reference has
exactly one vantage — the module it is written in. Two resolution rules exist
today only because resolution is deferred to consumers, who hold their own
module always and the site's module sometimes. Resolve at the site and the
second rule has nothing left to answer.

### 3. `DeclRef` is the currency

Every query that takes a name in order to decide identity takes a `DeclRef`
instead: `type_implements_trait`, `find_trait_impl_for_type_with_args`,
`locate_static_method_impl`, the impl indexes, the CM interface registry. The
recursion guard becomes `Vec<(TypeId, DeclRef)>`.

Names survive in exactly two places — the AST, which is syntax, and diagnostics,
through `Resolutions::display(id) -> impl Display`, a renderer rather than a
comparable value.

Flipping a parameter's type is what makes the work enumerable: the compiler
lists every caller that still holds a name. That is the property this design
needs and the reason it terminates.

### 4. The digest reads the table

`ImplHeader` — the digest `TraitEnv::build` makes of every `impl` block —
carries its target's and its trait's `DeclRef`, read from `Resolutions` rather
than resolved a second time, along with the vantage and each method's spans and
arity. Every whole-program check (inherent-impl collisions, orphan rules,
variadic overlap, sealed traits, trait-method arity) keys on those identities
and never walks `loaded_modules`: a second walk is how a pass ends up holding
syntax whose module it does not know.

## Consequences

The class stops being writable, for four separate reasons:

- **The question cannot be asked wrongly.** Resolving needs a reference site's
  `AstId`, and the site determines its module. There is no `(name, guessed
  frame)` entry point left to call.
- **The answer cannot be compared wrongly.** `DeclRef` equality is declaration
  identity, and there is no spelling on it to compare instead.
- **No case can be folded away.** `Unresolved` is its own answer, so "reaches no
  declaration" is never read as "a binder" — the two are distinct questions with
  distinct diagnostics.
- **A regression cannot land quietly.** A new reference-bearing node without an
  id fails the grammar test; a new query typed on names has nothing to hand it.

Costs and risks:

- Stage C touches every consumer of trait and type identity. The count above is
  the size: ~200 parameters, plus the maps and guards keyed alongside them.
- The pass must agree with today's answers before anything depends on it. Stage
  B is therefore shadow-only — computed, compared against the current per-site
  answers under `WADO_RESOLVE_SHADOW`, differences reported — so the blast
  radius is measured rather than discovered. Across all 2127 fixtures the
  impl-header comparison disagrees exactly once, on
  `inherent_impl_undeclared_type.wado`: the table answers `Unresolved` where the
  consumer answers `TypeParam`, which is the conflation stage C removes.
- `Resolutions` is a whole-program table built before elaboration. It must be
  populated for stdlib modules on the snapshot path too, or a snapshot restore
  resolves nothing.

## Migration

-
  1. [x] A — `AstId` on `TraitBound` and `AssocTypeBound`; the grammar test that
         keeps the invariant, and `walk_generic_params` so the new sites are
         reachable.
-
  2. [x] B — the resolution pass and `Resolutions`, in shadow mode: every site
         resolved, every impl-header answer compared against what `TraitEnv`
         derives, every difference logged. Nothing reads the table yet.
-
  3. [ ] C — flip consumers to `DeclRef`, subsystem by subsystem. Done:
         `find_trait_impl_for_type_with_args` compares identities when both
         sides have one, and the bound-enforcement choke point
         (`enforce_single_bound` / `check_and_register_bound`) carries the
         bound's. That closes #1785's unsound direction — a same-named foreign
         trait no longer satisfies a bound.

         What still keys on a name, each a place the class survives:

         - `TraitEnv`'s impl indexes (`has_any_methodful_impl_by_receiver`,
           `blanket_impls`, `trait_impl_modules`) are keyed by trait name, so a
           lookup that misses the header loop falls back to a spelling. This is
           what still rejects an aliased bound's `G::hello(x)`.
         - Stores that flatten a bound to its name and lose the site:
           `infer_holes`' recorded bounds, `type_param_bounds` on the struct and
           trait digests, `BlanketImpl::bounds`.
         - A compiler item (`Ord`, `Display`, `ReflectStruct`) is asked for by
           name; it should carry the `DeclRef` of the declaration it names.
         - `locate_static_method_impl`, the conversion-impl survey, and the CM
           interface registry.
-
  4. [ ] D — delete what the table replaces: `declaring_side_decl_key`,
         `canonical_decl_key_with`, `decl_identity_core`, `WrittenHead` and its
         `spelling_pending_migration` escape, `DeclKey = (ModuleSource, String)`,
         and `NamedType::source_interface`.
