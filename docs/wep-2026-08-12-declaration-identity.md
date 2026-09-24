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
an `AstId` means the same node in both tables, bundled wasm assets included. It
is checked, not assumed: an entry naming itself `#![stdlib("core:…")]` re-parses
a cached module, and drops the snapshot.

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
4. the prelude — what `core:prelude` exports, its own declarations and its
   re-exports alike, each gated on reaching outside `core:`. A sealed compiler
   item (`ReflectStruct`, `Member`, `Ref`) is among them, so it resolves for a
   module that never `use`d it and can then be diagnosed as sealed. The builtin
   types join on other grounds: `i32` and `f64` are `internal type`
   declarations in `core:prelude/primitive.wado`, and a type every module can
   write must not depend on where it was declared. This layer is
   unconditional, including for a module carrying `#![no_prelude]`, which is
   what lets `core:prelude/int128.wado` write `i64::MAX`. The attribute
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
    /// `T::Assoc`, named by `T`'s binder: it reaches a declaration only once
    /// `T` is a type.
    Projection(AstId),
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
consumer improvises around, so it panics rather than returning `None`. The cases
stay distinct on purpose: reading `Unresolved` as `Binder` loses the diagnostic a
name that reaches nothing deserves. `walked` keeps one more case apart from all
of them — a node no walk saw, which synthesis mints — because that is the only
one for which some other source of truth is honest.

Type resolution carries the site with it: the head's `AstId` reaches
`resolve_named_type` / `resolve_generic_type`, which read the declaration off
this table rather than re-running a scope lookup from wherever the walk stands.
An alias, a namespace prefix and a function-local `struct` reach their own
declarations with no vantage supplied.

`Unresolved` is not a synonym for "error", but an `impl` header's trait position
is: implementing a trait is naming it. A header's own reference site answers that
position and only it, so every header carries a declaration and dispatch has no
spelling to fall back to.

A `with` clause is the same: each effect it names is a reference site, a trait
head's and a function type's included. An effect parameter answers as its
binder. A name reaching no `interface` or resource is rejected where it is
written, however many modules declare an effect under it, so `with Stdout`
needs `Stdout` in scope like any other name. Fixture:
`error_with_unknown_effect.wado`.

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

A projection interns by its `bounds` too, so those are read from the declaration
`owning_trait` names, never from the associated type's bare name. A name-keyed
index gave every `Output` whichever bounds the first trait declaring that name
wrote, and two projections on one `T::Output` stopped comparing equal.

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

Where no such node exists, the reference carries its referent directly. A
rebuilt bound records it in `TraitBound::resolved`, which the parser leaves
empty, and every reader takes it ahead of the site — no name to look up, no
vantage to get wrong.

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
  `DefId`. The one direction that runs the other way is a WIT type name, which
  `TypeTable::cm_decl_in` resolves against the module that declares it. A module
  declares each WIT name once, so the `(name, module)` pair names a single
  declaration. What comes back is a `DefId`. The step runs once per interface,
  and every key past it is that identity.

  A generated `wasi:*` / `core:kiln/*` module has no alternative: no Wado
  resolver walked that namespace, so there is no reference site, and
  `CmInterfaceRegistry` parses its own copy of those modules once per process,
  so there is no declaring node this program's `DefTable` saw either. A user
  module that writes its own `#[cm(…)]` bindings is not in that position. It is
  an ordinary module of the program, its declarations are in the `DefTable`, and
  `cm_decl_in` reaches them through the same call.

  What the boundary may not do is carry the name further. An interface is the
  scope of a WIT type name, and a package holds several interfaces:
  `wasi:sockets/types` and `wasi:sockets/ip-name-lookup` each declare an
  `error-code`, and they are unrelated variants. A key built from the package,
  or from the bare type name, names one of them and silently drops the other.

### One declaring module per CM interface

What makes `(name, module)` name a single declaration is that a CM interface has
exactly one declaring module. A module that declares a `#[cm(…)]` type in an
interface another module already declares into is rejected, and the diagnostic
names the declaration it would have collided with.

This follows from the adopted identity rule rather than adding to it. Two
modules declaring into one interface would give `cm_decl_in` two answers for a
WIT name and no way to choose, and the registry keys a declaration by
`(source_interface, name)`, so the second registration would either overwrite
the first or trip a uniqueness check written for a different failure.

A module declaring an interface the standard library does not bundle is the
ordinary case, and the one the `#[cm(…)]` bindings exist for. The rule bites
where a module declares into `wasi:*` or `core:*`, which the standard library
already owns. It also requires that a module's types for one interface stay in
that module. Splitting them across files is rejected, because
the second file is a second declaring module. The `interface` whose operations
name those types is free to sit elsewhere, since it declares no type of its own.

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

