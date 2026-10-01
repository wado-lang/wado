# Research: Float Comparison, and What Rust's Users Wish It Had Done

Wado has decided one thing about comparing floats: it will not copy Rust's
`PartialEq` / `PartialOrd` split. This survey collects what Rust's users say
Rust should have done instead, and sets it beside the answers other languages
chose. It feeds
[WEP: One Order per Type](./wep-2026-09-23-comparison-traits.md).

## Rust's design

`f32` and `f64` implement `PartialEq` and `PartialOrd`, and neither `Eq` nor
`Ord`. The operators are IEEE. `total_cmp` (Rust 1.62) gives IEEE 754-2019
`totalOrder` as a method, but it is not an `Ord` impl. `Ord` must agree with
`PartialOrd`, so no impl could be both IEEE and total. `f64::max` is IEEE
754-2008 `maxNum`, which ignores a NaN.

## The voices

### Floats should sort with no ceremony

`vec.sort()` does not compile on a `Vec<f64>`. The idiom taught everywhere is
`sort_by(|a, b| a.partial_cmp(b).unwrap())`, which panics on a NaN. `total_cmp`
arrived years later, as a method, because a wrapper type "is not easy to add
because of the large API surface" (rust-lang/rust#72568). Users still ask for
floats to be `Ord` outright ("It would make life a lot easier, all around"),
and report that `total_cmp` sorts faster than `partial_cmp`. The `ordinal()`
proposal (rust-lang/libs-team#816) exists only to make `sort_by_key` usable on
floats.

### One float field should not disqualify a whole type

A struct holding an `f64` cannot derive `Eq`, `Ord` or `Hash`. It cannot key a
`BTreeMap` or join a `HashSet`, however unrelated the float is to its identity.
The `ordered-float` and `decorum` crates exist to undo this one field at a time.

### The names read backwards

`Eq` carries no method and states a law; `PartialEq` supplies `==`. A reader
expects the plain name to be the ordinary one. The names were `Eq` / `TotalEq`
and `Ord` / `TotalOrd` until rust-lang/rust#12520 (2014) renamed them, so that
"[the total traits] will be the only traits used by most generic code". The
same names also misstate the mathematics: users point out that `Ord` is a total
preorder, and that `PartialOrd` on a float is not a partial order, because
`NaN <= NaN` is false.

### A law between traits is a law nobody checks

Rust requires `Ord`, `PartialOrd`, `Eq` and `PartialEq` to agree, and nothing
enforces it. The standard library documentation shows impls that disagree by
mistake, which happens easily when some are derived and others written by hand.
Rust 1.81 replaced its sorts with ones that detect a non-total order and panic
("user-provided comparison function does not correctly implement a total
order"), and code that had worked for years started to panic.

### The order people use collapses NaN

`ordered-float` is the crate people reach for. Its order calls a NaN "greater
than all other values and equal to itself", treats every NaN alike whatever its
bits, and calls `-0.0` equal to `0.0`. That is not `totalOrder`: `total_cmp`
splits NaN by sign and payload, and `ordered-float` does not.

### NaN bits are not portable

RFC 3514 (Rust float semantics) makes a NaN result's sign and payload
non-deterministic: what `to_bits` shows "can depend on compiler version,
compiler flags, target architecture". Anything that orders by bits orders NaNs
by an accident of the machine. The Wasm specification says the same for every
arithmetic NaN: "its sign is non-deterministic". Only the Wasm deterministic
profile pins it.

### An operator should mean one thing

Swift's 2017 "Comparison Reform" pitch made `==` and `<` total in a generic
context and IEEE at a concrete float. It was not adopted. Xiaodi Wu objected to
"an identical spelling (e.g. `==`) giving two different answers with the same
values of the same type, depending on the generic context", and proposed a
different spelling for IEEE so the code shows which one it means. Kotlin ships
exactly that context split, statically typed `Double` against `Any` or a type
parameter, and documents it as a caveat.

### One trait with two orders, or one order generic code can trust

In the 2014 rename, pcwalton proposed a single `Ord` trait: `<` partial and
`.cmp()` total. glaebhoerl answered that generic code "most often requires" the
total one, and must be able to use the operators to get it. Rob Pike framed the
same choice for Go (golang/go#59531): a constraint built on `<` fails
unpredictably on a NaN, and one built on a total order forbids `<`. "Neither of
these paths is satisfactory."

### Hiding a NaN in a sort hides a bug

A total order sorts a NaN to one end, and a median taken afterwards is "an
error that looks like it isn't one". This argues for keeping the IEEE operators:
a NaN should not pass silently through code that never asked for it.

### Reaching the total order must not slow `<`

Making `<` itself total costs a branch on every comparison, and "that would
penalize the performance of all code, even code that does not need the total
ordering". The two orders have to live side by side, not one inside the other.

### `min` and `max` are their own problem

IEEE 754-2019 removed `minNum` and `maxNum`, because they are not associative,
and added `minimum` / `maximum`, which propagate a NaN and order `-0 < +0`.
Rust's `f64::max` ignores a NaN argument, as `maxNum` and 2019's
`maximumNumber` do, and may return either zero. Its `maximum` is not stable, and
`Ord::max` would be a third meaning had floats been `Ord`.

## Other languages

| Language          | `==` / `<`                 | What sorts and keys read      | NaN in that order         | `-0` vs `+0`  |
| ----------------- | -------------------------- | ----------------------------- | ------------------------- | ------------- |
| Rust              | IEEE                       | nothing (`total_cmp` by hand) | split by sign and payload | `-0 < +0`     |
| C++20             | IEEE (`<=>` partial)       | `std::strong_order` by hand   | split by sign and payload | `-0 < +0`     |
| Java              | IEEE                       | `compareTo`, `equals`         | one value, greatest       | `-0 < +0`     |
| Kotlin            | IEEE if statically a float | `compareTo`, `equals`         | one value, greatest       | `-0 < +0`     |
| Julia             | IEEE                       | `isless`, `isequal`           | one value, greatest       | `-0 < +0`     |
| Go                | IEEE                       | `cmp.Compare` for sorts       | one value, least          | equal         |
| Swift             | IEEE                       | `<` (NaN order unspecified)   | unspecified               | equal         |
| JavaScript        | IEEE                       | `Map`, `Set`: SameValueZero   | one value                 | equal         |
| Haskell           | IEEE, `compare` = GT       | `compare` (lawless)           | breaks the sort           | equal         |
| Haskell, proposed | total                      | `compare`                     | one value, least          | (not covered) |

Two patterns stand out. Every language keeps the operators IEEE; the one serious
proposal to change that (Haskell's, by Daniel Fischer) moved IEEE to separate
operators and was not adopted. And every language that built a total order into
its library collapsed NaN into one value. Only the by-hand functions of Rust and
C++ expose a NaN's sign and payload.

Where membership in a sequence is asked, Julia and Go read `==`, so a NaN is
never found in an array. Julia's `Set` and `Dict` read `isequal`, so it is found
there. Go's maps read `==`, so a NaN key can be stored and never reached, which
is part of why Go added `clear`.

## Wado today

What the compiler answered when this research was done is recorded in
[WEP: One Order per Type](./wep-2026-09-23-comparison-traits.md#what-the-compiler-answers-before-this-wep).

## What the voices ask for

1. A float sorts, and keys an ordered collection, with no wrapper and no
   ceremony.
2. A float field leaves its struct comparable, ordered and usable as a key.
3. `==` and `<` are IEEE, because every language keeps them so, and so does the
   one instruction that implements them.
4. An expression means the same thing at a concrete type and in a generic body.
5. Two relations a type carries cannot disagree by mistake. Either one derives
   from the other, or each has its own name and its own job.
6. The total order is deterministic. A NaN's sign and payload never decide an
   answer, since neither Wasm nor the CPU fixes them.
7. No name says `Partial`, and the plain name is the ordinary thing.
8. The total order costs nothing on code that uses `<`.

## Design candidates

### A: the status quo

The operators are IEEE at a float. A generic `<` reads `Ord::cmp`. `Ord` is
bit-level `totalOrder`. It fails 4 and 6, and the derived-`Eq` disagreement
fails 5.

### B: two relations, each with one job

The operators are IEEE at a float wherever they are written, a generic body
included, because a generic `<` resolves against the type it is instantiated
at. `Ord::cmp` is the total order, read by name: by `sort`, `TreeMap`,
`Iterator::min` / `max`, and every algorithm that orders or keys. For every type
without a float the operators read `cmp`, so the two coincide by construction.
`Ord`'s equality is `cmp` answering `Equal`, and `==` is not bound to it.

This is Julia's and Go's design with the context split removed. It meets 1–5, 7
and 8. It meets 6 once the float order collapses NaN (below). Pike's objection
still applies: a generic algorithm written with `<` is IEEE at a float. The
answer is that it is IEEE at a float written out by hand as well, so the generic
body adds no surprise of its own.

### C: the operators are total on a float

`NaN == NaN` is true and `<` is the total order. This is the Haskell proposal.
Each type then has one relation, so 4 and 5 hold by construction, with no rule
about which reader reads which. It gives up 3: `x != x` stops finding a NaN.

It fails 8 less than it first appears. If the order differs from IEEE only on a
NaN, a comparison against a constant that is not a NaN is still one IEEE
instruction. Between two variables it adds a NaN test or two, with no branch.

### The float total order

The order has to be chosen under either B or C. Both point to one NaN: every
NaN is `Equal` to every other, so Wasm's non-deterministic sign and payload
never reach an answer. Java, Kotlin, Julia, Go and `ordered-float` all do this.
NaN greatest is the majority choice (Java, Kotlin, Julia, Swift's pitch,
`ordered-float`).

The zeroes depend on the candidate. Under B, `-0 < +0` costs nothing, since the
operators stay IEEE. Under C it would make `x == 0.0` false for a `-0.0` that
ordinary arithmetic produced, so C calls the two `Equal`, as `ordered-float`
and Go do.

## Outcome

Wado took C, with one NaN, NaN greatest, and `-0.0` equal to `0.0`. The
decision and its reasons are in
[WEP: One Order per Type](./wep-2026-09-23-comparison-traits.md).

## Sources

- [rust-lang/rust#12520: the 2014 rename, with pcwalton's single-trait proposal](https://github.com/rust-lang/rust/pull/12520)
- [rust-lang/rust#72568: `total_cmp`](https://github.com/rust-lang/rust/pull/72568)
- [rust-lang/libs-team#816: `fN::ordinal`](https://github.com/rust-lang/libs-team/issues/816)
- [Rust forum: Total order for floats?](https://users.rust-lang.org/t/total-order-for-floats/99919)
- [Rust forum: Traits in `std::cmp` and mathematical terminology](https://users.rust-lang.org/t/traits-in-std-cmp-and-mathematical-terminology/69887)
- [Rust internals: fast finite floating-point types](https://internals.rust-lang.org/t/avoiding-partialord-problems-by-introducing-fast-finite-floating-point-types/5376)
- [RFC 3514: float semantics](https://rust-lang.github.io/rfcs/3514-float-semantics.html)
- [Rust 1.81 release notes: the new sorts](https://github.com/rust-lang/rust/blob/master/RELEASES.md#version-1810-2024-09-05)
- [`ordered-float`: `OrderedFloat`](https://docs.rs/ordered-float/latest/ordered_float/struct.OrderedFloat.html)
- [swift-evolution: Comparison Reform pitch](https://lists.swift.org/pipermail/swift-evolution/Week-of-Mon-20170410/035676.html)
- [swift-evolution: Abrahams and Wu on the context split](https://lists.swift.org/pipermail/swift-evolution/Week-of-Mon-20170417/036054.html)
- [Kotlin: floating-point equality](https://kotlinlang.org/docs/equality.html)
- [golang/go#59531: Pike on NaN and `ordered`](https://github.com/golang/go/issues/59531)
- [Go `cmp` package](https://pkg.go.dev/cmp)
- [Julia discourse: equalities of NaN](https://discourse.julialang.org/t/various-equalities-of-nan/42649)
- [Haskell libraries: new Eq and Ord instances for Double](https://mailman.haskell.org/archives/list/libraries@haskell.org/thread/RYZYB343VNWFRT76PWUEZDNSQWIVXM4H)
- The Wasm specification, `document/core/exec/numerics.rst`, § NaN Propagation
  (`vendor/wasm`)
