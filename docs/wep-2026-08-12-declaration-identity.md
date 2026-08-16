# WEP 2026-08-12: Declaration identity — one identity, one scope, one answer

## Context

A name in Wado source is module-relative. `Greet` written in `entry.wado` and
`Greet` written in `sub/other.wado` are two declarations; which one a spelling
means is a fact about the module that wrote it — its `use` list, its aliases, its
own declarations, the prelude behind them.

The compiler gets that wrong in a recurring, recognisable way: a program compiles
or fails depending on whether two unrelated declarations happen to share a
spelling, and renaming one of them changes the answer. That signature identifies
the whole class.

| issue | layer                      | symptom                                                        |
| ----- | -------------------------- | -------------------------------------------------------------- |
| #1298 | default-method synthesis   | trait resolved by global name                                  |
| #1348 | cross-module impl dispatch | keyed on a simple name                                         |
| #1769 | inherent-impl coherence    | collision bucket keyed on the written head                     |
| #1785 | trait-impl lookup          | aliased bound unsatisfiable; same-named foreign trait accepted |

`tests/fixtures/cross_module_same_name_*` is 26 fixtures, one per occurrence
found by hand, and the class keeps producing more.

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

`DefId` replaces `DeclKey`, `ImplTargetKey::Decl`, `TraitKey`, the
`(ModuleSource, String)` pairs `CompilerItems::Resolved` and
`Resolutions::declared` hand back, and the head of `FqTypeName` / `FqTraitName`.
Equality is index equality.

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
sparse, so per-declaration data cannot be a `Vec`, which is what axis C needs.

`DefId` is dense, never rendered, never serialised, never parsed. It is an index
into one table, so every fact that carries one must be read against the table
that minted it. The stdlib snapshot is where that stops being obvious: it caches
whole declaration facts — `ModuleDecls::clone_digests_from` hands a later compile
the stdlib's `ImplSig`s verbatim, and those compiles never re-run the decl pass
for a snapshot module. A `(ModuleSource, String)` key survived that boundary
because it describes a declaration rather than indexing one; a `DefId` does not,
and reading one against a freshly built table silently names some other
declaration.

So the table is seeded rather than rebuilt: `DefTable::build_seeded` continues the
snapshot's table, keeping every declaration it already identified at its `DefId`
and minting only what it never saw. `TypeTable` is seeded the same way and for the
same reason. What makes it sound is that the stdlib AST is parsed once per process
and shared, so an `AstId` means the same node in both tables — the invariant the
snapshot's reference re-seeding already relies on.

This is a precondition for every later step, not a detail of this one: a `DefId`
in `ResolvedType`, in a registry key, or in any other cached declaration fact
crosses the same boundary.

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

`Scope` is private to `crate::resolve`, and nothing outside it can run the walk
by name: the scope is reached only through a reference site, and a caller
holding a spelling and no site gets the frame derivation below instead — which
is not a scope and cannot pretend to be one. `SymbolTable`'s name-keyed accessors,
`ModuleImports` and `TypeLookup`'s import branch are deleted, and with them the
name-scope half of `module_import_scope`; what survives it is `namespace_imports_of`,
answering the one import fact the symbol table does not record — which module a
namespace alias stands for. The facts they carried that are not scope — a module's
re-export list, an interface's members — stay, keyed by `DefId`.

What an explicit `use` means is the analyzer's answer and only its answer: it
resolves aliases and re-export chains once and records them, and every consumer
reads that record. Re-walking the `use` declarations to answer the same question a
second way is what let a namespace-qualified import and a `pub use` barrel resolve
differently depending on which walk a pass happened to reach.

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
answer for every one; `ast.rs`'s
`every_reference_bearing_node_carries_an_ast_id` scans its own source — struct
declarations and struct-like enum variants alike, since half the reference
positions are variants — and fails on a name-bearing node with no id unless
`NAMED_WITHOUT_ID` registers the reason it needs none. A struct pattern's
qualifier is such a position: naming a type in pattern position is naming a
declaration.

`get` is total. A site the walk missed is a bug in the walk, not an absent answer a
consumer improvises around, so it panics rather than returning `None`. The three
cases stay distinct on purpose: reading `Unresolved` as `Binder` loses the
diagnostic a name that reaches nothing deserves. `walked` keeps a fourth case
apart from all three — a node no walk saw, which synthesis mints — because that
is the only one for which some other source of truth is honest.

Type resolution is the largest consumer and takes the site with it: `resolve_type`
hands the head's `AstId` to `resolve_named_type` / `resolve_generic_type`, which
read the declaration off this table rather than re-running a scope lookup from
wherever the walk stands. An alias, a namespace prefix and a function-local
`struct` reach their own declarations with no vantage supplied. One entry point
declines a site — `resolve_unsited_type_name`, for a `Self::` / `T::` receiver
the elaborator rewrote to a spelling no source segment names; giving the
static-call chain the receiver's own site is what removes it.

