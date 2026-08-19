# WEP: The Sequence Family — `Array<T>` / `List<T>` / `Slice<T>`

The standard for Wado's contiguous sequence types: roles, the `Sequence` /
`AsSlice` traits, naming, indexing contracts, and the Component Model boundary.

## Context

Three types cover contiguous sequences, introduced months apart. The axis is
sound — ownership × length-mutability — but capability never followed it.

| Type       | Representation                           | Introduced |
| ---------- | ---------------------------------------- | ---------- |
| `List<T>`  | `struct { repr: Array<T>, used }`        | 2026-02    |
| `Array<T>` | Wasm GC `array T` (definitionless)       | 2026-06-03 |
| `Slice<T>` | `struct { repr: &Array<T>, start, end }` | 2026-06-04 |

- `first`, `last`, `contains`, `windows`, `chunks`, and range indexing live on
  `List` alone, so they vanish the moment code takes a slice. With no `Deref`
  and no unsized types the only sharing mechanism is a trait, and none existed.
- [Iterator Reference Model](./wep-2026-07-05-iterator-reference-model.md) moved
  `iter()` to `&T` for `List` only; `Array` and `Slice` kept the old meaning.
- `Array<T>` and `Slice<T>` appear in no doc, take no `[…]` literal, and have no
  `Default`.
- Structural derivation makes `Slice` compare by backing identity and inspect as
  `Slice { repr: …, start: 0, end: 3 }` rather than `[1, 2, 3]`.

## Decision

### Roles

|                | Fixed length | Growable  |
| -------------- | ------------ | --------- |
| Owned          | `Array<T>`   | `List<T>` |
| Reference view | `Slice<T>`   | —         |

`Slice<T>` is the read-only vocabulary type: an algorithm that only reads is
written once against a slice, and the owned types reach it via `as_slice()`.

Conversion names are a closed rule — `as_*` returns a view referencing the
elements, `to_*` copies them:

| From → To                  | Method       | Element cost           |
| -------------------------- | ------------ | ---------------------- |
| `Array` / `List` → `Slice` | `as_slice()` | none                   |
| `Slice` → `Array` / `List` | `to_*()`     | copy                   |
| `List` → `Array`           | `to_array()` | copy, sized to `len()` |
| `Array` → `List`           | `to_list()`  | copy                   |

### Slice semantics

A slice references the whole backing array plus offsets, because Wasm GC has no
interior references. It is not free, only element-free: a view is an ordinary
value-semantic struct, so assigning one copies its three fields like any other.
What it never copies is what it points at, however long that is.

Two consequences are normative and belong in `docs/spec.md`. Both are
memory-safe under GC, and element access through a view is always a value copy.

- Snapshot — a view keeps referring to the buffer it was created from, so a
  source `List` that grows and reallocates is not observed.
- Aliasing — a write to the source that does not reallocate is visible.

### `Sequence` and `AsSlice`

Two traits, split by whether the implementor has a contiguous backing. The
element type is an associated type because a trait bound takes no positional
type arguments: `<S: Sequence<i32>>` does not parse, only
`<S: Sequence<Elem = i32>>` — the shape `Iterator` already uses for `Item`.

```wado
/// Read-only sequence algorithms. Needs no contiguous backing.
internal trait Sequence {
    type Elem;

    fn len(&self) -> i32;
    fn get_unchecked(&self, index: i32) -> Self::Elem;

    // default bodies, written against `len` + `get_unchecked` only
    fn is_empty(&self) -> bool;
    fn get(&self, index: i32) -> Option<Self::Elem>;
    fn first(&self) -> Option<Self::Elem>;
    fn last(&self) -> Option<Self::Elem>;
    fn position(&self, pred: fn mut(Self::Elem) -> bool) -> Option<i32>;
}

/// Contiguously-backed sequences. View-producing operations live here.
internal trait AsSlice: Sequence {
    fn as_slice(&self) -> Slice<Self::Elem> with stores[self];

    // default bodies forwarding to `Slice`'s inherent implementations
    fn slice(&self, start: i32, end: i32) -> Slice<Self::Elem>;
    fn iter_value(&self) -> SliceValueIter<Self::Elem>;
    fn iter_ref(&self) -> SliceRefIter<Self::Elem>;
    fn windows(&self, size: i32) -> SliceWindows<Self::Elem>;
    fn chunks(&self, size: i32) -> SliceChunks<Self::Elem>;
}
```

