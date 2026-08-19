# WEP: The Sequence Family — `Array<T>` / `List<T>` / `Slice<T>`

The standard for Wado's contiguous sequence types: their roles, the `Sequence` /
`AsSlice` traits, naming, indexing contracts, and the Component Model boundary.

## Context

Three types cover contiguous sequences, introduced months apart:

| Type       | Representation                           | Introduced |
| ---------- | ---------------------------------------- | ---------- |
| `List<T>`  | `struct { repr: Array<T>, used }`        | 2026-02    |
| `Array<T>` | Wasm GC `array T` (definitionless)       | 2026-06-03 |
| `Slice<T>` | `struct { repr: &Array<T>, start, end }` | 2026-06-04 |

The axis is sound — ownership × length-mutability — but capability does not
follow it. Three gaps account for nearly all of the divergence.

### The view type has no algorithm surface

`first`, `last`, `contains`, `windows`, `chunks`, and range indexing live on
`List` alone, so they vanish the moment code takes a slice. Wado has no `Deref`
and no unsized types, so the only sharing mechanism is a trait — and none was
written, leaving hand-duplication that was never completed.

### Iteration was aligned on `List` only

[Iterator Reference Model](./wep-2026-07-05-iterator-reference-model.md) moved
`iter()` to yield `&T`, but only for `List`. `Array::iter()` and `Slice::iter()`
still yield `T` under the superseded convention, and `iter_mut()` exists nowhere.

### `Array<T>` and `Slice<T>` are undocumented and under-implemented

Both are prelude-public, yet neither appears in `docs/spec.md` or the cheatsheet,
and neither the CM type-mapping table nor the snapshot/aliasing rules below are
written down anywhere. `Array<T>` takes no `[…]` literal and has no `Default`.

### Structural derivation is wrong for a view

`Eq` / `Ord` / `Inspect` are synthesized structurally on demand
([Trait Derivation](./wep-2026-06-25-trait-derivation.md)), so `Slice` compares
by backing identity — two views over equal contents are unequal — and inspects as
`Slice { repr: &[1, 2, 3], start: 0, end: 3 }` instead of `[1, 2, 3]`.

## Decision

### Roles

|               | Fixed length | Growable  |
| ------------- | ------------ | --------- |
| Owned         | `Array<T>`   | `List<T>` |
| Borrowed view | `Slice<T>`   | —         |

`Slice<T>` is the read-only vocabulary type: an algorithm that only reads a
sequence is written once against a slice, and the owned types reach it through
`as_slice()`.

Conversion names are a closed rule: `as_*` is a zero-copy view, `to_*` copies.

| From → To                  | Method       | Cost                   |
| -------------------------- | ------------ | ---------------------- |
| `Array` / `List` → `Slice` | `as_slice()` | zero-copy              |
| `Slice` → `Array`          | `to_array()` | copy                   |
| `Slice` → `List`           | `to_list()`  | copy                   |
| `List` → `Array`           | `to_array()` | copy, sized to `len()` |
| `Array` → `List`           | `to_list()`  | copy                   |

### Slice semantics

A slice holds a reference to the whole backing array plus offsets, because Wasm
GC has no interior references. Two consequences are normative and belong in
`docs/spec.md`:

- Snapshot — a view keeps referring to the buffer it was created from. If the
  source `List` grows and reallocates, the view does not observe it.
- Aliasing — a write to the source that does not reallocate is visible through
  the view.

Both are memory-safe under GC. Element access through a view is always a value
copy.

### `Sequence<T>` and `AsSlice<T>`

Two traits, split by whether the implementor has a contiguous backing.