Nothing above closes a declaration's _data_ reached by name. An index keyed by
an associated type's name can still answer with some trait's bounds for it: it
mints no identity, and it turns no name into a declaration. The rule that covers
it is §6, that a fact about a declaration is read through the `DefId` naming it.

### What still turns a name into a declaration

What is left, each with the reason. A declaration is whatever _identifies_ one,
so this spans both currencies: a `DefId`, and a `Symbol` row, which carries the
declaring node. Adding to the list is a design change — the alternative is
always to give the caller the reference site.

The one scope, which every other entry exists by not being:

- `Scopes::resolve`, `resolve_value`
- `Resolutions::resolve_in` — the same scope, for a spelling no walk visits,
  such as the attribute argument in `#[benign(E)]`

The three recorded facts the frame derivation is built from. Each is one tier,
none is a scope, and none takes a vantage a caller could get wrong:

- `imported_as` — the import tier alone, for a caller asking about the aliasing
- `prelude_decl` — the prelude tier, which has no vantage to be given
- `decls_named` — every declaration under the name and no choice between them

The derivation itself: those tiers in order, over the walk's own frame. Each has
a sited entry point a caller with a reference site reaches instead.

- `decl_key_or_local`, `TypeLookup::declaration` — for a rendered head
- `namespace_member` — the `ns$Name` alias a namespace import registers
- `bound_declaring_assoc_type` — which of a _binder's_ bounds declares a name.
  One algorithm on `TraitEnv`, reading each bound through the reference site its
  caller supplies, so a frame and a declaration-level resolver share it.

The same derivation in the `Symbol` currency, which §5's `DefId` columns subsume:

- `symbol_named`, `imported`, `lookup_in_module`, `lookup_in_module_with_visited`
- `decl_in_module` — `lookup_in_module` read back as an identity, for the
  positions no reference site answers: `builtin::f`, a namespace member,
  `core:rt`'s `panic` at a synthesised call, and a default expression's own
  module. Each names its module rather than searching for one, so no vantage
  is supplied.

Renderings still compared against a declaration's own name:

- `impl_target_decl_key` — a receiver's newtype chain against an impl's head, on
  the paths that name no block: an auto-derived `Eq` / `Ord`, and a method
  reached through a type parameter's bound, whose block monomorphization picks.
  A dispatch that matched a concrete block reads the module off it instead.
- `impl_head_decl_name` — an impl header's own head, filtering a static call's
  candidate blocks. Each side resolves in the module that wrote it, so an alias
  on either steers neither.

The Component Model boundary, permanent for the reason §9 gives:

- `cm_decl_in` and `cm_decl`, which resolve a WIT name to its declaration.
- `cm_decl_in_interface`, the same step taking the interface rather than the
  module: it asks the registry which module declares that interface, so a caller
  holding an interface FQ supplies no vantage of its own. A bundled `wasi:` /
  `core:` interface is addressed by its module's path through
  `cm_decl_in_module_named`, for a caller holding no `ModuleSource`.
- `find_named_type_by_source` and `find_named_type_by_module_name`, which take
  the same step and answer the `TypeId` the declaration was interned under.
- `cm_decl_def`, codegen's entry to the same step, reading the `DefId` off the
  `TypeId` those two return.

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

`Self` is the other half of that frame, and it travels with the bound for the
same reason its parameter space does. A bound written on a parameter means the
`Self` of the declaration that wrote it. A supertrait clause and a declared
parameter default (`Eq<Rhs = Self>`) are written in the trait's own space
instead, where `Self` is whichever type the bound stands on. The two travel
separately: a reader that supplies one for the other projects off the wrong
receiver.

Three facts say what `Self` means: the type it stands for, the trait whose
declaration names what is projected off it, and the bindings that trait's `impl`
wrote. `Self::Assoc` needs all three, so a frame is installed whole or not at
all. A frame carrying a receiver without its trait answers `Self` and leaves
`Self::Assoc` unresolved. One that keeps the enclosing walk's bindings answers it
off the type that walk was standing on.

A free function declares no `Self`. A bound there that writes one is rejected at
the declaration, naming the type parameter to write instead, at every position: a
trait argument, and an associated-type constraint nested under one.

An `impl` block binds its `Self` between its own names and their bounds. Its
target is resolved from those names (`impl<U> Maker<Container<U>> for Foo<U>`),
and a bound on one of them may project off that target
(`impl<O: Uses<Self::Item>> Run for Wrap<O>`). So the names are bound first, the
target resolved, and the bounds read last.

The trait solver's lowering cannot state every bound. `O: Uses<Self::Item>` is
one it cannot: the argument is a projection it has no name for. Such a bound
silences the solver about `O` alone, and the rest of the frame stands. A question
about any other type is answered as it would be without the bound. Declining the
whole frame instead takes down every receiver the declaration reaches.

