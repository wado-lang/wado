# WEP: Library-Defined Derivation over `Reflect`

## Context

Wado derives type-directed traits (`Eq`, `Ord`, `Inspect`, `Serialize`,
`Deserialize`, `Default`) by bespoke compiler synthesis — one hardcoded
synthesizer per trait. That does not scale, and it is closed to libraries: with
no macros and no dynamic reflection, a package cannot introspect a type.
[Jade](./wep-2026-06-13-jade.md) (type → JSON Schema) forces the issue — it is
an ordinary package, so type→schema must be expressible as library code.

`Reflect` is the escape: a sealed, compiler-synthesized facility that exposes a
type's structure at compile time, so every derivation becomes a generic `impl`
resolved at monomorphization. This WEP specifies that facility — the per-kind
reflection traits, the `Member` token surface, library-side wire naming, and the
built-in `#[validate]` attribute — plus the migration of the existing
synthesizers onto it.

## Principle

The compiler's only job is to expose a type's structure. Every derivation —
built-in `Inspect` / serde / `Default`, Jade's `JsonSchema`, user-written ones —
is a generic library `impl`, static and monomorphized. No per-capability
synthesizer, no macros, no dynamic reflection.

```wado
impl<T: Reflect<Fields = [..F]>, ..F: SomeTrait> SomeTrait for T {
    fn method(&self) -> R {
        let policy = Reflect::<T>::wire_name_policy();
        for let f of Reflect::<T>::field_tokens() {   // value + metadata per field
            // … wire_name(&f, policy) … f.get(self) …
        }
        let parts = [..F::method_of()];               // value-free, per field type
    }
}
```

Two channels serve a derivation: a type-level pack (`Fields` / `Cases`) drives
the value-free `[..F::method()]` expansion, and a token walk carries each
member's value and metadata together.

## Reflection traits

One sealed, `internal`, compiler-synthesized trait per type kind, reached only
through the trait-qualified form (`Reflect::<T>::…`, never `T::…`) so a type's
own method namespace stays the author's. A user `impl` is a compile error, and
the traits are callable only in monomorphized contexts. Reflection stays split
by kind: a derivation handles each kind with different code, and a type is
exactly one kind, so blanket impls over different kinds are disjoint.

```wado
internal trait Reflect {
    type Fields;                                 // [F_0, F_1, …]
    type FieldTokens;                            // [Field<Self, F_0>, …]
    fn type_name() -> String;
    fn field_names() -> List<String>;
    fn fields(&self) -> Self::Fields;            // field values as a tuple
    fn field_tokens() -> Self::FieldTokens;      // one token per field
    fn wire_name_policy() -> CaseStyle;          // #[serde(rename_all)], casing not applied
    fn construct(fields: Self::Fields) -> Self;  // assemble from field values (deserialize side)
    // fn type_doc() -> Option<String>;          // deferred
}

internal trait ReflectVariant {
    type Cases;                                  // payload pack [P_0, …]; unit cases are ()
    type CaseTokens;                             // [VariantCase<Self, P_0>, …]
    fn type_name() -> String;
    fn discriminant(&self) -> i32;
    fn cases() -> Self::CaseTokens;
}

internal trait ReflectEnum {
    fn type_name() -> String;
    fn case_tokens() -> List<EnumCase<Self>>;
    fn discriminant(&self) -> i32;
    fn from_discriminant(disc: i32) -> Option<Self>;
}

internal trait ReflectFlags {
    fn type_name() -> String;
    fn bit_tokens() -> List<FlagBit<Self>>;
    fn bits(&self) -> u64;                       // u64-normalized regardless of width
    fn from_bits(raw: u64) -> Option<Self>;
}
```

`from_discriminant` / `from_bits` return `Option` because an unknown input is a
normal deserialize error, not a bug. Struct and variant tokens carry a payload
type parameter and form a mapped pack (`[..Field<T, F>]` / `[..VariantCase<T,
P>]`); an enum case and a flag bit are atomic, so their tokens are homogeneous
`List`s.

## `Member` and the tokens

Every reflected member is a token implementing one sealed `Member` trait — the
shared attr-reading face, so wire-naming, validation, and doc logic is written
once and reused across kinds.

```wado
internal trait Member {
    fn name(&self) -> String;                        // source name
    fn wire_name_override(&self) -> Option<String>;  // #[serde(rename)], casing not applied
    // fn doc(&self) -> Option<String>;              // deferred
}

struct Field<T, F>       { … }  // Member + has_default() is_secret() validate() get(&self, v: &T) -> F
struct VariantCase<T, P> { … }  // Member + is_unit() validate() holds(&v) extract(&v) -> P construct(P) -> T
struct EnumCase<T>       { … }  // Member + discriminant() holds(&v) make() -> T
struct FlagBit<T>        { … }  // Member + bit() is_set(&v) set() -> T
```

Tokens are sealed to these four stdlib types and minted only by the `Reflect*`
walk (their fields are private), so a program cannot forge a member. `validate()`
is only on the value-bearing tokens. A `#[secret]` field reports `is_secret()`
and takes the value-opaque `Secret<F>` projection in `Fields` (see
[Struct Walkability](./wep-2026-07-10-struct-walkability.md)).

The value bridges (`get` / `extract` / `construct`) lower to a discriminant-keyed
access, so a forged token can trap but never misread a payload; after inlining
they fold to the code a hand-written impl would emit.

## Wire naming: the compiler exposes facts, casing lives in the library

The reflection layer exposes only the authored facts — a member's `rename`
override (`Member::wire_name_override`) and the type's `rename_all` policy
(`Reflect::wire_name_policy` as a `CaseStyle`). A resolved wire name is policy,
and casing is serialization vocabulary, not type structure, so it lives in
`core:serde`; any schema library (Jade) calls the same helper, so wire names
never diverge.