```wado
/// Read-only sequence algorithms. Needs no contiguous backing.
internal trait Sequence<T> {
    fn len(&self) -> i32;
    fn get_unchecked(&self, index: i32) -> T;

    // default bodies, written against `len` + `get_unchecked` only
    fn is_empty(&self) -> bool;
    fn get(&self, index: i32) -> Option<T>;
    fn first(&self) -> Option<T>;
    fn last(&self) -> Option<T>;
    fn position(&self, pred: fn mut(T) -> bool) -> Option<i32>;
}

/// Contiguously-backed sequences. View-producing operations live here.
internal trait AsSlice<T>: Sequence<T> {
    fn as_slice(&self) -> Slice<T> with stores[self];

    // default bodies forwarding to `Slice`'s inherent implementations
    fn slice(&self, start: i32, end: i32) -> Slice<T>;
    fn iter_value(&self) -> SliceValueIter<T>;
    fn iter_ref(&self) -> SliceRefIter<T>;
    fn windows(&self, size: i32) -> SliceWindows<T>;
    fn chunks(&self, size: i32) -> SliceChunks<T>;
}
```

`Array<T>`, `List<T>`, and `Slice<T>` implement both. Inherent methods shadow
trait methods ([Overload Resolution](./wep-2026-07-31-overload-resolution.md)),
so `Slice`'s own bodies win for `Slice` and the defaults do not recurse.

Only unbounded methods can be default bodies. A trait's type parameter takes no
bound — `trait SequenceEq<T: Eq>: Sequence<T>` does not parse — so `contains`,
`binary_search`, `starts_with`, and `ends_with`, which need `T: Eq` or `T: Ord`,
stay bounded inherent impls (`impl<T: Eq> Slice<T> { … }`) with thin bounded
forwarders on `Array` and `List`. `Iterator` already hits this wall: `sum` /
`min` / `max` live on `impl<T: Add> SliceValueIter<T>` rather than on the trait, which
is why `xs.iter().map(f).sum()` does not compile. Lifting the restriction needs
bounds on trait type parameters and associated types — a separate proposal, and
the enabler that would fold these methods back in.

`Sequence` default bodies must not construct a `Slice`. `sroa_param` scalarizes
single-field structs only, so a three-field `Slice` survives unless inlining
removes it; routing `list.first()` through `as_slice()` would turn an
`array.get` into a GC allocation. View-producing operations are exempt —
constructing the view is the operation.

Mutation is deliberately absent from both traits: `set`, `sort`, `reverse`, and
`[i] = v` need a mutable backing that `Slice` does not have, and length-changing
operations belong to `List` alone. They stay inherent. With only two possible
implementors, a `SequenceMut` trait would not pay for itself.

`String` implements neither. Its byte view stays on the existing `AsByteSlice` /
`as_byte_slice()`, whose name is honest about returning bytes. A future
`String: Sequence<char>` is left expressible — this is why `as_slice()` sits on
`AsSlice` and not on `Sequence`, since UTF-8 has no contiguous `char` backing —
but is out of scope: `len()` and `get_unchecked()` over UTF-8 are O(n), which
would silently make every `Sequence` default body O(n²).

### Naming

The value/reference axis is spelled out in every name that carries it. There is
no unmarked member — an unmarked name is what drifted before, when the plain
`Iter` came to mean the by-value iterator while `iter()` moved to references.

| Axis    | Token    | Yields   |
| ------- | -------- | -------- |
| value   | `Value`  | `T`      |
| shared  | `Ref`    | `&T`     |
| mutable | `RefMut` | `&mut T` |

`RefMut`, not `MutRef`: the marker traits read `Ref` / `RefMut`, and it matches
`std::cell::Ref` / `RefMut`.

| Type                 | Item       |
| -------------------- | ---------- |
| `Slice<T>`           | —          |
| `SliceValueIter<T>`  | `T`        |
| `SliceRefIter<T>`    | `&T`       |
| `SliceRefMutIter<T>` | `&mut T`   |
| `SliceWindows<T>`    | `Slice<T>` |
| `SliceChunks<T>`     | `Slice<T>` |

`Slice`, not `ArraySlice`: the latter states the backing rather than the role,
and `list.as_slice() -> ArraySlice` reads wrong. The argument for the prefix —
that `Slice` is too generic for a flat prelude, which cannot be shadowed — holds
far more strongly for `Iter` (which also collided with the `Iterator` trait) than
for `Slice`. Domain uses take qualified names (`TimeSlice`), and the conflict is
a clear compile error, not silent breakage.