Fixtures: `supertrait_binding_keeps_writer_frame.wado`,
`bound_self_projection_in_trait_method.wado`,
`bound_self_projection_in_impl_method.wado`,
`bound_self_projection_on_impl_param.wado`,
`bound_self_projection_in_trait_argument.wado`,
`bound_self_projection_on_trait_param.wado`,
`assoc_type_constraint_fn_over_self.wado`.

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

The key names the receiver by its declaration and arguments, and keeps any
reference layer. It never uses the `TypeId` slot, because a generic instance and
the struct it monomorphizes to are two slots for one type. An impl on the
reference itself answers before the reference is looked through.

Fixtures: `assoc_type_per_trait_args.wado`,
`reference_impl_assoc_type_projects.wado`,
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

### A parameter's number is where the target names it

A parameter of an `impl` block is substituted against the instance's type
arguments, so its number is the position the target writes it at.
`impl<T> Kind for Holder<Option<i32>, T>` puts `T` at 1. Numbering by
declaration order would put it at 0, where the instance carries `Option<i32>`. A parameter the target does not name
takes a slot past the ones it assigned, which no instantiation reaches.

One answer, `ImplParamSlots`, serves the block's own frame, its methods, and the
associated types it registers. Where each site computes its own, they read the
target differently. One unwraps a reference and another does not; one counts the
arguments the target writes and another the parameters it bound. A block both
sites reach then carries two numbers for one parameter, and the cross-check
against the trait solver reports it as a disagreement about whether the impl
applies at all.

A trait declaration carries two numberings, and they do not count the same
parameters. An argument position counts every parameter a site may write; a slot
counts only those a substitution fills, and an `fn`-bound parameter takes the
first and not the second. `trait_params_from_impl` reports both, so a reader
asking for one cannot land on the other.

### The head finds the candidates, the arguments choose

An impl is filed under its target's head, so every impl on `List<_>` answers a
lookup for `List<String>`. The written arguments decide which of them applies.
That decision is `inherent_impl_type_args_match`, wherever the answer is used.
Registering a block's associated types without it files
`impl Kind for List<u8>`'s `Out` under `List<String>` too. The last impl in
build order then decides both, which is the pick this WEP forbids.

A reference target is where a reading comes apart most easily. Every impl on one
is filed under a single receiver key, so the written arguments are what choose
between them. `impl_target_args` reads through the reference to the pointee's
arguments, and `ImplParamSlots` numbers the block's parameters by those
positions, so that is what a receiver supplies there.
`TypeSystem::impl_position_args` is the one answer. A caller handing the pointee
whole instead makes the match contradict the binding.

A blanket `impl<T: Bound> Trait for &T` writes no position at all, so its `T`
stands for the receiver's pointee rather than for an argument of it. The bound is
checked against that pointee, read off the receiver rather than off a position a
caller filled.

Fixtures: `assoc_type_per_receiver_args.wado`,
`assoc_type_binder_at_target_position.wado`,
`impl_on_reference_to_generic_head.wado`,
`bound_arg_behind_fn_bound_param.wado`.

## Keys past the Component Model boundary

A component's outer scope holds two kinds of type. One stands for a shape, and is
keyed by that shape. The other stands for a declaration, and is keyed by that
declaration. §9's boundary step is where the second kind gets its key: it answers
with a `DefId`, and every key past it is that identity.

A CM name cannot key the second kind, for the reason §9 gives for not carrying
the name further. A name alone puts every package in one namespace, and its
package puts every interface of that package in one. A key built from either
names one of two unrelated declarations and drops the other.

Every producer of such a type holds the interface it is emitting for, so it
reaches the declaration through that one step. A producer that reaches none has a
missing import, which is a diagnostic. Trying a second key instead is the shape
"What a derivation may not be" forbids.

### A canonical intrinsic's payload is a declaration

A payload is a type, so a nominal payload is its declaration. The CM name the ABI
spells for it travels beside the identity and is never compared. That is §8 at
this boundary.

The name such an intrinsic imports under is one of those renderings, and nothing
reads it back. An annotation names an operation and never a payload, so a payload
comes from the annotated signature, read where the declaration is in hand.

## Pattern qualifier arguments

A pattern's qualifier is a type, and by §6 a type is its declaration plus the
arguments it was instantiated at. So `Maybe<String>::Just` does not qualify a
`Maybe<i32>` scrutinee: the declarations agree and the instantiations do not.

Each written argument is read at its own reference site and compared against the
scrutinee's as a type. Counting them compares nothing, and comparing the
spellings is §4's defect one level out.