```wado
pub fn wire_name<M: Member>(m: &M, policy: CaseStyle) -> String {
    return match m.wire_name_override() {
        Option::Some(o) => o,                     // explicit override wins
        Option::None    => apply_case(policy, m.name()),
    };
}
```

`CaseStyle` is total (`Identity` when no `rename_all` is set). `apply_case`
covers the six styles (`camelCase`, `snake_case`, `PascalCase`,
`SCREAMING_SNAKE_CASE`, `kebab-case`, `SCREAMING-KEBAB-CASE`) and matches the
compiler's legacy `apply_rename_all`, locked by a shared test corpus.
(`core:args` reuses the same policy for CLI tags, so "wire name" is the
boundary-facing name in general, not a serde-only concept.)

## `#[validate(…)]`

A single generic, built-in attribute with a closed vocabulary, owned by no
library:

```wado
struct CreateUser {
    #[validate(min_length = 1, max_length = 64)]
    user_name: String,
    #[validate(format = "email")]
    email: String,
    #[validate(minimum = 0, maximum = 150)]
    age: i32 = 0,
}
```

Recognized keys: `min_length` / `max_length`, `minimum` / `maximum` /
`exclusive_minimum` / `exclusive_maximum`, `multiple_of`, `pattern`, `format`,
`min_items` / `max_items`, `unique_items`. The compiler parses them into a
`Validate` value — a struct of `Option` fields, one per key — and the closed
vocabulary does two things:

- Enforced at the `Deserialize` boundary: a violation is a `DeserializeError`
  (`InvalidValue`) with the field offset. The trust boundary — untrusted data
  entering the program's types — is where a wire contract is honestly enforced;
  trusted struct literals are deliberately not checked (that is the future
  refinement feature's domain).
- Exposed via the token: `Field::validate()` / `VariantCase::validate()`, so a
  schema library emits the corresponding keywords.

A closed vocabulary is what lets one annotation be both enforced (the compiler
knows each key's boundary check) and introspected (each key's schema mapping).
`description` is not a `#[validate]` concern — it comes from `///` doc comments
via `Member::doc()` / `type_doc()`.

## Status

- [x] `Reflect` struct reflection — `Fields` / `fields` / `field_names` /
      `type_name` / `field_tokens` / `wire_name_policy`; `Member` + `Field`
      tokens.
- [x] `ReflectVariant` / `ReflectEnum` / `ReflectFlags` + `VariantCase` tokens.
- [x] `core:serde::wire_name` / `apply_case` (library-side casing, decision 2-B).
- [x] `Member` on `VariantCase`; `EnumCase` / `FlagBit` tokens (enum / flags move
      to the token walk).
- [ ] `Reflect::construct(Fields) -> Self`.
- [ ] `#[validate]` — parse to `Validate`, expose via the tokens, enforce at the
      `Deserialize` boundary.
- [ ] `Member::doc()` / `type_doc()` (needs a doc-comment → string path).
- [x] Migrate struct `Inspect` onto a library blanket
      (`impl<T: Reflect<Fields = [..F]>, ..F: Inspect> Inspect for T` in
      `core:prelude/traits`): non-generic structs derive `Inspect` through the
      field walk instead of a bespoke synthesizer. Format dispatch (`:?` /
      `${}`, power-assert, auto-derive bodies) resolves the blanket coherently —
      routed only when the receiver satisfies the `Reflect` bound, so tokens /
      refs / non-`Reflect` types keep their own impls.
- [ ] Migrate the remaining `Inspect` kinds (variant / enum / flags / generic
      structs / newtypes) and serde / `Default` onto library impls over
      `Reflect`.

## Consequences

Benefits: one mechanism replaces an open-ended series of synthesizers, so new
derivations are ordinary library code; Jade needs no compiler change; the
compiler's special-case surface shrinks as `Inspect` / serde / `Default`
migrate; everything stays static and monomorphized; `#[validate]` is generic and
always enforced, so a constraint is never silently ignored.

Costs: the reflection layer still exposes serde's naming and `#[validate]`
vocabularies as facts, so `Member` is coupled to them — full neutrality waits for
user-defined attributes. Variant / enum / flags reflection is real compiler work.
`#[validate]` enforcement grows core serde with a bounded validation vocabulary.
A generic field-walk may monomorphize to less tight code than a hand-written
synthesizer, so each migration checks generated-code parity, not just output
parity.

## Open questions

- By-reference member reads: `get` / `extract` copy the value; a large payload
  may want a `&`-returning sibling once the `Inspect` migration can measure the
  copy cost.
- The `#[validate]` recognized key set: which JSON Schema assertions earn a
  first-class key versus hand-authoring on a `Schema` value.
- Coherence: a blanket `impl<T: Reflect<Fields = [..F]>, ..F> Trait for T`
  meeting concrete impls, under the variadic coherence rules of
  [WEP 2026-03-14](./wep-2026-03-14-variadic-type-parameters.md).

## Future directions

- Refinement predicates — in-memory invariants on a `newtype`, enforced at
  construction, distinct from `#[validate]`'s wire-boundary guard.
- User-defined attributes `@[foo(…)]` — a distinct syntax for
  library-interpreted attributes, kept visually separate from built-in `#[…]`,
  exposed via `Reflect` as opaque metadata.
- Renaming the serde attribute surface (`#[serde(rename)]` → `#[serde(wire)]`,
  `#[serde(rename_all)]` → `#[serde(case)]`) to match the `wire_name` vocabulary.
