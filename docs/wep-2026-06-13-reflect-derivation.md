# WEP: Library-Defined Derivation: `Reflect` Extensions and the `#[validate]` Attribute

## Context

Wado derives a growing set of type-directed traits — `Eq`, `Ord`, `Inspect`,
`Serialize`, `Deserialize`, `Default` — by **bespoke compiler synthesis** (each
hand-written into `synthesis/`, e.g. `serde_synth.rs`). Every new type-directed
capability has so far meant another hardcoded synthesizer. That model has two
limits that a concrete new requirement now makes acute:

1. It does not scale: each capability is compiler work, and the compiler grows
   a special case per trait.
2. It is closed to libraries. A third-party package cannot get a synthesizer,
   and Wado has **no macros and no dynamic reflection**, so it cannot introspect
   a type itself.

The forcing function is [`wep-2026-06-13-jade.md`](./wep-2026-06-13-jade.md)
(JSON Schema for Wado). Jade's capability B — "given a Wado type, produce its
JSON Schema" — is exactly a type-directed derivation, but Jade is an ordinary
package (`wado:jade`), not the compiler. It cannot be a synthesizer. Either
type→schema is impossible as library code, or the language grows a general
facility that lets libraries derive over a type's structure.

The escape was already chosen, in
[`wep-2026-03-14-variadic-type-parameters.md`](./wep-2026-03-14-variadic-type-parameters.md)
§10: **`Reflect`**, a compiler-synthesized trait that exposes a struct's fields
as a typed tuple at compile time (`type Fields = [..F]`, `field_names()`), in
the same static, monomorphized, no-dynamic-reflection lineage as tuple `for-of`
and `[..T::default()]`. That WEP's stated goal is to "remove compiler-magic
struct `Inspect`; replace with the `Reflect`-based impl" — i.e. move derivation
out of the compiler into library code. The variadic substrate (type packs,
tuple `for-of`, `[..T::method()]` expansion, variadic trait impls) is
implemented; the two pieces derivation most needs are designed but **unbuilt**
(WEP 2026-03-14 checklist): `Reflect` per-struct synthesis, and `where`-clause
pack binding `T: Trait<Assoc = [..F]>`.

Bringing Jade's needs to that design surfaced gaps. Current `Reflect` exposes
only field name + type + value. Schema derivation (and, once migrated, serde
itself) additionally needs each field's **wire name** (serde `rename` /
`rename_all` applied), **whether it has a default** (→ JSON Schema `required`),
its **doc string** (→ `description`), and the **validation constraints**
(`minLength`, `minimum`, `pattern`, `format`, …) that no type metadata carries
today — and that a library cannot add via an attribute of its own.

This WEP evolves WEP 2026-03-14 §10 to close those gaps generically, and adds a
single generic built-in `#[validate(…)]` attribute. It is the language-side
dependency of the Jade WEP; Jade ships no compiler change and consumes only
what is decided here.

### Scope

In scope:

- Finishing `Reflect` (per-struct synthesis + `where`-clause pack binding).
- Extending `Reflect` with per-field metadata (wire name, has-default, doc,
  `#[validate]` entries) and value-free type-level projection.
- Variant / enum / flags reflection.
- A generic built-in `#[validate(…)]` attribute with a closed vocabulary,
  enforced at the `Deserialize` boundary and exposed via `Reflect`.
- A staged migration of bespoke synthesizers to library code over `Reflect`.

Out of scope (recorded as future directions, not built here):

- Refinement predicates (`foo: T where <predicate>`).
- User-defined attributes under the `@[foo(…)]` syntax.
- Static (SMT-style) verification of any constraint.

## Decision

### Principle: derivation is library code over `Reflect`

The compiler's only job is to expose a type's structure at compile time. Every
derivation — the built-in `Inspect` / serde / `Default`, Jade's `JsonSchema`,
and future user-written ones — is a generic library `impl` over `Reflect`,
resolved at monomorphization. No per-capability synthesizer, no macros, no
dynamic reflection. The canonical shape, using the `where`-clause pack binding
established by WEP 2026-03-14 §11:

```wado
impl<T, ..F: SomeTrait> SomeTrait for T
where T: Reflect<Fields = [..F]>
{
    fn method(&self) -> R {
        let meta = T::field_meta();        // value-level per-field metadata
        let parts = [..F::method_of()];    // type-level, value-free, per field type
        // combine meta + parts …
    }
}
```

### 1. Finish `Reflect` (the two unbuilt items from WEP 2026-03-14)

- **Per-struct synthesis.** The compiler synthesizes `impl Reflect for S` for
  every struct `S`, with `type Fields = [F_0, F_1, …]`, `fields(&self)`
  returning the value tuple, `field_names()` the source names.
- **`where`-clause pack binding.** `T: Reflect<Fields = [..F]>` extracts the
  pack `F` from the concrete `Fields` tuple at monomorphization, so a derivation
  can expand `[..F::method()]`. This is the mechanism that makes derivation
  **value-free**: `schema_for::<T>()` has no instance, yet `[..F::json_schema()]`
  still expands per field type.

### 1a. API surface: a sealed trait, called as `Reflect::<T>::…`

`Reflect` is a **sealed trait** — the compiler synthesizes `impl Reflect for S`
for every eligible struct, and a user `impl Reflect for T` is a compile error.
Sealing is what lets a derivation trust the projection: a program cannot forge a
type's reflection. `Reflect` stays a trait (not a struct or a builtin type)
because the derivation mechanism binds it as `where T: Reflect<Fields = [..F]>`
(§1) — only a trait can carry that bound and its associated `Fields` pack.

Its members are reached **only** through the trait-qualified form, with the
subject type as a turbofish on the trait:

```wado
Reflect::<Point>::type_name()     // "Point"
Reflect::<Point>::field_names()   // ["x", "y"]
Reflect::<Point>::fields(&p)      // [p.x, p.y]
```

There is deliberately no bare `Point::type_name()` / `p.fields()` spelling: the
metadata is introspection _about_ a type, not part of the type's own API, so it
lives in the `Reflect` namespace and never pollutes the struct's method
namespace (a user's own `Point::type_name()` is unaffected). In generic code the
same form applies to a type parameter — `Reflect::<T>::field_names()`.

### 2. Extend `Reflect` with derivation metadata

The extension is **purely additive** over the §10 signature — `type Fields`,
`fields(&self)`, `field_names()`, and `type_name()` are retained verbatim (so
the §11 struct-`Inspect` example keeps compiling), and two members are added:

```wado
pub struct FieldMeta {
    name: String,            // source name, e.g. "user_name" (== field_names()[i])
    wire_name: String,       // serde rename / rename_all applied, e.g. "userName"
    has_default: bool,       // field has a default value `f: T = expr`
    doc: String,             // /// doc comment ("" if none)
    validate: List<ValidateEntry>,   // parsed #[validate(...)] (see §4)
    secret: bool,            // field is #[secret]; its Fields slot is Secret<F_k>
}

#[comp_feature("reflect")]
pub trait Reflect {
    type Fields;                       // [F_0, F_1, …]  — type-level pack  (§10)
    fn fields(&self) -> Self::Fields;  // value tuple (when an instance exists)  (§10)
    fn field_names() -> List<String>;  // source names  (§10)
    fn type_name() -> String;          // (§10)
    fn field_meta() -> List<FieldMeta>;  // added: wire name, default, doc, validate
    fn type_doc() -> String;             // added: /// on the type itself
}
```

`field_meta()` keeps index correspondence with `field_names()` and the
type-level `Fields` pack. A `#[secret]` field occupies its slot in `Fields` as
the value-opaque `Secret<F_k>` projection — see
[Struct Walkability](./wep-2026-07-10-struct-walkability.md). Two channels are unavoidable and intentional: field
**types** must stay a type-level pack (`Fields = [..F]`) so `[..F::method()]`
can expand; everything else is value-level metadata (`field_meta()`). A
derivation zips the two by index. Like the original, `Reflect` is
compiler-synthesized, cannot be user-implemented, and is callable only in
monomorphized contexts.

### 3. Variant / enum / flags reflection

`Reflect` as designed is struct-only. Sum and bitmask types get analogous
compile-time introspection so a derivation can lower a `variant` to JSON
Schema `oneOf`, an `enum` to a string `enum`, and `flags` to its bit set:

```wado
pub trait ReflectVariant {
    type Cases;                          // payload types as a pack ([P_0, P_1, …])
    fn case_meta() -> List<CaseMeta>;    // name, wire_name, discriminant, is_unit, doc
    fn type_name() -> String;
}
// ReflectEnum (discriminants + names), ReflectFlags (bit names) likewise.
```

The exact unification (one `Reflect` that reports a type kind, vs. three
traits) is an API question (see "Open questions"); the capability set is fixed
here.

### 4. The `#[validate(…)]` attribute

A single generic, built-in attribute with a **closed declarative vocabulary** —
not owned by any library:

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

Recognized keys (initial set): `min_length` / `max_length`, `minimum` /
`maximum` / `exclusive_minimum` / `exclusive_maximum`, `multiple_of`,
`pattern`, `format`, `min_items` / `max_items`, `unique_items`. The compiler
parses these into `ValidateEntry` values and does **two** things with them — and
together they are what stop the attribute from being silently inert, the
failure mode of Rust's `validator` crate (annotate, but nothing runs unless you
remember to call `.validate()`):

