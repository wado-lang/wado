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

### What the compiler answers before this WEP

Measured on x86_64 with `wado run`, at `-O0` and `-O2` alike:

- `Ord` on a float is bit-level `totalOrder`, so `[1.0, 0.0 / 0.0, -1.0].sort()`
  gives `[NaN, -1, 1]` on x86_64 and puts the NaN last on AArch64.
- For `struct P { x: f64 }`, the derived `==` and `cmp` disagree on a NaN and on
  the two zeroes.
- `f64::max(1.0, NaN)` is NaN, as Wasm's `f64.max` is, while
  `[1.0, 0.0 / 0.0].max()` on an iterator is `1.0`, because the negative NaN
  sorts least.
- `a < b` in a body generic over `T: Ord` reads `cmp`, and the same expression
  at `f32` is IEEE.
- `[NaN].contains(&NaN)` is false, since `List::contains` reads `==`, while a
  `TreeMap<f64, V>` reads `cmp` and finds a NaN key.

## Decision

### One equality and one order

A type has at most one equality and one order, and the operators read them:

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

A newtype reads the table by what it writes itself. One writing `cmp` alone
takes `==` from it, not from its base. One writing `eq` alone inherits no `Ord`
from its base, which orders values its `eq` may call equal, and a newtype over
it inherits none from it.

Each instance of a generic declaration reads its own row, by the impls that
reach it. `impl Ord for Ranked<i32>` gives `Ranked<i32>` its `==` from that
`cmp`, while `Ranked<String>` derives both from the members. `impl Eq for Named<i32>` withholds `Ord` from `Named<i32>` alone. The second row's `==` holds
where the written `cmp` does, under its bounds.

A reference reads no row. `==` on one compares what it points to, so
`impl Ord for &Item` gives `&Item` no `==` of its own.

The last row exists for speed. `==` on a `String` or a `List` stops at a length
mismatch, where `cmp` must walk the common prefix. C++20 kept the two apart for
the same reason: a written `<=>` does not generate `==` (P1185, "`<=>` != `==`").
Nothing proves or checks that a written pair agrees, and that risk is accepted.

The rule reads `Eq<Self>` only. An `Eq<Rhs>` for another type, such as
`StrSlice == String`, has no `Ord` beside it to agree with, and nothing ties it
to the type's own `==`. Only `==` and a string literal pattern read it: `sort`,
`TreeMap` and `contains` read `Eq<Self>` and `Ord`.

`AsStrSlice` gets no exception, though it requires `Eq<String>`. A type whose
`Eq<String>` is not byte equality, such as a case-insensitive string, answers `x == s` one way while `contains_str(x)` and
`get_str(x)`, which compare the bytes of `as_str_slice()`, answer the other.
Such a type keeps its own equality in its `Ord`, and does not claim to be text
by implementing `AsStrSlice`.

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
`x != x` is always false, so a NaN is tested with `is_nan()`.

### IEEE comparisons by name

Each float type carries IEEE 754's six comparisons as methods: `ieee754_eq`,
`ieee754_ne`, `ieee754_lt`, `ieee754_le`, `ieee754_gt` and `ieee754_ge`. The
operators keep the order, and IEEE's answers stay reachable under a name that
says which they are, as `ieee754_min` and `ieee754_max` are beside `min` and
`max`.