All three types implement both. Inherent methods shadow trait methods
([Overload Resolution](./wep-2026-07-31-overload-resolution.md)), so `Slice`'s
own bodies win and the defaults do not recurse. Those defaults go through
`len` / `get_unchecked`, never `as_slice()`, so `first()` stays an `array.get`
instead of a struct the optimizer has to remove again.

Only methods with no element bound can be defaults, since an associated type
carries no bound. `contains`, `binary_search`, `starts_with`, and `ends_with`
need `Elem: Eq` or `Elem: Ord`, so they stay bounded inherent impls with thin
forwarders. `Iterator` already hits this wall — `sum` / `min` / `max` live on
`impl<T: Add> SliceValueIter<T>`, not on the trait. Bounds on associated types
would fold all of them back in; that is a separate proposal.

Mutation is absent from both traits: `set`, `sort`, `reverse`, and `[i] = v`
need a mutable backing `Slice` lacks, and length-changing operations belong to
`List` alone. With two possible implementors a `SequenceMut` would not pay.

`String` implements neither; its byte view stays on `AsByteSlice`. A future
`String: Sequence` with `Elem = char` is left expressible — which is why
`as_slice()` sits on `AsSlice`, UTF-8 having no contiguous `char` backing — but
`len()` and `get_unchecked()` over UTF-8 are O(n), making every default O(n²).

### Naming

The value/reference axis is spelled out in every name that carries it. No
member is unmarked; an unmarked name is what drifted before, when the plain
`Iter` came to mean by-value while `iter()` moved to references.

| Axis    | Token    | Yields   |
| ------- | -------- | -------- |
| value   | `Value`  | `T`      |
| shared  | `Ref`    | `&T`     |
| mutable | `RefMut` | `&mut T` |

`RefMut`, not `MutRef`: the marker traits read `Ref` / `RefMut`, matching
`std::cell::Ref` / `RefMut`.

`Slice`, not `ArraySlice`: the latter states the backing rather than the role,
and `list.as_slice() -> ArraySlice` reads wrong. The argument for a prefix —
that a flat prelude cannot be shadowed — holds far more strongly for `Iter`
(which also collided with the `Iterator` trait). Domain uses take qualified
names (`TimeSlice`), and a conflict is a compile error, not silent breakage.

| Type                 | Item       |
| -------------------- | ---------- |
| `SliceValueIter<T>`  | `T`        |
| `SliceRefIter<T>`    | `&T`       |
| `SliceRefMutIter<T>` | `&mut T`   |
| `SliceWindows<T>`    | `Slice<T>` |
| `SliceChunks<T>`     | `Slice<T>` |

Methods take the same axis: `iter_value()`, `iter_ref()`, `iter_ref_mut()`, and
`SliceRefIter::iter_value()` where Rust says `copied()`. Wado's reference
semantics diverge — a `&T` into an array element is a snapshot copy that cannot
write back, and `&mut T` needs `T: RefMut` — so Rust's names would mislead,
which
[Checked/Unchecked Discipline](./wep-2026-05-16-string-checked-unchecked-discipline.md)
forbids. `IntoIterator` / `into_iter` are exempt: they are the `for-of`
desugaring hook, and `for-of` marks the axis in syntax already.

### Map and set traversals

`TreeMap` and `TreeSet` carry the same axis, since a caller reading either has
the same choice to make.

