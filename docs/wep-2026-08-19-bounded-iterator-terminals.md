# WEP: Bounded Iterator Terminals — `sum` / `product` / `min` / `max`

How a terminal whose body needs `Item: Add` or `Item: Ord` reaches every
iterator. Signatures and behaviour live in
[`core:prelude`](./stdlib-core-prelude.md).

## Context

`Iterator::Item` is an associated type and carries no bound, so a default body
needing `Item: Add` cannot be written on the trait. `sum` / `min` / `max`
therefore sat on `impl<T: Add> SliceValueIter<T>` and
`impl<T: Ord> SliceValueIter<T>`, and every adapter lost them —
`xs.iter_value().map(f).sum()` did not compile.

### The gap it left

The prelude and `core:collections` declare 29 `Iterator` implementors. One had
the terminals:

| Group       | Count | Types                                                                                    | `sum` / `min` / `max` |
| ----------- | ----- | ---------------------------------------------------------------------------------------- | --------------------- |
| Slice views | 1     | `SliceValueIter`                                                                         | ✓                     |
|             | 4     | `SliceRefIter`, `SliceRefMutIter`, `SliceWindows`, `SliceChunks`                         | ✗                     |
| Adapters    | 7     | `IterMap`, `IterFilter`, `IterEnumerate`, `IterTake`, `IterSkip`, `IterChain`, `IterZip` | ✗                     |
| Ranges      | 2     | `RangeExclusive`, `RangeInclusive`                                                       | ✗                     |
| Map / set   | 8     | `TreeSet{Ref,Value}Iter`, `TreeMap{Keys,Values,Entries}{Ref,Value}Iter`                  | ✗                     |
| String      | 7     | `StrCharIter`, `StrCharIndicesIter`, `StrLinesIter`, `StrSplit*Iter`, `StrUtf8ByteIter`  | ✗                     |

The first adapter in a chain ended it: `map`, `filter`, `skip`, `zip`, and
`chain` all dropped the terminal, and a `TreeMap`'s values, a range, or a
string's bytes never had it. Five more were missing everywhere, `SliceValueIter`
included: `product`, `min_by`, `max_by`, `min_by_key`, `max_by_key`.

That left `fold` with a hand-written closure as the only spelling for a sum —
and `fold` needs an explicit identity, which is exactly what `min` and `max`
have none of.

## Decision

Reach the element bound through a **method-level type parameter with a
default** — the shape `Iterator::collect` already uses:

```wado
fn collect<C: FromIterator<Elem = Self::Item> = List<Self::Item>>(&mut self) -> C
```

A terminal needing `Item: Add` is the same problem `collect` already solved, so
it gets the same answer: the bound moves to a carrier trait keyed on `Elem`, and
the parameter defaults to `Self::Item`.

### The eight terminals

`min_by`, `max_by`, `min_by_key`, and `max_by_key` need no element bound at all
and become plain defaults on `Iterator` — the `_by_key` pair's `K: Ord` is an
ordinary method type parameter, not a bound on an associated type.

Four need one, and route through a carrier trait:

```wado
internal trait Sum {
    type Elem;
    fn sum_iter<I: Iterator<Item = Self::Elem>>(iter: &mut I) -> Option<Self>;
}

impl<T: Add> Sum for T {
    type Elem = T;

    fn sum_iter<I: Iterator<Item = T>>(iter: &mut I) -> Option<T> {
        return iter.reduce(|a: T, b: T| a + b);
    }
}

// on Iterator, and the same shape for `product` / `min` / `max`
fn sum<S: Sum<Elem = Self::Item> = Self::Item>(&mut self) -> Option<S> {
    return S::sum_iter(self);
}
```

`Product` is the same over `T: Mul`; `Extremum` carries `min_iter` / `max_iter`
over `T: Ord`.

The call site never writes the parameter. An iterator whose `Item` implements
neither trait fails at the call, naming the missing bound, not at the trait
declaration — the same diagnostic shape `collect` produces.

