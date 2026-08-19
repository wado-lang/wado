# WEP: Bounded Iterator Terminals — `sum` / `product` / `min` / `max`

How a terminal whose body needs `Item: Add` or `Item: Ord` reaches every
iterator, closing the open roadmap item of
[The Sequence Family](./wep-2026-06-02-sequence-family.md).

## Context

The sequence-family WEP left one item unfinished:

> `sum` / `min` / `max` are bounded inherent methods on `SliceValueIter`
> alone, so `xs.iter_value().map(f).sum()` does not compile.

`Iterator::Item` is an associated type and carries no bound, so a default body
needing `Item: Add` cannot be written on the trait. The three methods sit on
`impl<T: Add> SliceValueIter<T>` and `impl<T: Ord> SliceValueIter<T>` instead,
and every adapter loses them.

### The gap, measured

The prelude and `core:collections` declare 29 `Iterator` implementors. One has
the terminals:

| Group       | Count | Types                                                                                    | `sum` / `min` / `max` |
| ----------- | ----- | ---------------------------------------------------------------------------------------- | --------------------- |
| Slice views | 1     | `SliceValueIter`                                                                         | ✓                     |
|             | 4     | `SliceRefIter`, `SliceRefMutIter`, `SliceWindows`, `SliceChunks`                         | ✗                     |
| Adapters    | 7     | `IterMap`, `IterFilter`, `IterEnumerate`, `IterTake`, `IterSkip`, `IterChain`, `IterZip` | ✗                     |
| Ranges      | 2     | `RangeExclusive`, `RangeInclusive`                                                       | ✗                     |
| Map / set   | 8     | `TreeSet{Ref,Value}Iter`, `TreeMap{Keys,Values,Entries}{Ref,Value}Iter`                  | ✗                     |
| String      | 7     | `StrCharIter`, `StrCharIndicesIter`, `StrLinesIter`, `StrSplit*Iter`, `StrUtf8ByteIter`  | ✗                     |

So the first adapter in a chain ends it: `map`, `filter`, `skip`, `zip`, and
`chain` all drop the terminal, and a `TreeMap`'s values, a range, or a string's
bytes never had it.

Five more terminals are missing from every type including `SliceValueIter`:
`product`, `min_by`, `max_by`, `min_by_key`, `max_by_key`.

The standing workaround is `fold` with a hand-written closure: `example/`, the
prelude tests, and several `tests/fixtures/closure_*.wado` spell out
`fold(0, |acc: i32, x: i32| acc + x)` where `sum()` is meant. `fold` needs an
explicit identity, which is exactly what `min` and `max` have none of.

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

Four need no element bound at all and become plain defaults on `Iterator`:

| Method       | Signature                                                                              |
| ------------ | -------------------------------------------------------------------------------------- |
| `min_by`     | `(&mut self, cmp: fn mut(&Self::Item, &Self::Item) -> Ordering) -> Option<Self::Item>` |
| `max_by`     | same                                                                                   |
| `min_by_key` | `<K: Ord>(&mut self, key: fn mut(Self::Item) -> K) -> Option<Self::Item>`              |
| `max_by_key` | same                                                                                   |

`min_by_key`'s `K: Ord` is an ordinary method type parameter, not a bound on an
associated type.

Four need one, and route through a carrier trait:

```wado
/// Summation over an iterator of `Elem`. The bound lives here because
/// `Iterator::Item` carries none.
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
```

`Product` is the same over `T: Mul`; `Extremum` carries `min_iter` / `max_iter`
over `T: Ord`. On `Iterator` they read:

```wado
fn sum<S: Sum<Elem = Self::Item> = Self::Item>(&mut self) -> Option<S> {
    return S::sum_iter(self);
}
fn product<P: Product<Elem = Self::Item> = Self::Item>(&mut self) -> Option<P> {
    return P::product_iter(self);
}
fn min<E: Extremum<Elem = Self::Item> = Self::Item>(&mut self) -> Option<E> {
    return E::min_iter(self);
}
fn max<E: Extremum<Elem = Self::Item> = Self::Item>(&mut self) -> Option<E> {
    return E::max_iter(self);
}
```