| Type                            | Item       | Reached by               |
| ------------------------------- | ---------- | ------------------------ |
| `TreeSetRefIter<T>`             | `&T`       | `iter_ref()`             |
| `TreeSetValueIter<T>`           | `T`        | `iter_value()`           |
| `TreeMapKeysRefIter<K, V>`      | `&K`       | `keys()`                 |
| `TreeMapKeysValueIter<K, V>`    | `K`        | `keys().iter_value()`    |
| `TreeMapValuesRefIter<K, V>`    | `&V`       | `values()`               |
| `TreeMapValuesValueIter<K, V>`  | `V`        | `values().iter_value()`  |
| `TreeMapEntriesRefIter<K, V>`   | `[&K, &V]` | `entries()`              |
| `TreeMapEntriesValueIter<K, V>` | `[K, V]`   | `entries().iter_value()` |

A map projection needs no axis suffix: `keys` already names what it yields, and
`keys_ref` would repeat the `&K` the signature states. Reference is the only
axis the three offer — a `&mut` key would break the ordering invariant, and a
`&mut` value buys nothing over `m[k] = v`. `TreeMapValuesValueIter` doubles the
word because the axis meets a projection already named `values`; the reading is
exact, and dropping either word would cost more than the repetition.

The iterators hold `&List<TreeMapEntry<K, V>>`, mirroring `Slice`'s
`&Array<T>`; holding it by value would deep-copy every entry at construction,
which the eager `entries()` did. Being views they inherit the aliasing rule, so
inserting or removing mid-traversal can skip or repeat an entry — a behaviour
change from the eager versions. `iter_value().collect()` takes a snapshot.

### Indexing

The index traits keep their four-way split: `Output: Ref` / `Output: RefMut`
bounds exclude scalars from `IndexRef`, and a scalar has no addressable cell, so
`IndexAssign` cannot fold into a `&mut`.

| Trait                              | Subscript   | `Output`    |
| ---------------------------------- | ----------- | ----------- |
| `IndexValue<I>` / `index_value`    | `c[i]`      | read `T`    |
| `IndexRef<I>` / `index_ref`        | `&c[i]`     | `&T`        |
| `IndexRefMut<I>` / `index_ref_mut` | `&mut c[i]` | `&mut T`    |
| `IndexAssign<I>` / `index_assign`  | `c[i] = v`  | written `T` |

All four name their associated type `Output`, not `Elem`: what a subscript
yields is the element only when the subscript is one position, and
`IndexValue<RangeExclusive<i32>>` yields a `Slice<T>`. `Sequence::Elem` keeps
its name because a sequence's elements really are elements.

The internal intrinsics follow the axis too — `array_get_value`,
`array_get_ref`, `array_get_ref_mut`. This mirrors no Wasm instruction names,
because the family never did: Wasm GC has no `array.get_ref`, and `array_new`
already maps to `array.new_default`.

### Bounds checks and the `_unchecked` contract

`.get(i)` returns `Option<T>` on all three types. `xs[i]` traps when out of
bounds; the message is implementation-defined, since Wado offers no trap
recovery and the trap itself is the whole contract. Each type checks exactly
what Wasm's own check leaves uncovered:

| Type    | Upper bound         | Lower bound                     |
| ------- | ------------------- | ------------------------------- |
| `Array` | `array.get`         | `array.get`                     |
| `List`  | `assert` — capacity | `array.get`                     |
| `Slice` | `assert` — the view | `assert` — `start + i` is valid |

`Array`'s backing is its length, so `array.get` is necessary and sufficient.
`List` and `Slice` have backings longer than `len()` — spare capacity, or the
region outside the view — so an upper-bound `assert` is required, and it earns
power-assert diagnostics for free. Only `Slice` also needs the lower bound: it
offsets by `start`, so `view[-1]` is a _valid_ backing index and would return an
element outside the view silently. `List[-1]` and `Array[-1]` trap in
`array.get`, a negative `i32` reading as a huge unsigned index.

