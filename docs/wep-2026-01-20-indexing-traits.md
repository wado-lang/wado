# WEP: Indexing Traits Design

Defines the `[]` operator for `List<T>`, `TreeMap`, and user-defined containers,
and the `Ref` marker that governs which elements can be handed out by reference.

## Context

Indexing has four distinct behaviors:

- Read by value: `let x = c[i]` binds a copy of the element.
- Write by value: `c[i] = v` replaces the element.
- Read by reference: `&c[i]` / a `&self`-method receiver aliases the element.
- Mutable access: `c[i].method()` where the method takes `&mut self`.

A Wasm GC constraint shapes the design: a scalar array element has no
addressable cell, so it can only be read or written **by value** — it can never
be handed out as `&scalar`. A GC-reference element (a heap object) can be handed
out as a live reference that aliases the stored element. The traits make this
distinction explicit rather than hiding it behind proxy objects (as C++'s
`vector<bool>` does).

## The four index traits

```wado
/// Read by value: `c[i]` -> Output (a copy).
internal trait IndexValue<IndexType> {
    type Output;
    fn index_value(&self, index: IndexType) -> Self::Output;
}

/// Write by value: `c[i] = v`.
internal trait IndexAssign<IndexType> {
    type Input;
    fn index_assign(&mut self, index: IndexType, value: Self::Input);
}

/// Read by reference: `&c[i]` -> &Output.
internal trait IndexRef<IndexType> {
    type Output: Ref;
    fn index_ref(&self, index: IndexType) -> &Self::Output;
}

/// Mutable reference: `c[i].mutating_method()` -> &mut Output.
internal trait IndexMutRef<IndexType> {
    type Output: Ref;
    fn index_mut_ref(&mut self, index: IndexType) -> &mut Self::Output;
}
```

The four are named for what indexing yields — a `Value`, a `Ref`, a `MutRef`, or
a write (`Assign`). They are independent: a container implements only the
behaviors it supports.

- `IndexValue` / `IndexAssign` carry no bound — reads copy out, writes copy in,
  so any element type works.
- `IndexRef` / `IndexMutRef` return a reference into the container, so their
  `Output` must be a reference type (`Ref`, below). A container of value-typed
  elements simply cannot implement them; its `[i]` is read by value.

`[]` at a use site behaves by which traits the container provides: a value read
binds a copy (`IndexValue`); `&c[i]` and a `&self`-method receiver take a
reference (`IndexRef`) when available, otherwise operate on a value copy; a
`&mut self`-method receiver takes a mutable reference (`IndexMutRef`); assignment
writes (`IndexAssign`). A `c[i].mutating_method()` on a container that offers
only value access is a compile error — you cannot mutate a copy in place.

## The `Ref` marker

`Ref` marks the **reference-identity** types: those whose value is a Wasm GC
reference — a heap object (or a handle to one) that `&T` can alias, so `==` on
`&T` compares identity. It is the property that lets a container hand out a live
reference to an element.

| Category   | Types                                                                                          | `Ref` |
| ---------- | ---------------------------------------------------------------------------------------------- | ----- |
| GC objects | `struct`, `variant`, `List<T>`, `String`, tuples, `TreeMap` / `TreeSet`, `fn`; `i128` / `u128` | yes   |
| References | `&T`, `&mut T`                                                                                 | yes   |
| Scalars    | `i8`…`u64`, `f32`, `f64`, `bool`, `char`; `enum`; `flags`                                      | no    |
| Handles    | `resource`                                                                                     | no    |
| Non-values | `()`, `never`                                                                                  | no    |

A `Newtype` follows its base type.

Two entries are load-bearing:

- `resource` is **not** `Ref`. A resource is an opaque handle, not a GC
  reference: it cannot be aliased, and a resource element is read by value. A
  reference "into" a resource element is meaningless.
- `&T` **is** `Ref`. A reference value is itself a GC handle, so a `List<&T>`
  element is a real reference for any `T`.

`Ref` is reference identity, which is distinct from in-place mutability. A
`variant` is `Ref` — `&variant` is a live handle you can read and pattern-match —
even though assigning a variant replaces the whole value rather than mutating it
in place (see [Reference Representation](./wep-2026-06-13-reference-representation.md)).
`Ref` is also unrelated to whether a value _holds_ references: a `struct` with
`&T` fields is `Ref` because the struct is a heap object, and a scalar `i32` is
not `Ref` even though `&i32` (which is `Ref`) can point at one.

`Ref` is a sealed marker: the compiler provides it for every eligible type and a
user `impl Ref` is rejected (a user who declares their own `trait Ref` owns that
name and is unaffected).

## `Ref` gates the trait `Output`, not the `&` operator

`type Output: Ref` on `IndexRef` / `IndexMutRef` is enforced: a container whose
`Output` is a value type cannot declare these traits — that would be a leaky
abstraction promising a live reference to an element that has none. Such a
container exposes `IndexValue` (a copy) instead. So `impl IndexRef<i32> for C
{ type Output = i32 }` is a compile error.

This gate is on the traits — the container's contract. It is **not** a gate on
the language `&` operator. `&c[i]` on a value-typed element (`&nums[i]` on a
`List<i32>`) stays legal: under Wado's value semantics a reference to a value
type is a reference to a _copy_, not a fake reference — aliasing and identity are
the province of reference types, which `Ref` names. This keeps the pervasive
read-only idiom `list.contains(&other[i])` (passing a scalar element to a `&T`
parameter) working. The one unsound case, `&mut <scalar element>` with an
expected write-back, is governed by
[Reference Representation](./wep-2026-06-13-reference-representation.md), not by
`Ref`.

## `List<T>`

`List<T>` implements `IndexValue` and `IndexAssign` for every element type, so
`c[i]` reads a copy and `c[i] = v` writes for all `T`. It does **not** implement
`IndexRef` / `IndexMutRef`; element references (`&xs[i]`, `&mut xs[i]`,
`xs[i].method()`, `&mut`-iteration) come from the language's reference model and
work for `Ref` elements — for a `List<Struct>`, `&xs[i]` aliases the element and
mutation through it is observed at the element; for a `List<i32>`, `&xs[i]` is a
value-copy reference.

## Consequences

- Honest about Wasm GC: no proxy objects, and a container never hands out a fake
  `&scalar`. `IndexValue` / `IndexAssign` are the value-semantics path for every
  element type; `IndexRef` / `IndexMutRef` add live references only where the
  element is a GC reference.
- `Ref` resolves a long-standing ambiguity with a crisp behavioral rule:
  `resource ∉ Ref`, `&T ∈ Ref`, `variant` / `fn` ∈ `Ref`.
- The cost is four traits instead of Rust's two, and the `Index` vs `IndexValue`
  distinction to learn.

## Related

- [Reference Representation and Mutation Write-Back](./wep-2026-06-13-reference-representation.md)
- [Iterator Reference Model](./wep-2026-07-05-iterator-reference-model.md)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Associated Types](./wep-2026-01-20-associated-types.md)
- [Operator Overloading](./wep-2026-01-18-operator-overloading.md)
