# WEP 2026-08-12: Symbol identity — a structured symbol, an injective encoding

## Context

Symbol collisions have been fixed one at a time for months. Each fix adds a
disambiguating segment at the site that broke, and each new name family reopens
the hole somewhere else. The cycle does not converge because there is no global
invariant to converge on — only local patches.

The prior two WEPs structured the *inputs* to mangling.
[Structured fq names](./wep-2026-07-28-structured-fq-names.md) replaced the
rendered `FqTypeName(String)` with `{reference, head, args}`.
[Name namespaces as types](./wep-2026-07-29-name-namespaces.md) split the
rendered form into `MangledName` / `DeclName` / `DeclPath` so a name from one
namespace cannot reach a consumer keyed on another.

Neither addressed the *output*. A symbol's identity is still the mangled
`String`:

```rust
pub struct TirFunction { pub name: String, pub module_source: ModuleSource, ... }
```

so for any two symbols `a` and `b`:

```
identity(a) == identity(b)   ⟺   render(a) == render(b)
```

`render` is a concatenation of variable-length pieces separated by single
characters that the pieces themselves may contain. It is therefore not
injective, and every collision fought so far is one instance of that.

### Where the encoding loses information

`render` writes atoms — a `ModuleSource` spelling, a declaration name — into a
grammar built from `/`, `::`, `^`, `<`, `>`, `,`, `&`, `$`, `@`. Nothing keeps
an atom from containing a metacharacter:

| atom                      | contains                      | source                                       |
| ------------------------- | ----------------------------- | -------------------------------------------- |
| `ModuleSource` spelling   | `/`, `:`, `<`, `>`, `//`      | `./a/b.wado`, `core:prelude`, `<entry>`, URLs |
| declaration name          | `$`                           | `namespace_member_alias` — `ns$Member`        |
| declaration name          | `@`                           | `mangle_local_item_name` — `Foo@3`            |
| declaration name          | `[`, `]`, `&`, `(`, `)`, `!`  | tuple, reference and unit heads               |

The consequences are the mangling helpers whose operands merge:

```rust
pub fn case_extract_helper_name(variant: &str, payload: &str) -> String {
    format!("$case_extract${variant}${payload}")
}
```

`(A$B, C)` and `(A, B$C)` render to one string. So do
`value_copy_helper_name("X$shallow")` and
`shallow_copy_helper_name(value_copy_helper_name("X"))`.
`cm_wrap_async_func_name` joins two snake_case identifiers with a single `_`,
so `("outgoing_body", "write")` and `("outgoing", "body_write")` are one name.
Every helper family in `name.rs` taking more than one operand joined them this
way.

The same defect sits under the primary function namespace. A `func_map` key was
the module joined to the local name by `/`:

```rust
MangledName::in_module(module, local) == format!("{module}/{local}")
```

A local name is itself module-qualified (`{receiver module}/{Type}::method`),
and one module's rendering can be another's prefix at a `/` boundary —
`core:prelude` and `core:prelude/list.wado` both exist in the stdlib. So

```
in_module(core:prelude,           "list.wado/Point::x")
in_module(core:prelude/list.wado, "Point::x")
```

are one key, and the two functions share one `func_map` slot.

### Where a collision goes when it happens

`wir_build::functions::register_single_function` skipped a key that was already
claimed:

```rust
let fq = MangledName::in_module(module_source, &mangled_name);
if ctx.func_map.contains_key(&fq) {
    return;   // silent
}
```

The loser is dropped and every call to it resolves to the winner's body. That
surfaces phases later as a Wasm validation type mismatch, or not at all.

Four ad-hoc assertions guard four narrower namespaces — `lower::translate`
(`FunctionId` uniqueness), `monomorphize` (`(module, name)` uniqueness),
`optimize::dce` (`function_id_for` injectivity), `link` (stub shadowing). Each
was added after a specific collision. None covers a family invented afterwards.

## Decision

> A symbol's identity is a structured Rust value. The mangled string is an
> injective encoding of that value, produced in one place, and never parsed.

Two properties follow, and they are enforced differently:

- Structural identity — every consumer keys on the structure, so two symbols
  differing anywhere are different keys regardless of how they render.
  Collisions become unrepresentable rather than undetected.
- Injective encoding — where a string must carry the identity (the Wasm name
  section, `wado dump`, an export name), the encoding is injective, proved by a
  round trip rather than asserted.

Structural identity is what removes the miscompiles. Injective encoding is what
lets the invariant be checked globally while the migration is in flight.

### 1. `Symbol` — the closed sum of everything nameable

The ~25 `format!` templates in `name.rs` are a sum type spelled in strings.
Written as a Rust type:

```rust
pub enum Symbol {
    /// A `fn` written in source, or a monomorphized instance of one.
    Function { module: ModuleSource, name: DeclName, args: Vec<FqTypeName> },
    /// A method at an impl site. `impl_module` is where the `impl` block is
    /// written; the receiver names its own declaring module.
    Method {
        impl_module: ModuleSource,
        receiver: FqTypeName,
        tr: Option<FqTraitName>,
        name: DeclName,
        args: Vec<FqTypeName>,
    },
    /// A compiler-synthesized helper.
    Synth(Synth),
}

pub enum Synth {
    ValueCopy { ty: FqTypeName, depth: CopyDepth },
    CaseExtract { variant: FqTypeName, payload: FqTypeName },
    CaseConstruct { variant: FqTypeName, payload: FqTypeName },
    VariantTag { variant: FqTypeName },
    FieldGet { owner: FqTypeName, field: FqTypeName },
    ParamSpec { base: Box<Symbol>, ordinal: u32 },
    Dispatch { receiver: FqTypeName, tr: FqTraitName, part: DispatchPart },
    ModuleInit { module: Option<ModuleSource> },
    ClosureFunctor { module: ModuleSource, ordinal: u32 },
    ConstObject { ordinal: u32 },
    Test { index: u32, meta: TestMetadata, name: Option<String> },
}
```

