# WEP: The `Ref` Marker Trait and Index-Trait Gating

Defines `Ref` — the sealed marker for reference-identity types — and uses it to
gate the reference-returning index traits (`IndexRef` / `IndexMutRef`).
Supersedes the `Reference`-bound sketch and the `Index` / `IndexMut` names in
[Indexing Traits Design](./wep-2026-01-20-indexing-traits.md) and corrects the
type classification implied by
[Reference Representation](./wep-2026-06-13-reference-representation.md).

## Context

`IndexRef<I>` returns `&Self::Output` and `IndexMutRef<I>` returns
`&mut Self::Output`. Handing out `&T` into a container element is only meaningful
when the element _is_ a reference you can alias — a Wasm GC handle. For
`List<i32>` there is no addressable cell to reference (`array.get` yields the
scalar by value); for `List<Point>` the element is a GC struct reference that
`&Point` names directly.

So the index traits need a predicate: _which `T` can back a real `&T` element
reference?_ The old [Indexing Traits](./wep-2026-01-20-indexing-traits.md) WEP
sketched `where T: Reference` but never pinned the membership, and
[Reference Representation](./wep-2026-06-13-reference-representation.md)
conflated two _different_ properties under one "in-place vs replace-on-assign"
axis. This WEP separates them and names the right one `Ref`.

## Two distinct properties

These are not the same question, and earlier docs treated them as one:

1. Reference identity — is a `T` value a Wasm GC reference (a heap-object
   handle), so `&T` aliases the very same object? This is what `IndexRef` needs.
2. In-place mutability — can a write _through_ `&mut T` land on the existing
   object (`r.f = v`, `r[i] = v`), versus needing to replace the whole value
   (`*r = v`)? This is [Reference Representation](./wep-2026-06-13-reference-representation.md)'s
   boxing axis (`Box<T>` for the replace-on-assign set).

A `variant` shows the split cleanly: it _is_ a GC struct reference (property 1
holds — `&variant` is a live handle you can read and pattern-match) yet it is
replace-on-assign (property 2 fails — changing the case replaces the whole
value). `Ref` is property 1.

## Decision: `Ref` is reference identity

`T: Ref` iff a value of `T` is represented as a Wasm GC reference — equivalently,
`type_id_to_wir_type(T)` is a reference type (`WirType::Ref` / `AbstractRef`),
not a scalar. This is a single, already-existing distinction in the backend.

| Category      | Types                                                                                                         | `Ref` |
| ------------- | ------------------------------------------------------------------------------------------------------------- | ----- |
| GC references | `struct`, `variant`, `List<T>`, `String`, tuples, `TreeMap` / `TreeSet` & GC-struct generics, `fn` / `fn mut` | yes   |
| Wide ints     | `i128` / `u128` (GC low/high pair structs, not scalars)                                                       | yes   |
| References    | `&T`, `&mut T` (a reference value is itself a GC handle)                                                      | yes   |
| Scalars       | `i8`…`u64`, `f32`, `f64`, `bool`, `char`; `enum`; `flags`                                                     | no    |
| Handles       | `resource` (opaque `i32` index into the CM handle table)                                                      | no    |
| Non-values    | `()` (unit), `never`                                                                                          | no    |

`Newtype` follows its base type.

Two entries deserve emphasis because they are exactly where the earlier framing
was wrong:

- `resource` is **not** `Ref`. A resource handle lowers to `i32` — an index into
  the component's handle table, not a GC reference. It has _handle_ semantics,
  not _reference_ semantics: you cannot alias it or hand out `&resource` into a
  container slot; `array.get` on a resource element returns the `i32` by value.
  The old "in-place reference types" grouping and the coarse `is_reference_type`
  (`!(primitive | unit | never)`) both wrongly counted it as a reference.
- `&T` **is** `Ref`. A reference value is always a GC handle — the referent's
  shared handle for a GC-reference referent, or a `Box<T>` handle for a scalar
  referent — so a `List<&T>` element is a real reference regardless of `T`.

`Ref` deliberately includes the replace-on-assign GC types (`variant`, `fn`):
they have reference identity even though they are boxed for `&mut`. The `&mut`
write-back soundness for those is a _separate_ gate (see below), not a reason to
exclude them from `Ref`.

Note `Ref` is _reference identity_, unrelated to whether a value _holds_ a
reference: a `struct` with `&T` fields is `Ref` because the struct itself is a GC
object, and a scalar `i32` is not `Ref` even though `&i32` (which is `Ref`) can
point at one.

### Sealed, prelude-declared, compiler-synthesized