### Reference iterators

The prelude implements `Eq` / `Ord` for `&T` and `&mut T` but not `Add`, so an
`iter_ref()` chain gains `min` / `max` as `Option<&T>` and not `sum`. Nothing
special-cases it — the carrier traits' bounds decide, and `iter_value()` names
the step back.

`Ord for &T` / `&mut T` had to be written out: the bound was already satisfied
without them, so the instance had no impl to be homed by and two modules asking
for one each minted their own, which the package-defines check rejects.

### `Option<T>`, not `T`

All eight return `Option`, where Rust gives `sum` / `product` a zero identity.
Wado has no `Zero`, `Default` would be a lie for `min` — `i32::default()` is not
the minimum of nothing — and one rule across the eight beats two.

### Ties

`min`, `min_by`, and `min_by_key` keep the first of equal elements; the `max`
three keep the last. The asymmetry is Rust's, and it is what makes the two ends
complementary over a sequence with duplicate keys.

### No inherent shadows

`SliceValueIter` carries none of the eight itself. An inherent method shadows a
default ([Overload Resolution](./wep-2026-07-31-overload-resolution.md)), so one
holding the same body is one that can silently drift from it.

### What stays out

`List`, `Array`, and `Slice` gain nothing. `Sequence`'s defaults could take the
same carrier-trait treatment, but `xs.iter_value().sum()` is the spelling, and a
second one on the owner would make the axis the sequence family exists to mark
optional again.

## Compiler support

The carrier traits lean on two blanket-impl behaviours neither of which worked,
neither specific to iterators. Each is pinned by a fixture under
`wado-compiler/tests/fixtures/`.

1. **A blanket bounded by a compiler-supplied trait must apply to a primitive.**
   `blanket_trait_impl_applies` answered its bound with an impl-index lookup,
   which finds `impl Ord for i32` but not the compiler-supplied `Add`, so
   `impl<T: Add> Sum for T` held for `String` and not for `i32`. Fixture:
   `blanket_impl_builtin_bound.wado`.

2. **A blanket's method must dispatch through a generic bound.**
   `resolve_type_param_dispatch` keyed the template by the call site's
   type-parameter head, which matches the blanket's receiver param only when the
   two are spelled alike. `fn f<D: Doubler>(x: D) { x.twice() }` against
   `impl<T: Add> Doubler for T` reached no instance and died at WIR build, after
   the elaborator had accepted the bound. Fixture:
   `blanket_impl_bound_dispatch.wado`.

## Alternatives

- **Bounds on associated types** (`type Item: Add`, or a conditional
  `where Self::Item: Ord`) — what the sequence-family WEP deferred to. The
  direct expression, but a language feature with a wide blast radius that this
  design needs none of. The two stay compatible: if it lands, the carrier traits
  collapse into direct bounds without a call site changing.
- **Nominal `impl Sum for i32`, one per primitive** — what Rust writes by macro.
  Wado has none, so it is 12 numeric primitives × 3 traits by hand, and every
  user type is still left out.
- **Keep them inherent, duplicated per iterator type** — 29 types × 8 methods,
  and the next adapter starts the duplication again.

## Consequences

- Every iterator gets the same eight terminals; an adapter no longer ends a
  chain.
- Three carrier traits enter the prelude namespace (`Sum`, `Product`,
  `Extremum`) as `internal`.
- `T: Add` does not pin `Add::Output = T`; the `reduce` body does, so a type
  whose `Add` widens is rejected inside `sum_iter` rather than at the bound.
- `fold` keeps its place. The `fold` call sites left in the tree are `fold`'s own
  tests and closure demonstrations, not sums written the long way.

## Roadmap

- [ ] Give `Sequence` the same treatment for `contains`, `binary_search`,
      `starts_with`, and `ends_with`, which stay bounded inherent impls with
      thin forwarders in
      [The Sequence Family](./wep-2026-06-02-sequence-family.md).
