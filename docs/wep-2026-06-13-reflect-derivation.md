# WEP: Library-Defined Derivation over `Reflect*`

## Principle

The compiler's only job is to expose a type's structure. Every derivation —
built-in `Inspect` / serde / `Default`, Jade's `JsonSchema`, user-written ones —
is a generic library `impl`, static and monomorphized. No per-capability
synthesizer, no macros, no dynamic reflection.

Two channels serve a derivation: a payload pack (`FieldTypes` / `CasePayloads`)
binds the per-member type variables `..F` / `..P` and drives the value-free
`[..F::method()]` expansion; a member walk — a tuple of member handles — carries
each member's value and metadata together.

```wado
impl<T: ReflectStruct<FieldTypes = [..F]>, ..F: SomeTrait> SomeTrait for T {
    fn method(&self) -> R {
        for let f of ReflectStruct::<T>::members() {  // value + metadata per field
            // … f.name() … f.get(self) …
        }
        let parts = [..F::method_of()];               // value-free, per field type
    }
}
```

## Reflection traits

One sealed, `internal`, compiler-synthesized trait per type kind, reached only
through the trait-qualified form (`ReflectStruct::<T>::…`, never `T::…`) so a type's own
method namespace stays the author's. A user `impl` is a compile error, and the
traits are callable only in monomorphized contexts (`T` a concrete type).
Reflection stays split by kind: a type is exactly one kind, so blanket impls over
different kinds are disjoint.

Every kind spells its member channel the same way: `type Members` plus
`fn members()`. A kind that has payloads adds a payload pack alongside it. The
type's scalar facts and the value→member build direction round out each trait.
Every per-member fact — name, wire override, doc, `is_unit` / `has_default` /
`is_secret`, validation, value access — lives on the member, so no kind carries a
parallel metadata list or value accessor.

```wado
internal trait ReflectStruct {                    // struct
    type FieldTypes;                             // payload pack [F_0, F_1, …]
    type Members;                                // [StructField<Self, F_0>, …]
    fn members() -> Self::Members;
    fn from_fields(fields: Self::FieldTypes) -> Self;  // assemble from field values
    fn type_name() -> String;
    fn wire_name_policy() -> CaseStyle;          // #[wire(name_policy)], casing not applied
}

internal trait ReflectVariant {                  // variant
    type CasePayloads;                           // payload pack [P_0, …]; unit cases are ()
    type Members;                                // [VariantCase<Self, P_0>, …]
    fn members() -> Self::Members;
    fn discriminant(&self) -> i32;
    fn type_name() -> String;
    fn wire_name_policy() -> CaseStyle;
}

internal trait ReflectEnum {                     // enum
    type Members;                                // [EnumCase<Self>, …]
    fn members() -> Self::Members;
    fn discriminant(&self) -> i32;
    fn from_discriminant(disc: i32) -> Option<Self>;
    fn type_name() -> String;
    fn wire_name_policy() -> CaseStyle;
}

internal trait ReflectFlags {                    // flags
    type Members;                                // [FlagsBit<Self>, …]
    fn members() -> Self::Members;
    fn bits(&self) -> u64;                        // u64-normalized regardless of width
    fn from_bits(raw: u64) -> Option<Self>;
    fn type_name() -> String;
    fn wire_name_policy() -> CaseStyle;
}
```

`members()` returns a tuple, walked by tuple `for-of`; a generic derivation binds
one pack in its header, and the elaborator resolves the walk to the known member
type so member methods stay callable. Which pack a derivation binds follows from
whether the kind has payloads. A struct or variant member carries a payload type
parameter, so its walk is a heterogeneous mapped pack (`[..StructField<T, F>]` /
`[..VariantCase<T, P>]`) derived from the payload pack — and binding
`FieldTypes = [..F]` / `CasePayloads = [..P]` is what lets a derivation constrain
the payload types (`..F: SomeTrait`). An enum case and a flag bit carry no
payload and nothing to constrain, so a derivation binds `Members = [..M]`
directly, which serves only to source the arity. Either way `Members` is the
single member channel; no kind carries a parallel metadata list. (A tuple carries
no runtime index, so a homogeneous walk finds a member by `holds` rather than by
discriminant index, matching the variant walk.)

A generic derivation over a member walk binds a type pack, and both instance and
`static` trait methods resolve through such a pack-bound blanket — a deserialize
entry (`T::from_wire(…)`) dispatches the same way a walk does. A variant is no
exception: `Variant::Case` owns that namespace, but a name that is not a case
falls through to the blanket.

