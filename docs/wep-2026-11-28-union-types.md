# WEP: Union Types

## Context

[WEP: Variant Payload Design](./wep-2026-01-25-variant-payload-design.md)
proposed anonymous union types beside named variants, and left them unbuilt.
This WEP takes them over, with what the web bindings have since shown about
them.

WebIDL writes unions everywhere, and
[WEP: The Web Interface for Wado](./wep-2026-04-01-web.md) lowers each one to a
`variant` it generates. That costs a web program in three ways:

- Every call spells the case: `div.append([NodeOrString::AsString("text")])`.
- The generated names grow with the constituents, as
  `F64OrDomPointInitOrF64OrDomPointInitList` does, and two unions that differ
  can end up with one name.
- A union's type changes as the slice grows. `set_attribute` takes a `String`
  while `TrustedHTML` is outside the slice, and a `TrustedHtmlOrString` once
  it joins, so every caller breaks.

Wado means to be a language for web front ends, so a union should be something
a program writes as the platform does.

What users of other languages complain about:

- TypeScript: unions themselves are well liked. The complaints are about
  narrowing that is hard to follow, and about holes in structural typing's
  soundness.
- OCaml's polymorphic variants: powerful, and known for error messages that
  are hard to read.
- Rust: there is no anonymous sum, so a program writes a small `enum` and its
  `From` impls by hand. That is where Wado's generated web variants are today.

## Decision

These come from WEP: Variant Payload Design.

- `A | B` is an anonymous sum type with set semantics. Internally it carries a
  tag, as a variant does.
- Any type may be a constituent, a primitive or a variant's case included:
  `i32 | String`, `Shape::Circle | Shape::Rectangle`.
- A union flattens and drops duplicates: `(A | B) | (B | A)` is `A | B`. A union
  of one type, `A | A`, is `Union<A>`, not `A`, so every union is taken apart
  the same way.
- Subset binding takes a union apart with a type pattern, into one
  constituent or a smaller union: `if let ab: A | B = abcd`, and
  `match x { ab: A | B => …, c: C => … }`.
- A `match` over a union is checked for exhaustiveness.

Two more follow from why this WEP exists:

- A union is represented as a `variant`, so it crosses the Component Model
  boundary as one.
- A WebIDL union lowers to a union type, in place of a generated variant.

## Open questions

- Injection: whether a value of a constituent's type is accepted where the
  union is expected, so that `set_attribute("id", "x")` compiles. Also whether
  an `A | B` is accepted where an `A | B | C` is.
- Overlap: constituents related by `extends`, as `Node | Element` are, or a
  type and a newtype over it. Which case a value takes and which pattern
  matches it are both ambiguous. WebIDL requires a union's constituents to be
  distinguishable.
- Literals: which case an integer literal takes in `f64 | i64`.
- Generics: what `T | U` is where `T` and `U` turn out to be one type.
- Order: the earlier WEP orders constituents by first occurrence. At the
  Component Model boundary, `A | B` and `B | A` are one type only if they
  lower to one case order, and the cases need names there.
- `null`: whether `T | null` exists beside `Option<T>`, which WebIDL's
  nullable types lower to today.
- Inference: whether `if c { 1 } else { "a" }` infers a union, or a union needs
  an expected type.

## Roadmap

- [ ] Union type syntax (`A | B`)
- [ ] Union type normalization (flattening, deduplication to `Union<A>`)
- [ ] Discriminant generation for union types
- [ ] Subset binding in `if let` (`if let x: A | B = abc`)
- [ ] Subset binding in `match`
- [ ] Exhaustiveness checking for union types
- [ ] `wado-from-idl` lowers a WebIDL union to a union type
