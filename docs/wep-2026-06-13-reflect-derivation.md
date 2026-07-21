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
(WEP 2026-03-14 checklist): `Reflect` per-struct synthesis, and inline pack
binding `T: Reflect<Fields = [..F]>`.

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

- Finishing `Reflect` (per-struct synthesis + inline pack binding).
- Extending `Reflect` with per-field metadata (wire name, has-default, doc,
  `#[validate]` entries) and value-free type-level projection.
- Variant / enum / flags reflection.
- A generic built-in `#[validate(…)]` attribute with a closed vocabulary,
  enforced at the `Deserialize` boundary and exposed via `Reflect`.
- A staged migration of bespoke synthesizers to library code over `Reflect`.

Out of scope (recorded as future directions, not built here):

- Refinement predicates — in-memory invariants attached to a `newtype`,
  enforced at construction (surface syntax undecided).
- User-defined attributes under the `@[foo(…)]` syntax.
- Static (SMT-style) verification of any constraint.

## Decision

### Principle: derivation is library code over `Reflect`

The compiler's only job is to expose a type's structure at compile time. Every
derivation — the built-in `Inspect` / serde / `Default`, Jade's `JsonSchema`,
and future user-written ones — is a generic library `impl` over `Reflect`,
resolved at monomorphization. No per-capability synthesizer, no macros, no
dynamic reflection. The canonical shape, using the inline pack binding
established by WEP 2026-03-14 §11:

```wado
impl<T: Reflect<Fields = [..F]>, ..F: SomeTrait> SomeTrait for T {
    fn method(&self) -> R {
        let meta = Reflect::<T>::field_meta();   // value-level per-field metadata
        let parts = [..F::method_of()];          // type-level, value-free, per field type
        // combine meta + parts …
    }
}
```

### 1. Finish `Reflect` (the two unbuilt items from WEP 2026-03-14)

- **Per-struct synthesis.** The compiler synthesizes `impl Reflect for S` for
  every struct `S`, with `type Fields = [F_0, F_1, …]`, `fields(&self)`
  returning the value tuple, `field_names()` the source names.
- **Inline pack binding.** The bound `T: Reflect<Fields = [..F]>` extracts the
  pack `F` from the concrete `Fields` tuple at monomorphization, so a derivation
  can expand `[..F::method()]`. This is the mechanism that makes derivation
  **value-free**: `schema_for::<T>()` has no instance, yet `[..F::json_schema()]`
  still expands per field type.

### 1a. API surface: a sealed trait, called as `Reflect::<T>::…`

`Reflect` is a **sealed trait** — the compiler synthesizes `impl Reflect for S`
for every eligible struct, and a user `impl Reflect for T` is a compile error.
Sealing is what lets a derivation trust the projection: a program cannot forge a
type's reflection. `Reflect` stays a trait (not a struct or a builtin type)
because the derivation mechanism binds it as `T: Reflect<Fields = [..F]>`
(§1) — only a trait can carry that bound and its associated `Fields` pack.

Its members are reached **only** through the trait-qualified form, with the
subject type as a turbofish on the trait:

```wado
Reflect::<Point>::type_name()     // "Point"
Reflect::<Point>::field_names()   // ["x", "y"]
Reflect::<Point>::fields(&p)      // [p.x, p.y]
```

The trait-qualified form is the only spelling: the metadata is introspection
_about_ a type, not part of the type's own API, so it lives in the `Reflect`
namespace and never appears among a struct's own methods (a struct's method
namespace is entirely the author's). In generic code the same form applies to a
type parameter — `Reflect::<T>::field_names()`.

### 2. Extend `Reflect` with derivation metadata

The extension is **purely additive** over the §10 signature — `type Fields`,
`fields(&self)`, `field_names()`, and `type_name()` carry over unchanged, and
two members are added. `Reflect` is a sealed, compiler-only trait, so it is
`internal` and anchored — trait and each callable member — through the
compiler-item registry:

