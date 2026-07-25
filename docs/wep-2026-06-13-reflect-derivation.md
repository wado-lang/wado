# WEP: Library-Defined Derivation over `Reflect`

## Principle

The compiler's only job is to expose a type's structure. Every derivation —
built-in `Inspect` / serde / `Default`, Jade's `JsonSchema`, user-written ones —
is a generic library `impl`, static and monomorphized. No per-capability
synthesizer, no macros, no dynamic reflection.

Two channels serve a derivation: a type-level pack (`Fields` / `Cases`) binds the
per-member type variables `..F` / `..P` and drives the value-free
`[..F::method()]` expansion; a token walk — a tuple of member tokens — carries
each member's value and metadata together.

```wado
impl<T: Reflect<Fields = [..F]>, ..F: SomeTrait> SomeTrait for T {
    fn method(&self) -> R {
        for let f of Reflect::<T>::field_tokens() {   // value + metadata per field
            // … f.name() … f.get(self) …
        }
        let parts = [..F::method_of()];               // value-free, per field type
    }
}
```

## Reflection traits

One sealed, `internal`, compiler-synthesized trait per type kind, reached only
through the trait-qualified form (`Reflect::<T>::…`, never `T::…`) so a type's own
method namespace stays the author's. A user `impl` is a compile error, and the
traits are callable only in monomorphized contexts (`T` a concrete type).
Reflection stays split by kind: a type is exactly one kind, so blanket impls over
different kinds are disjoint.

Each kind exposes exactly its type-level pack, one token walk returning a tuple,
the type's scalar facts, and the value→member build direction. Every per-member
fact — name, wire override, doc, `is_unit` / `has_default` / `is_secret`,
validation, value access — lives on the token, so no kind carries a parallel
metadata list or value accessor.

```wado
internal trait Reflect {                         // struct
    type Fields;                                 // [F_0, F_1, …]
    type FieldTokens;                            // [Field<Self, F_0>, …]
    fn field_tokens() -> Self::FieldTokens;
    fn construct(fields: Self::Fields) -> Self;  // assemble from field values (deserialize side)
    fn type_name() -> String;
    fn wire_name_policy() -> CaseStyle;          // #[serde(rename_all)], casing not applied
}

internal trait ReflectVariant {                  // variant
    type Cases;                                  // payload pack [P_0, …]; unit cases are ()
    type CaseTokens;                             // [VariantCase<Self, P_0>, …]
    fn cases() -> Self::CaseTokens;
    fn discriminant(&self) -> i32;
    fn type_name() -> String;
}

internal trait ReflectEnum {                     // enum
    type CaseTokens;                             // [EnumCase<Self>, …]
    fn case_tokens() -> Self::CaseTokens;
    fn discriminant(&self) -> i32;
    fn from_discriminant(disc: i32) -> Option<Self>;
    fn type_name() -> String;
}

internal trait ReflectFlags {                    // flags
    type BitTokens;                              // [FlagBit<Self>, …]
    fn bit_tokens() -> Self::BitTokens;
    fn bits(&self) -> u64;                        // u64-normalized regardless of width
    fn from_bits(raw: u64) -> Option<Self>;
    fn type_name() -> String;
}
```

Every token walk returns a tuple, walked by tuple `for-of`; a generic derivation
binds the token pack in its header, and the elaborator resolves the walk to the
known token type so member methods stay callable. Struct and variant tokens
carry a payload type parameter, so their walks are heterogeneous mapped packs
(`[..Field<T, F>]` / `[..VariantCase<T, P>]`) bound via the payload pack
(`Fields = [..F]` / `Cases = [..P]`). An enum case and a flag bit carry no
payload, so their tokens share one type; the pack is the token tuple itself,
bound as `CaseTokens = [..C]` / `BitTokens = [..B]` to source the arity. Either
way the token is the single member channel; no kind carries a parallel metadata
list. (A tuple carries no runtime index, so a homogeneous walk finds a member by
`holds` rather than by discriminant index, matching the variant walk.)

A generic derivation over a tuple walk binds a type pack, and both instance and
`static` trait methods resolve through such a pack-bound blanket — a deserialize
entry (`T::from_wire(…)`) dispatches the same way a walk does.

`from_discriminant` / `from_bits` return `Option` because an unknown input is a
normal deserialize error, not a bug. `construct` assembles a struct from its
field-value tuple; `discriminant` / `bits` read the live tag off a value.

## Members and tokens

Every reflected member is a token implementing the sealed `Member` trait — the
shared attr-reading face, so wire-naming, validation, and doc logic is written
once and reused across kinds.

```wado
internal trait Member {
    fn name(&self) -> String;                        // source name
    fn wire_name_override(&self) -> Option<String>;  // #[serde(rename)], casing not applied
    fn doc(&self) -> Option<String>;                 // /// doc comment
}

struct Field<T, F>       { … }  // Member + index() has_default() is_secret() validate() get(&self, v: &T) -> F
struct VariantCase<T, P> { … }  // Member + index() is_unit() validate() holds(&v) extract(&v) -> P construct(P) -> T
struct EnumCase<T>       { … }  // Member + discriminant() holds(&v) make() -> T
struct FlagBit<T>        { … }  // Member + bit() is_set(&v) set() -> T
```

Tokens are sealed to these four stdlib types and minted only by the `Reflect*`
walk (their fields are private), so a program cannot forge a member. `validate()`
is only on the value-bearing tokens (`Field` / `VariantCase`). A `#[secret]` field
reports `is_secret()` and takes the value-opaque `Secret<F>` projection in
`Fields` (see [Struct Walkability](./wep-2026-07-10-struct-walkability.md)).

The value bridges (`get` / `extract` / `construct` / `make` / `set`) lower to a
discriminant-keyed access, so a forged token can trap but never misread a payload;
after inlining they fold to the code a hand-written impl would emit.

## Wire naming

The reflection layer exposes only the authored facts — a member's `rename`
override (`Member::wire_name_override`) and the struct's `rename_all` policy
(`Reflect::wire_name_policy` as a `CaseStyle`). A resolved wire name is policy, and
casing is serialization vocabulary, not type structure, so it lives in
`core:serde`; any schema library (Jade) calls the same helper, so wire names never
diverge.

```wado
pub fn wire_name<M: Member>(m: &M, policy: CaseStyle) -> String {
    return match m.wire_name_override() {
        Option::Some(o) => o,                     // explicit override wins
        Option::None    => apply_case(policy, m.name()),
    };
}
```

`CaseStyle` is total (`Identity` when no `rename_all` is set). `apply_case` covers
the six styles (`camelCase`, `snake_case`, `PascalCase`, `SCREAMING_SNAKE_CASE`,
`kebab-case`, `SCREAMING-KEBAB-CASE`). `core:args` reuses the same policy for CLI
tags, so "wire name" is the boundary-facing name in general, not a serde-only
concept.

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
`Validate` value — a struct of `Option` fields, one per key — which is:

- Enforced at the `Deserialize` boundary: a violation is a `DeserializeError`
  (`InvalidValue`) with the field offset. Trusted struct literals are not checked.
- Exposed via the token: `Field::validate()` / `VariantCase::validate()`, so a
  schema library emits the corresponding keywords.

`description` is not a `#[validate]` concern — it comes from `///` doc comments via
`Member::doc()`.

## Related WEPs

- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md)
- [Struct Walkability — Field Walks over `Reflect` and `#[secret]` Fields](./wep-2026-07-10-struct-walkability.md)
- [Jade — JSON Schema for Wado](./wep-2026-06-13-jade.md)
- [Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md)
