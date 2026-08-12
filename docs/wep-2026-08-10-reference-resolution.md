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

A mangled function name is the one place a name is still the currency, because
Wasm requires one. There it carries the identity instead of a spelling:
`FqTypeName` names the receiver by its declaring module and `FqTraitName` names
the trait by its. Both are constructed only from a declaration and render on
demand; neither is parsed back.

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
  3. [x] C — flip consumers to `DeclRef`, subsystem by subsystem. Done:

     - `find_trait_impl_for_type_with_args` compares identities when both
       sides have one, and the bound-enforcement choke point
       (`enforce_single_bound` / `check_and_register_bound`) carries the
       bound's. That closes #1785's unsound direction — a same-named
       foreign trait no longer satisfies a bound.
     - `ImplHeader` reads its trait's `DeclRef` off the table, and the impl
       index matches on it rather than on the header's spelling.
     - **The mangled name's trait segment is a declaration.** A method
       mangle already named its receiver by the declaring module; the trait
       half now does too, through `name::FqTraitName` — the same
       unforgeable-by-construction discipline `FqTypeName` carries. It
       replaces `LocalMethodName`'s three separate fields (`trait_name`,
       `base_trait_name`, `base_trait_module`), which could disagree, with
       one that cannot. Its constructors take a declaration: the impl
       header's site (through `Resolutions`), a `CompilerItems` entry
       (`trait_fq`), or a resolved `DeclKey`. Flipping the field's type
       enumerated every producer — ~180 of them — which is the property
       this design was chosen for. This closes #1785's remaining direction
       (an aliased bound reaches the impl that defines the method) and the
       collision where two same-named traits implemented for one receiver
       mangled to one name and one impl overwrote the other.
     - **Every bound position is a reference site.** The resolution walk
       reached a generic parameter's bounds and nothing else: `walk_item`'s
       `Item::Trait` arm visited neither `trait Sub: Super`'s supertraits
       nor `type A: Bound`'s bounds, so those sites had no entry and every
       consumer of an inherited bound fell back to resolving a spelling.
       All three positions now route through
       `AstVisitor::visit_trait_bounds`, which is one rule rather than
       three copies of one.
     - **The table ranked the prelude above a module's own declarations.**
       `resolve_name` asked `SymbolTable::lookup` first, which is imports
       _then prelude_, before the module's own declarations — the shape of
       #1298, in the table meant to end it. The layers are now ordered
       binders → explicit imports → own declarations → prelude, matching
       what the consumers derive.
     - **A qualified path in expression position is a reference site too.**
       `Trait::method(recv, …)` reaches dispatch as a substring of an
       `Ident`'s name, which no vantage owns. The path's leading segment
       already carries its own `AstId` for LSP navigation; the resolution
       walk now records it, and the UFCS dispatcher names the required
       trait from it.
     - `AssocTypeProjection` carries its bounds as `FqTraitName`, answered
       where the trait declaration wrote them. It was the last bound store
       that kept a spelling, and it kept one because the sites it needed
       were the ones the walk never reached.

       Measured on the e2e suite by making the frame fallback panic:
       4141 trait references reached it before these three fixes, 4 after,
       and those 4 are sites the table _does_ hold — with `Unresolved`,
       for a name that reaches no import, no local declaration and no
       prelude entry. A bodiless derive (`impl Deserialize for Point;`)
       may legitimately name a stdlib trait the module never `use`d, and
       only the declaration indexes can answer for it. So the rule the
       code now asserts is: **a site absent from the table is a bug in the
       walk; a site present but undeclared is the frame derivation's to
       answer.**

     - `fq_trait_name_written` is gone. Its one real caller was operator
       dispatch falling back to an auto-derived `Eq` / `Ord`, which knows
       the trait as a compiler item; `auto_derive_by_trait` now hands that
       item back, so the trait is named by its declaration.

       What still keys on a name, each a place the class survives:

     - `blanket_impls` and `trait_impl_modules` are keyed by trait name.
       `TraitImplModuleIndex`'s doc says re-keying the receiver "would
       require build-time import resolution that `TraitEnv::build` doesn't
       have plumbed through" — stage B plumbed it through, and
       `impl_target_key` already computes the receiver's `ImplTargetKey` in
       the same loop that builds this index, so the stated blocker is gone.
     - **The impl-module index answers in two receiver namespaces, and
       they are different questions.** A mangled head (`mod/Widget`) picks
       out one declaration; a declared name (`Widget`) picks out any
       declaration spelling itself that way. The index held one map and one
       lookup, while its two producers wrote different namespaces: the AST
       layer stored the heads impl blocks wrote, the synthesis layer stored
       `base_struct_name()`. A query reached exactly one layer, decided by
       which namespace it happened to speak.

       The two namespaces now get separate storage, and both layers write
       both maps from one receiver identity — `Receiver::head_key` and
       `Receiver::decl_key` off the same `ImplTargetKey` — so they cannot
       drift apart. The AST layer is derived from the impl headers'
       resolved targets rather than the heads they wrote.

       **The query takes a receiver, not a spelling.** `ImplReceiver` used
       to be `Mangled(&str) | Declared(&str)`, and every caller chose its
       own label — which is the same defect one level up: nine callers said
       `Mangled` while three of them held a bare declaration name. It now
       carries `Of(&Receiver)` (the index derives both spellings, so there
       is no namespace to get wrong), `Instantiated(&MangledName)` (a
       receiver with its type arguments, which only the mangled namespace
       can spell) and `Declared(&DeclName)` (a caller that genuinely holds
       nothing else). A bare `&str` can no longer claim to be mangled.
       Candidate lists are built by one function, so each entry's form is
       decided by what produced it.

       Three rules fell out, each of which the single-namespace index had
       hidden:

     - **A type parameter has no spelling in the declaration namespace.**
       The synthesis layer recorded core's generic `impl<T: …>` under the
       declaration key `("T", trait)`, where it answered for a user
       `struct T`. The AST layer already skipped these; the two layers
       now share the rule.
     - **A bodiless derive asks for an impl, it does not host one.**
       Type-param dispatch runs through a blanket only when the receiver
       has no per-type impl, and `impl Serialize for Config;` sat in the
       index as though it held a body — so dispatch stopped at a module
       with none and nothing downstream was instantiated.
     - **A frame's own type parameter wins over a same-named
       declaration.** The declaration indexes cannot see binders, so a
       name-only lookup inside a generic frame must stop before them.

     - **An impl header's trait is the declaration its site resolved to.**
       A fixture declaring `trait Sub { fn sub(&self) -> i32; }` and
       implementing it was rejected with "method `sub` takes 0 parameter(s)
       but `Sub` declares 1": the arity came from `core:prelude`'s arithmetic
       `Sub`. Renaming the trait made the same program compile, which is this
       class's signature.

       The arity check already compared identities; the identity was wrong,
       because the header derived it by resolving its own spelling through a
       layer that cannot see a trait. The header carries `trait_ref` — the
       table's answer for the site it wrote — and that answers now.

       The receiver half of the header answers the same way, and one rule
       keeps the two ends of a lookup together: a head that names no
       declaration gets no module. `ImplTargetKey::of_decl` is where that is
       decided, mirroring `FqTypeName::of_head`, so a definition reached
       through a written head and a lookup reached through a resolved type
       land on the same key. The tuple family is the case that needs it:
       `internal type [..T]` gets no symbol, so no vantage can name it and
       only a module-free key agrees.

     - **A namespace-qualified head writes its member's declaration name.**
       `geo::Tag` writes `Tag`; which module declares it is the reference
       site's answer, read from the `ns$member` alias the namespace import
       registers. Rendering these heads as `Unknown` filed an impl under a
       name no lookup asks for — its methods went missing with no diagnostic
       — and made coherence treat every one of them as foreign.
     - Stores that flatten a bound to its name and lose the site:
       `infer_holes`' recorded bounds, `type_param_bounds` on the struct and
       trait digests, and `BlanketImpl::bounds`.
       `find_method_in_trait_bounds` takes the bounds themselves and
       answers from the winning one's site.
     - The associated-type registries key on `tir::TraitKey`, the trait's
       declaring module and declared name, filled from the impl header's
       site and from each blanket bound's. What still asks by spelling is
       `resolve_assoc_type_qualified`, because an `AssocTypeProjection`
       records the trait as a `String`; it declines when two declarations
       sharing the name disagree rather than letting registration order
       decide.
     - The synthesis gate — `bound_driven_synth_requests` and
       `SynthesisCtx`'s `pending` / `requested` — keys on `TraitKey`. The
       traits that drive synthesis are all compiler items, so
       `OnBoundTrait::compiler_item` reads the declaration off the registry
       instead of the bound's spelling.