Methods take the same axis: `iter_value()`, `iter_ref()`, `iter_ref_mut()`, and
`SliceRefIter::iter_value()` in place of a Rust-style `copied()`. Wado's
reference semantics diverge from Rust's — a `&T` into an array element is a
snapshot copy that cannot write back, and `&mut T` is available only for
`T: RefMut` — so reusing Rust's names would mislead, which
[Checked/Unchecked Discipline](./wep-2026-05-16-string-checked-unchecked-discipline.md)
forbids.

`IntoIterator` / `into_iter` are exempt: they are the `for-of` desugaring hook,
not a hand-called method, and `for-of` already marks the axis in syntax
(`for x of xs` / `&xs` / `&mut xs` yield `T` / `&T` / `&mut T`).

The index traits keep their four-way split — it follows from Wado's constraints,
since `Elem: Ref` / `Elem: RefMut` bounds exclude scalars from `IndexRef`, and a
scalar has no addressable cell, so `IndexAssign` cannot fold into a `&mut`. All
four name the same element type, so all four call it `Elem`.

| Trait                              | Element access |
| ---------------------------------- | -------------- |
| `IndexValue<I>` / `index_value`    | reads `T`      |
| `IndexRef<I>` / `index_ref`        | reads `&T`     |
| `IndexRefMut<I>` / `index_ref_mut` | reads `&mut T` |
| `IndexAssign<I>` / `index_assign`  | writes `T`     |

The internal intrinsics follow the same axis: `array_get_value`, `array_get_ref`,
`array_get_ref_mut` (and `array_get_value_u8`). Naming them this way breaks no
mirror of the Wasm instruction names, because the family never was one — Wasm GC
has no `array.get_ref` or `array.get_mut_ref`, and `array_new` already maps to
`array.new_default`.

### Indexing and the `_unchecked` contract

`.get(i)` returns `Option<T>` on all three types.

`xs[i]` traps when out of bounds. The message is implementation-defined: Wado
offers no trap recovery, so the trap itself is the whole contract. This makes the
per-type difference in mechanism correct rather than drift:

- `Array[i]` — the backing is the length, so `array.get`'s own bounds check is
  necessary and sufficient. An added check would be pure duplicate cost.
- `List[i]` / `Slice[i]` — the backing is longer than `len()` (spare capacity, or
  the region outside the view), so the Wasm check is insufficient; it would read
  a stale slot. An explicit check is required anyway, so it uses `assert` and
  gets power-assert diagnostics for free.

Two mechanism-level drifts are corrected: `Slice` hand-rolls its check as
`if … { panic(…) }` instead of `assert`, and `List`'s index traits assert only
`index < used`, omitting the lower bound that its own `insert` / `remove` /
`swap` check. Both become `assert 0 <= index < len`.

`get_unchecked` carries this contract:

> The caller must guarantee `0 <= index < len()`. Violating it yields an
> unspecified value of `T` or traps. It is never undefined behavior and never
> compromises memory safety.

This is a structural guarantee, not an aspiration: every path bottoms out in
`array.get`, which Wasm GC always bounds-checks. Wado's `_unchecked` is
categorically weaker than Rust's — it elides a semantic check, not a memory check
— which is why Wado needs no `unsafe`. The name is borrowed from Rust; the
contract is not.

`Array::get` therefore returns `Option<T>` rather than `T`, and the raw trapping
access is `Array::get_unchecked`, filling a gap in the family.

### Component Model boundary

Eligibility splits by direction. Lowering (Wado supplies the list) needs only a
contiguous region and a length. Lifting (Wado receives it) needs an owner for
freshly lifted memory, and return-position generics are not expressible.

| Type       | Lift (`export` param, `import` return) | Lower (`import` param, `export` return) |
| ---------- | -------------------------------------- | --------------------------------------- |
| `List<T>`  | ✓                                      | ✓                                       |
| `Array<T>` | ✓                                      | ✓                                       |
| `Slice<T>` | ✗ — definition-site error              | ✓                                       |

`Array<T>` must be accepted consistently: `wit_emit.rs` lowers it to `list<T>`
while `cm_binding/types.rs` reports it as having no CM representation.

