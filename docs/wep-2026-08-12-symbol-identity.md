# WEP 2026-08-12: Symbol identity — a name is an encoding, never an identity

## Context

[Reference resolution](./wep-2026-08-10-reference-resolution.md) made a
declaration's identity `DeclRef`, and named its own exception:

> A mangled function name is the one place a name is still the currency, because
> Wasm requires one.

This WEP is about that exception, and about the residue the same design left on
the lookup side. Both are one defect: a `String` standing in for an identity.
When it does, two symbols that should differ can *be* the same symbol — not a
lookup going wrong, but two things becoming one.

### Three properties have been confused

Three separate guarantees have been pursued under the word "collision", and only
the third prevents one:

| property         | established by                    | says                                             |
| ---------------- | --------------------------------- | ------------------------------------------------ |
| unforgeable      | WEP 07-29 — `MangledName`         | a name can only be minted by an authority        |
| namespace-typed  | WEP 07-28 / 07-29 — `DeclName`    | a declaration name cannot pass as a mangled one  |
| **injective**    | nothing yet                       | distinct symbols never render to the same string |

The first two constrain *who writes a name* and *where it may be handed*. Neither
says anything about two different symbols arriving at the same characters. That
is why stricter name types kept landing while collisions kept arriving: the
property being enforced was never the property being violated.

### Why site-local fixes cannot converge

Injectivity is a statement about *every pair* of symbols. A disambiguating
segment added where a collision was observed establishes it for that pair and
tells you nothing about the rest. There is no sequence of local repairs that
accumulates into a global property, so the cycle is not a discipline failure —
it is the wrong shape of fix.

The same reasoning appears in WEP 08-10 one level up: constraining the
*derivation* narrowed producers while consumers still accepted names, so the
defect moved rather than left. The currency has to change here too.

### The measurements

|                                                              |     |
| ------------------------------------------------------------ | --- |
| maps keyed by a `String` name                                | 633 |
| `name: String` fields                                        | 208 |
| `(ModuleSource, String)` identity pairs                      | 133 |
| ad-hoc separator scans over a name (`::` `^` `<` `/` `$` `__`) | 115 |
| `format!` sites minting a name outside `name.rs`             |  87 |
| name families declared in `name.rs`                          |  42 |

`wado-compiler/AGENTS.md` states the rule: "Name mangling and monomorphization
go through `name.rs`. No other component knows a name format." The last two rows
are what that rule is worth while the currency is `String` — it binds the
producers that choose to obey, and `format!` is available everywhere else.

### Where the encoding loses information

A mangled name is a grammar of `/`, `::`, `^`, `<`, `>`, `,`, `&`, `$`, `@`
written over atoms that may contain those same characters: a `ModuleSource`
spelling carries `/`, `:` and — for `<entry>` and remote URLs — `<`, `>` and
`//`; a declaration name carries `$` from `namespace_member_alias`, `@` from
`mangle_local_item_name`, and `[`, `]`, `&`, `(`, `)`, `!` from the tuple,
reference and unit heads.

Every join of two variable-length operands was therefore ambiguous:

| join                                       | merges                                                          |
| ------------------------------------------ | --------------------------------------------------------------- |
| `cm_wrap_async_func_name` — `_`            | `("outgoing_body","write")` = `("outgoing","body_write")`        |
| `dispatch_wrapper_name` — `__`             | `("A__B","c")` = `("A","B__c")`                                  |
| `case_extract` / `case_construct` / `field_get` — `$` | `(A$B, C)` = `(A, B$C)`                               |
| `shallow_copy_helper_name` — `$shallow` suffix | shallow-of-`X` = deep-of-`X$shallow`                        |
| `dispatch_global_name` vs `dispatch_wrapper_name` | shared `__effect` prefix                                 |
| `MangledName::in_module` — `/`             | `core:prelude` + `list.wado/Point::x` = `core:prelude/list.wado` + `Point::x` |

The last one is the primary function namespace, and both of its modules are
stdlib modules: a local name is itself module-qualified, and one module's
rendering is another's prefix at a `/` boundary.

Disambiguating that key surfaced a second instance of the same defect one layer
down. `MangledName::in_module` is a lookup key, so changing its spelling should
cost nothing outside `wir_build` — but `wir_func_type_key` derives the WIR type
table's key from it, so the string is at once the `func_map` identity and a
display artifact in every `wado dump`. Bracketing the module moved 45,926 lines
of golden fixture. A name doing two jobs is what this WEP is about, and the
measurement is that the identity and the rendering are not yet separable even
where the identity is purely internal.

### Where a collision goes

`wir_build::functions::register_single_function` returned when a `func_map` key
was already claimed. The loser was dropped and its calls resolved to the
winner's body — a Wasm validation type mismatch several phases later, or nothing
at all.