`Unresolved` is not a synonym for "error", but an `impl` header's trait position
is: implementing a trait is naming it. `impl Deserialize for Point;` written in a
module that never named `Deserialize` used to compile, resolved by a global scan
over every declaration index — and the header then carried no identity, so
dispatch was left comparing spellings. The header's own reference site answers and
only it, so every header carries a declaration and the comparison has nothing else
to fall back to.

### 4. Queries take identities, never a name beside one

Every query that decides identity takes a `DefId` and does not take the name.

```rust
// before — the identity is optional, so 16 of 30 callers omit it
fn type_implements_trait(&self, …, trait_name: &str, trait_ref: Option<DeclRef>) -> bool;

// after — a caller without an identity cannot call
fn type_implements_trait(&self, …, trait_: DefId) -> bool;
```

Four rules, each of which the current signature breaks:

- An identity parameter is never `Option`. Optional means the caller may decline,
  and the measurement says the caller declines.
- An identity parameter never travels beside the name it would be compared
  against. A name in the same argument list is a fallback waiting to be written,
  and `same_trait`'s `impl_trait_name == trait_name` is that fallback already
  written.
- A declaration is compared to a declaration, never to the spelling that reached
  it. `def_name(def) == written` reads as a check and behaves as a filter: it
  declines exactly when the two spellings differ, which is exactly when an
  import alias, a namespace prefix, or a local item's `@AstId` mangle is in
  play. Backward type-argument inference held four of these, so a generic
  variant named through an alias inferred nothing at all.
- A diagnostic reads its spelling at the point of reporting, from the site and the
  AST — never from a name threaded down for the purpose.

Flipping a parameter's type is what makes the work enumerable: the compiler lists
every caller that still holds a name, and each is either given the site it lost or
shown to have one already.

### 5. Declaration data is keyed by `DefId`

The seven name-keyed registries collapse into `DefId`-indexed columns on
`DefTable`: fields, cases, members, methods, type parameters, bounds, visibility,
span. `TypeLookup`'s four-tier scope walk disappears, because there is nothing
left to walk — the caller arrives holding the `DefId` its site resolved to.

The tables a walk builds as it goes are keyed the same way. `ModuleDecls`'
`local_*` maps — the fields, cases, members and newtypes the module being
elaborated has contributed so far — are keyed by declaration, so a module-level
`struct Box` and a function-local one of that name are two entries rather than
one the later insert wins. No separate tier is needed to keep the two apart.

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
strings, and a `ResolvedType` can no longer be built for a declaration that does
not exist. `AssocTypeProjection::owning_trait` becomes a `DefId` too, which is
what `resolve_assoc_type_qualified` needs to stop declining when two declarations
share a name. `Newtype` gains the same head/arguments split `Struct` has, so
`impl_receiver_key` and `newtype_own_name` stop handing the impl index a fused
spelling no `impl` header writes.

A shape no declaration names — a tuple, a reference, a function type, a pack — has
no `DefId` and needs none; each is already its own variant. Primitives are not
special: `i32`, `()` and `!` are `internal type` declarations in
`core:prelude/primitive.wado` and get `DefId`s like anything else.

An anonymous struct is such a shape and is _not_ already its own variant, which
is the one place this rule needs a decision rather than an application. A struct
literal with no type name interns as a `Struct` today, under a spelling
synthesized from its fields, and two literals of the same shape deliberately
reach one type — so there is no declaration to identify and no node to identify
it by.

It does not become a ninth variant. Measured: adding one breaks twelve exhaustive
matches, and that number is the trap — an anonymous struct rides the `Struct`
path through field access, layout and codegen at every one of the 121 sites that
match `Struct` today, and those sites would stop matching it _silently_. The
compiler would report the twelve it can see and none of the rest, which is the
opposite of what this design asks of a migration.

So the head splits instead of the variant:

```rust
Struct { def: StructDef, type_args: Vec<TypeId> },

enum StructDef {
    Decl(DefId),
    /// A shape, interned by its fields. Not forgeable from a name either.
    Anon(AnonStructId),
}
```

Every site that matches `Struct` keeps matching it; every site that reads the
head has to say which case it means, and the compiler lists them. The
synthesized `__anon_{…}` spelling goes: it exists only to key the interner, and
the fields are the key. A shape's fields are filed under its `AnonStructId`
beside the declarations' under their `DefId`s, so nothing has to render a
spelling to store them and nothing has to reproduce that spelling to read them
back.

Interning identity does not change. `TypeTable` keys an interned type by its
rendered spelling, because holding argument `TypeId`s as identity would mint two
types where equivalent-but-distinct ids meet — such ids demonstrably exist, which
is why `Monomorphizer::try_queue_function` dedupes a blanket instance reached from
two dispatch sites. What this step buys is that the head and the arguments become
separately readable, not that identity changes.