Fixtures: `pattern_qualifier_type_args_read_error.wado`,
`pattern_qualifier_type_param_arg.wado`.

## Known gap: a CM type can reach outer scope with no identity

A reader that holds only the CM export spells the Wado name by `PascalCase`-ing
the CM name. A declaration whose own name is spelled otherwise is not that name,
so §9's step answers nothing and the alias is keyed by the export it was made
from. One interface spells that export name once, so the key collides with
nothing today.

Which declarations a component reaches this way is not established, nor whether
any of them is spelled such that the step could answer.

## Known gap: a CM name still reaches a declaration by search

A reference the Component Model boundary synthesizes carries the interface its
own declaring module registers, so §9's step answers it. Where a reference
arrives without one, the registry searches for the name instead, one kind of
declaration at a time, over the one fixed order of the six kinds.

That is all three shapes "What a derivation may not be" forbids at once. The
search spans every interface in reach; each kind's map answers in registration
order; and a kind that declines because two interfaces spell the name hands the
question to the next kind. `ErrorCode` is the instance: a variant in four
`wasi:` interfaces and an enum in `wasi:cli/types`, so the variants decline and
the enum answers. That holds for any module's `ErrorCode`, including one a user
wrote.

Two kinds answering with different interfaces is a disagreement, and what it
yields depends on which search asked. A search scoped to a namespace prefix
takes the first kind that answers, so the order is a silent tiebreak. A search
over every kind at once refuses instead, so the name resolves to nothing. Which
of the two a reference meets follows from where it arrived, not from anything it
says.

A reference arrives without a declaring interface in three positions:

- A world body names an export type that no import resolves. Only the world's
  own namespace scopes such a name, and that namespace does not reach a type
  another package declares.
- A lib-local type is registered under its package's default interface, which no
  CM namespace covers, so only a program-wide unique match reaches it.
- A reference synthesized while emitting a CM instance carries at most the
  interface being emitted, which need not be the one that declares it. That is
  where `ErrorCode` arrives.

## Known gap: an abstract qualifier argument is not compared

Where either side of a pattern qualifier leaves a position abstract, that
position is accepted uncompared. A generic body writes its own type parameter
where the scrutinee carries a concrete type, which is ordinary code. A parameter
names no instantiation, so the scrutinee's argument has nothing to disagree with.

What this admits is a body whose parameter is bound, at the instantiation being
compiled, to a type the scrutinee's argument contradicts. The pattern is taken
as matching, and nothing later rejects it.

## Known gap: the operator paths compare an impl target by spelling

`inherent_impl_type_args_match` decides whether a receiver reaches an impl, and
it compares structurally. The arithmetic and indexing lookups do not ask it.
They ask `verify_impl_type_compatibility`, which compares a written argument's
head against the receiver's rendered type name and reads a free parameter off a
set of parameter names rather than off its reference site.

What it admits is §8's hazard on those two paths: an alias spelled like a
parameter, a qualified `ns::Tag`, and two declarations rendering alike are each
decided by the rendering.

## Known gap: a reference impl target has no name of its own

`name::Receiver::Ref` spells the reference kind and nothing else. The pointee
rides in the type-argument list, which is the blanket `impl<T> Trait for &T`
written out, and it is the only reference target the naming layer can say. A
target that is a reference to a named head has no spelling of its own, so its
definition and its call sites mint two different names for one method.

What it admits is a reference impl on a named head that is unreachable wherever
the two names differ. `impl Show for &Wrap<i32>` called directly on a
`&Wrap<i32>` reports that `&i32` does not implement `Show`;
`impl<T> Show for &Wrap<T>` reached inside a frame monomorphized for
`&Wrap<i32>` reports that `Wrap<i32>` does not. The same generic impl called
directly works, because both sides mint the same name. The pending fixture is
`reference_impl_named_head_dispatch.wado`.

## Known gap: a synthesised type carries only a spelling

A synthesised bound carries its referent (§7); a synthesised `ast::Type` has no
field to carry one. The Component Model binding synthesis builds types such as
`Fields`, `Response` and `WaitableSet` whose nodes no walk visited, so
`decl_key_at` answers them through the frame derivation. What it admits is §7's
hazard at those nodes: the frame, not the synthesis, decides which declaration a
spelling reaches.

## Known gap: a data declaration binds no `Self` for its own bounds

A `trait` and an `impl` each bind a `Self` their parameters' bounds may project
off. A `struct` or `variant` declaration does not, so
`struct Wrap<O: Uses<Self::Item>>` is rejected where the same bound on
`impl ... for Wrap<O>` is read. What it admits is a bound that can only be
written on the impl, so a constraint the data declaration means to carry has to
be restated at each impl that needs it. The pending fixture is
`data_decl_bound_projects_off_self.wado`.
