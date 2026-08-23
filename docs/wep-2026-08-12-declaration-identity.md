# WEP 2026-08-12: Declaration identity — one identity, one scope, one answer

## Context

A name in Wado source is module-relative. `Greet` written in `entry.wado` and
`Greet` written in `sub/other.wado` are two declarations; which one a spelling
means is a fact about the module that wrote it — its `use` list, its aliases, its
own declarations, the prelude behind them.

Anything that treats a spelling as the declaration gets that wrong in one
recognisable way: a program compiles or fails depending on whether two unrelated
declarations happen to share a spelling, and renaming one of them changes the
answer. That signature identifies the whole class, and these are the instances it
has been recorded under:

| issue | layer                      | symptom                                                        |
| ----- | -------------------------- | -------------------------------------------------------------- |
| #1298 | default-method synthesis   | trait resolved by global name                                  |
| #1348 | cross-module impl dispatch | keyed on a simple name                                         |
| #1769 | inherent-impl coherence    | collision bucket keyed on the written head                     |
| #1785 | trait-impl lookup          | aliased bound unsatisfiable; same-named foreign trait accepted |

`tests/fixtures/cross_module_same_name_*` holds a fixture per known occurrence.

## Decision

There is one identity for a declaration, it is not constructible from a name, and
it is the only thing any query compares. Names travel in one direction only: out
of the identity, for humans and for Wasm.

### 1. `DefId` — the one identity

Every declaration in the program gets a `DefId`: an opaque dense index into a
`DefTable` built once, after loading, from every module's items.

```rust
pub struct DefId(u32);          // private field, `crate::defs` only

pub struct DefTable { /* dense rows indexed by DefId */ }

impl DefTable {
    pub fn module(&self, def: DefId) -> &ModuleSource;
    pub fn name(&self, def: DefId) -> &str;          // a rendering, not a key
    pub fn ast_id(&self, def: DefId) -> AstId;
    pub fn kind(&self, def: DefId) -> DefKind;
    pub fn parent(&self, def: DefId) -> Option<DefId>;
    pub fn members(&self, def: DefId) -> &[DefId];
    pub fn of_ast_id(&self, id: AstId) -> Option<DefId>;
}
```

A member is a declaration too. A struct's fields, a variant's cases, a trait's
methods each get a `DefId` under their owner, so the case a pattern names and the
field a projection reads are identities rather than strings looked up against
their owner. Members the symbol table already collected — an effect or resource
method, registered there under its importable `Owner::method` name — keep that one
identity and are only linked to their owner, so nothing gets two.

The properties are in what is absent:

- No public constructor. `DefId` is minted by `DefTable::declare`, which is
  private to `crate::defs` and called only by `DefTable::build`. Rust's privacy is
  the enforcement; no lint and no test is needed to hold it.
- No `DefTable::lookup(module, name)`. **There is no function from a name to an
  identity outside the resolve pass.** This is the single rule the design rests
  on: a consumer holding only a name cannot obtain an identity, so it cannot
  compare one, so it must be given the site instead.
- No fallible-to-fabricated path. A name reaching no declaration produces
  `Resolution::Unresolved`, a value the consumer must handle — never a `DefId`
  standing for a declaration that does not exist.

Nothing else identifies a declaration. Impl target keys, trait keys, and the
heads of `FqTypeName` and `FqTraitName` all carry a `DefId`, and equality is
index equality.

A head that reaches no declaration is not given one: `ImplTargetKey` carries an
`Undeclared` case for a written name that resolves to nothing and for the
anonymous struct shapes no declaration names. It holds a spelling because there
is no identity to hold, and no query can mistake it for one.

A rendering may be _stored_ beside an identity; it may never be read back into
one. `FqTraitName`'s head is a `DeclaredHead` — the `DefId`, plus the declaring
module and the declared name its one constructor reads off the table — so a
mangle needs no table at hand, while equality and hashing compare the `DefId`
alone.

`AstId` is deliberately not reused as the identity, though the symbol table is
already keyed by the declaring node's. Two reasons: `AstId` is the id type of
_every_ node and `AstId::fresh()` is public, so a use-site id type-checks wherever
a declaration id is expected and one can be minted from nothing; and `AstId` is
sparse, so per-declaration data cannot be the dense columns §5 keys by it.

