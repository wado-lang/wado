# WEP: Indexing Traits Design

Defines the `[]` operator for `List<T>`, `TreeMap`, and user-defined containers,
and the `Ref` / `RefMut` markers that govern which elements can be handed out by
shared reference and by mutable reference.

## Context

Indexing has four distinct behaviors:

- Read by value: `let x = c[i]` binds a copy.
- Write by value: `c[i] = v` replaces the element.
- Read by reference: `&c[i]` or a `&self`-method receiver aliases the element.
- Mutable access: `c[i].method()` where the method takes `&mut self`.

A Wasm GC constraint shapes the design: a scalar array element has no addressable
cell, so it can only be read or written by value — never handed out as `&scalar`.
A GC-reference element (a heap object) can be aliased by a live reference. The
traits make this split explicit instead of hiding it behind proxy objects (as
C++'s `vector<bool>` does).

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

The traits are independent — a container implements only the behaviors it
supports. `IndexValue` / `IndexAssign` carry no bound (reads copy out, writes copy
in). `IndexRef` returns a shared reference, so its `Output` must be `Ref`;
`IndexMutRef` returns a mutable reference, so its `Output` must additionally be
mutated in place, `RefMut`. A container of value-typed elements cannot implement
the reference traits; its `[i]` is read by value.

A use site dispatches on the traits provided: `&c[i]` and a `&self` receiver take
`IndexRef` when available, else a value copy; a `&mut self` receiver takes
`IndexMutRef`; a bare read binds a copy (`IndexValue`); assignment writes
(`IndexAssign`). `c[i].mutating_method()` on a value-only container is a compile
error — a copy cannot be mutated in place.

## The `Ref` and `RefMut` markers

Two properties gate the two reference traits:

- `Ref` marks the reference-identity types: those whose value is a Wasm GC
  reference — a heap object (or handle) that `&T` can alias, so `==` on `&T`
  compares identity. It lets a container hand out a live shared reference.
- `RefMut` marks the in-place-mutable types: the `Ref` types mutated in place
  rather than replaced wholesale on assignment. It lets a container hand out a
  live `&mut` whose writes land on the stored element. `RefMut` is a strict
  subset of `Ref`.

| Category             | Types                                                                       | `Ref` | `RefMut` |
| -------------------- | --------------------------------------------------------------------------- | ----- | -------- |
| In-place GC objects  | `struct`, `List<T>`, `String`, tuples, `TreeMap` / `TreeSet`; `i128`/`u128` | yes   | yes      |
| Replace-on-assign GC | `variant`, `fn`                                                             | yes   | no       |
| References           | `&T`, `&mut T`                                                              | yes   | yes      |
| Scalars              | `i8`…`u64`, `f32`, `f64`, `bool`, `char`; `enum`; `flags`                   | no    | no       |
| Handles              | `resource`                                                                  | no    | no       |
| Non-values           | `()`, `never`                                                               | no    | no       |

A `Newtype` follows its base type. Three entries are load-bearing:

- `resource` is not `Ref` — an opaque handle, not a GC reference: it cannot be
  aliased, so a resource element is read by value.
- `&T` is `Ref` (and `RefMut`) — a reference value is itself a GC handle, so a
  `List<&T>` element is a real reference for any `T`.
- `variant` and `fn` are `Ref` but not `RefMut` — `&variant` is a live handle to
  read and pattern-match, but assignment replaces the whole value, so a `&mut
  variant` cannot write through (see
  [Reference Representation](./wep-2026-06-13-reference-representation.md)). A
  container hands variants out by shared reference, never mutable.

Neither marker is about whether a value _holds_ references: a `struct` with `&T`
fields is `Ref` because the struct is a heap object; a scalar `i32` is not `Ref`
even though `&i32` is.

Both are sealed: the compiler provides each for every eligible type and rejects a
user `impl Ref` / `impl RefMut` (a user's own same-named `trait` owns that name).

## The markers gate the trait `Output`, not the `&` operator

`type Output: Ref` on `IndexRef` and `type Output: RefMut` on `IndexMutRef` are
enforced: a container whose `Output` is a value type cannot declare `IndexRef`,
and one whose `Output` is replace-on-assign cannot declare `IndexMutRef` — either
promises a reference the element cannot back. It exposes `IndexValue` instead, so
`impl IndexRef<i32> for C { type Output = i32 }` is a compile error.

The gate is on the traits, not the language `&`. `&c[i]` on a value element
(`&nums[i]` on `List<i32>`) stays legal: under value semantics a reference to a
value type is a reference to a copy, not a fake alias — identity is the province
of `Ref` types. This keeps the read-only idiom `list.contains(&other[i])`
working. The unsound case, `&mut <scalar element>` with write-back, is governed
by [Reference Representation](./wep-2026-06-13-reference-representation.md), not
by these markers.

## Container coverage

- `List<T>` and `Array<T>`: all four — `IndexValue` / `IndexAssign` for every
  element, `IndexRef` for `T: Ref`, `IndexMutRef` for `T: RefMut`.
- `TreeMap<K, V>`: all four keyed by `K` — value read / write for every `V`,
  `IndexRef` for `V: Ref`, `IndexMutRef` for `V: RefMut`.
- `ArraySlice<T>`: `IndexValue` and `IndexRef` (`T: Ref`) only — a shared view
  holds no `&mut` to hand out.

So a `List<Struct>` gets all four; a `List<Variant>` gets all but `IndexMutRef`;
a `List<i32>` gets only the value traits, and `&nums[i]` on it is a value-copy
reference from the language's reference model.

## Consequences

- Honest about Wasm GC: no proxy objects, no fake `&scalar`. `IndexValue` /
  `IndexAssign` cover every element; the reference traits add live references only
  where the element is a GC reference.
- `Ref` / `RefMut` resolve a long-standing ambiguity with crisp rules:
  `resource ∉ Ref`; `&T ∈ RefMut`; `variant` / `fn` ∈ `Ref` but `∉ RefMut`.
- The cost is four traits instead of Rust's two, two markers, and the `IndexRef`
  vs `IndexValue` distinction.

## Related

- [Reference Representation and Mutation Write-Back](./wep-2026-06-13-reference-representation.md)
- [Iterator Reference Model](./wep-2026-07-05-iterator-reference-model.md)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Associated Types](./wep-2026-01-20-associated-types.md)
- [Operator Overloading](./wep-2026-01-18-operator-overloading.md)