`Ref` is a marker trait declared in `core:prelude/traits`, anchored by
`#[compiler_item("ref")]`, and sealed: the compiler synthesizes `impl Ref for T`
for every eligible `T`, and a user `impl Ref` is rejected (a user who declares
their own `trait Ref` owns that name and is unaffected) — the same sealing
`Reflect` uses.

```wado
#[compiler_item("ref")]
internal trait Ref {}
```

Eligibility is the reference-identity predicate above, evaluated at the
type-system level (mirroring the WIR predicate): true for everything except
scalar primitives (`i128` / `u128` excepted), `enum`, `flags`, `resource` /
generic resources, `unit`, and `never`.

## Decision: the index traits

Four traits, named by what indexing yields — `Value`, `Ref`, `MutRef`, or a
write (`Assign`) — so the surface is symmetric and self-describing:

```wado
// Value-semantics core — every indexable container. No `Ref` bound: reads copy
// out, writes copy in, so any element type works.
internal trait IndexValue<I>  { type Output; fn index_value(&self, i: I) -> Self::Output; }
internal trait IndexAssign<I> { type Input;  fn index_assign(&mut self, i: I, v: Self::Input); }

// Reference indexing — `Output` must have reference identity. The `Ref` bound
// lives on the associated type, so returning `&Output` is only well-typed when
// the element is actually a reference.
internal trait IndexRef<I>    { type Output: Ref; fn index_ref(&self, i: I) -> &Self::Output; }
internal trait IndexMutRef<I> { type Output: Ref; fn index_mut_ref(&mut self, i: I) -> &mut Self::Output; }
```

`IndexRef` / `IndexMutRef` rename Rust's `Index` / `IndexMut`: the `Value` /
`Ref` / `MutRef` suffixes read off the return shape, matching `IndexValue` /
`IndexAssign` (Wado already diverges from Rust's two-trait `Index` model, so
in-Wado symmetry wins over Rust familiarity). Methods follow the same rule:
`index_value` / `index_ref` / `index_mut_ref` / `index_assign`.

Anchoring `Ref` on `type Output` (rather than as a `where T: Ref` on each impl)
makes "you may return `&Output` only when `Output` is a reference" a property of
the trait itself: `impl IndexRef<i32> for List<i32>` fails to type-check because
`i32: Ref` does not hold, with no per-container special-casing.

The index traits are the extension point for _user_ containers. `List<T>` /
arrays are not implemented on top of them: they provide `IndexValue` /
`IndexAssign` for the `[]` value read/write of every element type, and their
element _references_ (`&xs[i]`, `&mut xs[i]`, `xs[i].m()`, `&mut`-iteration) are
compiler intrinsics governed by
[Reference Representation](./wep-2026-06-13-reference-representation.md), not by
`IndexRef` / `IndexMutRef`. `List` deliberately does _not_ implement the
reference index traits — that keeps a single lowering for array element refs
rather than a trait impl shadowing the intrinsic.

```wado
impl IndexValue<i32>  for List<T> { type Output = T; /* array.get */ }  // all T
impl IndexAssign<i32> for List<T> { type Input  = T; /* array.set */ }  // all T
// &xs[i] / &mut xs[i] / xs[i].m() : reference-representation intrinsic, not a trait.
```

### Resolution

For a container that indexes _through the traits_ (a user container), the
compiler desugars `container[i]` by which traits resolve:

- `let x = a[i]` (value read) → `IndexValue::index_value` (a copy — value
  semantics).
- `a[i] = v` → `IndexAssign::index_assign`.
- `&a[i]` → `IndexRef::index_ref` (`Output: Ref`, enforced at the impl site — a
  scalar-`Output` container simply cannot implement `IndexRef`; it offers
  `IndexValue`, and its `[i]` is read by value).
- `a[i].m()` with `m(&self)` → `IndexRef::index_ref` receiver when the container
  implements it, else `IndexValue::index_value` on a temporary copy.
- `&mut a[i]` / `a[i].m()` with `m(&mut self)` → `IndexMutRef::index_mut_ref`.

For `List` / arrays, the same surface syntax lowers to the reference-representation
intrinsic instead: a value read/write is `array.get` / `array.set`; `&xs[i]` on a
`Ref` element is the shared handle; `&mut xs[i]` follows the reference-representation
forbid / carve-out.

### `Ref` gates the trait `Output`, not the `&place` operator

`Ref` is a _type/trait_ predicate, not a gate on the `&` operator. Two different
things spell `&container[i]`:

- The `IndexRef` / `IndexMutRef` _trait_ result (`&Self::Output`), gated by
  `type Output: Ref`. A user container cannot hand out a reference to a
  value-typed element — that would be a leaky abstraction (the caller would
  think it aliases the element). This is enforced at the impl site.