`Slice<T>` is rejected at the definition site in both directions until the
lowering path exists, because reaching `wit_emit` instead drops the
component-type section for the whole component and breaks the static "an
`export` appears in WIT" guarantee of
[Visibility](./wep-2026-06-25-visibility-internal-pub-export.md). Opening the
lowering column needs the canonical-ABI lowering for a view (read `repr` /
`start` / `end`, copy the range out) plus the `wit_emit` mapping.

Generic ergonomics stay out of the language. `wado-from-idl` emits a raw binding
taking `Slice<T>` plus a thin Wado wrapper:

```wado
fn send_raw(&self, data: Slice<u8>, …) -> …;

pub fn send<S: AsSlice<u8>>(&self, data: S, …) -> … {
    return self.send_raw(data.as_slice(), …);
}
```

Callers keep writing `f(xs)`, and a sub-range crosses the boundary without the
`to_list()` copy it needs today. The alternative — making `AsSlice<T>` a
WIT-representable bound projecting to `list<T>` — would add a bound-to-WIT
projection plus a position-dependent legality rule to the language, at the one
place where "one declaration, no overloading, no implicit conversion" binds
hardest. `core:cbor` / `core:json` / `core:digest` already prove the wrapper
pattern via `AsByteSlice`.

This applies to top-level parameters only. A `list<T>` inside a `record` field
has no position polymorphism and stays `List<T>` (e.g. `wasi:http`'s
`FieldValue = List<u8>`).

## Consequences

- One read-only algorithm surface instead of three partial copies; taking a
  slice no longer loses methods.
- Rust muscle memory is intentionally broken at the iterator methods, where
  Wado's semantics genuinely differ. The rename is wide but mechanical.
- Every view type must carry hand-written `Eq` / `Ord` / `Display` / `Inspect`;
  falling through to structural derivation is a defect, not a default.
- The `Slice` name is claimed from the prelude and can no longer be user-defined.
- `Sequence` default bodies are instantiated per implementor, but each is small
  and DCE removes unused ones.
- Methods needing an element bound stay duplicated across the three types until
  trait type parameters can carry bounds.

## Roadmap

The naming above is in place. The phases below are independent of each other.

### Phase B — `Sequence` / `AsSlice`

- [ ] Introduce both traits, move the read-only surface onto `Slice` inherent
      methods, implement for `Array` / `List` / `Slice`, and delete the
      duplicated methods.
- [ ] Benchmark `list.first()` and `list.windows(n)` against today to confirm no
      `Slice` allocation survives inlining.

### Phase C — Iteration

- [ ] `iter_value()` / `iter_ref()` / `iter_ref_mut()` on all three types;
      `for x of &mut xs` yielding `&mut T` for `T: RefMut`;
      `SliceRefIter::iter_value()` replacing `copied()`.
- [ ] Give the iterator adaptors the bounded inherent `sum` / `min` / `max` that
      only `SliceValueIter` carries today, so they chain after `map` / `filter`.

### Phase D — Trait implementations

- [ ] `[a, b, c]` `Display` / `Inspect` for `Slice`, element-wise `Eq` / `Ord`
      for `Slice`, `Default` for `Array`, `[…]` literals for `Array`, and
      `IndexValue<RangeExclusive<i32>>` / `<RangeInclusive<i32>>` for `Slice`.
- [ ] Reject structural derivation of `Eq` / `Ord` / `Inspect` for view types.
- [ ] `assert 0 <= index < len` on every `List` and `Slice` index trait.

### Phase E — Boundary and documentation

- [ ] Accept `Array<T>` in `cm_binding`, matching `wit_emit`.
- [ ] Lower `Slice<T>` through the canonical ABI and map it in `wit_emit`, then
      narrow the definition-site error to lifting positions only.
- [ ] The `wado-from-idl` `AsSlice` wrapper.
- [ ] Document `Array<T>` and `Slice<T>` in `docs/spec.md` (including the CM type
      mapping table and the snapshot/aliasing rules) and the cheatsheet.