```wado
pub struct FieldMeta {
    name: String,            // source name, e.g. "user_name" (== field_names()[i])
    wire_name: String,       // serde rename / rename_all applied, e.g. "userName"
    has_default: bool,       // field has a default value `f: T = expr`
    doc: String,             // /// doc comment ("" if none)
    validate: List<ValidateEntry>,   // parsed #[validate(...)] (see §4)
    secret: bool,            // field is #[secret]; its Fields slot is Secret<F_k>
}

#[compiler_item("reflect")]
internal trait Reflect {
    type Fields;                          // [F_0, F_1, …]  — type-level pack  (§10)
    #[compiler_item("reflect_fields")]
    fn fields(&self) -> Self::Fields;     // value tuple (§10)
    #[compiler_item("reflect_field_names")]
    fn field_names() -> List<String>;     // source names  (§10)
    #[compiler_item("reflect_type_name")]
    fn type_name() -> String;             // (§10)
    fn field_meta() -> List<FieldMeta>;   // added: wire name, default, doc, validate
    fn type_doc() -> String;              // added: /// on the type itself
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
Schema `oneOf`, an `enum` to a string `enum`, and `flags` to its bit set.

### 3a. One sealed trait per type kind

Reflection stays split by kind — `Reflect` (struct), `ReflectVariant`,
`ReflectEnum`, `ReflectFlags` — rather than folding into one kind-reporting
trait. Derivations are static and monomorphized: a derivation handles each
kind with different code, so a per-kind bound is the natural selector. And
since a type is exactly one kind, blanket impls over different kinds are
disjoint by construction — `impl<T: Reflect> Trait for T` and
`impl<T: ReflectVariant> Trait for T` can never overlap, which keeps the
coherence question ("Open questions") no harder than the struct case. Each
trait follows §1a: sealed, `internal`, reached only as
`ReflectVariant::<T>::…`, never a bare `T::case_meta()`.

The meta structs in this section carry only what the compiler knows without
attribute parsing — source name, discriminant / bit, unit-ness. The §2
metadata extension later adds `wire_name` / `doc` / `validate` to field, case,
and bit metas uniformly, keeping this section orthogonal to §2 and §4.

### 3b. `ReflectEnum`

An enum value is its `i32` discriminant, so both directions of the value
bridge are trivial:

```wado
pub struct EnumCaseMeta {
    name: String,
    discriminant: i32,
}

#[compiler_item("reflect_enum")]
internal trait ReflectEnum {
    fn type_name() -> String;
    fn case_meta() -> List<EnumCaseMeta>;
    fn discriminant(&self) -> i32;
    fn from_discriminant(disc: i32) -> Option<Self>;
}
```

`from_discriminant` returns `Option` rather than trapping: its caller is
typically a deserializer, where an unknown discriminant is a normal error, not
a bug. Serialization is `case_meta()[discriminant()].name`; deserialization is
the reverse lookup plus `from_discriminant`.

### 3c. `ReflectFlags`

```wado
pub struct FlagBitMeta {
    name: String,
    bit: u64,
}

#[compiler_item("reflect_flags")]
internal trait ReflectFlags {
    fn type_name() -> String;
    fn bit_meta() -> List<FlagBitMeta>;
    fn bits(&self) -> u64;
    fn from_bits(raw: u64) -> Option<Self>;
}
```

`bit` / `bits()` normalize to `u64` regardless of the representation width, so
one generic derivation covers every flags type; the widening is lossless.
`from_bits` rejects unknown bits with `None` (CM semantics: unknown flag bits
are an error), same `Option` rationale as `from_discriminant`.

### 3d. `ReflectVariant` and the case walk

```wado
pub struct VariantCaseMeta {
    name: String,
    discriminant: i32,
    is_unit: bool,
}

