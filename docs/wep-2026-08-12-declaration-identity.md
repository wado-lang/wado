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

### The diagnosis

The generator is not that resolution happens late. It is that a **declaration has
no identity object**, so several things stand in for one, and every one of them
can be manufactured out of a name.

#### A. Identity has no type

What the compiler compares is `DeclKey = (ModuleSource, String)` — a _description_
of a declaration, freely constructible from any module and any string. Nothing in
the type separates "the key of a declaration that exists" from "a key made up from
the name I was holding and the module I happened to be in". The compiler makes
them up on purpose: `Elaborator::decl_key_or_local` ends
`.unwrap_or_else(|| (self.current_module_source.clone(), name.to_string()))`, and
five other sites substitute the writing module the same way. A fabricated key
compares equal to a real one whenever the spelling matches, and 28 sites assemble
a `DeclKey` from a module and a string by hand, so the fabrication is not
distinguishable at the type level from any other key.

`Resolutions: AstId -> DeclRef` moves in the right direction, but
`Resolutions::declared()` hands back a `(ModuleSource, String)` again, so identity
holds only between the table and the first call. Everything downstream is back in
the forgeable currency.

#### B. Scope has five implementations

"What does this name mean in module M" is answered by five independent bodies of
code, populated by five separate walks over the same `use` declarations:

| implementation                   | layering                                            |
| -------------------------------- | --------------------------------------------------- |
| `SymbolTable::lookup`            | imports, then `core:prelude`'s re-exports           |
| `resolve::module_scope_lookup`   | imports, own decls, prelude, prelude impl modules   |
| `trait_env::module_import_scope` | explicit `use`, prelude re-exports, then case names |
| `sem::imports::ModuleImports`    | four parallel per-name maps, aliases split out      |
| `types::TypeLookup::lookup_ref`  | fn-local, module-local, current module, imports     |

They disagree. Only the third brings variant/enum case names into scope; only the
second sees the prelude's `internal` declarations; only the fifth has a
function-local tier. A consumer reaching a different one gets a different answer
for the same name in the same module, with no diagnostic, because each is
individually plausible.

#### C. Declaration data is name-keyed

A declaration's contents live in per-kind, per-module, name-keyed maps —
`all_struct_fields`, `all_newtypes`, `all_variant_cases`, `all_enum_cases`,
`all_flags_cases`, `all_resource_types`, `all_generic_newtypes`, each an
`IndexMap<ModuleSource, IndexMap<String, V>>` with a module-local and a
function-local tier above it.

So reading a declaration _is_ a scope walk. A consumer that wants a struct's
fields must re-resolve a name, which needs a vantage, which it may not have. That
is why name parameters are everywhere rather than in a few places: 867
`*name: &str` parameters in `wado-compiler/src`, of which 394 are a bare `name`,
80 `method_name`, 63 `struct_name`, 57 `trait_name`, 19 `type_name`.

### Why the previous attempts did not converge

Three earlier passes over this ground each fixed a real thing and left the class
alive. Their failure modes are the design constraints for this one.

#### An optional identity beside a mandatory name is a permanent bypass

`type_implements_trait(…, trait_name: &str, trait_ref: Option<DeclRef>)` is the
shape the last migration produced. Measured over the crate: 30 call sites, of
which 16 pass `None` outright, 14 forward whatever they received, and **none**
passes a freshly derived identity. Every one of the 16 falls through `same_trait`
to `impl_trait_name == trait_name`. The identity parameter changed no answer at
more than half the call sites, because the caller was free not to have one.

#### A resolved fact re-spelled as syntax is resolved again

Synthesis builds `Type::Named(name)` with `AstId::fresh()` at 59 sites. It knows
the referent — it just constructed it — but emits a spelling whose site the table
never walked, so the elaborator resolves it from whatever module it is standing
in. The removed defect re-enters through the back door.

#### Namespacing the strings made the distinction expressible, not enforced

`MangledName` / `DeclName` / `DeclPath` and the structured `FqTypeName` /
`FqTraitName` stopped a bare `&str` from passing as a mangled name, and that was
worth doing. But the namespaces they separate are all _renderings of one thing_,
so separating them multiplies the forms a consumer must choose between instead of
removing the choice:

| namespace        | example                            | what it really is |
| ---------------- | ---------------------------------- | ----------------- |
| mangled identity | `core:prelude/list.wado/List<i32>` | `(DefId, args)`   |
| declaration name | `List`                             | `DefId`           |
| struct-list key  | bare head + qualified args         | `(DefId, args)`   |
| template key     | `List`                             | `DefId`           |
| instance key     | `List<i32>`                        | `(DefId, args)`   |

Every observed failure was a caller picking the wrong rendering:
`Receiver::head_key` where `decl_key` was meant emptied SROA's method catalog and
broke go-to-definition; `MangledName::new` accepting any string let a declaration
name be promoted by hand; `LocalMethodName` stored `struct_name` beside the
structure it renders, under an unenforced invariant. With one identity and one
rendering point there is nothing to pick.

#### A rendered name cannot be read back

This constrains what the final layer may do. A `ModuleSource` may itself contain
`/` and `<`, and a type argument carries its own module path, so no split on `/`,
`<` or `,` is correct in general. A rendering is also not reversible:
`ModuleSource` cannot be rebuilt from text without `ModuleSourceInterner`. Any
reader that decodes a rendered name back into its parts is a silent dependency on
the encoding, and `wado-compiler/src` has seven such functions.

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

`AstId` is deliberately not reused as the identity, though the symbol table is
already keyed by the declaring node's. Two reasons: `AstId` is the id type of
_every_ node and `AstId::fresh()` is public, so a use-site id type-checks wherever
a declaration id is expected and one can be minted from nothing; and `AstId` is
sparse, so per-declaration data cannot be a `Vec`, which is what axis C needs.

`DefId` is a compilation-local index by design: dense, never rendered, never
serialised, never parsed. A stdlib snapshot stores `(ModuleSource, AstId)` and
re-derives `DefId`s on restore through `DefTable::of_ast_id`, so the index never
has to be stable across processes.

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
   never `use`d it and can then be diagnosed as sealed;
5. the case names of variant / enum / flags types in scope, which a type of the
   same name always shadows.

`Scope` is private to `crate::resolve`. `SymbolTable`'s name-keyed accessors,
`module_import_scope`, `ModuleImports` and `TypeLookup`'s import branch are
deleted. The facts they carried that are not scope — a module's re-export list, an
interface's members — stay, keyed by `DefId`.

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
    pub fn at(&self, site: AstId) -> Resolution;   // total, not Option
}
```

Every node that names a declaration carries an `AstId`, and the walk records an
answer for every one; `ast.rs`'s
`every_reference_bearing_node_carries_an_ast_id` scans its own source and fails on
a name-bearing node with no id unless `NAMED_WITHOUT_ID` registers the reason it
needs none.

`at` is total. A site the walk missed is a bug in the walk, not an absent answer a
consumer improvises around, so it panics rather than returning `None`. The three
cases stay distinct on purpose: reading `Unresolved` as `Binder` loses the
diagnostic a name that reaches nothing deserves.

`Unresolved` is not a synonym for "error". A bodiless derive
(`impl Deserialize for Point;`) may name a stdlib trait the module never `use`d.
That is a gap in the language rule, closed by making the derive's trait reference
resolve like any other reference — never by a second lookup chain behind the
consumer's back.

### 4. Queries take identities, never a name beside one

Every query that decides identity takes a `DefId` and does not take the name.

```rust
// before — the identity is optional, so 16 of 30 callers omit it
fn type_implements_trait(&self, …, trait_name: &str, trait_ref: Option<DeclRef>) -> bool;

// after — a caller without an identity cannot call
fn type_implements_trait(&self, …, trait_: DefId) -> bool;
```

Three rules, each of which the current signature breaks:

- An identity parameter is never `Option`. Optional means the caller may decline,
  and the measurement says the caller declines.
- An identity parameter never travels beside the name it would be compared
  against. A name in the same argument list is a fallback waiting to be written,
  and `same_trait`'s `impl_trait_name == trait_name` is that fallback already
  written.
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

This is what removes the consumers' need for a vantage. A pass reading a struct's
fields no longer needs to know which module it is standing in, so it can no longer
stand in the wrong one.

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

Interning identity does not change. `TypeTable` keys an interned type by its
rendered spelling, because holding argument `TypeId`s as identity would mint two
types where equivalent-but-distinct ids meet — such ids demonstrably exist, which
is why `Monomorphizer::try_queue_function` dedupes a blanket instance reached from
two dispatch sites. What this step buys is that the head and the arguments become
separately readable, not that identity changes.

### 7. Synthesis records referents, it does not spell names

A pass that synthesises a reference knows what it refers to, so it records that:

```rust
// before — a name whose site the table never walked
Type::Named(NamedType::new(AstId::fresh(), "Display".into(), span))

