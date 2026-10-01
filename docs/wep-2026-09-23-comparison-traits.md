# WEP: One Order per Type

## Context

### Two relations compete for the operators

A float can be compared two ways. IEEE 754's comparisons call a NaN unordered
against everything, itself included, and call `-0.0` and `0.0` one value. A
total order gives every pair an answer, which is what `sort`, an ordered map
and a `T: Ord` bound need. Every language decides which of the two `==` and `<`
mean, and how a program reaches the other.

### What Rust's users wish it had done

Rust keeps the operators IEEE and makes floats implement only `PartialEq` and
`PartialOrd`. [Research: Float Comparison](./research-float-comparison.md)
collects what its users say about the result:

- `vec.sort()` does not compile on a `Vec<f64>`, and the idiom taught in its
  place panics on a NaN.
- One float field takes `Eq`, `Ord` and `Hash` from a whole struct, so it cannot
  key a map.
- The names read backwards: `PartialEq` supplies `==`, and the plain `Eq` only
  states a law.
- The law that `Eq`, `PartialEq`, `Ord` and `PartialOrd` agree is checked by
  nothing. Rust 1.81's sorts began to panic on code where they did not.

Wado will not copy the `Partial` split. That leaves the question of which
relation the operators read.

### Two relations on one type need a rule for every reader

The languages that keep IEEE operators and add a total order beside them
(C++20, Java, Julia, Go) all need a rule for which reader reads which relation.
`sort` reads one and `<` the other. Julia's `Set` finds a NaN and its arrays do
not. A Go map stores a NaN key that no lookup reaches again. A struct's derived
`==` and its derived `cmp` disagree whenever a field holds a float.

Moving the split into the type system does not remove it. Swift's 2017
"Comparison Reform" pitch made `==` total in a generic body and IEEE at a
concrete float. It was not adopted, because the same expression on the same
values would answer differently depending on where it was written. Kotlin
ships that split and documents it as a caveat.

### The reason NaN is unequal to itself is gone

IEEE made `NaN != NaN` so that `x != x` could detect a NaN in languages that had
no `isnan`. Wado has `is_nan()`.

### NaN bits are not a value

Wasm leaves the sign of an arithmetic NaN to the host, and its payload too when
an input is not canonical. On x86_64 `0.0 / 0.0` is a negative NaN; on AArch64
it is positive. An order that reads a NaN's bits, as IEEE 754 `totalOrder` and
C++20's `std::strong_order` do, sorts the same program's NaNs differently on two
machines.

## Decision

### One equality and one order

Every type has one equality and one order, and the operators read them:

- `==` and `!=` read `Eq::eq`.
- `<`, `<=`, `>` and `>=` read `Ord::cmp`.

No type gives an operator a meaning of its own, floats included. A comparison
in a body generic over `T: Ord` means what it means at the concrete type.

Where a type implements both, `a == b` holds exactly when `a.cmp(&b)` is
`Equal`.

The traits keep the names `Eq` and `Ord`. Nothing is `Partial`, and `Ordering`
keeps its three cases.

### One source for `==` and `cmp`

Rust states the same law and checks it nowhere. Its usual breach is a
hand-written order beside a derived equality: a `cmp` that skips a cache field,
and a derived `==` that compares it. Clippy's `derive_ord_xor_partial_ord` lint
exists because deriving one of the pair and writing the other breaks the law.
So in Wado the two are independent only where someone writes both:

| Written    | `==` comes from         | `cmp` comes from           |
| ---------- | ----------------------- | -------------------------- |
| neither    | the members, derived    | the members, derived       |
| `cmp` only | `cmp`: equal on `Equal` | the written `cmp`          |
| `eq` only  | the written `eq`        | nothing: a use is an error |
| both       | the written `eq`        | the written `cmp`          |

The first three rows cannot disagree. An order cannot be built from an equality,
so the third row asks for `cmp` rather than deriving one that ignores the
written `eq`.

The last row exists for speed. `==` on a `String` or a `List` stops at a length
mismatch, where `cmp` must walk the common prefix. C++20 kept the two apart for
the same reason: a written `<=>` does not generate `==` (P1185, "`<=>` != `==`").

Nothing proves a written pair agrees, and that risk is accepted. The `test`
world narrows it: each call to either method also computes the other and traps
when they disagree. Production code pays nothing. A disagreement surfaces in a
test rather than as a sort that panics in production, as Rust 1.81's did.

The rule reads `Eq<Self>` only. An `Eq<Rhs>` for another type, such as
`StrSlice == String`, has no `Ord` beside it to agree with.

### The float order

`f16`, `bf16`, `f32` and `f64` share one order:

- Every NaN is one value. It equals every NaN, whatever its sign and payload,
  and is greater than every other value, `+Inf` included.