#[compiler_item("reflect_variant")]
internal trait ReflectVariant {
    type Cases;       // payload types as a pack [P_0, P_1, …]; unit cases are ()
    type CaseTokens;  // [Case<Self, P_0>, Case<Self, P_1>, …] (§3e)
    fn type_name() -> String;
    fn case_meta() -> List<VariantCaseMeta>;
    fn discriminant(&self) -> i32;
    fn cases() -> Self::CaseTokens;
}
```

The struct walk does not transfer to variants: `fields(&self)` returns a tuple
because every field exists at once, but variant payloads are mutually
exclusive — there is no value tuple to return, and any payload access is
necessarily guarded by the runtime discriminant. Value-free derivation needs
no new machinery; `Cases` plus the §1 pack-map already give Jade's `oneOf`:

```wado
impl<T: ReflectVariant<Cases = [..P]>, ..P: JsonSchema> JsonSchema for T {
    fn json_schema() -> Schema {
        let payload_schemas: List<Schema> = List::from_tuple([..P::json_schema()]);
        // zip with case_meta() → oneOf
    }
}
```

Value-level derivation — serialize's case dispatch, deserialize's case
construction — is the new problem. A dedicated match-shaped expansion syntax
(Zig's `inline else`) was considered and rejected: it covers only the
destructuring direction — deserialization has no scrutinee, the value is yet
to be built — so construction would need a second mechanism anyway, and new
surface syntax is the most expensive kind of change (parser, formatter,
grammar, LSP). A visitor API fails differently: closures cannot be generic
over the per-case payload type.

### 3e. `Case<T, P>` tokens

Payload values cannot be enumerated, but case handles can. `cases()` returns a
tuple of tokens, one per case, each carrying the payload type statically and
the case index as a value:

```wado
pub struct Case<T, P> {
    index: i32,  // private; tokens are minted only by cases()
}

impl<T: ReflectVariant, P> Case<T, P> {
    fn index(&self) -> i32 { return self.index; }
    fn holds(&self, v: &T) -> bool {
        return ReflectVariant::<T>::discriminant(v) == self.index;
    }
    #[compiler_item("reflect_case_extract")]
    fn extract(&self, v: &T) -> P;         // payload of this case; traps if !holds(v)
    #[compiler_item("reflect_case_construct")]
    fn construct(&self, payload: P) -> T;  // builds this case around `payload`
}
```

A `for-of` over `cases()` is the same heterogeneous-tuple expansion as the
`fields()` walk: each iteration statically binds `Case<T, P_k>`, so `extract`
returns `P_k` and `construct` accepts `P_k`, fully typed. One mechanism serves
both directions:

```wado
// Serialize: dispatch on the live case (externally tagged form).
impl<T: ReflectVariant<Cases = [..P]>, ..P: Serialize> Serialize for T {
    fn serialize(&self, s: &mut Serializer) {
        let metas = ReflectVariant::<T>::case_meta();
        for let c of ReflectVariant::<T>::cases() {
            if c.holds(self) {
                let meta = metas[c.index()];
                if meta.is_unit {
                    s.string(&meta.name);
                } else {
                    s.begin_object();
                    s.key(&meta.name);
                    c.extract(self).serialize(s);
                    s.end_object();
                }
            }
        }
    }
}