Each is one Wasm instruction. Code that ports an IEEE comparison, or a loop
that compares two loaded floats and cannot pay the order's NaN tests (see
[Cost](#cost)), says so with one call. Written with an operator and `is_nan()`
instead, each predicate is a compound expression that is easy to get wrong.

The names carry the `ieee754_` prefix because a bare `eq` would collide with
`Eq::eq`, and because a SIMD vector's `eq` and `lt` already name IEEE lane
comparisons.

No IEEE three-way comparison is offered. IEEE's comparison is partial, so it
could only return `Option<Ordering>`, which reads as the order's `cmp` and
costs a branch to consume. The order is a float's one three-way comparison.

### `min`, `max` and `clamp`

No total order can match Wasm's `f64.min` and `f64.max`, which are IEEE
754-2019 `minimum` and `maximum`. Both of those return a NaN when either
argument is one. An order puts NaN at one end, so only one of `min` and `max`
can return it. Putting NaN least would only move the disagreement from `min` to
`max`.

So the two meanings take two names. `min` and `max` follow the order everywhere,
on a float as through `Iterator` or a `T: Ord` bound, so one name means one
thing. `ieee754_min` and `ieee754_max` carry IEEE's meaning under the
`ieee754_` prefix the comparisons carry, and lower to Wasm's instructions.
`f64::min` therefore costs what a `<` between two variables costs (see
[Cost](#cost)) and a select, instead of one instruction.

Of two `Equal` arguments, `min` returns the first and `max` the second, as
Stepanov argued and Rust's `Ord::min` and `max` do. `Iterator::min` and `max`
already break ties that way, so two arguments answer as a two-element list
does, and the pair never returns one argument twice. The rule costs nothing:
either tie-break is a `<=` with the operands in some order, and only which
operand carries the NaN test differs.

`clamp` keeps a NaN `x` as NaN. The order alone would give `high`, since NaN is
greatest, and that hides a NaN in exactly the place a range check is meant to
catch bad input. A clamp that returns its input's NaN is what IEEE code expects.
The exception is stated with `clamp`'s rule, so it does not have to be inferred
from the order.

### Float patterns

A float literal stays out of patterns, though one equality now gives it a
meaning. Whether a float equals a literal turns on rounding the source does not
show: `0.1 + 0.2` is not `0.3`, and an arm `0.3 =>` would miss it. A NaN is the
smaller problem. Rust admits float literal patterns, matches them by `==`, and
since 1.77 rejects a NaN constant in one (rust-lang/rust#41620).

A constant carries the same rounding: a `global` holding `0.1` is the literal
under another name. So a constant whose type is or holds a float is not a
pattern either, at the top of an arm or nested. `f64::INFINITY` is excluded too,
though it does not round, because one rule for every float constant is simpler
than an exception for the exact ones.

A [range pattern](./spec-patterns.md#range-patterns) takes a float, as Rust's
does. A range is `low <= x` and `x < high` (or `x <= high`), the comparisons an
`if` chain would write, so rounding moves only a value on a bound, as it does
for the `if`. Under the float order:

- `-0.0` falls in every range holding `0.0`.
- A NaN bound is an error, as in Rust. `1.0..=NaN` would match every `+Inf` and
  NaN, which no reader expects.
- A NaN scrutinee is greater than every bound and matches no range, as IEEE's
  comparisons say. A `match` on a float therefore always needs `_`, which makes
  the NaN case a written one.
- Equal bounds (`1.0..=1.0`) are an error, since they spell the literal pattern
  this section refuses.

An integer literal bound on a float is the float of its value (`0..<1.5`), as
`let x: f64 = 1` is. Rust refuses both. Wado's expression rule already takes
the literal, and a pattern reading it otherwise would be the exception.

A bound may also be any constant: an immutable `global` or an associated
constant. A range takes the rounding a constant carries as it takes a
literal's, so the reason the constant is no pattern alone does not reach it. A
constant's value shows only when the match runs, so the range compares by the
type's order then, as a constant pattern compares by `==`. The checks above need
the values, so they apply where the compiler knows them: literals and a
primitive's limits.

### Comparing a value with itself warns

Reflexivity gives every comparison of an expression with itself one answer, on
every type. Such a comparison is either a mistake or a NaN test carried over
from another language, where it is now always false. The
[`self_comparison` lint](./spec-expressions.md#the-self_comparison-lint) warns
about all six operators, and on a float names `is_nan()`. It reports only an
expression that performs no effect, since one that does may answer differently
the second time. For the same reason it skips one that writes: an assignment, a
`&mut` borrow, a `&mut self` method such as an iterator's `next`, a call of a
closure, which may capture by `&mut`, and a call handed a value that holds a
`&mut`, which the callee may write through. GCC's `-Wtautological-compare` and
Clippy's `eq_op` warn on the same shape.

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

The slowdown this costs is accepted. Measured on 2026-10-02, it is up to 30% on
a loop that does little but compare two loaded floats, and none outside such
loops:

| Loop over 200,000 random `f64`s                 | Before   | After    | Change       |
| ----------------------------------------------- | -------- | -------- | ------------ |
| `if xs[i - 1] <= xs[i] { n += 1 }`, and `==`    | 0.625 ms | 0.812 ms | 30% slower   |
| `if xs[i] > xs[best] { best = i }`              | 0.504 ms | 0.621 ms | 23% slower   |
| `if xs[i] > 0.25 { n += 1 }`                    | 0.443 ms | 0.444 ms | no change    |
| `sort()` on a `List<f64>`                       | 39.6 ms  | 38.7 ms  | within noise |
| `sort()` on a `List<f32>` holding the same data | 35.8 ms  | 37.5 ms  | within noise |

Every Wado row of the benchmark suite (`mise run all-wado`) stayed within noise.

`f32::max` costs more than a comparison. It was one `f32.max` instruction and is
now a comparison and a `select`. wasmtime compiles a float `select` to a
conditional branch on x86, and where the larger operand changes at random the
branch is mispredicted half the time. Measured on 2026-10-02 over Loam's kernels,
copied into loops over 262,144 random `f32`s:

| Kernel                                    | Before   | After    | Change      |
| ----------------------------------------- | -------- | -------- | ----------- |
| `Relu`: `map(xs, \|v\| f32::max(v, 0.0))` | 1.116 ms | 2.680 ms | 140% slower |
| `MaxPool`: 2×2 taps through `f32::max`    | 0.988 ms | 2.059 ms | 108% slower |
| `Softmax`: a row's `f32::max`             | 0.548 ms | 0.742 ms | 35% slower  |

`f32::ieee754_max` runs each kernel within noise of the old `f32::max`. Loam's
kernels call it, since it propagates a NaN as `max` does and differs only in
the sign of a zero result. The `microgpt` benchmark, which calls `f64::max` for
its ReLU and its softmax, stayed within noise.

"Before" is `origin/main` at `ba07da1b9` and "after" is this WEP's change on
top of it. Both compilers were built `--release`, and each program ran at `-O2`
under the vendored wasmtime, on a 4-core Intel Xeon cloud VM. A figure is the
best of six runs. A change counts only where the two compilers' ranges do not
overlap, which is what `benchmark/ab.ts` decides.

### Rejected

- IEEE operators beside a separate total order, as C++20, Java, Julia and Go
  do. It keeps two relations on one type, and needs the rule for every reader
  described above.
- A generic `<` that reads the total order while a concrete one is IEEE. This
  is the split Swift declined and Kotlin documents as a caveat.
- IEEE 754 `totalOrder` as the order. It reads a NaN's bits, which Wasm leaves
  to the host. It also orders `-0.0 < 0.0`, and with the operators reading it,
  `x == 0.0` would be false for a `-0.0` that ordinary arithmetic produced.
- Checking a written `eq` and `cmp` against each other in the `test` world, on
  every call. A `cmp` that calls `==` on its own type would recurse through the
  check, and nested types would pay it once per level.
- NaN least, as Go and the Haskell proposal chose. A guard `x <= limit` would
  then pass a NaN, where IEEE rejects it.

## Roadmap

- [x] Implement the float `Eq` and `Ord` impls and the operator lowering for
  all four float types, and remove `OperatorOrd`.
- [x] Rewrite the fixtures that pin today's answers (`float_total_order.wado`,
  `half_ieee_compare.wado`, now `half_float_order.wado`) to pin these.
- [x] Measure the cost on comparison-heavy code and sorting, and record it in
  [Cost](#cost).
- [x] Derive `Eq` from a written `cmp`, and reject a use of `Ord` on a type
  whose `Eq` is written and whose `cmp` is not, a marker included. Each with a
  fixture.
- [x] Apply the table to a newtype: its own `cmp` gives its `==`, and its own
  `eq` alone leaves it no `Ord`, through an operator, a bound and a method
  call. Each with a fixture.
- [x] Make `f32::min`, `f64::min` and their `max` follow the order, add
  `minimum` and `maximum` on the Wasm instructions, and make `clamp` keep a NaN
  `x` and trap on a NaN bound, `high` included, which `low <= high` passes.
  Each with a fixture. Then measure Loam's kernels, which compare floats
  through `f32::max`, and record it in [Cost](#cost).
- [x] Rename `minimum` and `maximum` to `ieee754_min` and `ieee754_max`.
- [x] Add the six `ieee754_*` comparison methods to `f16`, `bf16`, `f32` and
  `f64`, each lowering to one Wasm instruction, with a stdlib test holding each
  method's answers on a NaN, `-0.0` and an ordinary pair.
- [x] Report the `self_comparison` lint, with a fixture for each operator, the
  float hint, a chain, an operand that performs an effect, and `allow`.
- [x] Accept float range patterns, with fixtures for `-0.0`, a NaN scrutinee, a
  NaN bound, equal bounds, and a `match` that lacks `_`.
- [x] Reject a constant pattern whose type is or holds a float, with fixtures
  for `f64::INFINITY`, a `global` float, and a nested struct constant holding
  one.

## Known gaps

A range with a constant bound other than a primitive's limit is checked for
nothing its values decide. A NaN constant bound is no error, so `1.0..=LIMIT`
with `LIMIT` a NaN matches every value from `1.0` up, NaN included. Reversed
and empty ranges, overlapping arms, and coverage go unreported, and a `match` on
an integer needs `_` beside such a range even where the arms cover every value.

The NaN test goes only where an operand is a constant. Nothing yet proves that a
computed operand is not a NaN, so `a < b` between two variables pays the test
even where neither can be one.

A float `min` or `max` is a conditional branch under wasmtime on x86, where
`ieee754_min` and `ieee754_max` are one instruction each. Over operands whose
order changes at random, `max` costs up to two and a half times `ieee754_max`,
as
[Cost](#cost) measures.

The `self_comparison` lint warns about a call of a function that reads or
writes a `global mut`, since such a function declares no effect yet
([Global Variables](./spec-expressions.md#global-variables)). With `next_id()`
incrementing a counter and returning it, `next_id() != next_id()` is true, and
the lint says it is always false.

The compiler states the table in [One source for `==` and `cmp`](#one-source-for--and-cmp)
twice. The trait solver states it as impls, each paired with the written impl it
follows from, and lets precedence between the impls reaching an instance pick
the row, and a bound reads the row from there. The elaborator reads the written
impls reaching the instance in `comparison_written_alone`, which an operator, a
method call and synthesis ask. Nothing makes the two agree but the fixtures.