- `-0.0` equals `0.0`.
- Any other two values compare as IEEE 754 compares them.

So the line runs `-Inf < … < -1 < 0 < 1 < … < +Inf < NaN`, and `-0.0` stands
where `0.0` does.

`Equal` therefore never depends on a NaN's bits, which Wasm does not fix. The
two zeroes are equal because ordinary arithmetic produces `-0.0`
(`-1.0 * 0.0`, rounding `-0.3`), and `x == 0.0` must hold for it.

NaN goes last rather than first, as in Java, Kotlin, Julia and `ordered-float`.
With NaN greatest, `x < c` and `x <= c` are false for a NaN `x`, as IEEE says.
So a guard against an upper bound, or a search for a minimum, skips a NaN
exactly as it did under IEEE. A search for a maximum finds the NaN, as Wasm's
`f64.max` does.

### Where the answers leave IEEE

They differ only when an operand is a NaN:

| Expression            | IEEE  | Wado  |
| --------------------- | ----- | ----- |
| `NaN == NaN`          | false | true  |
| `NaN < x`, `NaN <= x` | false | false |
| `x < NaN`, `x <= NaN` | false | true  |
| `NaN > x`, `NaN >= x` | false | true  |
| `x > NaN`, `x >= NaN` | false | false |

Here `x` is not a NaN.

The comparisons now obey the laws of an order. `!(a < b)` is `a >= b`, exactly
one of `a < b`, `a == b` and `a > b` holds, and `x == x` holds for every `x`.
`x != x` is always false, so a NaN is tested with `is_nan()`. Each IEEE
predicate is still one expression of an operator and `is_nan()`, listed in
[Float Comparison](./spec-standard-traits.md#float-comparison).

### What follows

- `sort` puts every NaN last. The two zeroes are `Equal`, so a stable sort keeps
  them in input order.
- A `TreeMap<f64, V>` holds one NaN key and one zero key.
- `List::contains`, a `TreeMap` lookup and a struct's derived `==` all reach
  the same answer, since there is one equality to reach.
- A SIMD vector is untouched. Its `==` compares bits, and its lane comparisons
  are IEEE intrinsics called by name.

### Cost

A comparison against a constant that is not a NaN costs one IEEE instruction.
`x < c`, `x <= c`, `x == c` and `x != c` are IEEE's answers already. `x > c` is
`!(x <= c)`, and `x >= c` is `!(x < c)`.

Between two variables the IEEE instruction is joined with NaN tests, with no
branch:

- `a < b` is IEEE `a < b`, or `b` is a NaN and `a` is not.
- `a == b` is IEEE `a == b`, or both are NaNs.

Where the optimizer proves an operand is not a NaN, the test goes.

### Rejected

- IEEE operators beside a separate total order, as C++20, Java, Julia and Go
  do. It keeps two relations on one type, and needs the rule for every reader
  described above.
- A generic `<` that reads the total order while a concrete one is IEEE. This
  is the split Swift declined and Kotlin documents as a caveat.
- IEEE 754 `totalOrder` as the order. It reads a NaN's bits, which Wasm leaves
  to the host. It also orders `-0.0 < 0.0`, and with the operators reading it,
  `x == 0.0` would be false for a `-0.0` that ordinary arithmetic produced.
- NaN least, as Go and the Haskell proposal chose. A guard `x <= limit` would
  then pass a NaN, where IEEE rejects it.

## Roadmap

- [ ] Implement the float `Eq` and `Ord` impls and the operator lowering for
  all four float types, and remove `OperatorOrd`.
- [ ] Rewrite the fixtures that pin today's answers (`float_total_order.wado`,
  `half_ieee_compare.wado`) to pin these.
- [ ] Measure the cost on comparison-heavy code, sorting and Loam's kernels
  among it, and record it here.
- [ ] Derive `Eq` from a written `cmp`, and reject a use of `Ord` on a type
  whose `Eq` is written and whose `cmp` is not. Each with a fixture.
- [ ] In the `test` world, check every call to a written `eq` or `cmp` against
  the other, with a fixture whose disagreeing pair traps.

## Known gaps

Whether an `Eq<Rhs>` for another type must agree with the type's own `==` is
not settled. `StrSlice == String` should answer as `String == String` does, and
nothing says so or checks it.

`f64::min` and `f64::max` lower to Wasm's `min` and `max`, which are IEEE
754-2019 `minimum` and `maximum`. `f64::min(1.0, NaN)` is NaN, where
`Iterator::min` over the same values reads the order and answers `1.0`.

A NaN test ported from another language as `x != x` is always false, and
nothing warns.

Float literal patterns are rejected, though one equality would give them a
meaning.