// Deserialize: construct the named case — the direction a match-shaped
// syntax cannot express.
impl<T: ReflectVariant<Cases = [..P]>, ..P: Deserialize> Deserialize for T {
    fn deserialize(d: &mut Deserializer) -> Result<T, DeserializeError> {
        let key = d.case_name()?;
        let metas = ReflectVariant::<T>::case_meta();
        for let c of ReflectVariant::<T>::cases() {
            if metas[c.index()].name == key {
                return Result::Ok(c.construct(Deserialize::deserialize(d)?));
            }
        }
        return Result::Err(DeserializeError::unknown_case(key));
    }
}
```

Properties:

- Safety does not depend on sealing. `extract` lowers to a discriminant switch
  and traps on mismatch; a forged token can trap, never misread a payload.
  Duplicate payload types (`A(i32) | B(i32)`) stay unambiguous — the lowering
  keys on the index, not the payload type.
- Zero-cost after optimization. `cases()` returns a literal tuple, so after
  inlining `c.index()` is a constant, `holds` folds to `disc == k`, and the
  unrolled loop reduces to the match chain a hand-written impl would contain.
  No const generics needed.
- `extract` copies the payload (value semantics), the same behavior as
  `fields()` copying field values (see "Open questions" for a by-reference
  sibling).
- The name is `holds` because `matches` lexes as a keyword token (the infix
  pattern-test operator) and cannot be a method name.

### 3f. Constructor-mapped packs

The one type-system addition this section needs: under a
`T: ReflectVariant<Cases = [..P]>` bound, `cases()` has type
`[..Case<T, P>]` — the `Cases` pack mapped through a type constructor. The §1
pack-map splices a pack-independent return type repeated `|P|` times
(`TypePack::mapped_elem`); this generalizes the splice to substitute the pack
parameter per element (`Case<T, P>[P := P_k]` at position `k`). Identity packs
(`R = P`) and constant maps (`R` without the pack parameter) become special
cases of the same rule. At the concrete level `CaseTokens` is an ordinary
synthesized tuple type; the mapped pack only appears in generic projection,
exactly as `fields()` projects to `[..F]`.

### 3g. Staging

Ordered so each stage is independently testable and the type-system change
lands before its consumer:

1. `ReflectEnum` synthesis — no new machinery.
2. `ReflectFlags` synthesis — the `u64` bridge only.
3. Constructor-mapped packs — the `mapped_elem` generalization (§3f).
4. `ReflectVariant`, `Case<T, P>`, `cases()` synthesis, and the
   `extract` / `construct` lowering.

Each stage is proven with local-trait fixtures (the `reflect_derive_schema`
pattern); the completion proof is a library-code variant `Inspect` matching
the current synthesizer's output. One requirement surfaced by totalizing
`Cases`: a trait used as a `..P` bound needs a `()` impl for unit cases
(`Inspect` has one in the prelude; serde gains one with §5 if missing).

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
  invariants are the future refinement feature's job, not this attribute's.
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

- [x] Sealed trait + `Reflect::<T>::` trait-qualified dispatch (§1a): a user
      `impl Reflect` is a compile error; there is no bare `T::method()` spelling,
      so struct namespaces stay clean.
- [x] `Reflect` per-struct synthesis of `type_name()` / `field_names()` — the
      value-free string metadata. `field_names()` collects a homogeneous string
      tuple through the general `List::from_tuple` constructor.
- [x] `Reflect` per-struct synthesis of `fields(&self)` and the `Fields`
      associated tuple (the remaining §10 members). `fields(&self)` returns the
      values as a heterogeneous tuple `[self.f_0, …]`; the tuple type is
      registered as the struct's `Fields` associated-type resolution.
- Inline pack binding `impl<T: Reflect<Fields = [..F]>, ..F: Trait>` (the
  unbuilt item from WEP 2026-03-14 §11), staged:
  - [x] Blanket-impl selection on a synthesized bound: a struct satisfies the
        compiler-synthesized `T: Reflect` (and `T: Default`) bound, so a blanket
        derivation `impl<T: Reflect> Trait for T` resolves for it. The
        blanket-candidate bound check consults synthesized-trait eligibility, not
        only explicit `impl`s.
  - Generic `Reflect::<T>::…` resolution — the trait-qualified call resolves
    for a generic `T: Reflect` (deferred dispatch), not just a concrete
    struct:
    - [x] Value-free members `field_names()` / `type_name()`: resolved to their
          fixed return types and monomorphized to each struct's synthesized
          method via a type-param-receiver dispatch.
    - [x] `fields()`: resolves to the projected pack `[..F]` read off `T`'s
          `Reflect<Fields = [..F]>` bound, monomorphized per struct.
  - [x] Pack projection: the monomorphizer derives `F` from `T`'s `Fields` tuple
        (`resolve_assoc_type`) rather than from the receiver, so a `for-of` over
        `Reflect::<T>::fields(self)` walks the fields with per-element dispatch.
  - [x] Pack-map expansion `[..F::method()]` where the method's return type is
        pack-independent (e.g. `[..F::json_schema()]`). `TypePack` gained a
        `mapped_elem: Option<TypeId>`: `..F` (identity) vs `..F::method()` (the
        return type repeated `|F|` times). Substitution splices it, `for-of`
        binds the loop variable to the return type, and the value expansion
        rewrites the call per field type. Enables schema-style derivations.
- [ ] `Reflect` metadata extension — `field_meta()` (`wire_name`,
      `has_default`, `doc`, `validate`) and `type_doc()`.
- [ ] `#[validate(…)]` attribute — parse the closed vocabulary into
      `ValidateEntry`; surface it on `FieldMeta::validate`.
