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

The first adapter in a chain ended it, and a `TreeMap`'s values, a range, or a
string's bytes never had one. Five more were missing everywhere,
`SliceValueIter` included: `product`, `min_by`, `max_by`, `min_by_key`,
`max_by_key`. That left `fold` as the only spelling for a sum — and `fold` needs
an explicit identity, which is exactly what `min` and `max` have none of.

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

impl<T: Add<Output = T>> Sum for T {
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

`Product` is the same over `T: Mul<Output = T>`; `Extremum` carries `min_iter` /
`max_iter` over `T: Ord`. `reduce` folds back into `T`, so the operator carriers
pin `Output` as Rust's do.

The call site never writes the parameter. An iterator whose `Item` implements
neither trait fails at the call, naming the missing bound, not at the trait
declaration — the same diagnostic shape `collect` produces.

### Reference iterators

A `Ref` chain reaches none of the eight: `iter_ref().sum()` and `.min()` are
compile errors naming the missing `Sum` / `Extremum`, and `iter_value()` names
the step back to the values. Nothing special-cases it — two rules on how a
reference satisfies a bound decide, and both are the compiler's:

- A receiverless method has no receiver to auto-deref, so `&T` does not inherit
  a bound on a trait declaring one. Without this the bound held with no instance
  to dispatch to, and the call reached WIR build as an ICE.
- `==` on a reference is identity ([Iterator Reference Model](./wep-2026-07-05-iterator-reference-model.md)),
  so no ordering follows from it and `&T` is not `Ord` — which `Ord: Eq` would
  otherwise contradict. A struct holding a reference derives no `Ord` either.

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

The carrier traits lean on five bound-resolution behaviours none of which
worked, none specific to iterators. Each is pinned by a fixture under
`wado-compiler/tests/fixtures/`.

1. **A blanket bounded by a compiler-supplied trait must apply to a primitive.**
   `blanket_trait_impl_applies` answered its bound with an impl-index lookup,
   which finds `impl Ord for i32` but not the compiler-supplied `Add`, so
   `Sum`'s blanket held for `String` and not for `i32`. Fixture:
   `blanket_impl_builtin_bound.wado`.

2. **The bound's identity is the blanket's, not the asking module's.** The
   operator traits carry no compiler item, so a bare spelling let a user
   `trait Rem` lend every primitive its arithmetic, and a user `trait Add`
   downstream take it away. Read off `decl_ref` now. Fixtures:
   `error_same_named_operator_trait.wado`,
   `blanket_builtin_bound_shadowed_spelling.wado`.

3. **A blanket's bound keeps its associated-type constraints.** The index
   carried each bound's trait but not its `Output = T`, so `Product`'s blanket
   accepted any `Mul`. The operator itself now yields `T::Output` rather than
   assuming `Self`. Fixtures: `error_operator_output_widens.wado`,
   `operator_output_projection.wado`.

4. **A projection over a rigid type parameter is a type, not a pending
   question.** `T::Out` was deferred as "awaiting its impl", so
   `fn go<T: Widen>(a: T) -> T { return a.widen(); }` type-checked. Only a
   projection over an instance (`Array<T>::Elem`) still owes an impl its
   answer. Deciding them exposed two builders that had been hidden behind the
   deferral: one projection builder now serves every site, so a signature's
   `T::Output` and one synthesized for `a * b` are the same type; and a bound's
   answers (`D: Derived<Elem = i32>`) reach a default body naming a
   _supertrait's_ associated type. Fixtures:
   `error_assoc_projection_return_mismatch.wado`, `assoc_projection_rigid.wado`,
   `super_trait_assoc_in_default_body.wado`.

5. **A blanket's method must dispatch through a generic bound.**
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
- `Sum` and `Product` are bounded `T: Add<Output = T>` / `T: Mul<Output = T>`,
  as Rust's are: `reduce` folds back into `T`. A widening operator impl (`Cm *
  Cm -> Area`) stays legal and simply does not reach them.
- `fold` keeps its place. The `fold` call sites left in the tree are `fold`'s own
  tests and closure demonstrations, not sums written the long way.

## Roadmap

- [ ] Give `Sequence` the same treatment for `contains`, `binary_search`,
      `starts_with`, and `ends_with`, which stay bounded inherent impls with
      thin forwarders in
      [The Sequence Family](./wep-2026-06-02-sequence-family.md).