`DefId` is dense, never rendered, never serialised, never parsed. It indexes one
table, so every fact carrying one must be read against the table that minted it.
The stdlib snapshot crosses that boundary: it caches whole declaration facts, and
a compile restoring it never re-runs the decl pass for a snapshot module. So the
table is seeded rather than rebuilt — `DefTable::build_seeded` continues the
snapshot's table, keeping every declaration it already identified at its `DefId`
and minting only what it never saw, and `TypeTable` is seeded the same way. What
makes that sound is that the stdlib AST is parsed once per process and shared, so
an `AstId` means the same node in both tables. An entry module claiming a stdlib
identity (`#![stdlib("core:…")]`) is parsed from its own file instead, so the
snapshot is dropped for that compile (`stdlib_snapshot::covers`) and the closure
is elaborated from source.

The rule binds every cached declaration fact, not just this one: a `DefId` in a
`ResolvedType` or a registry key crosses the same boundary.

### 2. `Scope` — the one implementation of visibility

One type answers "what does this name mean in module M", and it is the only place
a name becomes a `DefId`.

```rust
struct Scopes { /* per-module imports, per-module own declarations, the prelude */ }

impl Scopes {
    fn resolve(&self, module: &ModuleSource, name: &str) -> Option<DefId>;
}
```

The layers are stored rather than flattened per module: the prelude is in scope
everywhere, and copying it into each module's map would cost the prelude's size
times the module count for no added answer. The binders are the walk's, since they
are scoped to the item being walked rather than to the module.

The layers are ordered, and the order is the specification rather than a lookup's
incidental fallbacks:

1. the enclosing items' type-parameter binders, innermost first;
2. the module's explicit imports, keyed by local name so an alias resolves to what
   it aliases, including the `ns$member` aliases a namespace import registers;
3. the module's own declarations, including the function-local items in scope at
   the site — an import whose local name the module also declares is rejected,
   so this layer and the one above it can never both answer and the order
   between them is unobservable;
4. the prelude — its re-exports, then its implementation modules, so an `internal`
   compiler item (`ReflectStruct`, `Member`, `Ref`) resolves for a module that
   never `use`d it and can then be diagnosed as sealed. This layer is
   unconditional, including for a module carrying `#![no_prelude]`: `i32` and
   `f64` are `internal type` declarations in `core:prelude/primitive.wado`, so
   the prelude's implementation is what makes the language's own types nameable,
   and `core:prelude/int128.wado` writing `i64::MAX` needs it. The attribute
   exempts a module from the prelude _collision check_ — it is the prelude, so
   it may declare `Option` — and never governed what a name means. A module's
   own declarations already rank above this layer, so nothing it defines can be
   shadowed by the prelude's copy of it.
5. the case names of variant / enum / flags types in scope, which a type of the
   same name always shadows.

`Scope` is private to `crate::resolve`, and nothing outside it runs the walk by
name: the scope is reached only through a reference site, and a caller holding a
spelling and no site gets the frame derivation below instead — which is not a
scope and cannot pretend to be one. No name-keyed scope accessor stands beside
it. The facts such accessors would carry that are _not_ scope — a module's
re-export list, an interface's members, which module a namespace alias stands
for — are kept, keyed by `DefId`.

What an explicit `use` means is the analyzer's answer and only its answer: it
resolves aliases and re-export chains once and records them, and every consumer
reads that record. Re-walking the `use` declarations to answer the same question
a second way makes what a name means depend on which walk a pass happened to
reach.

### 3. `Resolutions` — the one answer, total over reference sites

```rust
pub enum Resolution {
    Def(DefId),
    /// The type parameter's own node. A binder is not a declaration — it is
    /// scoped to the item that wrote it and named only from inside — so it gets
    /// no `DefId`.
    Binder(AstId),
    Unresolved,
}

pub struct Resolutions { /* AstId -> Resolution */ }

impl Resolutions {
    pub fn get(&self, site: AstId) -> Resolution;  // total, not Option
}
```