- [ ] `#[validate]` enforcement in the synthesized `Deserialize`
      (`DeserializeError` / `InvalidValue` on violation).
- Variant / enum / flags reflection (§3), staged per §3g:
  - [x] `ReflectEnum` synthesis (§3b): the four members plus generic
        projection under a `T: ReflectEnum` bound, including type-param
        static calls (`T::by_name`) dispatching through a blanket impl.
        Fixtures: `reflect_enum_meta`, `reflect_enum_derive`.
  - [x] `ReflectFlags` synthesis with the `u64`-normalized bit bridge (§3c),
        including generic projection under a `T: ReflectFlags` bound.
        Fixtures: `reflect_flags_meta`, `reflect_flags_derive`.
  - [x] Constructor-mapped packs: generalize `TypePack::mapped_elem` to
        per-element pack substitution (§3f).
  - [x] `ReflectVariant` + `Case<T, P>`: synthesis of the concrete members,
        `cases()`, the `extract` / `construct` lowering, and the generic
        `Cases = [..P]` projection with the constructor-mapped `cases()` type
        (§3d–§3f). Fixtures: `reflect_variant_meta`, `reflect_variant_cases`,
        `reflect_variant_derive`.
  - [ ] Completion proof: a library-code variant `Inspect` matching the
        synthesizer's output (§3g).
- [ ] Migrate `Inspect` / `InspectAlt` to the `Reflect`-based impl; remove the
      compiler-magic struct path (WEP 2026-03-14's stated goal).
- [ ] Migrate serde struct/variant `Serialize` / `Deserialize` to library code;
      move `#[validate]` enforcement into the generic `Deserialize`.
- [ ] Migrate `Default` to `[..F::default()]`.
- [ ] Coherence rules for a blanket `Reflect` impl vs. concrete impls
      (depends on the unchecked coherence items in WEP 2026-03-14).

## Future directions

- **Refinement predicates.** In-memory invariants on a `newtype`, enforced at
  construction / `as` conversion in the `assert` doctrine ("cannot be disabled,
  always reliable"), distinct from `#[validate]`'s wire-boundary guard.
  Anchoring a refinement to a `newtype` bounds _when_ the check fires
  (conversion only), à la Ada subtype predicates. General predicates are
  enforced but opaque to schema derivation; only a structured subset
  (comparisons, length) could map to schema keywords. This is the deliberate
  division of labor: `#[validate]` guards data crossing the boundary; the
  refinement feature guards data already in memory. (Its surface syntax is
  undecided and out of scope here.)
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
  `List<FieldMeta>` descriptor (chosen here) vs. associated consts. (Whether
  variant/enum/flags fold into one kind-reporting trait is settled: separate
  traits, §3a.)
- **By-reference case extraction.** `Case::extract` copies the payload (value
  semantics). Large payloads may want a `&P`-returning sibling once the
  `Inspect` migration can measure the copy cost; `fields()` shares the
  property, so a fix should cover both.
- **`#[validate]` recognized key set.** Which JSON Schema assertions earn a
  first-class key versus being left to hand-authoring on a `Schema` value.
- **Coherence.** Interaction with the variadic coherence rules still unchecked
  in WEP 2026-03-14 (non-VG-wins / VG-overlap-forbidden) when a blanket
  `impl<T: Reflect<Fields = [..F]>, ..F> Trait for T` meets concrete impls
  (e.g. a primitive's own `impl Trait`).
- **Enforcement breadth.** Whether `#[validate]` should ever also fire at
  construction. Deferred: that is the refinement feature's domain, kept
  separate on purpose.