// after — the referent, which needs no resolution
Type::Resolved(display_def)
```

`ast::Type` gains a `Resolved(DefId)` variant, absent from parsed syntax and
produced only by synthesis. Type resolution handles it by returning the
declaration: no name to look up, no vantage to get wrong. This closes the 59
synthesised reference sites.

### 8. Mangled names are rendered once, never parsed

Wasm needs a string, so one place produces one. `SymbolPath` is a structured
function identity — the defining module, the receiver's `DefId` and type
arguments, the trait's `DefId` and type arguments, the method name and its type
arguments — and it renders on demand.

Every question a consumer answers today by splitting a mangled string
(`split_local_method_name`, `split_trait_method_receiver`, `split_head_and_args`,
`split_base_name`, `extract_local_name`, `rebase_monomorph_method`,
`replace_type_name_in_mangled`) becomes a field access, and those functions are
deleted rather than deprecated. `MangledName` is constructible only from a
`SymbolPath`, so a declaration name cannot be promoted to a mangled one by hand.

`FqTypeName`, `FqTraitName`, `Receiver`, `TypeHead`, `DeclName` and `DeclPath` are
subsumed: their job was to keep a rendered string honest about which namespace it
belonged to, and a `DefId` has no namespaces to confuse. `LocalMethodName`'s
remaining stored spellings become derived renderings of the same `SymbolPath`.

Two rules survive from the structured-name work and still apply to the renderer:

- A name minted for a definition and a name built to look one up must come from
  one function, or nothing makes them agree. This is what silently killed every
  ref-impl candidate (`&<List<i32>>` against `&List<i32>`) and what let DCE key
  definitions and call sites two ways. The regression test asserts the two sides
  agree, rather than pinning either one's output.
- A surviving `TypeId` must stay readable. `TypeTable::retain` closes over each
  surviving struct's `type_args` transitively, so a struct cannot survive spelling
  itself with an id that no longer resolves.

The rendered format is unchanged; the emitted Wasm is byte-identical.

### 9. What names are still for

Three things, none of them comparable:

- Source syntax. The AST holds what the programmer wrote, so the formatter and the
  LSP reproduce it.
- Diagnostics. A message says what the programmer wrote, read off the site.
- The Component Model boundary. An export name is an ABI fact derived from a
  `DefId`, never used to look one up.

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
- `Resolutions::at` is total and panics on a missing site, so a coverage hole in
  the walk fails on the first fixture that reaches it instead of degrading to a
  name comparison.
- `every_reference_bearing_node_carries_an_ast_id` fails on a new name-bearing AST
  node without an id, so a new reference position cannot be added silently.
- One test asserts `wado-compiler/src` contains no function taking a module and a
  name and returning an identity. That is the shape of the chain this design
  removes, and the one property the type system cannot state.

## Migration

What is left, in the order it has to happen: the scope before anything reads it,
synthesis before the table can be total, the name-keyed storage before the
mangling. Each step compiles, passes the suite, and ends with a mechanical
completion check.

- [ ] `Scope` — one implementation of what a name means in a module. Done when
      `SymbolTable`'s name lookups, `module_import_scope`, `ModuleImports` and
      `TypeLookup`'s import branch are deleted.
  - [ ] The prelude tier. `module_scope_lookup` ignores `#![no_prelude]` and
        admits every kind; `module_import_scope` honours the attribute and admits
        types and traits. The opt-out should hold — with the prelude's
        implementation modules still reachable, since that is what lets a sealed
        compiler item resolve for a module that never named it.
  - [ ] Function-local items (`Stmt::Item`). The symbol table collects only
        module-level declarations, so a local `struct` has no identity and no
        scope entry; `TypeLookup`'s function-local tier answers for it by name.
        It needs a `DefId` scoped to its declaring function.