Every node that names a declaration carries an `AstId`, and the walk records an
answer for every one. A struct pattern's qualifier is such a position: naming a
type in pattern position is naming a declaration. The nodes that name something
and deliberately carry no id are the ones naming no declaration — an attribute,
a WIT interface id, a world export's own name — and the ones building the module
scope rather than consulting it (`UseItemSimple`, `UseItem::InterfaceFunctions`,
whose local names are unambiguous within one module by construction), plus
`StructPatternField`, a field of a known struct type rather than a
module-scoped name.

`get` is total. A site the walk missed is a bug in the walk, not an absent answer a
consumer improvises around, so it panics rather than returning `None`. The three
cases stay distinct on purpose: reading `Unresolved` as `Binder` loses the
diagnostic a name that reaches nothing deserves. `walked` keeps a fourth case
apart from all three — a node no walk saw, which synthesis mints — because that
is the only one for which some other source of truth is honest.

Type resolution carries the site with it: the head's `AstId` reaches
`resolve_named_type` / `resolve_generic_type`, which read the declaration off
this table rather than re-running a scope lookup from wherever the walk stands.
An alias, a namespace prefix and a function-local `struct` reach their own
declarations with no vantage supplied.

`Unresolved` is not a synonym for "error", but an `impl` header's trait position
is: implementing a trait is naming it. A header's own reference site answers that
position and only it, so every header carries a declaration and dispatch has no
spelling to fall back to.

### 4. Queries take identities, never a name beside one

Every query that decides identity takes a `DefId` and does not take the name.

```rust
fn type_implements_trait(&self, …, trait_: DefId) -> bool;
```

Four rules:

- An identity parameter is never `Option`. Optional means the caller may decline,
  and a caller that may decline does.
- An identity parameter never travels beside the name it would be compared
  against. A name in the same argument list is a fallback waiting to be written.
- A declaration is compared to a declaration, never to the spelling that reached
  it. `name(def) == written` reads as a check and behaves as a filter: it
  declines exactly when the two spellings differ, which is exactly when an import
  alias, a namespace prefix, or a local item's `@AstId` mangle is in play.
- A diagnostic reads its spelling at the point of reporting, from the site and the
  AST — never from a name threaded down for the purpose.

### 5. Declaration data is keyed by `DefId`

Declaration data is `DefId`-indexed columns on `DefTable`: fields, cases,
members, methods, type parameters, bounds, visibility, span. No registry is
keyed by a name, and no consumer walks a scope to reach one — the caller arrives
holding the `DefId` its site resolved to.

The tables a walk builds as it goes are keyed the same way. `ModuleDecls`'
`local_*` maps — the fields, cases, members and newtypes the module being
elaborated has contributed so far — are keyed by declaration, so a module-level
`struct Box` and a function-local one of that name are two entries rather than
one the later insert wins. No separate tier is needed to keep the two apart.

Reaching them takes an identity or the site that resolved to one:
`variant_cases_of` / `enum_cases_of` / `flags_members_of` / `struct_fields_of`
take the `DefId`, and `variant_cases_at` / `enum_cases_at` / `flags_members_at`
mirror `declaration_at` for a written qualifier — the `Color` of `Color::Red`,
read off its own path segment in both annotate and reify so the two cannot
disagree about which `Color`. There is no by-name form beside them.

A key whose subject may also be a shape no declaration names takes the head
rather than the declaration. `synthesis::traits::SynthRequests` — the
`(receiver, module, trait)` triples a bound-driven derivation was asked for — is
keyed by `TypeHead`: its `Declared` compares by `DefId`, its `Shape` — an
anonymous literal, a monomorphized instantiation — by its rendering, which is
all such a shape has. `SynthesisCtx::key` hands one over from `FqTypeName::head`
instead of rendering it, and `TypeTable::record_bound_driven_synth_request` takes
the same head off the receiver's own type, so the producer and the consumer
cannot key two ways.

This is what removes the consumers' need for a vantage. A pass reading a struct's
fields does not need to know which module it is standing in, so it cannot stand
in the wrong one, and `with_module_perspective_for` does not swap these tables
when it enters another module: a declaration-keyed entry answers for its
declaration from anywhere.

### 6. Types carry `DefId`

`ResolvedType`'s nominal variants carry a `DefId` in place of
`(name: String, module_source: ModuleSource)`:

```rust
Struct   { def: DefId, type_args: Vec<TypeId> },
Enum     { def: DefId },
Variant  { def: DefId },
Resource { def: DefId },
Newtype  { def: DefId, type_args: Vec<TypeId>, base_type: TypeId },
Flags    { def: DefId },
```

`TypeId` equality then means declaration equality without the interner comparing
strings, and a `ResolvedType` cannot be built for a declaration that does not
exist. `AssocTypeProjection::owning_trait` carries a `DefId` for the same reason.
`Newtype` carries the same head/arguments split `Struct` has, so the impl index
is never handed a fused spelling no `impl` header writes.

A shape no declaration names — a tuple, a reference, a function type, a pack — has
no `DefId` and needs none; each is already its own variant. Primitives are not
special: `i32`, `()` and `!` are `internal type` declarations in
`core:prelude/primitive.wado` and get `DefId`s like anything else.

An anonymous struct is such a shape and is not already its own variant. A struct
literal with no type name interns as a `Struct`, and two literals of the same
shape deliberately reach one type — so there is no declaration to identify and no
node to identify it by.

It does not become a variant of its own: an anonymous struct rides the `Struct`
path through field access, layout and codegen, and a separate variant would make
every one of those sites stop matching it silently. The head splits instead:

```rust
Struct { def: StructDef, type_args: Vec<TypeId> },

enum StructDef {
    Decl(DefId),
    /// A shape, interned by its fields. Not forgeable from a name either.
    Anon(AnonStructId),
}
```

Every site that matches `Struct` keeps matching it, and every site that reads the
head says which case it means. A shape has no synthesized spelling: its fields
are its key, filed under its `AnonStructId` beside the declarations' under their
`DefId`s, so nothing renders a spelling to store them and nothing reproduces one
to read them back.

An interned type is keyed by its rendered spelling. Holding argument `TypeId`s as
identity would mint two types where equivalent-but-distinct ids meet, and such
ids exist — a blanket instance reached from two dispatch sites is one. The head
and the arguments are separately readable; that is what carrying a `DefId` buys,
not a change of interning identity.

`TypeTable` renders every mangled name, so it holds the `DefTable` its heads
index — attached where `Resolutions` is built, and on the snapshot restore path,
whose seeded table hands back the same identities by construction.

A local item's type is distinct because its declaration is, but the mangled
namespaces downstream are name-keyed and monomorphization asserts `(module,
name)` is unique across the emitted function set. So `mangle_local_item_name`'s
`@AstId` suffix stays, as what keeps a _rendering_ injective — which every mangle
owes (§8). The direction is what matters: written at one site, read back at
none.

### 7. Synthesis records referents, it does not spell names

A pass that synthesises a reference knows what it refers to, so it records that
rather than spelling a name for someone else to resolve.

Where the referent is a declaration the walk already visited, the cheapest form
of recording it is to name that node: the `Self: <this trait>` bound a trait's
own body carries is minted with the trait declaration's own `AstId`, and the walk
answers for that node with the trait itself. No new id, no new table, and the
bound resolves like any written one.

Where no such node exists, the reference carries its referent directly:
`ast::Type` gains a `Resolved(DefId)` variant, absent from parsed syntax and
produced only by synthesis. Type resolution returns the declaration — no name to
look up, no vantage to get wrong.

### 8. Mangled names are rendered once, never parsed

Wasm needs a string, so one place produces one. `LocalMethodName` is the
structured function identity — the defining module, the receiver and its type
arguments, the trait and its type arguments, the method name and its type
arguments, every head an identity or a shape — and it renders on demand.

A mangled name is never split back apart: every question about one is a field
access on the structured identity, and no function parses a mangle. `MangledName`
is constructible only from such an identity, so a declaration name cannot be
promoted to a mangled one by hand.

`FqTypeName`, `FqTraitName`, `Receiver`, `TypeHead` and `DeclName` are the
pieces it is built from. Each keeps its own namespace honest — the mangled one,
the declaration one — and each compares by the `DefId` its head carries, so
being separate types costs nothing in identity.