The `$`-join ambiguity is gone at the representation, before rendering is
considered: `CaseExtract { A$B, C }` and `CaseExtract { A, B$C }` are different
values. A derived family nests structurally — `ParamSpec { base: Box<Symbol> }`
— so `f$spec0$shallow` cannot be read as either of its two decompositions.

`Symbol` is closed. A new name family is a new variant, and there is no
`format!` to reach for instead, because the mint takes a `Symbol`.

### 2. `SymbolTable` — the single mint and the one-to-one gate

```rust
pub struct SymbolTable {
    symbols: Vec<Symbol>,
    by_symbol: HashMap<Symbol, SymbolId>,
    by_render: HashMap<Box<str>, SymbolId>,
}

impl SymbolTable {
    pub fn intern(&mut self, sym: Symbol) -> SymbolId;  // panics on a render collision
    pub fn render(&self, id: SymbolId) -> &str;
    pub fn get(&self, id: SymbolId) -> &Symbol;
}
```

`intern` panics when `render(sym)` is already claimed by a different `Symbol`,
naming both structures. One always-on check, covering every family and every
phase — including families invented later — in place of four assertions that
each cover one.

`SymbolId` is `Copy + Eq + Hash`, so a map that keys a symbol gets both correct
and cheaper than the `(ModuleSource, String)` pairs it replaces.

### 3. Injective rendering, proved by a round trip

Two boundaries need separating, and they take different treatments.

A *composite* operand — an already-rendered name — is bracketed, because
escaping it would rewrite the structure it is a rendering of. That is stage 0's
`SynthName`, and it carries forward unchanged.

An *atom* — a `ModuleSource` spelling, a declaration name — is escaped: `%` is
the escape, `%xx` hex encodes a metacharacter inside it. Ordinary names contain
no metacharacter and render unchanged, so dumps stay readable and the escape
appears only on the shapes that need it (`<entry>`, a remote URL, a namespace
alias, a local item).

Injectivity is not asserted. A test-only `parse` recovers the structure and a
property test over generated symbols asserts

```
∀ s. parse(render(s)) == Some(s)
```

A total round trip *is* injectivity. Production code never calls `parse`; the
function exists to discharge the proof obligation, and it lives under
`#[cfg(test)]` so it cannot be reached from a consumer.

## Why this ends the cycle

A new name family cannot reintroduce the defect:

- it must be a `Synth` variant, so its operands are structural and cannot merge;
- if its *rendering* is ambiguous, the round-trip property test fails when the
  variant is written, not months later;
- if it collides with an existing family anyway, `SymbolTable::intern` fails on
  the first fixture that exercises it, naming both structures.

The three checks are ordered by how early they fire: authoring, first fixture,
never.

## Migration

`TirFunction.name` is read in roughly two thousand places, so this lands in
three stages, each shippable on its own.

- [x] Stage 0 — make the existing encoding injective. Every join whose operands
      could merge is bracketed by a single writer (`SynthName`), and
      `MangledName::in_module` brackets the module. `register_single_function`
      no longer drops a second claim on a `func_map` key silently: it asserts
      the claim comes from the same `FuncId`. A bug fix, not the design — it
      closes the holes that exist rather than making new ones unrepresentable.
- [ ] Stage 1 — mint through `Symbol`, keep the `String`. Every `format!` name
      family in `name.rs` becomes a `Symbol` constructor; `TirFunction.name`
      becomes `table.render(id)`. No consumer changes, and the gate goes live.
- [ ] Stage 2 — carry the id. `TirFunction { symbol: SymbolId, .. }` beside
      `name`. The identity maps key on `SymbolId`: `func_index`, `func_map`,
      DCE's `call_graph`, `funcid_map`, and the `(ModuleSource, String)` pairs
      in `NirPackage`.
- [ ] Stage 3 — drop the string. `name` leaves NIR and WIR. Rendering happens at
      codegen, at `dump`, and in diagnostics.

Stage 1 makes a collision *detectable*; stage 2 makes it *impossible*. The four
ad-hoc assertions retire at the end of stage 2, when the namespaces they guard
are keyed structurally.

## Consequences

The four narrow assertions become one wide one, and the class of defect they
were each added for stops being reachable.

Costs and risks:

- Stage 1 changes no rendered name, so golden fixtures are untouched — except
  where a rendering was ambiguous, which is the bug being fixed.
- The escaper does change a rendered name wherever an atom carries a
  metacharacter. Those are the `<entry>` module, remote URLs, namespace aliases
  and local items; the golden fixtures covering them are regenerated.
- `Symbol` holds `ModuleSource` (interned, O(1) clone) and `FqTypeName` (not
  interned). Interning the symbol makes the per-map cost a `u32`, but building
  one still allocates. Stage 2 is where that pays back.
- The rendered-string escape hatches noted in WEP 2026-07-29 —
  `split_base_name` on trait names, `display_type_name` at the WIR layer — are
  parses. They are deleted in stage 2 when WIR carries structure, not worked
  around.