- The language `&<place>` operator on a `List` / array element. This is governed
  by [Reference Representation](./wep-2026-06-13-reference-representation.md), not
  by `Ref`. Under Wado's value semantics `&scalar-element` is a reference to a
  value _copy_ (a boxed snapshot) — sound for reads and pervasive
  (`list.contains(&other[i])`, passing `&scalar` to a `&T` parameter) — so it is
  **permitted**, unchanged by this WEP.

So `&nums[i]` on `List<i32>` is _not_ an error. Under value semantics a reference
to a value type is a reference to a copy — not a "fake" reference; aliasing and
identity are the province of reference types (GC objects), which `Ref` names.
Applying `Ref` to the `&place` operator was tried and reverted — it broke the
pervasive read-only `&scalar[i]` idiom across the ecosystem.

The one genuinely unsound case, `&mut <scalar element>` with an expected
write-back, is Reference Representation's forbid / carve-out, not `Ref`'s
concern.

## Consequences

- One user-facing marker, `Ref`, with a crisp definition (reference identity =
  GC reference) that matches the backend representation exactly and resolves the
  two long-standing ambiguities: `resource ∉ Ref`, `&T ∈ Ref`.
- `IndexRef` / `IndexMutRef` enforce element referenceability through `type Output:
  Ref`, killing the C++ `vector<bool>` fake-reference class of bug by
  construction rather than by convention.
- No proxy objects, no user container handing out a fake `&scalar`; `IndexValue`
  / `IndexAssign` remain the honest value-semantics path for every element type.
- `Ref` stays a type/trait predicate. The language `&scalar[i]` operator is
  unchanged (a reference-representation snapshot, sound for reads), so the
  pervasive `list.contains(&other[i])` idiom keeps working.
- The `variant` / `fn` case is handled once, at the place level, by
  reference-representation — the index traits do not re-derive it.

## Status

- [x] `Ref` declared as the sealed `#[compiler_item("ref")]` marker in the
      prelude, sealed alongside `Reflect`.
- [x] `Ref` eligibility (reference-identity predicate `is_ref_identity`), so
      `T: Ref` resolves: `resource ∉ Ref`, `&T ∈ Ref`, `variant` / `fn` ∈ `Ref`.
      Fixtures: `ref_bound_satisfied`, `ref_bound_{scalar,resource,enum,flags}_rejected`.
- [x] Rename the prelude `Index` / `IndexMut` traits to `IndexRef` /
      `IndexMutRef` (methods `index_ref` / `index_mut_ref`) and update the
      elaborator's index-desugaring lookups. Migrated the user-container
      fixtures: struct-`Output` impls renamed; scalar-`Output` impls (which were
      returning a boxed `&i32`) moved to `IndexValue`, matching real `List` /
      `TreeMap`.
- [x] `type Output: Ref` bound declared on `IndexRef` / `IndexMutRef`.
- [x] Enforce the `type Output: Ref` bound at impl sites. Added a general
      impl-site associated-type bound check (`enforce_impl_assoc_type_bounds`):
      an impl's `type X = Concrete` is checked against every bound the trait
      declares on `X`, so `impl IndexRef<i32> for Cell { type Output = i32 }`
      now errors (`i32` does not implement `Ref`). General, not `Ref`-specific —
      it also covers `IntoIterator::Iter: Iterator` etc. Fixture:
      `index_ref_scalar_output_rejected`.
- [x] `List` element references: keep the reference-representation intrinsic and
      do _not_ implement `IndexRef` / `IndexMutRef` for `List<T>` (a trait impl
      would only shadow it). `&xs[i]` / `&mut xs[i]` / `xs[i].m()` work through it
      for `Ref` elements.
- [x] Do _not_ gate the language `&scalar[i]` operator on `Ref`. A gate in
      `resolve_unary` was tried and reverted: `&scalar[i]` is a
      reference-representation snapshot, sound for reads and used pervasively
      (`list.contains(&other[i])`), so rejecting it broke the ecosystem. `Ref`
      gates the trait `Output`, not the `&` operator.

The feature is complete: `Ref` is defined and resolves correctly, the four
index traits are named, and the reference-returning pair is gated on
`Output: Ref` and enforced at impl sites. The `&scalar[i]` language operator is
left to reference-representation.

## References

- [Indexing Traits Design](./wep-2026-01-20-indexing-traits.md)
- [Reference Representation and Mutation Write-Back](./wep-2026-06-13-reference-representation.md)
- [Iterator Reference Model](./wep-2026-07-05-iterator-reference-model.md)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Resource Ownership](./wep-2026-05-21-resource-ownership.md)