Nothing a name is built from is stored as text. `FqTraitName::args` and
`LocalMethodName::method_type_args` hold `FqTypeName`s, and
`trait_env::written_type_args` builds one per argument off the argument's own
reference site — so an `impl Index<K>` header and a call site reach the same
head, and a `From<Foo>` segment names the module that declares `Foo`. There is
one renderer for a type argument: `TypeTable::mangle_type_arg_for_generic` _is_
`FqTypeName::to_mangled`, so a definition's name and a lookup's name cannot be
spelled by two functions that drift.

Two rules bind the renderer:

- A name minted for a definition and a name built to look one up must come from
  one function, or nothing makes them agree. A regression test asserts the two
  sides agree rather than pinning either one's output.
- A surviving `TypeId` must stay readable. `TypeTable::retain` closes over each
  surviving struct's `type_args` transitively, so a struct cannot survive
  spelling itself with an id that no longer resolves; and it re-enters each
  survivor under the spelling `intern` entered it by — `Box` for the declaration,
  `Box<i32>` for that instantiation. Re-indexing on the declaration name alone
  puts every instantiation on one entry, and a query documented to return
  declarations returns an instantiation: the same defect one layer down, two
  things rendering the same.

The rendered format is not itself a constraint. A mangle has to be injective and
has to agree between the site that mints a name and the site that looks one up;
what it spells is free to change, and the emitted Wasm changing with it is a
golden-fixture update, not a regression.

### 9. What names are still for

Three things, none of them comparable:

- Source syntax. The AST holds what the programmer wrote, so the formatter and the
  LSP reproduce it.
- Diagnostics. A message says what the programmer wrote, read off the site —
  except in the one case where what the programmer wrote does not separate the
  two sides: `expected 'Point', found 'Point'`, two declarations of that name.
  `TypeTable::type_names_for_mismatch` renders both plainly and qualifies each
  only when the two strings are equal, so every other message keeps its short
  form. The qualified spelling is the `MODULE#SYMBOL` notation of WEP
  2026-06-14, and comes from the same renderer the plain one does, so the two
  cannot drift.
- The Component Model boundary. An export name is an ABI fact derived from a
  `DefId`. The one direction that runs the other way is a WIT type name inside
  a generated `wasi:*` / `core:kiln/*` module, which `TypeTable::cm_decl_in`
  resolves: no Wado resolver walked that namespace, so there is no reference
  site, and `CmInterfaceRegistry` parses its own copy of those modules once per
  process, so there is no declaring node this program's `DefTable` saw either.
  `wado-from-idl` generates one module per interface and each declares a WIT
  name once, so the `(name, module)` pair names a single declaration by
  construction. Reachable from `synthesis::cm_binding` alone.

A name is never a map key, never an equality operand, and never a parameter that
decides which declaration is meant.

## Enforcement

Each mechanism states what it makes impossible, not what it discourages.

- `DefId`'s field and `DefTable::declare` are both private to `crate::defs`. A
  pass cannot mint an identity. Enforced by the module system.
- `DefTable` has no name-keyed lookup and `Scope` is private to `crate::resolve`.
  A pass cannot turn a name into an identity. Enforced by the absence of the API.
- Identity parameters are non-`Option` and are not accompanied by their own name.
  A caller without an identity does not compile. Enforced by the type checker.
- `Resolutions::get` is total and panics on a missing site, so a coverage hole in
  the walk fails on the first fixture that reaches it instead of degrading to a
  name comparison.

The list is closed by the type system and the module system, not by a test: no
mechanism above can be worked around locally, so a new violation needs a new
API, and adding one is a review decision.

### What still turns a name into a declaration

What is left, each with the reason. A declaration is whatever _identifies_ one,
so this spans both currencies: a `DefId`, and a `Symbol` row, which carries the
declaring node. Adding to the list is a design change — the alternative is
always to give the caller the reference site.

The one scope, which every other entry exists by not being:

- `Scopes::resolve`, `resolve_value`

The three recorded facts the frame derivation is built from. Each is one tier,
none is a scope, and none takes a vantage a caller could get wrong:

- `imported_as` — the import tier alone, for a caller asking about the aliasing
- `prelude_decl` — the prelude tier, which has no vantage to be given
- `decls_named` — every declaration under the name and no choice between them

The derivation itself: those tiers in order, over the walk's own frame. Each has
a sited entry point a caller with a reference site reaches instead.