The table renders the head, so it has to be able to. `TypeTable::type_name` is
what mints every mangled name, and with a `DefId` in place of the spelling it
needs the `DefTable` to read one out — an `Arc<DefTable>` attached where
`Resolutions` is built, and again on the snapshot restore path, whose seeded
table hands back the same identities by construction. This is also what retires
`mangle_local_item_name`: a local item's type is distinct because its
declaration is, not because its spelling carries an `AstId`.

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

Every question a consumer answers today by splitting a mangled string
(`split_local_method_name`, `split_trait_method_receiver`, `split_head_and_args`,
`split_base_name`, `extract_local_name`, `rebase_monomorph_method`,
`replace_type_name_in_mangled`) becomes a field access, and those functions are
deleted rather than deprecated. `MangledName` is constructible only from a
structured identity, so a declaration name cannot be promoted to a mangled one
by hand.

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

Two rules survive from the structured-name work and still apply to the renderer:

- A name minted for a definition and a name built to look one up must come from
  one function, or nothing makes them agree. This is what silently killed every
  ref-impl candidate (`&<List<i32>>` against `&List<i32>`) and what let DCE key
  definitions and call sites two ways. The regression test asserts the two sides
  agree, rather than pinning either one's output.
- A surviving `TypeId` must stay readable. `TypeTable::retain` closes over each
  surviving struct's `type_args` transitively, so a struct cannot survive spelling
  itself with an id that no longer resolves. It must also re-enter each survivor
  under the spelling `intern` entered it by — `Box` for the declaration,
  `Box<i32>` for that instantiation. Rebuilding the index on the declaration name
  alone put every instantiation of `Box` on one entry, so the last survivor
  answered for the declaration and for its siblings, and
  `find_decl_type_by_name` — documented to return declarations only — returned an
  instantiation. That is the same defect one layer down: a rendering standing in
  for an identity, and two things rendering the same.

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
- `every_reference_bearing_node_carries_an_ast_id` fails on a new name-bearing AST
  node without an id, so a new reference position cannot be added silently.
- `no_reachable_function_turns_a_name_into_an_identity` scans `wado-compiler/src`
  for a reachable function taking a module and a name and handing back a
  declaration. That is the shape of the chain this design removes, and the one
  property the type system cannot state. Two remain, listed in
  `NAME_TO_IDENTITY` with the reason each belongs there: `imported_as`, which
  answers what a module imported under a local name rather than what a spelling
  means, and `cm_decl_in`, the Component Model boundary. The test fails on a
  third, and equally on a stale entry, so neither the class nor the list can
  grow.

## The frame derivation

A name whose reference site is not at hand still has to reach a declaration:
`find_struct_module_source` asked for a synthesis target, `symbol_named` for a
mangled name, a `*_case(name)` lookup for a resolved type's rendered head.
Nothing walks a module's scope for them. Three recorded facts answer instead, in
order:

1. `Resolutions::imported_as` — what this module `use`d under that local name.
   The one import fact that is not a scope lookup: it cannot reach another
   module's imports, and it answers with what an alias aliases.
2. `TraitEnv::decls_named`, filtered to the module in hand — every declaration
   written under the name, whichever module declares it. It holds what modules
   _declare_, never what they import, so no alias can steer it.
3. `Resolutions::prelude_decl` — what the prelude puts in scope under the name.
   The prelude tier alone, and it takes no vantage because it cannot be given
   one: the prelude is in scope in every module.

The three are the module's own reach, so a declaration the module cannot see
stays unseen here — the derivation never widens to the whole program, and a name
no module brought into scope is unresolved, the same answer the walk gives.

There is no fourth tier, and the derivation is not what a qualified call uses.
`Type::method` names its receiver at its own path segment, which the resolve
pass answered for like any other reference, so `impl_target_at` reads that site
and the spelling is never split back into an identity. Before it did, the
receiver of a default argument spliced into a caller keyed to whatever the
_caller_ called `Type` — a caller declaring its own same-named type silently
took the impl bucket, and a whole-program unique-name lookup was what covered
the gap. A site answers where that lookup guessed, so the guess is gone.

Nor does any tier take a vantage it could get wrong: `decls_named` takes no
module at all, and the caller that knows which module it means filters the
iterator itself. That is why the derivation cannot be mistaken for a scope, and
why `no_reachable_function_turns_a_name_into_an_identity` is a statement about
production code rather than a ratchet over it.

What still prevents some collisions by convention rather than by key is
`name::mangle_local_item_name`'s `@AstId` suffix, and the `local_item_renders`
index that reads that suffix back into a declaration. That is a convention every
minting site and every lookup site has to keep. The evidence it is a convention:
closing one fixture — two sibling functions each declaring `struct Box<T>` —
took seven separate fixes, at type resolution, the struct literal, the WIR
lookup key, template admission, template lookup, the registration name, and the
instantiation scan. Each was a distinct caller of the same first-wins index. A
removed mechanism takes one fix.