A derivation's method may carry type parameters of its own — serde's
`serialize<S: Serializer>`. The instance keys on the receiver, the bound's pack,
and the method's own arguments together, so one blanket body serves every
(subject, format) pair. Inferring such a parameter from an argument needs the
receiver to be a bound type parameter (`fn f<T: Serialize>(v: &T)`, the shape
every serde entry point takes); a concrete subject reaching the derivation
through the blanket needs the turbofish.

Every kind spells its build direction `from_<channel>`. `from_discriminant` /
`from_bits` return `Option` because an unknown raw input is a normal deserialize
error, not a bug; `from_fields` is total, since a field-value tuple is already
typed. `discriminant` / `bits` read the live tag off a value.

## Members

Every reflected member is a handle implementing the sealed `Member` trait — the
shared attr-reading face, so wire-naming, validation, and doc logic is written
once and reused across kinds.

```wado
internal trait Member {
    fn name(&self) -> String;                        // source name
    fn wire_name_override(&self) -> Option<String>;  // #[wire(name)], casing not applied
    fn doc(&self) -> Option<String>;                 // /// doc comment
}

struct StructField<T, F> { … }  // Member + index() has_default() is_secret() validate() get(&self, v: &T) -> F
struct VariantCase<T, P> { … }  // Member + discriminant() is_unit() validate() holds(&v) extract(&v) -> P construct(P) -> T make() -> T
struct EnumCase<T>       { … }  // Member + discriminant() holds(&v) make() -> T
struct FlagsBit<T>        { … }  // Member + bit() is_set(&v) set() -> T
```

`VariantCase::make()` builds a unit case, which `construct` cannot serve: a
derivation walking the case members writes one body per case, and
`construct(())` type-checks only where the payload is `()`. It traps on a case
that is not unit, like every other value bridge over a mismatched member.

Members are sealed to these four stdlib types and minted only by `members()`
(their fields are private), so a program cannot forge one. `validate()` is only on
the value-bearing members (`Field` / `VariantCase`). A `#[secret]` field reports
`is_secret()` and takes the value-opaque `Secret<F>` projection in `FieldTypes`
(see [Struct Walkability](./wep-2026-07-10-struct-walkability.md)).

The value bridges (`get` / `extract` / `construct` / `make` / `set`) lower to a
discriminant-keyed access, so a forged member can trap but never misread a payload;
after inlining they fold to the code a hand-written impl would emit.

Reflection stops at these four handles: they are themselves generic structs, and
a handle's own `Members` would mention `StructField<Self, …>`, growing `Self`
without bound. They are not reflectable, by the same seal that rejects a user
`impl`.

## Building a struct from a streaming format

A struct's build direction is `from_fields`, which takes the field values in
declaration order — but a self-describing format delivers them in wire order,
and a pack map (`[..F::method(args)]`) applies one expression per element with
no per-element context beyond a mutable cursor. Neither side can reorder alone,
so the derivation holds the values: a slot tuple `[..Option<F>]`, written by the
wire loop at the index the format reports and read back in declaration order.

```wado
impl<T: ReflectStruct<FieldTypes = [..F]>, ..F: Deserialize> Deserialize for T {
    fn deserialize<D: Deserializer>(d: &mut D) -> Result<T, DeserializeError> {
        let members = ReflectStruct::<T>::members();
        let mut sd = d.begin_struct(&ReflectStruct::<T>::type_name(), members.len())?;
        let mut slots = [..F::empty_slot()];              // [..Option<F>]
        while let Some(index) = sd.next_field::<T>()? {
            let mut taken = false;
            for let [i, _] of slots.enumerate() {
                if i == index {
                    slots[i] = Option::Some(sd.value()?);
                    taken = true;
                }
            }
            if !taken {
                sd.skip()?;
            }
        }
        sd.end()?;
        return Result::Ok(ReflectStruct::<T>::from_fields(
            [for let [i, slot] of slots.enumerate() { resolve_field(slot, &members[i])? }],
        ));
    }
}
```

`resolve_field` is ordinary library code: the slot's value where the wire
carried one, else the field's declared default, else a `MissingField` error
naming it.

The wire protocol is unchanged — `next_field` over `FieldSchema`, one streaming
pass — so no format implementation moves. Four things the language and the
reflection API do not have yet are what it waits on:

- `.enumerate()` over a variadic (pack-typed) `for-of`. The concrete-tuple form
  exists; the deferred one rejects it.
- The `[for let [i, v] of tuple.enumerate() { expr }]` collection form
  (WEP 2026-03-14 §8b), which assembles `[..F]` out of `[..Option<F>]`. A pack
  map cannot: it applies one expression to every element, with no index to pair
  a slot with its member.
- The `.enumerate()` index usable as a tuple subscript. It is a compile-time
  constant by construction, but `slots[i]` is rejected today.