Each bound is its own `assert` rather than a chained `assert 0 <= i < len`,
which today renders no operand values
([#1855](https://github.com/wado-lang/wado/issues/1855)) and stops the
bounds-check elimination from collapsing an index-write loop into `array.fill`
([#1856](https://github.com/wado-lang/wado/issues/1856)).

`get_unchecked` carries this contract:

> The caller must guarantee `0 <= index < len()`. Violating it yields an
> unspecified value of `T` or traps. It is never undefined behavior and never
> compromises memory safety.

That is structural, not aspirational: every path bottoms out in `array.get`,
which Wasm GC always bounds-checks. Wado's `_unchecked` elides a semantic check,
not a memory check — categorically weaker than Rust's, and why Wado needs no
`unsafe`. The name is borrowed from Rust; the contract is not.

### Component Model boundary

Eligibility splits by direction. Lowering (Wado supplies the list) needs only a
contiguous region and a length; lifting (Wado receives it) needs an owner for
freshly lifted memory, and return-position generics are not expressible.

| Type       | Lift (`export` param, `import` return) | Lower (`import` param, `export` return) |
| ---------- | -------------------------------------- | --------------------------------------- |
| `List<T>`  | ✓                                      | ✓                                       |
| `Array<T>` | ✓                                      | ✓                                       |
| `Slice<T>` | ✗ — definition-site error              | ✓                                       |

`Array<T>` must be accepted consistently: `wit_emit.rs` lowers it to `list<T>`
while `cm_binding/types.rs` reports it as having no CM representation.

`Slice<T>` is rejected at the definition site in both directions until the
lowering path exists — anywhere within the type, not merely at its head, since a
slice nested in a tuple or payload degrades identically. Reaching `wit_emit`
instead drops the component-type section for the whole component, breaking the
static "an `export` appears in WIT" guarantee of
[Visibility](./wep-2026-06-25-visibility-internal-pub-export.md).

Generic ergonomics stay out of the language. `wado-from-idl` emits a raw binding
taking `Slice<T>` plus a thin Wado wrapper:

```wado
fn send_raw(&self, data: Slice<u8>, …) -> …;

pub fn send<S: AsSlice<Elem = u8>>(&self, data: S, …) -> … {
    return self.send_raw(data.as_slice(), …);
}
```

Callers keep writing `f(xs)`, and a sub-range crosses the boundary without the
`to_list()` copy it needs today. The alternative — making `AsSlice` a
WIT-representable bound projecting to `list<T>` — would add a bound-to-WIT
projection plus a position-dependent legality rule, at the one place where "one
declaration, no overloading, no implicit conversion" binds hardest.
`core:cbor` / `core:json` / `core:digest` already prove the wrapper pattern via
`AsByteSlice`. This applies to top-level parameters only; a `list<T>` inside a
`record` field stays `List<T>`.

## Consequences

- One read-only algorithm surface instead of three partial copies; taking a
  slice no longer loses methods.
- Rust muscle memory is intentionally broken at the iterator methods, where
  Wado's semantics genuinely differ. The rename is wide but mechanical.
- A view has to write its own `Eq` / `Ord` / `Display` / `Inspect`. Structural
  derivation is not wrong — `&T == &T` is reference identity by design — but it
  answers about the view rather than the elements.
- The `Slice` name is claimed from the prelude and can no longer be user-defined.
- `Sequence` defaults are instantiated per implementor; each is small and DCE
  removes unused ones.
- Methods needing an element bound stay duplicated until bounds on associated
  types exist.

## Roadmap

- [ ] `sum` / `min` / `max` are bounded inherent methods on `SliceValueIter`
      alone, so `xs.iter_value().map(f).sum()` does not compile.
- [ ] Accept `Array<T>` in `cm_binding`, matching `wit_emit`.
- [ ] Lower `Slice<T>` through the canonical ABI and map it in `wit_emit`, then
      narrow the definition-site error to lifting positions only.
- [ ] The `wado-from-idl` `AsSlice` wrapper.
- [ ] Document `Array<T>` and `Slice<T>` in `docs/spec.md` (CM type mapping,
      snapshot/aliasing rules) and the cheatsheet.