-
  4. [x] D — delete what the table replaces. Done: `declaring_side_decl_key`,
         `canonical_decl_key_with`, `decl_identity_core`,
         `trait_decl_key_in_frame`, `declared_trait_name`,
         `find_trait_decl_key`, `find_effect_or_resource_decl_key`,
         `find_static_method_decl_key`, `declaring_side_key`, `WrittenHead` and
         its `spelling_pending_migration` escape, and
         `NamedType::source_interface`.

     Still standing: `DeclKey = (ModuleSource, String)`, and
     `canonical_decl_key` — which now runs the table's own lookup, so it is
     no longer a second derivation, but it is still a receiver-side API
     reached by name. The second row of the measurements table is its call
     graph, and flipping that parameter's type is what will enumerate the
     rest.

### The measurements, as they stand

The two counts the Context section opened with, plus the debt this migration
has itself created:

|                                             | at the start | now |
| ------------------------------------------- | ------------ | --- |
| `trait_name: &str` parameters               | 114          | 67  |
| `struct_name` / `type_name: &str`           | 93           | 92  |
| `.base_name()` — an identity flattened back | 0            | 26  |

The receiver half has not moved as a count, but the flip has now been run where
it decides an answer: `ImplReceiver` takes a receiver identity, and the
compiler enumerated all twelve call sites. The second row is the rest of the
receiver surface — parameters that still travel as names because the API they
call has not been flipped — and it is the honest measure of how much of this
design is unbuilt.