Four assertions guard four narrower namespaces (`lower::translate`,
`monomorphize`, `optimize::dce`, `link`). Each was added after a specific
collision; none covers a family invented afterwards.

### The lookup side kept a fabricated key

The trait-driven registries key on

```rust
pub(crate) type DeclKey = (ModuleSource, String);
```

and `Elaborator::decl_key_or_local` produces one for callers that must have a
bucket:

```rust
if self.annotate_ctx.trait_ctx.type_params.contains_key(name) {
    return (self.current_module_source.clone(), name.to_string());   // a binder
}
self.canonical_decl_key(name)
    …
    .unwrap_or_else(|| (self.current_module_source.clone(), name.to_string()))
```

Both fallbacks *invent* a key in the declaration namespace. The first hands a
binder `T` the same key a module-level `struct T` gets — the exact conflation
WEP 08-10 removed from `ImplReceiver` ("a type parameter has no spelling in the
declaration namespace"), still live here because `DeclKey` is a pair of strings
rather than the `DeclRef` that WEP introduced. The second gives an unresolved
name the writing module, so one declaration reached from two modules gets two
keys, and a name that reaches no declaration gets a key that a later declaration
can grow into.

`Resolutions::declaration_named` returns `Option<(ModuleSource, String)>`: it
takes the `AstId` identity and flattens it back to a name pair. That is the
`.base_name()` debt WEP 08-10 counts in its own measurements table, at the point
where identity becomes a key.

## Decision

> A name is an encoding of an identity, never the identity. The identity is a
> Rust value; the string is produced from it, at one place, and never parsed.

Two halves, because a symbol is named in two directions.

### Part A — the mangled name

#### 1. `Symbol` completes the shape `FunctionId` already has

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
08-10's discipline. What is left as a `String` is exactly what still collides:

- `FreeFunctionName::name` holds a fully mangled string, so every synthesized
  family (`$value_copy…`, `$field_get…`, `__Dispatch…`, a `param_spec` clone, a
  monomorphized method rewritten by the call-rewrite path) rides in it as text;
- `MethodName::method_name` holds `method<args>`, fusing the method's own name
  with its type arguments.

So this is not a new type; it is the same one finished:

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

`Synth` is the substance of the change. The eight `$`-and-`__` families are a
sum type that was being spelled in strings; written as a Rust enum, their
operands are fields and cannot merge — `CaseExtract { A$B, C }` and
`CaseExtract { A, B$C }` are different values whatever they render as. A derived
family nests (`ParamSpec { base: Box<Symbol> }`) rather than suffixing, so a
decomposition question never arises.

Equality is structural. `Symbol` is `Eq + Hash`, so every map that keys a
function keys an identity.

`Symbol` is deliberately **not** `DeclRef`, and not built from `AstId`. A
mangled name must be deterministic across compilations — golden fixtures, the
Wasm name section, reproducible builds — and `AstId`'s space is a process-global
counter whose value depends on unrelated parse history (`mangle_local_item_name`
already documents this and encodes only the module-local index). `DeclRef`
answers "which declaration"; `Symbol` answers "which emitted function", which is
a declaration *plus an instantiation*, plus the families that have no
declaration at all. They are different questions and want different types.

#### 2. Rendering is one direction

`render: Symbol -> MangledName` is the only bridge, and there is no inverse in
the crate. Every question a consumer used to answer by splitting a name — the
declaring module, the receiver, the trait, the type arguments, "is this a value
copy helper" — is a field access or a `matches!` on `Symbol`.

The 115 separator scans are the enumeration of this work, and flipping
`TirFunction.name`'s type is what produces the list. That is the property this
design is chosen for, and the reason it terminates: the compiler names every
site that still holds a string, one error at a time, and the count only falls.

#### 3. Injectivity is proved, not asserted

Two boundaries, two treatments:

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

A total round trip *is* injectivity, for all pairs, including the ones no
fixture exercises — which is the property no site-local fix can give. `parse`
lives under `#[cfg(test)]`: it exists to discharge the obligation, not to be
called.

#### 4. The interner is the backstop during the migration

```rust
impl SymbolTable {
    pub fn intern(&mut self, sym: Symbol) -> SymbolId;  // panics on a render collision
}
```

`intern` panics when a rendering is already claimed by a *different* `Symbol`,
naming both. It covers families whose renderer has not been migrated yet, so the
guarantee does not wait for the last one. It replaces the four narrow assertions,
and `SymbolId` being `Copy + Eq + Hash` is what makes the 633 name-keyed maps
cheaper than the pairs they replace rather than more expensive.

### Part B — the lookup key

#### 5. A fallback key is a fabricated identity

`DeclKey` becomes `DeclRef` — the identity WEP 08-10 already produces — and the
two fallbacks in `decl_key_or_local` stop being expressible:

```rust
pub enum DeclRef {
    Decl(AstId),
    Binder(BinderId),
    Unresolved,
}
```

A binder is `Binder(BinderId)`, which no declaration can equal, so a frame's `T`
and a module's `struct T` are two keys. A name reaching nothing is `Unresolved`,
which is not a bucket — a caller that "must produce some bucket" is a caller
whose question was wrong, and the compiler lists them when the type flips.

`Resolutions::declaration_named` returns `DeclRef` rather than
`Option<(ModuleSource, String)>`, so identity stops being flattened at the point
it becomes a key.

#### 6. A by-name index answers a set, never a winner

`blanket_impls: IndexMap<String, Vec<BlanketImpl>>` and `trait_impl_modules` are
keyed by trait name, so two same-named traits share a bucket. The rule:

> A by-name index maps a spelling to the **set** of declarations that spell
> themselves that way. Choosing among them is scope resolution, which happens
> once, at the reference site, in `Resolutions`.

An index that returns one answer for a name has already made a choice it has no
vantage to make. Re-keying these on `DeclRef` is `impl_target_key_at`'s existing
loop, which WEP 08-10 notes already computes the identity in the same pass.

## Why the class stops being writable

- **A new name family cannot merge its operands.** It is a `Synth` variant, and
  a variant's fields are not a string. There is no `format!` to reach for,
  because the mint takes a `Symbol`.
- **A new family with an ambiguous rendering fails at authoring.** The round-trip
  property test covers all pairs, so it fails when the variant is written, not
  when a fixture happens to hit it.
- **A residual collision fails at the first fixture.** `intern` panics naming
  both structures, instead of one function silently replacing another.
- **A lookup cannot invent a key.** `Unresolved` is not a bucket and `Binder` is
  not a declaration, so neither can be compared equal to something real.
- **The work is enumerable.** Each type flip makes the compiler list the sites
  that still hold a name. The counts in the measurements table are the progress
  metric, and they only fall.

## Migration

`TirFunction.name` is read across ~2000 sites, so this lands in stages, each one
shippable and each one measured by the table above.

- [x] Stage 0 — make the existing encoding injective. One writer (`SynthName`)
      brackets every multi-operand join; `MangledName::in_module` brackets the
      module; `register_single_function` asserts the re-claim comes from the same
      `FuncId` instead of dropping a function. A bug fix, not the design: it
      closes the holes that exist rather than making new ones unrepresentable.
- [ ] Stage 1 — `Synth`. The eight synthesized families become variants; their
      mints move behind `Symbol`. Self-contained in `name.rs` and its producers.
- [ ] Stage 2 — `Symbol` replaces `FunctionId`, and `TirFunction` carries it
      beside `name`. `intern`'s gate goes live. The identity maps key on
      `SymbolId`: `func_index`, `func_map`, DCE's `call_graph`, `funcid_map`, and
      the `(ModuleSource, String)` pairs in `NirPackage`.
- [ ] Stage 3 — `DeclKey` becomes `DeclRef`; the by-name indexes answer sets.
      Independent of stages 1–2 and separately valuable.
- [ ] Stage 4 — `name` leaves NIR and WIR. Rendering happens at codegen, at
      `dump`, and in diagnostics.

Stage 2 is what makes a collision detectable everywhere; stages 2 and 3 together
are what make one unrepresentable.

## Consequences

Costs and risks:

- Stage 0's cost was almost entirely in one place, and not the expected one.
  Re-spelling the eight synthesized families moved ~1,400 lines of golden
  fixture; bracketing the `func_map` key moved 45,926, because
  `wir_func_type_key` renders it. Separating the WIR type key's display from the
  function's identity is stage 2's work, and until then any change to an
  internal key pays a corpus-wide diff.
- Stage 2 touches every consumer of a function name. The flip is what enumerates
  them; it is also what makes the branch large.
- `Symbol` holds `ModuleSource` (interned, O(1) clone) and `FqTypeName` (not
  interned), so building one allocates. Interning repays that only once maps key
  on `SymbolId`, which is why stages 1 and 2 are not worth splitting further.
- Stage 3 reaches the trait registries, which WEP 08-10 left keyed by name, and
  its `Unresolved` arm will surface callers that currently proceed on a
  fabricated key. Each is a real question with no answer, and each needs a
  decision rather than a default.
- The escaper changes the rendering of any atom carrying a metacharacter — the
  `<entry>` module, remote URLs, namespace aliases, local items. Their golden
  fixtures move once, at stage 2.

### The measurements, as they stand

|                                              | at the start | now |
| -------------------------------------------- | ------------ | --- |
| maps keyed by a `String` name                | 633          | 633 |
| ad-hoc separator scans over a name           | 115          | 115 |
| `format!` sites minting a name outside `name.rs` | 87       |  87 |
| non-injective joins in `name.rs`             | 6            |   0 |

Only the last row has moved. It is the row stage 0 was aimed at, and the first
three are the honest measure of how much of this design is unbuilt.
