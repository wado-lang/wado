# WEP 2026-08-12: Symbol identity — a name is an encoding, never an identity

## Context

[Reference resolution](./wep-2026-08-10-reference-resolution.md) made a
declaration's identity `DeclRef` and named its own exception:

> A mangled function name is the one place a name is still the currency, because
> Wasm requires one.

That exception is one of three places identity is still carried as text. This
WEP covers all three, because they are one defect with three symptoms: a
`String` standing where a value belongs. Where it does, two things that should
differ can *be* the same thing — not a lookup going wrong, but two symbols
becoming one.

### Three properties have been confused

Three guarantees have been pursued under the word "collision". Only the third
prevents one:

| property        | established by                 | says                                             |
| --------------- | ------------------------------ | ------------------------------------------------ |
| unforgeable     | WEP 07-29 — `MangledName`      | a name can only be minted by an authority        |
| namespace-typed | WEP 07-28 / 07-29 — `DeclName` | a declaration name cannot pass as a mangled one  |
| **injective**   | nothing yet                    | distinct symbols never render to the same string |

The first two constrain *who writes a name* and *where it may be handed*.
Neither says anything about two different symbols arriving at the same
characters. That is why stricter name types kept landing while collisions kept
arriving: the property being enforced was never the property being violated.

Injectivity is a statement about *every pair* of symbols. A disambiguating
segment added where a collision was observed establishes it for that pair and
says nothing about the rest, so no sequence of local repairs accumulates into
it. The cycle is not a discipline failure — it is the wrong shape of fix.

### The measurements

|                                                                |     |
| -------------------------------------------------------------- | --- |
| maps keyed by a `String` name                                  | 633 |
| `name: String` fields                                          | 208 |
| `(ModuleSource, String)` identity pairs                        | 133 |
| ad-hoc separator scans over a name (`::` `^` `<` `/` `$` `__`) | 115 |
| `format!` sites minting a name outside `name.rs`               |  87 |
| name families declared in `name.rs`                            |  42 |
| production `AstId::fresh()` sites                              |  12 |

`wado-compiler/AGENTS.md` states the rule: "Name mangling and monomorphization
go through `name.rs`. No other component knows a name format." Rows five and six
are what that rule is worth while the currency is `String` — it binds the
producers that choose to obey, and `format!` is available everywhere else. The
same finding one level up is why WEP 08-10 concluded that constraining the
derivation moves the defect rather than removing it.

### Symptom 1 — the mangled name is not injective

A mangled name is a grammar of `/`, `::`, `^`, `<`, `>`, `,`, `&`, `$`, `@`
written over atoms that may contain those same characters. A `ModuleSource`
spelling carries `/` and `:`, and `//`, `<`, `>` for remote URLs and `<entry>`.
A declaration name carries `$` from `namespace_member_alias`, `@` from
`mangle_local_item_name`, and `[`, `]`, `&`, `(`, `)`, `!` from the tuple,
reference and unit heads.

Every join of two variable-length operands is therefore ambiguous:

| join                                                  | merges                                                   |
| ----------------------------------------------------- | -------------------------------------------------------- |
| `cm_wrap_async_func_name` — `_`                       | `("outgoing_body","write")` = `("outgoing","body_write")` |
| `dispatch_wrapper_name` — `__`                        | `("A__B","c")` = `("A","B__c")`                           |
| `case_extract` / `case_construct` / `field_get` — `$` | `(A$B, C)` = `(A, B$C)`                                   |
| `shallow_copy_helper_name` — `$shallow` suffix        | shallow-of-`X` = deep-of-`X$shallow`                      |
| `dispatch_global_name` vs `dispatch_wrapper_name`     | shared `__effect_` prefix                                 |
| `MangledName::in_module` — `/`                        | `core:prelude` + `list.wado/Point::x` = `core:prelude/list.wado` + `Point::x` |

The last is the primary function namespace, and both modules in it are stdlib
modules: a local name is itself module-qualified, and one module's rendering is
another's prefix at a `/` boundary.

The table is a record, not a specification. A test asserting these six pairs
render differently would be this WEP's own argument made at the test level — six
pairs fixed, nothing said about the rest — and after §2 each pair is two
distinct enum values, so such a test could not fail. The specification is the
round-trip property, and it is the only test this design asks for.