- [ ] `ast::Type::Resolved(DefId)`; synthesis stops spelling names. Done when no
      synthesis site builds a `NamedType` from a `&str`. This has to come before
      the table can be total: a synthesised reference carries an `AstId::fresh`
      the walk never saw, so every consumer must keep tolerating a missing answer
      while those 59 sites exist.
- [ ] `Resolutions::at` made total, once nothing mints an unwalked site. Done
      when the `Option` is gone from the signature — five call sites read it
      today.
- [ ] The impl header's own trait reference. A header whose trait position names
      no declaration — a bodiless derive naming a stdlib trait its module never
      `use`d — still leaves `same_trait` comparing spellings. It is the last
      spelling comparison in trait dispatch, and it closes when the scope answers
      for that position.
- [ ] `ResolvedType` nominal variants carry `DefId`. Done when `ResolvedType`
      holds no `(name, module_source)` pair. The nominal variants are matched at
      ~790 sites, 188 of them in or-patterns that bind one `name` across
      `Struct | Enum | Variant | Newtype | Flags | Resource | GenericInstance`;
      each such group needs its arms split, which is the point — those patterns
      are what let an instantiated spelling be read as a declaration name.
- [ ] Declaration data moved onto `DefTable`; `TypeLookup`'s scope walk deleted.
      Done when no `IndexMap<ModuleSource, IndexMap<String, _>>` remains.
- [ ] `SymbolPath`; the mangled-name parsers deleted; DCE retention keys the
      struct's identity rather than re-deriving a name that must match one built
      elsewhere. Done when `name.rs` exports no function taking a mangled string.

The `Scope` step is the one with a real risk of behaviour change, because the
remaining scopes disagree and unifying them picks a winner. Each disagreement is
a decision made deliberately, with a fixture, rather than one absorbed.

## Consequences

The class stops being writable for four independent reasons, each of which holds
on its own:

- The question cannot be asked wrongly. Resolving needs a reference site, and the
  site determines its module. There is no `(name, guessed module)` entry point.
- The answer cannot be compared wrongly. `DefId` equality is declaration identity,
  and there is no spelling on it to compare instead.
- No case can be folded away. `Unresolved` is its own answer, so "reaches no
  declaration" is never read as "a binder".
- A regression cannot land quietly. A new reference-bearing node without an id
  fails the grammar test; a new query typed on names has nothing to hand it; a
  missed site panics rather than falling back.

Costs and risks:

- The flip reaches every consumer of declaration identity. The honest measure is
  the 867 `*name: &str` parameters, not the 57 spelled `trait_name`.
- `DefTable` is a whole-program table built before elaboration. It must be
  populated on the stdlib snapshot path too, or a snapshot restore resolves
  nothing.
- Moving the declaration data relocates the elaborator's hottest lookups. `DefId`
  indexing is a `Vec` access where the current path is two hash lookups and a
  scope walk, so the expectation is a speed-up; it is measured, not assumed.
- Unifying the scope changes answers where the implementations disagree today.
  Each change is a fixture.

### Measurements

The numbers this design is aimed at, measured over `wado-compiler/src`:

| quantity                                            | at the start | now     |
| --------------------------------------------------- | ------------ | ------- |
| `*name: &str` parameters                            | 867          | 860     |
| independent walks over the `use` declarations       | 3            | 1       |
| implementations of "what does this name mean in M"  | 5            | 5       |
| name-keyed per-module declaration registries        | 7            | 7       |
| `type_implements_trait` callers passing no identity | 16 of 30     | 0 of 30 |
| synthesised reference sites absent from the table   | 59           | 59      |
| mangled-name parsing functions                      | 7            | 7       |
| hand-assembled `(module, name)` keys                | 28           | 22      |
| of those, substituting the writing module           | 6            | 6       |

Each row reaches zero — or one, for the rows counting implementations — when its
step lands. A row that stops falling means a step was declared done while a bypass
survived it, which is what happened to the earlier `trait_name: &str` count, and
the reason this document measures the bypass rather than the parameter.

The bypass row is the one that matters, and it is closed: the query takes a
`DefId` and nothing else, so there is no `None` left to pass. The `*name: &str`
row has barely moved because the names still travelling are the ones the
name-keyed registries force, and those go with the storage.