- `decl_key_or_local`, `TypeLookup::declaration` — for a rendered head
- `namespace_member` — the `ns$Name` alias a namespace import registers
- `scoped_trait_decl_key` — filtered to the trait index, for a bound's spelling
- `bound_declaring_assoc_type` — which of a _binder's_ bounds declares a name

The same derivation in the `Symbol` currency, which §5's `DefId` columns subsume:

- `symbol_named`, `imported`, `lookup_in_module`, `lookup_in_module_with_visited`

One rendering still compared against a declaration's own, which goes when the
impl index carries `DefId`s:

- `impl_target_decl_key` — a receiver's newtype chain against an impl's head

The Component Model boundary, permanent for the reason §9 gives:

- `cm_decl_in`, `cm_decl`

## The frame derivation

A name whose reference site is not at hand still has to reach a declaration — a
synthesis target, a mangled name's head. Nothing walks a module's scope for it.
Three recorded facts answer instead, in order:

1. `Resolutions::imported_as` — what this module `use`d under that local name.
   The one import fact that is not a scope lookup: it cannot reach another
   module's imports, and it answers with what an alias aliases.
2. `TraitEnv::decls_named`, filtered to the module in hand — every declaration
   written under the name, whichever module declares it. It holds what modules
   _declare_, never what they import, so no alias can steer it.
3. `Resolutions::prelude_decl` — what the prelude puts in scope under the name.
   The prelude tier alone, and it takes no vantage because it cannot be given
   one: the prelude is in scope in every module.

The three are a module's own reach, so a declaration it cannot see stays unseen
here — the derivation never widens to the whole program, and a name no module
brought into scope is unresolved, the same answer the walk gives.

Which module is "this" one is the walk's position, and the walk is not always
standing where the name was written: a parameter or field default is read at the
call site and written in the declaring module. The writing module answers first,
or a caller declaring its own same-named type takes the answer away from the
module that wrote the name — this WEP's defect class by the back door. Both
frames come from the walk's position; the derivation takes no module, so no
caller can supply a vantage.

There is no fourth tier, and a caller that can avoid the derivation does.
`Type::method` names its receiver at its own path segment, which the resolve pass
answered for like any other reference, so the site is read and the spelling is
never split back into an identity. The derivation answers only where a caller
holds a mangled spelling and has no site to give.

No tier takes a vantage it could get wrong: `decls_named` takes no module at all,
and the derivation filters it by a frame of the walk's own. That is why it cannot
be mistaken for a scope, and why it is sanctioned rather than scheduled for
removal.

### A bound means what its writer wrote

Syntax means what the frame that wrote it says. A supertrait bound reached
through `T: Derived` was written in `Derived`'s frame, so `Item = A` there is
`Derived`'s `A`. An inherited bound carries its writer, and a right-hand side
naming the writer's own parameters stays abstract rather than binding to a name
the asking frame happens to share.

Fixture: `supertrait_binding_keeps_writer_frame.wado`.

### What a derivation may not be

A derivation reaches a module's own scope and no further. Three shapes look like
more of it and are not, each answering from declarations no module involved can
see, so what a name means comes to depend on the rest of the program.

- A program-wide unique match, declining when two modules declare the name.
  Declining is not neutral: the caller's fallback is the comparison this design
  removes.
- A first-in-build-order pick — a name-keyed index with no ambiguity check.
- A second key tried when the first misses, which makes the order a silent
  tiebreak. One key, built from the receiver the caller holds: the path
  segment's own site, the middle segment of `ns::Type::method`, or the receiver
  type's declaration. A head naming none falls to the frame derivation.

Where a position reaches nothing the answer is a diagnostic, not a wider search.
An `impl` header's trait position is §3's case: implementing a trait is naming
it, so reaching nothing is "trait not in scope", and the key it gets carries a
spelling no query can mistake for an identity.

A rendering is never read back into a declaration, and a name-keyed map is the
same defect waiting for a reader.

## Trait arguments

An identity is a declaration plus the arguments it was instantiated at, and an
associated type belongs to that: `<Cm as Combine>::Out` and
`<Cm as Combine<Inch>>::Out` are two answers. Keying them by the declaration
alone let one impl overwrite the other's.