`.base_name()` is the shape of the remaining compromise: a caller holds an
identity and flattens it to a name because the API it calls has not been
flipped yet. Each one is a place the class survives, and the count rose
because flipping `AssocTypeProjection`'s bounds to identities put four more
callers in that position. It should reach zero.

### What splitting the namespaces surfaced

Separating the two receiver namespaces turned a set of latent defects into
failures, because each had been covered by a spelling coincidence. Two are
older than this WEP:

- **An identity flattened back to a name, then resolved again.**
  `is_trait_instance_method` resolved an aliased head to its declaration and
  then looked the _declared name_ up in the call site's frame. A module
  importing `Alpha as Ay` never has `Alpha` in scope, so the second resolution
  found nothing. The bare-name index scan that used to answer it is gone, which
  is what made the round-trip visible. Every by-name trait lookup now has a
  key-taking form.
- **A value blanket's receiver reached the bound check as a reference.**
  `Serialize::serialize` takes `&self`, so the receiver arrives as `&T` while
  the blanket's parameter is the value; asking the reference for its reflection
  kind answers `None` and rejects the blanket that should have served the call.

Two were mine, and both are the same mistake in different clothes — changing
something whose correctness I had not measured:

- Ordering the layers by what the query looked like (synthesised first for a
  mangled receiver) was a hypothesis. Measuring the last green build against
  this one refuted it: there the synthesised layer held no entry for
  `(Config, Serialize)`, so the AST layer answered. Recording both namespaces
  in the synthesised layer fills that slot, and preferring it then returns a
  different module. AST-first is load-bearing and now says so.
- The candidate builder wrapped every extra spelling in `MangledName`, but
  those come from struct-instantiation keys — declaration names. So the query
  asked the fq-keyed map for a bare `Config`, which that map never holds.

That last one is worth stating as a limit of the design rather than a slip.
`ImplReceiver` stops a caller passing a bare `&str` as mangled, but
`MangledName::new` takes any string, so a caller can still promote a
declaration name by hand — which is exactly what happened. The type's doc calls
its constructors "a bounded set of entry points"; they are not bounded yet.
Closing that means `MangledName` is constructible only from
`Receiver::head_key` / `Receiver::mangle`.

**The method that worked.** Four rounds of hypothesis-and-probe found nothing:
each confirmed only where the defect was not. Building the last green commit in
a worktree and diffing its behaviour against this one found a real defect on
every attempt. Where a change lands in a subsystem with entangled behaviours,
diffing against the last known-good build beats reasoning about which behaviour
moved.

### The residual name-only callers

A consumer holding a bare name and no reference site used to resolve it
through its own chain: a symbol probe, then four bare-name scans over the
trait, effect/resource, static-method and struct-like decl indexes, then the
writing module. That chain is a second derivation beside the table, and a
second derivation is the defect generator this WEP names — it is what keyed an
impl header's trait to `core:prelude`'s arithmetic `Sub` while the program
declared its own.

The table now exposes the scope order it runs at every site
(`Resolutions::declaration_named`), and the name-only callers go through it.
One implementation, so a name-only answer and the answer its site would have
got cannot differ. That retires `canonical_decl_key_with`,
`decl_identity_core`, `trait_decl_key_in_frame`, `declared_trait_name`,
`find_trait_decl_key`, `find_effect_or_resource_decl_key`,
`find_static_method_decl_key` and `declaring_side_key`. "Is this name a
trait / an effect / a resource" is now asked of the declaration index with a
resolved key rather than scanned for by bare name.

The remaining work on this axis is per-call-site: each name-only caller that
_could_ hold a site should take one. `canonical_decl_key` is where they land
until then, and its call graph is the second row of the measurements table.

### The CM registry's source interface

`NamedType::source_interface` was a resolved fact stored on the syntax node —
a second answer beside `Resolutions`, keyed by the same `AstId`, free to
disagree. It is now an answer table on `CmInterfaceRegistry`, keyed by the
reference site. Monotone and first-writer-wins, so an already-answered
reference (a shared `core:kiln/types` record) keeps its interface.

The table is interior-mutable, because synthesis mints new reference sites
while the registry is shared as `Arc<CmInterfaceRegistry>`. That makes the
registry per-compilation: `build_from_stdlib` hands back a process-wide
`OnceLock` singleton, and a `&self` write through it would make one
compilation's answers every later one's. The compilation takes its own copy
before anything can write. The world registry stays shared — nothing writes it.

The WIT importer is the only pass that knows a component reference's precise
owning interface, so it answers into a batch that rides `LoadResult` to the
registry rather than being re-derived from the binding module's own interface
FQ. Those answers have to land _before_ the component's decls are registered:
registration resolves a parameter's newtype through the interface its
reference names.