- **Enforce at the `Deserialize` boundary.** After reading each field,
  `Deserialize` runs that field's checks; a violation returns a
  `DeserializeError` (kind `InvalidValue`) with the field's offset. This holds
  for _anyone_ deserializing, with or without Jade. The trust boundary —
  untrusted external data entering the program's types — is the natural and
  honest place to enforce a wire contract. Values constructed in trusted code
  (a struct literal) are deliberately _not_ checked here; whole-program
  invariants are the future `where` refinement's job, not this attribute's.
  (Where the check physically lives — synthesized `Deserialize` now, generic
  library `Deserialize` after §5 — is a migration detail; the guarantee is the
  same either way.)
- **Expose via `Reflect`.** The same entries appear in `FieldMeta::validate`, so
  Jade (and any schema/validation library) reads them and emits the
  corresponding schema keywords. A _closed_ vocabulary is exactly what lets one
  annotation be both enforced (the compiler knows each key's boundary check) and
  introspected (the compiler/library knows each key's schema mapping).

`description` is _not_ a `#[validate]` concern: it comes from `///` doc
comments, surfaced as `FieldMeta::doc` / `type_doc()`.

### 5. Migrate bespoke synthesizers to library code (staged)

Once §1–§3 land, the hand-written synthesizers become generic library impls
over `Reflect`, shrinking the compiler's special-case surface:

1. **`Inspect` / `InspectAlt`** first — already the stated goal of WEP
   2026-03-14; the lowest-risk proof.
2. **serde `Serialize` / `Deserialize`** — the struct/variant walks move to
   library code. `Deserialize`'s `#[validate]` enforcement moves with it, now
   reading `FieldMeta::validate`; the compiler is left exposing the entries via
   `Reflect`, nothing more.
3. **`Default`** — `[..F::default()]` over the field pack.

Each migration is its own PR with golden-output parity against the current
synthesizer. The end state: the compiler synthesizes `Reflect` (and exposes
`#[validate]` through it); everything else, enforcement included, is library
code.

## Implementation checklist

Ordered so each step is independently testable; Layer-B-of-Jade is unblocked
after the first three.

- [ ] `Reflect` per-struct synthesis — `Fields`, `fields`, `field_names`,
      `type_name` (the unbuilt item from WEP 2026-03-14 §10).
- [ ] `where`-clause pack binding `T: Reflect<Fields = [..F]>` (the unbuilt
      item from WEP 2026-03-14 §11).
- [ ] `Reflect` metadata extension — `field_meta()` (`wire_name`,
      `has_default`, `doc`, `validate`) and `type_doc()`.
- [ ] `#[validate(…)]` attribute — parse the closed vocabulary into
      `ValidateEntry`; surface it on `FieldMeta::validate`.
- [ ] `#[validate]` enforcement in the synthesized `Deserialize`
      (`DeserializeError` / `InvalidValue` on violation).
- [ ] `ReflectVariant` / `ReflectEnum` / `ReflectFlags` synthesis.
- [ ] Migrate `Inspect` / `InspectAlt` to the `Reflect`-based impl; remove the
      compiler-magic struct path (WEP 2026-03-14's stated goal).
- [ ] Migrate serde struct/variant `Serialize` / `Deserialize` to library code;
      move `#[validate]` enforcement into the generic `Deserialize`.
- [ ] Migrate `Default` to `[..F::default()]`.
- [ ] Coherence rules for a blanket `Reflect` impl vs. concrete impls
      (depends on the unchecked coherence items in WEP 2026-03-14).

## Future directions

- **Refinement predicates — `foo: T where <predicate>`.** For true _in-memory
  invariants_, enforced at construction / `as` conversion in the `assert`
  doctrine ("cannot be disabled, always reliable"), distinct from
  `#[validate]`'s wire-boundary guard. Anchoring a refinement to a `newtype`
  bounds _when_ the check fires (conversion only), à la Ada subtype predicates.
  General predicates are enforced but opaque to schema derivation; only a
  structured subset (comparisons, length) could map to schema keywords. This is
  the deliberate division of labor: `#[validate]` guards data crossing the
  boundary; `where` guards data already in memory.
- **User-defined attributes — `@[foo(…)]`.** A distinct syntax for
  library-interpreted attributes, kept visually separate from built-in `#[…]`
  so it is always clear which attributes the compiler knows and which a library
  gives meaning to — the ambiguity Rust never resolved (`#[serde(…)]` and
  built-in `#[inline]` look identical). Exposed via `Reflect` as opaque
  per-field metadata. `#[validate]` is the built-in that demonstrates the
  generic, non-library-owned principle now; `@[…]` is the open system later.

## Consequences

### Benefits

- One mechanism (`Reflect`) replaces an open-ended series of bespoke
  synthesizers; new type-directed derivations are ordinary library code.
- Jade's capability B becomes pure library code; the compiler never learns
  about Jade.
- The compiler's special-case surface _shrinks_ over time as `Inspect` / serde /
  `Default` migrate onto `Reflect`.
- No macros, no dynamic reflection; everything stays static and monomorphized,
  consistent with Wado's existing compile-time-expansion idioms.
- `#[validate]` is generic and enforced, so a constraint is never silently
  ignored — consistent with the `assert` "always reliable" doctrine.

### Costs and trade-offs

- `Reflect` must thread serde naming and `#[validate]` into its metadata, so the
  trait is coupled to those vocabularies. This is the price of moving derivation
  out of the compiler: the metadata the synthesizers read internally must become
  part of the exposed surface.
- Variant / enum / flags reflection is real new compiler work beyond the
  struct-only design of WEP 2026-03-14.
- Enforcing `#[validate]` in `Deserialize` grows core serde with a closed
  validation vocabulary. It is generic (not Jade-specific) and bounded by the
  recognized key set, but it is core surface nonetheless.
- A generic field-walk derivation may monomorphize to less tight code than a
  hand-written synthesizer. Each migration in §5 must check generated-code
  parity (size and speed), not just output parity.

### Open questions

- **`Reflect` metadata API shape.** Parallel `List`s vs. a single
  `List<FieldMeta>` descriptor (chosen here) vs. associated consts; and whether
  variant/enum/flags fold into one kind-reporting `Reflect` or stay separate
  traits.
- **`#[validate]` recognized key set.** Which JSON Schema assertions earn a
  first-class key versus being left to hand-authoring on a `Schema` value.
- **Coherence.** Interaction with the variadic coherence rules still unchecked
  in WEP 2026-03-14 (non-VG-wins / VG-overlap-forbidden) when a blanket
  `impl<T, ..F> Trait for T where T: Reflect<Fields = [..F]>` meets concrete
  impls (e.g. a primitive's own `impl Trait`).
- **Enforcement breadth.** Whether `#[validate]` should ever also fire at
  construction. Deferred: that is the refinement `where` feature's domain, kept
  separate on purpose.