The call site never writes the parameter. An iterator whose `Item` implements
neither trait fails at the call, naming the missing bound, not at the trait
declaration — the same diagnostic shape `collect` produces.

### Reference iterators

The prelude implements `Eq` / `Ord` for `&T` but not `Add`, so an `iter_ref()`
chain gains `min` / `max` as `Option<&T>` and not `sum`. Nothing special-cases
it — the carrier traits' bounds decide, and `iter_value()` names the step back.

### `Option<T>`, not `T`

All four return `Option`, keeping `SliceValueIter::sum`'s current meaning rather
than Rust's zero identity. Wado has no `Zero`, and `Default` would be a lie for
`min` — `i32::default()` is not the minimum of nothing. One rule across the four
beats a `T` for two and an `Option<T>` for the other two.

### The three inherent methods go

`SliceValueIter`'s `sum` / `min` / `max` are deleted, not kept as shadows. The
default bodies are the same loop over `next()`, and an inherent method that
shadows a default ([Overload Resolution](./wep-2026-07-31-overload-resolution.md))
is one that can silently drift from it.

### What stays out

`List`, `Array`, and `Slice` gain nothing. `Sequence`'s defaults could take the
same carrier-trait treatment, but `xs.iter_value().sum()` is the spelling, and a
second one on the owner would make the axis the sequence family exists to mark
optional again.

## Compiler prerequisites

Two defects blocked the design, neither specific to iterators. Both are fixed
here, each with a fixture under `wado-compiler/tests/fixtures/`.

1. **A blanket bounded by a compiler-supplied trait never applied to a
   primitive.** `blanket_trait_impl_applies` answered its bound with an
   impl-index lookup, which finds `impl Ord for i32` but not the
   compiler-supplied `Add`, so `impl<T: Add> Sum for T` held for `String` and
   not for `i32`. Fixture: `blanket_impl_builtin_bound.wado`.

2. **A blanket's method did not dispatch through a generic bound.**
   `resolve_type_param_dispatch` keyed the template by the call site's
   type-parameter head, which matches the blanket's receiver param only when the
   two are spelled alike. `fn f<D: Doubler>(x: D) { x.twice() }` against
   `impl<T: Add> Doubler for T` reached no instance and died at WIR build, after
   the elaborator had accepted the bound.
   Fixture: `blanket_impl_bound_dispatch.wado`.

Neither changes the WIR of an existing fixture: regenerating all 336 goldens
against the fix moves only line numbers already stale on `main`.

## Validation

The design was prototyped against the fixed compiler. Each of these was rejected
before it and now compiles and runs:

```wado
xs.iter_value().map(|x: i32| x * 2).sum()
xs.iter_value().filter(|x: i32| x > 1).sum()
xs.iter_value().map(|x: i32| x * 2).min()
xs.iter_value().zip(xs.iter_value()).map(|p: [i32, i32]| p.0 * p.1).sum()
xs.as_slice().iter_value().skip(1).sum()
(1..=10).sum()
xs.iter_value().product()
xs.iter_value().min_by_key(|x: i32| -x)
words.iter_value().max_by_key(|w: String| w.len())
words.iter_value().max()          // String, via the same blanket
m.values().iter_value().sum()     // TreeMap
s.iter_value().max()              // TreeSet
```

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
  whose `Add` widens is rejected in `sum_iter` rather than at the bound — as
  `SliceValueIter::sum` already does.
- The `fold`-as-`sum` spellings become `sum()`; the ones that stay are the ones
  that genuinely fold.

## Roadmap

- [ ] Add `Sum` / `Product` / `Extremum` and the eight defaults to
      `core:prelude/traits.wado`; delete the three inherent `SliceValueIter`
      methods.
- [ ] Cover the eight in `iterator_test.wado`, over an adapter and a bare
      iterator each.
- [ ] Replace the `fold`-as-`sum` spellings that are really sums.
- [ ] Document the terminals in `docs/spec.md` and the cheatsheet.