- `StructField::default_value() -> Option<F>`, exposing a field's declared
  default (`f: T = expr`) to a build derivation. It lowers to an index-keyed
  bridge, as `get` does.

The alternative — a `DeserializeStruct` serving fields in declaration order —
moves the reordering into every format (JSON, CBOR, NSD, `core:args`), each
buffering the whole object before the first field can be read. Rejected: it
charges every format for what one derivation needs, and the single streaming
pass is what the protocol is for.

## Visibility

A type satisfies a `T: Reflect*` bound only where every one of its members is
visible. A declaration carries a single synthesized impl, so `members()` is
fixed and cannot be shortened for a caller that sees less; admitting a type on
one public member would enumerate its private ones alongside it. The shape is
observable as a whole or not at all. A type declaring no member is genuinely
memberless and always satisfies.

This gates reflection written _about_ a type, not the impls derived _for_ it. A
derived impl is synthesized at the declaration, where every member is visible,
so `${v:?}` and serde keep rendering a foreign type in full — that is the
declaring module's own choice of representation, the same as deriving it by
hand. What visibility withholds is a third party enumerating a shape its owner
did not expose.

This is what keeps an abstraction like `TreeMap` out of a downstream
`T: ReflectStruct` without naming it: its fields are private, so nothing outside
its module can enumerate them.

## Generic types

A generic type reflects through one generic impl over `S<T, …>`, not through a
per-instantiation impl. `FieldTypes` / `CasePayloads` / `Members` bind the
declaration's own parameters and each instantiation substitutes them:

```wado
struct Pair<T> { left: T, right: i32 }
// FieldTypes = [T, i32]   →   Pair<String>: FieldTypes = [String, i32]
```

The trait shape is unchanged, so no reflection API is generic-specific. The
alternative — synthesizing a concrete impl once `Pair<String>` exists — is
circular: a derivation needs `Pair<String>: ReflectStruct` _while_
monomorphizing, which is exactly when the instance appears.

`type_name()` is the declared name (`"Pair"`, not `"Pair<String>"`), matching
the plain-struct case and Rust's `{:?}`. Open: a schema library keying `$defs`
per instantiation needs an identity that separates `Pair<String>` from
`Pair<i32>`. That is a distinct fact (the instance's type arguments), not a
different spelling of `type_name`, and waits for a consumer.

Two rules bound what is reflectable:

- A member's own type never disqualifies its owner. An associated-type
  projection looks like an exception — an iterator adapter's
  `pub f: fn mut(I::Item) -> U` does not substitute the declaration's own
  parameters — but the synthesized impl carries the declaration's bounds, so
  `I::Item` resolves at the instantiation like any other projection.
- A type whose members are not all visible at the use site is not reflectable
  there (see [Visibility](#visibility)). This is what leaves `TreeMap`'s
  hand-written `Inspect` as the only candidate: a generic instance's mangled
  name (`TreeMap<String, i32>`) does not match the impl written for `TreeMap`,
  so the derivation would otherwise take it.

### Value bridges

A generic type's value bridges (`$field_get$S$F`, `$case_extract$V$P`,
`$case_construct$V$P`) are minted _after_ monomorphization — the only synthesis
that is. Lowering names a bridge by the concrete subject and member mangles, and
members sharing a mangled member type share one index-dispatched bridge:
`Pair<i32>` merges `left: T` with `right: i32`, `Pair<String>` keeps them apart.
The grouping exists only per instantiation, and the two call sites are
indistinguishable, so a generic bridge could not be selected.

A generic struct becomes its own monomorphized declaration, so its bridges are
minted off the declaration list. A generic variant never does (WEP 2026-02-09),
so its instantiations are the `GenericInstance` types naming it, read off the
type table. Its tag read is minted per instantiation too (`$variant_tag$V`): no
shared declaration exists to host one, and the base variant type has no GC
layout, so the receiver must be the instance.

## Wire naming

The reflection layer exposes only the authored facts — a member's `rename`
override (`Member::wire_name_override`) and the type's `name_policy`
(`wire_name_policy` as a `CaseStyle`, on every kind). A resolved wire name is policy, and
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

`CaseStyle` is total (`Identity` when no `name_policy` is set). `apply_case` covers
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
- Exposed via the member: `StructField::validate()` / `VariantCase::validate()`, so a
  schema library emits the corresponding keywords.

`description` is not a `#[validate]` concern — it comes from `///` doc comments via
`Member::doc()`.

## Related WEPs

- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md)
- [Struct Walkability — Field Walks over `ReflectStruct` and `#[secret]` Fields](./wep-2026-07-10-struct-walkability.md)
- [Jade — JSON Schema for Wado](./wep-2026-06-13-jade.md)
- [Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md)
