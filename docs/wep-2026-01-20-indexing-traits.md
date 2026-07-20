# WEP: Indexing Traits Design

Defines the `[]` operator for `List<T>`, `TreeMap`, and user-defined containers,
and the `Ref` / `RefMut` markers that govern which elements can be handed out by
shared reference and by mutable reference.

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
    type Output: RefMut;
    fn index_mut_ref(&mut self, index: IndexType) -> &mut Self::Output;
}
```

The four are named for what indexing yields — a `Value`, a `Ref`, a `MutRef`, or
a write (`Assign`). They are independent: a container implements only the
behaviors it supports.

- `IndexValue` / `IndexAssign` carry no bound — reads copy out, writes copy in,
  so any element type works.
- `IndexRef` returns a shared reference into the container, so its `Output` must
  be a reference type (`Ref`, below).
- `IndexMutRef` returns a mutable reference into the container, so its `Output`
  must additionally be mutated in place, not replaced on assign (`RefMut`, below).

A container of value-typed elements simply cannot implement the reference
traits; its `[i]` is read by value.

`[]` at a use site behaves by which traits the container provides: a value read
binds a copy (`IndexValue`); `&c[i]` and a `&self`-method receiver take a
reference (`IndexRef`) when available, otherwise operate on a value copy; a
`&mut self`-method receiver takes a mutable reference (`IndexMutRef`); assignment
writes (`IndexAssign`). A `c[i].mutating_method()` on a container that offers
only value access is a compile error — you cannot mutate a copy in place.

## The `Ref` and `RefMut` markers

Two distinct properties gate the two reference traits:

- `Ref` marks the **reference-identity** types: those whose value is a Wasm GC
  reference — a heap object (or a handle to one) that `&T` can alias, so `==` on
  `&T` compares identity. It is the property that lets a container hand out a
  live shared reference to an element.
- `RefMut` marks the **in-place-mutable** reference types: the `Ref` types whose
  value is mutated in place rather than replaced wholesale on assignment. It is
  the property that lets a container hand out a live _mutable_ reference — a
  `&mut` whose writes land on the stored element. `RefMut` is a strict subset of
  `Ref`.

| Category             | Types                                                                       | `Ref` | `RefMut` |
| -------------------- | --------------------------------------------------------------------------- | ----- | -------- |
| In-place GC objects  | `struct`, `List<T>`, `String`, tuples, `TreeMap` / `TreeSet`; `i128`/`u128` | yes   | yes      |
| Replace-on-assign GC | `variant`, `fn`                                                             | yes   | no       |
| References           | `&T`, `&mut T`                                                              | yes   | yes      |
| Scalars              | `i8`…`u64`, `f32`, `f64`, `bool`, `char`; `enum`; `flags`                   | no    | no       |
| Handles              | `resource`                                                                  | no    | no       |
| Non-values           | `()`, `never`                                                               | no    | no       |

A `Newtype` follows its base type.

Three entries are load-bearing:

- `resource` is **not** `Ref`. A resource is an opaque handle, not a GC
  reference: it cannot be aliased, and a resource element is read by value. A
  reference "into" a resource element is meaningless.
- `&T` **is** `Ref` (and `RefMut`). A reference value is itself a GC handle, so a
  `List<&T>` element is a real reference for any `T`.
- `variant` and `fn` are `Ref` but **not** `RefMut`. `&variant` is a live handle
  you can read and pattern-match, but assigning a variant replaces the whole
  value rather than mutating it in place, so a `&mut variant` cannot write
  through (see
  [Reference Representation](./wep-2026-06-13-reference-representation.md)). A
  container therefore hands variants out by shared reference (`IndexRef`) but not
  by mutable reference (`IndexMutRef`).

Neither marker is about whether a value _holds_ references: a `struct` with `&T`
fields is `Ref` because the struct is a heap object, and a scalar `i32` is not
`Ref` even though `&i32` (which is `Ref`) can point at one.

Both are sealed markers: the compiler provides each for every eligible type and a
user `impl Ref` / `impl RefMut` is rejected (a user who declares their own
same-named `trait` owns that name and is unaffected).

## The markers gate the trait `Output`, not the `&` operator

`type Output: Ref` on `IndexRef` and `type Output: RefMut` on `IndexMutRef` are
enforced: a container whose `Output` is a value type cannot declare `IndexRef`,
and one whose `Output` is replace-on-assign cannot declare `IndexMutRef` — either
would be a leaky abstraction promising a reference to an element that cannot back
it. Such a container exposes `IndexValue` (a copy) instead. So `impl IndexRef<i32>
for C { type Output = i32 }` is a compile error.

This gate is on the traits — the container's contract. It is **not** a gate on
the language `&` operator. `&c[i]` on a value-typed element (`&nums[i]` on a
`List<i32>`) stays legal: under Wado's value semantics a reference to a value
type is a reference to a _copy_, not a fake reference — aliasing and identity are
the province of reference types, which `Ref` names. This keeps the pervasive
read-only idiom `list.contains(&other[i])` (passing a scalar element to a `&T`
parameter) working. The one unsound case, `&mut <scalar element>` with an
expected write-back, is governed by
[Reference Representation](./wep-2026-06-13-reference-representation.md), not by
these markers.

## `List<T>`

`List<T>` implements all four index traits, each with the element bound that
makes it sound:

- `IndexValue` / `IndexAssign` for every element type — `c[i]` reads a copy and
  `c[i] = v` writes for all `T`.
- `IndexRef` for `T: Ref` — `&xs[i]` aliases the stored element.
- `IndexMutRef` for `T: RefMut` — `&mut xs[i]` and `xs[i].mutating_method()`
  write through to the stored element.

So a `List<Struct>` gets all four; a `List<Variant>` gets everything but
`IndexMutRef` (a variant is `Ref`, not `RefMut`); a `List<i32>` gets only the
value traits, and `&nums[i]` on it is a value-copy reference from the language's
reference model rather than a `Ref` alias.

## Consequences

- Honest about Wasm GC: no proxy objects, and a container never hands out a fake
  `&scalar`. `IndexValue` / `IndexAssign` are the value-semantics path for every
  element type; `IndexRef` / `IndexMutRef` add live references only where the
  element is a GC reference.
- `Ref` and `RefMut` resolve a long-standing ambiguity with crisp behavioral
  rules: `resource ∉ Ref`; `&T ∈ RefMut`; `variant` / `fn` ∈ `Ref` but
  `∉ RefMut` (shared references but no mutable ones).
- The cost is four traits instead of Rust's two, two markers to learn, and the
  `IndexRef` vs `IndexValue` distinction.

## Related

- [Reference Representation and Mutation Write-Back](./wep-2026-06-13-reference-representation.md)
- [Iterator Reference Model](./wep-2026-07-05-iterator-reference-model.md)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Associated Types](./wep-2026-01-20-associated-types.md)
- [Operator Overloading](./wep-2026-01-18-operator-overloading.md)
