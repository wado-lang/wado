# Library-Defined Derivation: `Reflect` Extensions and the `#[validate]` Attribute

## Context

Wado derives a growing set of type-directed traits — `Eq`, `Ord`, `Inspect`,
`Serialize`, `Deserialize`, `Default` — by **bespoke compiler synthesis** (each
hand-written into `synthesis/`, e.g. `serde_synth.rs`). Every new type-directed
capability has so far meant another hardcoded synthesiser. That model has two
limits that a concrete new requirement now makes acute:

1. It does not scale: each capability is compiler work, and the compiler grows
   a special case per trait.
2. It is closed to libraries. A third-party package cannot get a synthesiser,
   and Wado has **no macros and no dynamic reflection**, so it cannot introspect
   a type itself.

The forcing function is [`wep-2026-06-13-jade.md`](./wep-2026-06-13-jade.md)
(JSON Schema for Wado). Jade's capability B — "given a Wado type, produce its
JSON Schema" — is exactly a type-directed derivation, but Jade is an ordinary
package (`wado:jade`), not the compiler. It cannot be a synthesiser. Either
type→schema is impossible as library code, or the language grows a general
facility that lets libraries derive over a type's structure.

The escape was already chosen, in
[`wep-2026-03-14-variadic-type-parameters.md`](./wep-2026-03-14-variadic-type-parameters.md)
§10: **`Reflect`**, a compiler-synthesised trait that exposes a struct's fields
as a typed tuple at compile time (`type Fields = [..F]`, `field_names()`), in
the same static, monomorphised, no-dynamic-reflection lineage as tuple `for-of`
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
- A staged migration of bespoke synthesisers to library code over `Reflect`.

Out of scope (recorded as future directions, not built here):

- Refinement predicates (`foo: T where <predicate>`).
- User-defined attributes under the `@[foo(…)]` syntax.
- Static (SMT-style) verification of any constraint.

## Decision

### Principle: derivation is library code over `Reflect`

The compiler's only job is to expose a type's structure at compile time. Every
derivation — the built-in `Inspect` / serde / `Default`, Jade's `JsonSchema`,
and future user-written ones — is a generic library `impl` over `Reflect`,
resolved at monomorphisation. No per-capability synthesiser, no macros, no
dynamic reflection. The canonical shape:

```wado
impl<T: Reflect<Fields = [..F]>, ..F: SomeTrait> SomeTrait for T {
    fn method(&self) -> R {
        let meta = T::field_meta();        // value-level per-field metadata
        let parts = [..F::method_of()];    // type-level, value-free, per field type
        // combine meta + parts …
    }
}
```

### 1. Finish `Reflect` (the two unbuilt items from WEP 2026-03-14)

- **Per-struct synthesis.** The compiler synthesises `impl Reflect for S` for
  every struct `S`, with `type Fields = [F_0, F_1, …]`, `fields(&self)`
  returning the value tuple, `field_names()` the source names.
- **`where`-clause pack binding.** `T: Reflect<Fields = [..F]>` extracts the
  pack `F` from the concrete `Fields` tuple at monomorphisation, so a derivation
  can expand `[..F::method()]`. This is the mechanism that makes derivation
  **value-free**: `schema_for::<T>()` has no instance, yet `[..F::json_schema()]`
  still expands per field type.

### 2. Extend `Reflect` with derivation metadata

`Reflect` gains value-level, per-field metadata, kept in index correspondence
with the type-level `Fields` pack:

```wado
pub struct FieldMeta {
    name: String,            // source name, e.g. "user_name"
    wire_name: String,       // serde rename / rename_all applied, e.g. "userName"
    has_default: bool,       // #[serde(default)] or `f: T = expr`
    doc: String,             // /// doc comment ("" if none)
    validate: List<ValidateEntry>,   // parsed #[validate(...)] (see §4)
}

pub trait Reflect {
    type Fields;                       // [F_0, F_1, …]  — type-level pack
    fn fields(&self) -> Self::Fields;  // value tuple (when an instance exists)
    fn field_meta() -> List<FieldMeta>;
    fn type_name() -> String;
    fn type_doc() -> String;
}
```

Two channels are unavoidable and intentional: field **types** must stay a
type-level pack (`Fields = [..F]`) so `[..F::method()]` can expand; everything
else is value-level metadata (`field_meta()`). A derivation zips the two by
index.

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

Recognised keys (initial set): `min_length` / `max_length`, `minimum` /
`maximum` / `exclusive_minimum` / `exclusive_maximum`, `multiple_of`,
`pattern`, `format`, `min_items` / `max_items`, `unique_items`. The compiler
parses these into `ValidateEntry` values and does **two** things with them — and
together they are what stop the attribute from being silently inert, the
failure mode of Rust's `validator` crate (annotate, but nothing runs unless you
remember to call `.validate()`):

