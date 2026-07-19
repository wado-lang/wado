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

`List<T>`:

```wado
impl IndexValue<i32>  for List<T> { type Output = T; /* array.get */ }           // all T
impl IndexAssign<i32> for List<T> { type Input  = T; /* array.set */ }           // all T
impl<T: Ref> IndexRef<i32>    for List<T> { type Output = T; /* element ref */ } // T: Ref
impl<T: Ref> IndexMutRef<i32> for List<T> { type Output = T; /* element ref */ } // T: Ref
```

### Resolution

The compiler desugars `container[i]` by which traits resolve:

- `let x = a[i]` (value read) → `IndexValue::index_value` (a copy — value
  semantics). `List` always has this, so scalar-element reads are unaffected.
- `a[i] = v` → `IndexAssign::index_assign`.
- `&a[i]` → `IndexRef::index_ref`. If `Output` is not `Ref` (e.g. `&nums[i]` on
  `List<i32>`) this is a type error: a value-typed element has no reference
  identity to borrow. Diagnostic names the fix (bind by value: `let x = nums[i]`).
- `a[i].m()` with `m(&self)` → `IndexRef::index_ref` receiver when `Output: Ref`,
  else `IndexValue::index_value` on a temporary copy.
- `&mut a[i]` / `a[i].m()` with `m(&mut self)` → `IndexMutRef::index_mut_ref`,
  subject to the write-back gate below.

### The `IndexMutRef` write-back gate

Because `Ref` includes the replace-on-assign GC types (`variant`, `fn`),
`IndexMutRef` is _offered_ for them, but a `&mut variant` / `&mut fn` into an
array slot cannot support `*r = v` replacement — there is no stable box cell for
the element. This is precisely
[Reference Representation](./wep-2026-06-13-reference-representation.md)'s domain.

Decision: gate this at the **place**, not with a second marker trait. The
reference-representation forbid / carve-out is evaluated on the `container[i]`
place, so `&mut a[i]` and `a[i].m()` (`&mut self`) are rejected — or lowered via
the non-escaping temp + write-back carve-out — identically for replace-on-assign
elements, whether the element ref comes from the built-in `List` intrinsic or an
`IndexMutRef` impl. `Ref` is the _necessary_ gate (it excludes scalars, `enum`,
`flags`, `resource`); reference-representation is the _additional_ gate for
`&mut` write-back soundness. This makes `&mut xs[i]` obey the same rule and emit
the same diagnostic as `&mut x` on a local — one concept, no new surface.

The alternative — a second sealed marker (in-place-mutability, the boxing-set
complement) that statically withholds `IndexMutRef` from replace-on-assign
elements — is rejected for now: the place-level check already makes the design
sound with less surface, and a future need can still add the marker without
reworking this.

## Consequences

- One user-facing marker, `Ref`, with a crisp definition (reference identity =
  GC reference) that matches the backend representation exactly and resolves the
  two long-standing ambiguities: `resource ∉ Ref`, `&T ∈ Ref`.
- `IndexRef` / `IndexMutRef` enforce element referenceability through `type Output:
  Ref`, killing the C++ `vector<bool>` fake-reference class of bug by
  construction rather than by convention.
- No proxy objects, no fake `&i32` into scalar arrays; `IndexValue` /
  `IndexAssign` remain the honest value-semantics path for every element type.
- The `variant` / `fn` case is handled once, at the place level, by
  reference-representation — the index traits do not re-derive it.

## Status

- [x] `Ref` declared as the sealed `#[compiler_item("ref")]` marker in the
      prelude, sealed alongside `Reflect`.
- [ ] `Ref` eligibility synthesis (reference-identity predicate), so `T: Ref`
      resolves; correct `resource ∉ Ref`, `&T ∈ Ref`, `variant`/`fn` ∈ `Ref`.
- [ ] Rename the prelude `Index` / `IndexMut` traits to `IndexRef` /
      `IndexMutRef` (methods `index_ref` / `index_mut_ref`) and update the
      elaborator's index-desugaring lookups.
- [ ] `type Output: Ref` bound on `IndexRef` / `IndexMutRef`.
- [ ] `impl<T: Ref> IndexRef / IndexMutRef for List<T>`, and route `&a[i]` /
      `a[i].m()` through them (or keep the `List` intrinsic and use `Ref` only as
      the diagnostic gate — decide during implementation).
- [ ] Place-level reference-representation check on `IndexMutRef` receivers.

## References

- [Indexing Traits Design](./wep-2026-01-20-indexing-traits.md)
- [Reference Representation and Mutation Write-Back](./wep-2026-06-13-reference-representation.md)
- [Iterator Reference Model](./wep-2026-07-05-iterator-reference-model.md)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Resource Ownership](./wep-2026-05-21-resource-ownership.md)