A bound is a bare name — the parser reads `<...>` after one as associated-type
bindings — so a bound always means the trait at its declared defaults. The
arguments an identity carries are therefore only the ones an impl writes beyond
those defaults, `Self` meaning the impl's target: `impl Add<Cm> for Cm` and
`impl Add for Cm` are one identity, `impl Add<Inch> for Cm` another. One
predicate decides it for both the impl's minted name and the key its associated
types register under, so §8's rendering and the registry cannot disagree.

A bound therefore selects only an impl at that instantiation, and the
associated type it pins is read from whichever registry the serving impl wrote
to — a concrete impl records a resolution, a generic one a definition to
substitute. Reading one of the two is what let a widening
`impl<T> Mul for W<T>` satisfy `Mul<Output = T>`.

Fixtures: `assoc_type_per_trait_args.wado`,
`impl_writes_default_trait_arg.wado`,
`error_bound_needs_default_instantiation.wado`,
`error_blanket_pinned_assoc_generic_impl.wado`.

## Compiler items

A trait the compiler supplies behaviour for — an operator, indexing, a literal
builder, `Eq` / `Ord`, a reflection kind — is recognised by a
`DefId → CompilerItem` map, never by a spelling in the asking scope, which
answers for a user trait sharing the name and declines where a module shadows
the prelude's. Every site deciding an operator asks it: which impls a bound
admits, which primitives supply arithmetic, which instruction a call lowers back
to, which right-hand type a literal takes.

A type that merely erases to a scalar — a newtype, `flags`, an `enum` — is its
own declaration, so an impl it writes outranks the erased form's instruction.
Only a primitive _is_ the instruction.

Fixtures: `error_user_trait_does_not_capture_add.wado`,
`error_user_trait_does_not_capture_index.wado`,
`user_trait_method_survives_relowering.wado`,
`eq_ord_manual_impl_wins.wado`.

## Impl target arguments

An `impl` header's type arguments are asked the same question twice — which
positions the header pins decides both how its methods are _named_ and which
receivers reach that name — so one predicate answers it,
`TypeSystem::impl_arg_pins_a_position`.

Reading the target is the same hazard one step earlier, and "the target's
arguments" is three questions. Consumers asking different ones shared a reading
because the questions sound alike; each now has its own.

- **Does this receiver reach this impl, and does this argument pin its
  position?** `impl_target_args` — a shape comparison, reading through a
  reference and counting a tuple's elements.
- **What does the header bind and the block record?** `impl_target_head_args` —
  the head's own argument list, narrower by the tuple target, which binds as a
  variadic pack through its own path.
- **What name does the definition mint?** The whole target, references peeled,
  rendered. The only one that must agree outside the elaborator: monomorphization
  asserts the minted `(module, name)` is unique, so a duplicate-definition check
  asks exactly that and nothing else.

Sharing a reading fails quietly, as an answer narrower or wider than the
question wanted — reading _arguments_ where the minted _name_ was asked makes
`&Cw<i32>` and `&Dw<i32>` alike, the pointee that separates them being what a
shape comparison peels. So a reading is named for its question, never for the
shape it reads.

Each argument is read at its own reference site, never by the shape of its
spelling:

- A binder is free where it stands and nowhere else: `impl<T> Slot<[i32, T]>`
  still wants a two-element tuple starting `i32`. Asking the site is what keeps
  an alias spelled like a parameter, or a qualified `ns::Tag`, from reading as
  a binder.
- A binder the header cannot bind — one nested in a shape — matches nothing,
  since a receiver matching it would have nothing to instantiate.
- Naming no declaration is not matching anything: a tuple, a reference and a
  function type are shapes, and a receiver either has the shape or has not.

The comparison is structural. Rendering both sides is §8's hazard from the
inside — an AST against a `TypeId` needs two renderers agreeing at every depth,
which cannot be arranged. Declarations compare through `TypeHead` (`DefId` where
one is named, the rendering where nothing declares the shape, so `i32` and `()`
compare without being nominal types); every other shape compares as itself, a
reference by kind, a tuple by arity, a function type by parameters and return.
Nothing is spelled, so nothing can be spelled two ways.