- **Enforce at the `Deserialize` boundary.** The synthesised `Deserialize`, after
  reading each field, runs that field's checks; a violation returns a
  `DeserializeError` (kind `InvalidValue`) with the field's offset. This holds
  for _anyone_ deserialising, with or without Jade. The trust boundary —
  untrusted external data entering the program's types — is the natural and
  honest place to enforce a wire contract. Values constructed in trusted code
  (a struct literal) are deliberately _not_ checked here; whole-program
  invariants are the future `where` refinement's job, not this attribute's.
- **Expose via `Reflect`.** The same entries appear in `FieldMeta::validate`, so
  Jade (and any schema/validation library) reads them and emits the
  corresponding schema keywords. A _closed_ vocabulary is exactly what lets one
  annotation be both enforced (the compiler knows each key's boundary check) and
  introspected (the compiler/library knows each key's schema mapping).

`description` is _not_ a `#[validate]` concern: it comes from `///` doc
comments, surfaced as `FieldMeta::doc` / `type_doc()`.

### 5. Migrate bespoke synthesisers to library code (staged)

Once §1–§3 land, the hand-written synthesisers become generic library impls
over `Reflect`, shrinking the compiler's special-case surface:

1. **`Inspect` / `InspectAlt`** first — already the stated goal of WEP
   2026-03-14; the lowest-risk proof.
2. **serde `Serialize` / `Deserialize`** — the struct/variant walks move to
   library code; the compiler retains only `Reflect` synthesis plus the
   `#[validate]` boundary enforcement hook.
3. **`Default`** — `[..F::default()]` over the field pack.

Each migration is its own PR with golden-output parity against the current
synthesiser. The end state: the compiler synthesises `Reflect` (and enforces
`#[validate]`); everything else is library code.

## Future directions

- **Refinement predicates — `foo: T where <predicate>`.** For true _in-memory
  invariants_, enforced at construction / `as` conversion in the `assert`
  doctrine ("cannot be disabled, always reliable"), distinct from
  `#[validate]`'s wire-boundary guard. Anchoring a refinement to a `newtype`
  bounds _when_ the check fires (conversion only), à la Ada subtype predicates.
  General predicates are enforced but opaque to schema derivation; only a
  structured subset (comparisons, length) could map to schema keywords. This is
  the deliberate division of labour: `#[validate]` guards data crossing the
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
  synthesisers; new type-directed derivations are ordinary library code.
- Jade's capability B becomes pure library code; the compiler never learns
  about Jade.
- The compiler's special-case surface _shrinks_ over time as `Inspect` / serde /
  `Default` migrate onto `Reflect`.
- No macros, no dynamic reflection; everything stays static and monomorphised,
  consistent with Wado's existing compile-time-expansion idioms.
- `#[validate]` is generic and enforced, so a constraint is never silently
  ignored — consistent with the `assert` "always reliable" doctrine.

### Costs and trade-offs

- `Reflect` must thread serde naming and `#[validate]` into its metadata, so the
  trait is coupled to those vocabularies. This is the price of moving derivation
  out of the compiler: the metadata the synthesisers read internally must become
  part of the exposed surface.
- Variant / enum / flags reflection is real new compiler work beyond the
  struct-only design of WEP 2026-03-14.
- Enforcing `#[validate]` in `Deserialize` grows core serde with a closed
  validation vocabulary. It is generic (not Jade-specific) and bounded by the
  recognised key set, but it is core surface nonetheless.
- A generic field-walk derivation may monomorphise to less tight code than a
  hand-written synthesiser. Each migration in §5 must check generated-code
  parity (size and speed), not just output parity.

### Open questions

- **`Reflect` metadata API shape.** Parallel `List`s vs. a single
  `List<FieldMeta>` descriptor (chosen here) vs. associated consts; and whether
  variant/enum/flags fold into one kind-reporting `Reflect` or stay separate
  traits.
- **`#[validate]` recognised key set.** Which JSON Schema assertions earn a
  first-class key versus being left to hand-authoring on a `Schema` value.
- **Coherence.** Interaction with the variadic coherence rules still unchecked
  in WEP 2026-03-14 (non-VG-wins / VG-overlap-forbidden) when a blanket
  `impl<T: Reflect<…>> Trait for T` meets concrete impls.
- **Enforcement breadth.** Whether `#[validate]` should ever also fire at
  construction. Deferred: that is the refinement `where` feature's domain, kept
  separate on purpose.