Where a collision goes: `wir_build::functions::register_single_function` returns
when a `func_map` key is already claimed. The loser is dropped and its calls
resolve to the winner's body — a Wasm validation type mismatch several phases
later, or nothing at all. Four assertions guard four narrower namespaces
(`lower::translate`, `monomorphize`, `optimize::dce`, `link`); each was added
after a specific collision and none covers a family invented afterwards.

### Symptom 2 — the lookup key is a pair of strings

The trait-driven registries key on

```rust
pub(crate) type DeclKey = (ModuleSource, String);
```

`SymbolTable::define` is reached only through `analyze::define_unique`, which
diagnoses a duplicate, so that pair *is* unique for declarations. The defect is
not the pair's ambiguity — it is that a caller with no declaration can build one
anyway. `Elaborator::decl_key_or_local`:

```rust
if self.annotate_ctx.trait_ctx.type_params.contains_key(name) {
    return (self.current_module_source.clone(), name.to_string());   // a binder
}
self.canonical_decl_key(name)
    …
    .unwrap_or_else(|| (self.current_module_source.clone(), name.to_string()))
```

Both arms *invent* a key in the declaration namespace. The first gives a binder
`T` the key a module-level `struct T` would get — the conflation WEP 08-10
removed from `ImplReceiver` ("a type parameter has no spelling in the
declaration namespace"), still live here because `DeclKey` is a pair of strings
rather than the `DeclRef` that WEP introduced. The second gives an unresolved
name the writing module, so a name that reaches no declaration occupies a key a
later declaration can grow into.

`DeclRef` already draws both distinctions — `Binder(AstId)` and `Unresolved` are
separate variants. They are lost on the way to the key, because
`Resolutions::declaration_named` returns `Option<(ModuleSource, String)>`: the
table takes the `AstId` identity and flattens it back to a name pair, and
`Resolutions` stores `decls: IndexMap<AstId, (ModuleSource, String)>` to be able
to. That is WEP 08-10's own `.base_name()` debt, at the point where identity
becomes a key.

Two by-name indexes below that pick a winner they have no vantage to pick.
`Resolutions::prelude_declarations` folds every prelude module's declarations
into one `IndexMap<String, AstId>` with `or_insert` — first writer wins across
~20 modules, where `define_unique` only checks within one.
`SymbolTable::register_import` has no duplicate guard, so two `use` clauses
importing one name leave the last, and `module_scope_lookup` consults imports
before the module's own declarations, so an import silently shadows a local
declaration.

### Symptom 3 — `AstId::fresh()` is where identity leaves the system

`AstId` is an `AstIdSpace` — a process-global atomic counter — plus a
module-local dense index. Two consequences follow, and both are already known to
the codebase:

- Its value is **not deterministic across processes**, which is why
  `mangle_local_item_name` encodes only `id.local()`: "encoding it would leak
  into mangled WIR names and make compiler output non-deterministic."
- `AstId::fresh()` mints in a reserved space for "a synthesized node that no
  module owns and no source position wrote", and its doc says such an id "must
  never become a fact / symbol key".

`Resolutions` is built by walking the loaded modules, so it cannot answer for a
fresh id. The invariant is therefore stated with an exemption, in all three
places it is asserted:

```rust
debug_assert!(answer.is_some() || site.is_synthetic(), …);
```

A synthesized site has no answer, so each consumer grows its own — the second
answer table beside `Resolutions` that WEP 08-10 removed in one place
(`NamedType::source_interface`) and the migration recreated in others. The
clearest is `elaborator/method_call.rs`, which mints fresh ids purely to key a
private table it fills in the same expression:

```rust
let mut resolved: IndexMap<AstId, FqTraitName> = IndexMap::default();
let bounds: Vec<ast::TraitBound> = named.iter().map(|b| {
    let id = AstId::fresh();
    resolved.insert(id, b.clone());
    ast::TraitBound { id, name: b.base_name().to_string(), … }
}).collect();
```

It holds `FqTraitName` — the identity — and writes a `String` into the node
beside a fresh id, then carries the identity alongside so it can be recovered.

`synthesis::cm_binding::types::type_id_to_ast_type` is the same conversion
without the side table: it turns a resolved `TypeId` back into `ast::Type` with
bare names and fresh ids, and its comments are the cost —
"must not pick up the CM source", "resolves to *this* type's package — not
whichever unique-by-name match the registry happens to find first. Without the
hint, the three variant `ErrorCode`s are non-unique and resolution falls through
to the lone `wasi:cli` enum, mis-lifting a filesystem variant as an i32."

That is identity being discarded and then reconstructed from a name, with a
`pkg_hint` string added to make the reconstruction work. It is the whole defect
in one function.

## Decision

> A name is an encoding of an identity, never the identity. The identity is a
> Rust value; the string is produced from it, at one place, and never parsed.

Three parts, one per symptom. They are independent — each is separately
valuable — and they share one property, which is why they are one WEP: each is
carried out by flipping a type, so the compiler enumerates the sites that have
not moved. That is what makes the work finite and the counts above the progress
metric.

### 1. `Symbol` completes the shape `FunctionId` already has

`FunctionId` is the canonical key of `NirPackage::func_index` and is already
half-structural:

```rust
pub enum FunctionId {
    Free(FreeFunctionName),   // { module_source, name: String, base_name: Option<String> }
    Method(MethodName),       // { module_source, struct_name: FqTypeName,
                              //   trait_name: Option<FqTraitName>, method_name: String }
}
```

`MethodName` names its receiver and its trait by their declaring modules — WEP
08-10's discipline, already applied. What remains a `String` is exactly what
collides: `FreeFunctionName::name` holds a fully mangled string, so every
synthesized family and every monomorphized method rewritten by the call-rewrite
path rides in it as text; `MethodName::method_name` holds `method<args>`, fusing
the method's name with its type arguments.

This is not a new type. It is that one finished:

```rust
pub enum Symbol {
    /// A `fn` written in source, with the arguments it was instantiated at.
    Function { module: ModuleSource, name: DeclName, args: Vec<FqTypeName> },
    /// A method at an impl site. `impl_module` is where the `impl` block is
    /// written; the receiver and the trait each name their own declaring module.
    Method {
        impl_module: ModuleSource,
        receiver: FqTypeName,
        tr: Option<FqTraitName>,
        name: DeclName,
        args: Vec<FqTypeName>,
    },
    /// A helper no declaration names.
    Synth(Synth),
}

pub enum Synth {
    ValueCopy { ty: FqTypeName, depth: CopyDepth },
    CaseExtract { variant: FqTypeName, payload: FqTypeName },
    CaseConstruct { variant: FqTypeName, payload: FqTypeName },
    VariantTag { variant: FqTypeName },
    FieldGet { owner: FqTypeName, field: FqTypeName },
    Dispatch { label: FqTypeName, part: DispatchPart },
    CmWrapAsync { interface: DeclName, method: DeclName },
    ParamSpec { base: Box<Symbol>, ordinal: u32 },
    ModuleInit { module: Option<ModuleSource> },
    ClosureFunctor { module: ModuleSource, ordinal: u32 },
    ConstObject { ordinal: u32 },
    Test { index: u32, meta: TestMetadata, name: Option<String> },
}
```

`Synth` is the substance. The eight `$`-and-`__` families are a sum type spelled
in strings; written as an enum their operands are fields and cannot merge —
`CaseExtract { A$B, C }` and `CaseExtract { A, B$C }` are different values
whatever they render as. A derived family nests
(`ParamSpec { base: Box<Symbol> }`) rather than suffixing, so no decomposition
question arises. Equality is structural, so every map that keys a function keys
an identity.

`Symbol` is deliberately not built from `AstId`. A mangled name must be
deterministic across compilations — golden fixtures, the Wasm name section,
reproducible builds — and `AstIdSpace` is a process-global counter.
`DeclRef` answers "which declaration"; `Symbol` answers "which emitted
function", which is a declaration *plus an instantiation*, plus the families
that have no declaration at all. Different questions, different types.

### 2. Rendering is one direction, and its injectivity is proved

`render: Symbol -> MangledName` is the only bridge, and the crate holds no
inverse. Every question a consumer answers today by splitting a name — the
declaring module, the receiver, the trait, the type arguments, "is this a value
copy helper" — becomes a field access or a `matches!`.

Two boundaries need separating, and they take different treatments:

- A *composite* operand — an already-rendered name — is **bracketed**. Escaping
  it would rewrite the structure it is a rendering of.
- An *atom* — a `ModuleSource` spelling, a declaration name — is **escaped**:
  `%` is the escape and `%xx` encodes a metacharacter inside it. Ordinary names
  contain no metacharacter and render unchanged, so dumps stay readable and the
  escape appears only where it is needed (`<entry>`, a remote URL, a namespace
  alias, a local item).

A test-only `parse` recovers the structure, and a property test over generated
symbols asserts

```
∀ s. parse(render(s)) == Some(s)
```

A total round trip *is* injectivity — for all pairs, including the ones no
fixture exercises, which is the property no site-local fix can give. `parse`
lives under `#[cfg(test)]`: it exists to discharge the obligation, not to be
called.

While families are being migrated one at a time, `SymbolTable::intern` is the
backstop: it panics when a rendering is already claimed by a *different*
`Symbol`, naming both. One always-on check in place of four narrow ones, and
`SymbolId` being `Copy + Eq + Hash` is what makes the 633 name-keyed maps
cheaper than the pairs they replace rather than more expensive.

### 3. A key is an identity, and an index answers a set

`DeclKey` becomes `DeclRef`. The two fabricating arms of `decl_key_or_local`
stop being expressible: a binder is `Binder(AstId)`, which no declaration can
equal, and a name reaching nothing is `Unresolved`, which is not a bucket.
`Resolutions::declaration_named` returns `DeclRef`, so identity is no longer
flattened at the point it becomes a key, and `Resolutions::decls` — the map that
exists to support the flattening — goes with it.

A caller that "must produce some bucket" is a caller whose question was wrong.
The compiler lists them when the type flips; each is a real question with no
answer today, and each needs a decision rather than a default.

Below that, the rule for every by-name index:

> A by-name index maps a spelling to the **set** of declarations that spell
> themselves that way. Choosing among them is scope resolution, which happens
> once, at the reference site.

`prelude_declarations` becomes `IndexMap<String, SmallVec<AstId>>`, so a name
declared in two prelude modules is an ambiguity to diagnose rather than a
first-writer-wins silence. `register_import` rejects a second binding for one
name instead of overwriting. `blanket_impls` and `trait_impl_modules`, which WEP
08-10 left keyed by trait name, re-key on `DeclRef` —
`impl_target_key_at` already computes the identity in the same loop.

### 4. A synthesized reference carries its referent

`ast::Type` gains a resolved variant:

```rust
pub enum Type {
    Named(NamedType),
    …
    /// A type the compiler already knows the declaration of. Carries no
    /// spelling to re-resolve and no site for `Resolutions` to answer for.
    Resolved { decl: DeclRef, args: Vec<Type>, span: Span },
}
```

`type_id_to_ast_type` emits it instead of a bare name and a fresh id, and its
`pkg_hint` machinery — the string added to make reconstruction work — is
deleted rather than fixed. `method_call.rs`'s synthesized bounds carry their
`FqTraitName` directly, and its private `IndexMap<AstId, FqTraitName>` goes with
them.

`AstId::fresh()` then names only nodes that genuinely refer to nothing, and the
`|| site.is_synthetic()` exemption on "every reference site is resolved" can be
removed — which is the test that this part is finished.

This is what WEP 08-10 already prescribes for the one case it hit ("a
synthesized reference … knows its referent, so it is recorded directly rather
than spelled and re-resolved"), generalised: **the compiler must never write a
name it will have to read back.** Every one of the 12 production `fresh()` sites
holds the identity at the moment it discards it.

## Why the class stops being writable

- **A new name family cannot merge its operands.** It is a `Synth` variant, and
  a variant's fields are not a string. There is no `format!` to reach for,
  because the mint takes a `Symbol`.
- **An ambiguous rendering fails at authoring.** The round-trip property covers
  all pairs, so it fails when the variant is written, not when a fixture happens
  to hit it.
- **A residual collision fails at the first fixture.** `intern` panics naming
  both structures, instead of one function silently replacing another.
- **A lookup cannot invent a key.** `Unresolved` is not a bucket and `Binder` is
  not a declaration, so neither can compare equal to something real.
- **An index cannot pick a winner.** It holds a set; choosing is resolution's
  job, and resolution has the vantage.
- **A synthesized node cannot lose its referent.** `Type::Resolved` carries it,
  so there is no name to reconstruct from.
- **The work is enumerable.** Each type flip makes the compiler list the sites
  that still hold a name, and the counts only fall.

## Migration

Four independent tracks. Within a track the order matters; across tracks it does
not.

- [ ] A1 — `Synth`. The eight synthesized families become variants and their
      mints move behind `Symbol`. Contained in `name.rs` and its producers.
- [ ] A2 — `Symbol` replaces `FunctionId`; `TirFunction` carries it beside
      `name`; `intern`'s gate goes live. The identity maps key on `SymbolId`:
      `func_index`, `func_map`, DCE's `call_graph`, `funcid_map`, and the
      `(ModuleSource, String)` pairs in `NirPackage`.
- [ ] A3 — `name` leaves NIR and WIR. Rendering happens at codegen, at `dump`,
      and in diagnostics. This is where `wir_func_type_key` stops deriving a
      display string from a lookup key.
- [ ] B1 — `DeclKey` becomes `DeclRef`; `declaration_named` returns one;
      `Resolutions::decls` is deleted.
- [ ] B2 — the by-name indexes answer sets; duplicate imports and duplicate
      prelude declarations are diagnosed.
- [ ] C1 — `Type::Resolved`; `type_id_to_ast_type` and `method_call.rs`'s
      synthesized bounds emit it.
- [ ] C2 — the `is_synthetic()` exemption is removed from the three
      `debug_assert!`s that carry it.

A2 is what makes a collision detectable everywhere. A2, B1 and C1 together are
what make one unrepresentable.

## Consequences

Costs and risks:

- A2 touches every consumer of a function name. The flip is what enumerates
  them; it is also what makes the branch large.
- `Symbol` holds `ModuleSource` (interned, O(1) clone) and `FqTypeName` (not
  interned), so building one allocates. Interning repays that only once maps key
  on `SymbolId`, which is why A1 and A2 are not worth splitting further.
- Any change to a rendered name pays a corpus-wide golden diff, and the split is
  not where it looks. Measured on the branch that prototyped A1: re-spelling the
  eight synthesized families moved ~1,400 lines of golden fixture, while
  bracketing `MangledName::in_module` — a purely internal lookup key — moved
  45,926, because `wir_func_type_key` renders it into the WIR type table. The
  identity and the display are the same string even where the identity never
  leaves `wir_build`. A3 is what separates them, so the escaper's own rendering
  changes should land after it, not before.
- B1's `Unresolved` arm will surface callers that currently proceed on a
  fabricated key. Each needs a decision.
- B2 may reject programs that compile today — a duplicate `use`, a name declared
  in two prelude modules. Both are ambiguities the compiler is currently
  resolving by insertion order, so a diagnostic is the correction, but it is a
  breaking change and needs its own fixtures.
- C1 adds a variant to `ast::Type`, matched widely. The unparser and the
  formatter must render it back to a spelling for display, which is the one
  place a `DeclRef` is turned into a name on purpose.

### The measurements, as they stand

Nothing has moved. The table in the Context section is the baseline, and each
track's progress is one of its rows reaching zero:

| track | row                                            | now |
| ----- | ---------------------------------------------- | --- |
| A     | `format!` sites minting a name outside `name.rs` |  87 |
| A     | ad-hoc separator scans over a name             | 115 |
| A / B | maps keyed by a `String` name                  | 633 |
| B     | `(ModuleSource, String)` identity pairs        | 133 |
| C     | production `AstId::fresh()` sites              |  12 |

Each row falls only as its track lands, and none of them is a test that can be
written first. The one test this design adds is the round-trip property, and it
cannot exist until `Symbol` does — which is why A1 is the first track rather
than a test-first step in front of it.
