# WEP: Comparison Traits — the Operator Order and the Total Order

## Context

`Ord` does two jobs. It is what `sort`, `TreeMap` and every `T: Ord` bound read,
and it is what `<`, `<=`, `>` and `>=` mean for a type with no Wasm instruction
to lower to. For every type but a float those two are the same order, so one
trait carrying both went unnoticed.

A float separates them. `Ord::cmp` answers `Ordering`, which has three cases;
an IEEE comparison has four answers, the fourth being that there is none. So
`cmp` cannot express IEEE, and as long as it returns a bare `Ordering` it must
be the total order — which then disagrees with `<` on a NaN and on the two
zeroes. The divergence is forced by the return type rather than chosen.

Today the two jobs are split by accident of lowering rather than by name:

- `f32` and `f64` lower `<` to an instruction, so the operator is IEEE and
  never reaches `cmp`.
- `f16` and `bf16` have no instruction, so
  [their ordering operators](./wep-2026-09-22-half-precision-primitives.md)
  dispatch to `OperatorOrd`, an `internal` trait of four `bool` methods written
  for exactly this.
- Through a `T: Ord` bound there is no instruction either, so
  `fn max<T: Ord>(a: T, b: T) { if a > b { … } }` gives `f32` the _total_
  order, where the same expression written at `f32` gives IEEE.

That last one is the sharp edge. A generic body and a concrete body disagree
about the same operator on the same type, and nothing in either source says
which order is in play.

## Decision

### Two orders, two names

`Ord` becomes what the comparison operators mean. `TotalOrd` becomes the total
order that `sort`, `TreeMap` and key comparison read.

```wado
pub trait Ord: Eq with () {
    fn lt(&self, other: &Self) -> bool;
    fn le(&self, other: &Self) -> bool;
    fn gt(&self, other: &Self) -> bool;
    fn ge(&self, other: &Self) -> bool;
}

pub trait TotalOrd with () {
    fn total_cmp(&self, other: &Self) -> Ordering;
}
```

`Ord` carries four `bool` methods rather than one `cmp`, because that is the
shape the operators need and the shape a float can answer. `Option<Ordering>`
would add a case to allocate and match on where four booleans say the same
thing, and every one of them lowers to a single instruction on the types that
have one.

A type that writes `TotalOrd` gets `Ord`'s four methods from it, so nothing but
a float ever writes both. The four float types write both: `TotalOrd` as IEEE
754-2019 `totalOrder`, `Ord` as IEEE comparison.

After this, `<` means one thing wherever it is written — at a concrete type,
through a bound, on a half, in a generic body — and `sort` means one thing.
Neither can be reached by accident from the other's name.

### `TotalOrd` does not require `Eq`

`Ord: Eq` holds: both are IEEE, so `!(a < b) && !(a > b)` and `a == b` disagree
only where IEEE says unordered, and `Eq` is the operator's own answer.

`TotalOrd` requires nothing. Its equality is `total_cmp` answering `Equal`,
which for a float is not `Eq::eq`: the total order separates `-0.0` from `0.0`
and calls a NaN equal to itself. A `TreeMap<f32, V>` keyed by the total order
must read that equality and not `==`, or a lookup disagrees with the ordering
that placed the entry.

### The comparison operators are IEEE on every float

This follows from the split rather than adding to it, and is what makes the
sharp edge above go away: with `Ord` as the operator trait, a `T: Ord` bound
and a concrete `f32` reach the same impl.

Making the operators _total_ instead — one order for everything — was weighed
and refused. It is coherent, and it would restore trichotomy. It fails on three
counts. `-0.0 == 0.0` would become false, and negative zero arises silently from
ordinary arithmetic where a NaN is rare and usually an error, so it trades a
loud problem for a quiet one. Every float comparison would cost a sign-magnitude
key instead of one instruction, in loops that do nothing else. And `core:simd`
exposes twelve comparison instructions (`f32x4_lt` and its siblings) that are
IEEE by the Wasm specification and cannot be changed, so a scalar `<` that was
total would disagree with the lane-wise one — the same inconsistency, moved
somewhere it cannot be closed.

C++20 reached the same split from the other direction, keeping `operator<`
partial and adding `strong_order` beside it.

## Roadmap

1. Declare `TotalOrd` in `core:prelude`, with `Ord`'s four methods defaulting
   to it. Done when a type writing only `total_cmp` compares with `<`.
2. Move the total-order impls: every `impl Ord for T { cmp }` in the corpus
   becomes `impl TotalOrd for T { total_cmp }`. Done when `Ord` is written by
   nothing but the floats.
3. Move the bounds that mean the total order — `sort`, `sorted`, `TreeMap`,
   `TreeSet` and the key comparisons under them — from `T: Ord` to
   `T: TotalOrd`. Done when each bound names the order its body reads.
4. Give `f32` and `f64` an `Ord` impl, and fold `OperatorOrd` into `Ord` so the
   half types implement it directly. Done when `OperatorOrd` is gone and the
   elaborator's ordering dispatch names one trait.
5. Extend auto-derivation to `TotalOrd`, so a plain struct still sorts with no
   declaration. Done when the derivation tests pass against the new name.

## Known gaps

`min` and `max` take `T: Ord` today and each caller means one order or the
other. Which they should take is unsettled, and the two answers give different
results for a NaN.

A float reached through a `T: Ord` bound is IEEE, so a generic body may find no
answer at all — `a < b`, `a > b` and `a == b` all false. Nothing in the bound
warns that the order is partial.

`Eq` is not split. A total equality exists and differs from IEEE's on a float,
but it is reachable as `total_cmp` answering `Equal`, so no second trait names
it. Whether a type can mean the total equality without naming its order is open.

Auto-derived `TotalOrd` on a struct holding a float compares each field by the
total order, so two structs that compare equal under `==` may not under
`total_cmp`. Nothing reports the mixture.
