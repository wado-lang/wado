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

The pair is not the weakest form of this, and describing the defect as a pair hid
the rest of it. Sites compare a type's name against a bare string literal —
`name == "Result"`, `name == "Option"`, `name == "String"` — with no module in
the comparison at all. Grepping those four spellings finds twenty-six, in
`codegen`, `monomorphize`, `parser`, `kiln`, `synthesis`, `wir_build` and the
optimizer; the true count is higher, since that pattern only asks about four
declarations. `compiler_item.rs` opens by warning against
exactly this, which is what its registry exists to replace, and they survived
anyway. What they decide is not incidental: whether `?` may be used on a value,
whether a value lowers to a CM `result<ok, err>` or to a general payload, whether
a type is represented as nullable. A module declaring its own `Result` answered
yes to all of them.

A search for the pair shape cannot find these, which is why they outlived a
document written to end the class. The rule the design needs is stronger than
"do not build a `DeclKey` by hand": **a spelling never decides which declaration
is meant**, whether it is compared against a pair, against another spelling, or
against a literal.

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

A pass that mints a reference knows its referent — it just constructed it — but
emits a spelling whose site the table never walked, so the elaborator resolves it
from whatever module it is standing in. The removed defect re-enters through the
back door.

The reach is small and worth stating exactly, because the shape matters more than
the count: `AstId::fresh()` appears at 63 sites, 53 of them under `#[cfg(test)]`
and most of the rest building an AST type purely to compute a Component Model
layout, which no scope ever resolves. Two are real reference sites — the
`Self: <this trait>` bound the elaborator mints for a trait's own body, and the
bound list a qualified call rebuilds — and each already knew its referent while
spelling a name anyway.

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
    pub fn get(&self, site: AstId) -> Resolution;  // total, not Option
}
```

Every node that names a declaration carries an `AstId`, and the walk records an
answer for every one; `ast.rs`'s
`every_reference_bearing_node_carries_an_ast_id` scans its own source and fails on
a name-bearing node with no id unless `NAMED_WITHOUT_ID` registers the reason it
needs none.

`get` is total. A site the walk missed is a bug in the walk, not an absent answer a
consumer improvises around, so it panics rather than returning `None`. The three
cases stay distinct on purpose: reading `Unresolved` as `Binder` loses the
diagnostic a name that reaches nothing deserves.

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

An anonymous struct is such a shape and is *not* already its own variant, which
is the one place this rule needs a decision rather than an application. A struct
literal with no type name interns as a `Struct` today, under a spelling
synthesized from its fields, and two literals of the same shape deliberately
reach one type — so there is no declaration to identify and no node to identify
it by.

It does not become a ninth variant. Measured: adding one breaks twelve exhaustive
matches, and that number is the trap — an anonymous struct rides the `Struct`
path through field access, layout and codegen at every one of the 121 sites that
match `Struct` today, and those sites would stop matching it *silently*. The
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
the fields are the key.

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
  itself with an id that no longer resolves. It must also re-enter each survivor
  under the spelling `intern` entered it by — `Box` for the declaration,
  `Box<i32>` for that instantiation. Rebuilding the index on the declaration name
  alone put every instantiation of `Box` on one entry, so the last survivor
  answered for the declaration and for its siblings, and
  `find_decl_type_by_name` — documented to return declarations only — returned an
  instantiation. That is the same defect one layer down: a rendering standing in
  for an identity, and two things rendering the same.

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
- `Resolutions::get` is total and panics on a missing site, so a coverage hole in
  the walk fails on the first fixture that reaches it instead of degrading to a
  name comparison.
- `every_reference_bearing_node_carries_an_ast_id` fails on a new name-bearing AST
  node without an id, so a new reference position cannot be added silently.
- One test asserts `wado-compiler/src` contains no function taking a module and a
  name and returning an identity. That is the shape of the chain this design
  removes, and the one property the type system cannot state.

## Migration

What is left, in the order it has to happen: the storage before the scope,
synthesis before the table can be total, both before the mangling. Each step
compiles, passes the suite, and ends with a mechanical completion check.

- [x] Declaration data keyed by `DefId`. All seven registries key on the
      declaration, and `TypeLookup`'s `lookup_ref` / `lookup_ref_in` /
      `fn_local_first` are deleted with them.
- [x] `Scope` — one implementation of what a name means in a module. The
      per-name import maps are deleted, `SymbolTable::lookup` is deleted, and
      nothing outside `Scopes` answers what a name means. The symbol table
      keeps `lookup_in_module`, which asks a module what it *declares* rather
      than what a spelling means from some vantage — the question
      `Resolutions::declared_in` exposes, and the one `Scopes` is built out of.
      A caller that wants the declaration's symbol row reaches it through the
      identity: `Elaborator::symbol_named` resolves the name once and reads the
      row off the declaring node.
  - [x] Function-local items (`Stmt::Item`). The symbol table collects only
        module-level declarations, so a local `struct` had no identity, and the
        registries kept two name-keyed tiers above them for it: an ephemeral
        `fn_local_*` map that tracked the annotate walk's position, and a
        durable `local_*` map keyed by a mangled storage name no declaration
        carries. `DefTable` now walks function bodies for `Stmt::Item`, so a
        local item is a declaration like any other and the two tiers are one
        `DefId`-keyed map plus the walk's own position.

        The resolve pass records the shadowing at each site: a local item
        enters scope at its own declaration statement and leaves it with the
        block, so `fn a` and `fn b` writing the same spelling resolve to two
        declarations and neither reaches the other's.

        One index survives and says exactly what is left to do. A local item's
        *type* still interns under `{name}@{AstId}`, so the two same-named
        structs stay two types, and a consumer that destructured a
        `ResolvedType` arrives holding that spelling instead of the identity
        the type came from. `local_item_renders` turns it back, in one place,
        and goes when `ResolvedType` carries the declaration — there is then no
        rendered name to ask with. The suite found this rather than review:
        `core:args`'s own test declares a local `Options` and
        `impl Deserialize for Options;`, and the bound stopped holding the
        moment the mangled entry went.
- [x] `Resolutions::get` made total. The `Option` is gone, so no consumer has a
      "no answer" case to write a fallback for.
- [x] Synthesis records referents. The
      completion check "no synthesis site builds a `NamedType` from a `&str`"
      counts the wrong thing, the way the `ResolvedType` step's "790 sites" did.
      Three production sites build one: `wit_consume` generates module AST that
      then goes through the ordinary loader and resolve pass, so its names are
      resolved at their own sites like any source; `cm_abi` and
      `component_model` build a type purely to compute a Component Model
      layout, which no scope ever resolves. None of those is the defect.

      The defect is a reference a consumer resolves from a spelling that was
      minted beside a known referent, and there is one: the bound list a
      qualified call rebuilds pairs each bound with the `FqTraitName` it stands
      for in a side map, keyed by an `AstId` the walk never saw. Nothing queries
      those ids today — `Resolutions::get` is total across the corpus, which is
      the evidence — so the invariant held by that pairing rather than by the
      type. `TraitBound::resolved` carries it now: a parsed bound leaves it
      `None` and is answered at its own site, a rebuilt one already knows.

      This was written as gating the totality step, on the theory that a
      synthesised reference carries an `AstId::fresh` every consumer must
      tolerate a missing answer for. It did not gate it, for the reason above,
      and totality landed first.
- [x] `ResolvedType` nominal variants carry `DefId`. Done when `ResolvedType`
      holds no `(name, module_source)` pair.

      The sites that read the pair as an *identity* are the ones that change
      an answer, and `TypeTable::decl_of_type` already serves them — it
      predates this design, and resolves a monomorphization and a
      `GenericInstance` back to the declaration they were spelled from. Each
      migration so far has been one call replacing a destructure-and-look-up,
      and each deleted the guard that existed to decide whether the pair meant
      what the caller hoped: `contains_variant(name)`, a probe of two per-kind
      indexes, `ref_name == struct_name`.

      But the completion criterion is the fields, not the answers, so the
      count that governs the work is how many sites *bind* one: **544, across
      55 files**, of 880 patterns over the eight nominal variants (the rest
      match with `..` and are untouched). The earlier "~790, and most of them
      are fine" was measuring the wrong set and reading it optimistically.

      The eight cannot be migrated one variant at a time. They are fused by
      or-patterns — `Struct { decl_name: name, .. } | Variant { name, .. } |
      Resource { name, .. } => …` binds one `name` across alternatives, so an
      alternative whose field changed no longer binds it and the arm has to be
      split. All 115 such groups mix more than one nominal variant, and 43 of
      them include `Struct`. Flipping `Resource` alone, the smallest at 27,
      would split arms shared with variants that have not moved. They go
      together.

      Flipping all eight at once was tried and measured: **754 compile errors**
      in `wado-compiler`, concentrated in `tir.rs` (176), the elaborator (about
      250 over `method_call`, `expr`, `reify`, `stmt`, `method_lookup`,
      `trait_query`, `operators`), and thinning out across the optimizer,
      monomorphizer, synthesis and codegen. That is the whole change in one
      non-compiling step, which is more than a session can verify, so it lands
      the other way round:

      - the head types (`StructDef`, `AnonStructId`) and the renderer land
        first — `TypeTable` holds an `Arc<DefTable>` and answers `def_name` /
        `def_module`, which is what lets a flipped variant spell itself at all;
      - each nominal variant then gains its `def` **beside** the pair, so every
        construction site is forced to produce an identity while every reader
        still compiles;
      - readers move to `def` file by file, each step green;
      - the pair fields are deleted last, which is when the criterion is met.

      The construction sites are the real work, not the readers: 107 calls to
      `make_struct` / `make_variant` / `make_enum` / `make_flags` /
      `make_newtype` / `make_resource` / `make_generic_instance` /
      `make_monomorphized_struct*`. An elaborator caller has the identity from
      the site it resolved; a `monomorphize::substitute` caller builds a type
      out of a mangled spelling and has nothing, so it has to reach the base
      declaration through `decl_of_type`. Those are the sites this design is
      actually about, and no shim can carry them.

      The expand step was run to the point where that is provable, and the
      numbers are worth keeping. Adding `def` beside the pair on the four
      simple variants — `Enum`, `Variant`, `Resource`, `Flags` — is 88 errors,
      of which **79 are patterns that list every field** and take a `..`
      mechanically. The other **9 are constructors**, and they do not yield to
      a rule: about 25 callers reach them, and the `synthesis/*` ones hold a
      name and a module and nothing else. Several of those are not
      constructing at all — they re-intern a type that already exists purely to
      get its `TypeId` back, which is `find_decl_type_by_name` written as a
      `make_`. Rewriting them to ask for the declaration is the fix, and it is
      per-site work, not a substitution.

      So the ratio to plan against is roughly nine parts mechanical to one part
      judgment, with the judgment concentrated in synthesis and the
      monomorphizer. Budget the step by the constructors, not by the error
      count.

      Do not trust a grep for the pair shape. It also matches
      `ExprKind::GlobalVarGet`, which is a NIR node carrying a global's name —
      a real problem, and a different one. Read each candidate.

  - [x] An instantiation records the declaration it came from.
        `TypeTable::decl_of_type` answers for a nominal type out of
        `symbol_by_type`, but for a `GenericInstance` it _derives_ the answer at
        query time: `find_decl_type_by_name(name, module)` scans for the base
        declaration's own type, then reads that. So the identity of `Option<i32>`
        depends on the bare `Option` type still being in the table — and
        `TypeTable::prune` keeps only what is reachable, which after
        monomorphization is the instances, not the base.

        This is not hypothetical. Rewriting `as_option` to compare declarations
        instead of the literal `"Option"` passed every unit and fixture test and
        broke `package-gale` at WIR validation with
        `type mismatch: expected (ref $type), found nullref` — codegen reads
        `as_option` to decide a nullable representation, and got `None` for a
        value that is an `Option`. `is_result` took the same fix and passed,
        which is what makes the gap easy to miss.

        The fix is that interning an instantiation registers it against its base
        declaration, the way `register_mono_type` already does for
        monomorphizations, so the answer is stored rather than re-derived from a
        spelling that outlives what it names. Nothing else in this step is safe
        until it is: every consumer that stops reading `(name, module_source)`
        off a type starts depending on `decl_of_type` instead.

  - [x] An anonymous struct is a shape, not a nameless declaration.
        `ResolvedType::Struct` is reached two ways: from a `struct`
        declaration, and from a struct literal with no type name, which
        `resolve_struct_literal` interns under a synthesized `__anon_{…}`
        spelling built from its own field list. The second has no declaration
        and can get no `DefId` — the literal is not a declaration site either,
        since two literals of the same shape intern to one type by design.

        `Struct`'s *head* becomes `StructDef::Decl(DefId) | Anon(AnonStructId)`
        rather than the variant splitting in two. See the `Types carry DefId`
        section for the measurement that decides it: a ninth variant breaks
        twelve exhaustive matches and silently orphans the anonymous struct at
        the other 121 sites that match `Struct` today.

  - [x] TIR declarations carry identity. This is the part that is structural
        rather than mechanical. `Lowering` keys its variant-case and
        struct-field maps by `TirVariant` / `TirStruct`'s own
        `(name, module_source)`, so the pattern translator reaches a case index
        through a spelling. Those maps key on a `DefId` only once the TIR
        declaration carries one, which reaches to codegen — and it is what
        `ResolvedType` losing the pair forces, since a downstream consumer will
        have nothing to read off the type.

        What a struct's identity is, here, is the same pair the type carries:
        `TirStruct` holds `(StructDef, Vec<TypeId>)`, not a bare `DefId`. A
        `DefId` alone cannot key the field map, because `TreeMap<String, i32>`
        and `TreeMap<K, V>` share a declaration and not a field list; the
        head-and-args pair is the type's own intern key, so keying on it is
        the same question the map was asking, minus the rendering step. The
        variant map does take a bare `DefId`: case names and indices belong to
        the declaration, and an instantiation adds nothing to them.

        Carrying the identity is what emptied most of `synthesis/traits.rs`,
        which held 33 of the 80 remaining `decl_named_in` callers and now
        holds none. Every one had the same shape: a `collect_*` pass walks
        `module.structs` / `.variants` / `.enums`, keeps the name and drops
        the declaration, and the consumer asks for the declaration back. The
        fix is to keep it — the collectors return it, and the synthesis
        targets (`ReflectTarget`, `ReflectVariantTarget`, `ReflectEnumTarget`)
        carry it too.

        That collapse is also what surfaced the defect the design predicts.
        A function-local `struct` has no module-level declaration, so every
        name-keyed lookup for one answers `None` — and `lookup_field_type`
        was name-keyed. A local struct's fields were reachable while its name
        was in scope (a struct literal resolves through the function-local
        tier), and unreachable once its type arrived through a generic
        instantiation carrying no spelling: `parse::<Options>(…)` on a local
        `Options` typed every field `unknown`. Field lookup now asks the
        struct's head, and the two paths that had disagreed became one.

        What is left of that residue is a different question, and a smaller
        one. Ten of the forty-seven are `TypeTable`'s own stdlib constructors
        — `make_future`, `make_stream`, `make_byte_list` — each spelling a
        prelude type with a string literal and a guessed module. They are
        fabrications by the same definition, but they name types the compiler
        itself owns, which is what the compiler-item registry is for; they
        close by gaining a `CompilerItem`, not by carrying an identity from
        somewhere upstream.

        Carrying the identity is what made four more `decl_named_in` callers
        collapse — `reify_variant_decl`, the `Box` plan, `instantiate_struct`
        and the reflect bridge each already held the declaration they were
        asking for by name — and it exposed one site that was asking for a
        declaration that never existed: an anonymous struct literal looked up
        `__anon_{a:i32}` in the declaration index, which only answered because
        the literal path had registered its own rendered spelling there first.
        Interning the shape removes both halves.
- [x] `RequiredTrait` carries a `Resolution`. A qualified call's trait prefix
      can name a type-parameter binder or reach no declaration, so a bare
      `DefId` cannot stand for it — the answer the site already has can.

- [ ] The flip's e2e cost, measured. Running the whole suite to completion
      for the first time since the flip found **120 failing fixtures**, all of
      one shape: a type now renders or identifies differently than the
      registry that has to find it. Two are fixed — a synthesized effect
      dispatch struct asked the declaration index for itself (59), and a
      struct literal naming nothing `expect`ed a declaration (6). Three
      remain, and each is the pair surviving on one side of a boundary the
      identity already crossed:

      - tuple trait dispatch cannot resolve `[a,b]^Trait::method` at all.
        Half of it was the spelling: `FqTypeName::tuple` knew a tuple is
        `[a,b]`, `FqTypeName::builtin` did not, and every by-name route
        reaches the family through `builtin` now that it is a declaration —
        fixed, with a test that the two routes agree. The other half is the
        module, and it is the same shape one level up: see the entry below;
      - an anonymous struct's synthesized impl registers under a key the
        bound check does not ask for, now that its head is a shape;
      - a function-local struct's type rendered as its plain declared name
        while the WIR registry keys the mangled `Point@AstId(N)` storage
        spelling. `DefTable` could not tell a local declaration from a
        module-level one, so nothing downstream could either; `Def` now
        carries `function_local` and `TypeTable::decl_render_name` applies
        the disambiguator. `def_name` stays the *declared* name — what an
        `impl` header spells — and the two namespaces are finally distinct
        at the source rather than by convention.

      The lesson for the budget is the one the migration section already
      states, and it was still underestimated: the constructors are the work.
      Every one of these is a construction site that kept its spelling.

      All but one are now closed, leaving a single fixture at two
      optimization levels — a generic `struct` declared inside a `test`
      block, which reaches WIR build and fails to find its instantiation
      registered.

      Its lesson is about measurement, not identity. The WIR panic names the
      type through `mangle_type_name`, whose `GenericInstance` arm reads
      `def_name` — the *declared* name — so it prints `Box` whether or not
      the declaration is flagged function-local. Two rounds were spent
      inferring "the flag is unset" from that message; probing the flag
      directly showed it set. A diagnostic rendered through a different path
      than the one under test is not evidence about that path. A local `type
      UserId = …` still reports `UserId does not implement Inspect`; giving
      `synthesis/traits.rs`'s newtype loops the type's rendered name instead
      of the declared one changed nothing, measured, and was reverted. And an
      anonymous struct's synthesized `ReflectStruct` impl does not answer the
      `Serialize` bound that `core:serde`'s blanket derives it from.

      That one is fixed, and it was the gap this design predicts.
      `walk_structural_derive_members` reached a struct's fields through
      `def.decl()?` — `None` for a shape — so the walk bailed for every
      anonymous struct, no bound-driven synthesis request was recorded, and
      a `Serialize` bound the type structurally satisfies was declined.
      Asking the head answers for both kinds. Four fixtures green.

      Worth recording is how it was found, because three guesses ahead of it
      were wrong. Probes eliminated three of the five `TraitBoundNotSatisfied`
      emit sites, and a fourth hardcodes `Ord`, leaving `enforce_single_bound`
      — the primitive every bound-enforcement path funnels through. Reading
      *back* from a site the measurement had pinned reached the walk in one
      step. Reading *forward* from the symptom had missed it three times.

      One trap, worth naming because it caught this investigation: the
      diagnostic's spelling proves nothing about the type's. Every unresolved
      -bound message runs through `display_type_name`, which calls
      `strip_local_item_id` — so a local item prints `UserId` whether or not
      its type renders `UserId@AstId(N)`. Read the intern key, not the
      message.

      Read directly, it names the disagreement exactly. Two probes:

          PROBE nt.name="UserId@2" rendered=Some("UserId@2")
          PROBE unresolved call_name=
              "…/UserId^core:prelude/traits.wado/Inspect::inspect"

      The synthesis side agrees with itself — the newtype's declared-side
      name and its type's rendering are both `UserId@2`, so `function_local`
      reaches that path. The *call* names the receiver `UserId`, plain. So it
      is a spelling mismatch after all, on the call-minting side, and one end
      of it is now known precisely.

      Read back from the pinned site, the chain is: `lib.rs`'s violation
      report ← `wir_package.trait_bound_violations` ←
      `wir_build::translate`'s `unresolved_trait_call_or_trap`, whose `name`
      is a NIR `FunctionRef` that resolved to nothing ← minted during
      elaboration. `assert id == 42` builds its message through the
      Formatter / `Inspect` / `String` stack, so the producer is the
      template-formatting path — `elaborator::assert` is only the capture
      scanner and mints nothing. Probe where that call's `FunctionRef` is
      built and compare its receiver spelling against `UserId@2`.

      What is not yet known is which producer mints that call. Routing
      `TypeTable::fq_type_name`'s `Newtype` / `Enum` / `Variant` / `Flags` /
      `GenericInstance` arms through `decl_render_name` — they read
      `def_name` while the `Struct` arm reads `struct_head_name` — was the
      obvious candidate and changed nothing, measured, so it was reverted
      too. `method_call`'s newtype paths were checked by reading: they go
      through `nominal_head`, which renders correctly.

      The next candidate is `TypeTable::type_name`, whose `Newtype` arm
      (`tir.rs`, the `strip_local_item_id(self.def_name(*def))` line) hands
      back the stripped spelling by construction — it is the *display*
      renderer. If something on the call path reaches for it, that is the
      seam, and it would be the fourth instance of this migration's one
      recurring mistake: a display rendering used where an identity was
      meant. Probe before editing — the two `eprintln!`s above show both
      ends in a single build, and three plausible fixes in a row measured as
      no-ops when reasoned about instead.

      Four attempts that changed nothing were reverted rather than kept on
      the argument that they were more correct in principle. That rule is
      what makes the measurements above worth anything.

      The method is the transferable part. Every probe this migration ran
      answered its question; every hypothesis reasoned out from the code
      cost a build and moved nothing. These types are dense enough that
      reading them predicts the wrong producer more often than the right
      one — so print the two names and compare them, rather than deducing
      which one must be wrong.

- [x] Tuple trait dispatch resolves again. The tuple family becoming a
      declaration gave it a *module*, and three of the four routes into
      `FqTypeName` then spelled it `core:prelude/types.wado/[]<i32,String>` —
      a name no impl is registered under and no other mangler produces. Trait
      dispatch resolved to nothing: WIR build reported `[i32,String]` does
      not implement `Inspect`, lowering minted an extern stub for a name the
      package defines, and `package-gale` failed on the latter.

      The tuple is the one declaration whose spelling is not
      `Module/Head<args>` — it is `[a,b]`, bare — so the decision belongs at
      every constructor, not just at `tuple`. `builtin`, `of_head` and
      `declared` now agree with it, and the unit test walks all four routes.
      80 fixtures across `inspect_*`, `tuple_*`, `serde_json_tuple` and
      `variadic_tuple_literal_index`, at O0 and O2, were 64/16 and are now
      80/0.

      One refuted theory is worth recording so it is not re-run. The
      declaring module and the impl module genuinely do differ for this
      family — the declaration is in `types.wado`, every `impl … for [..T]`
      in `tuple.wado` — and that genuinely violates the convention
      `module_source_for_trait_impl` falls back on. Making it answer `None`
      for a tuple changed nothing, 64/16 before and after. The module was
      never the problem; the spelling was, twice.
      `module_source_for_trait_impl` reads a receiver's declaring module and
      the monomorphizer falls through to it "by convention" when no per-type
      impl answers — the convention being that a generic `impl` for a type
      sits in that type's own module. The tuple family breaks it: the
      declaration and its `#[compiler_item("tuple")]` are in
      `core:prelude/types.wado` while every variadic `impl … for [..T]` is in
      `core:prelude/tuple.wado`. A call to `[char,char]^InspectAlt::inspect_alt`
      is minted under `types.wado` while the definition is emitted under
      `tuple.wado`, and lowering's stub-shadowing assertion catches it —
      `package-gale` is the reproducer. The convention is the thing to remove:
      a receiver's impl module is a question `TraitEnv` answers, not one a
      declaring module stands in for.

- [ ] `SymbolPath`; the mangled-name parsers deleted; DCE retention keys the
      struct's identity rather than re-deriving a name that must match one built
      elsewhere. Done when `name.rs` exports no function taking a mangled string.

The storage came before the scope because it is what the scope was for.
`TypeLookup::lookup_ref` walked fn-local, module-local, current module and
imports to turn a spelling into the `(module, name)` pair its registry was keyed
by; there was nothing else it did. Each registry that moved to `DefId` deleted
one caller of that walk, and the walk went when the last one did. The other order
would have meant keeping a flat name scope serving seven kind-partitioned
registries at once, which it cannot: a query that misses in one registry must
fall through rather than shadow, and one flat answer cannot both shadow and fall
through.

Function-local items are what still keeps two name-keyed tiers above the
registries. A local `struct` has no `DefId`, and its durable entry is keyed by a
mangled storage name that no declaration carries, so `TypeLookup` reads those two
maps directly. They go when a local item gets an identity scoped to its declaring
function.

Unifying the scope was the step with a real risk of behaviour change, and the
disagreements it surfaced were settled rather than absorbed. Two of them are
worth stating, because both were narrower than they looked:

- The per-name import maps were not one scope but five questions sharing a map.
  Each reader gets the one it meant — `imported_as` where the aliasing is the
  point, `in_scope` for a tie-break between same-named foreign declarations,
  `value_named` for a name written where a case is reachable. Sharing one map is
  what made them look like a scope.
- The two trait-scope helpers were blind to the prelude so that an ambient
  compiler trait stayed distinguishable from a same-named user `trait`, and both
  their call sites read `None` as "then it is the ambient one". The layered scope
  finds that ambient declaration, and it compares equal, so the answer does not
  move; the distinction is carried by the `decl_index` filter, which is where it
  belongs. The one case that now differs — the prelude declaring a same-named
  trait that is not the compiler item — is an ambiguity worth answering rather
  than defaulting past.

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
| `*name: &str` parameters                            | 867          | 880     |
| independent walks over the `use` declarations       | 3            | 1       |
| implementations of "what does this name mean in M"  | 5            | 1       |
| name-keyed per-module declaration registries        | 7            | 0       |
| `type_implements_trait` callers passing no identity | 16 of 30     | 0 of 30 |
| spelling comparisons in trait dispatch              | 2            | 0       |
| synthesised references a consumer re-resolves       | 2            | 0       |
| mangled-name parsing functions                      | 7            | 5       |
| `decl_named_in` callers — the name-keyed residue     | 121          | 47      |
| `decl_key_or_local` occurrences — the fabrication   | 26           | 30      |

Each row reaches zero — or one, for the rows counting implementations — when its
step lands. A row that stops falling means a step was declared done while a bypass
survived it, which is what happened to the earlier `trait_name: &str` count, and
the reason this document measures the bypass rather than the parameter.

Five rows are closed. The query takes a `DefId` and nothing else, so there is no
`None` left to pass; trait dispatch compares declarations at both ends, with no
spelling comparison left in the path; no declaration's contents are reached by a
name and a module any more, which took `TypeLookup`'s scope walk with it; the
resolution table is total, so a consumer has no missing answer to fall back from;
and one implementation answers what a name means in a module.

The synthesis row closed without the step that was supposed to close it, which is
worth recording because the estimate was wrong in a useful direction. Two suite
runs — one removing the fresh-id escape from the call-site assertions, one moving
the assertion inside `get` so `declared`'s callers were covered — showed the
escape answering for nothing. The measurement that mattered was never "how many
sites mint a fresh id" but "how many of them a consumer resolves", and the answer
had already reached zero.

The scope row is at one. `module_import_scope` and `ModuleImports` no longer
answer what a name means — the per-name import maps are deleted — though the
former still computes a set of visible spellings for `module_visible_types`,
which is a heuristic rather than a resolution and does not belong to this
design. `SymbolTable::lookup` was the last one and is deleted; what remains
there is `lookup_in_module`, which answers what a module declares, not what a
spelling means from a vantage.

Two rows have gone _up_, and both say the same thing. `decl_key_or_local` sat at
24 for one commit, when a qualified call's required trait was made a `DefId`, and
came back — now 30 — when that turned out to be wrong: the required trait can
name a type-parameter binder or a name that reaches no declaration at all, and a
`DefId` cannot stand for either. `RequiredTrait` carries a `Resolution` now, and
the row still climbed, because what it counts is the pair a consumer *renders*
to reach a name-keyed neighbour, and those neighbours are in `ResolvedType`. The
`*name: &str` row moves for the same reason. Both rows fall when the types carry
identity, not before, and neither is evidence about the steps that have landed.
