# WEP: The Operator Order and the Total Order

## Context

### Wado's problem

`Ord` does two jobs. It is what `sort`, `TreeMap` and every `T: Ord` bound read,
and it is what `<`, `<=`, `>` and `>=` mean for a type with no Wasm instruction
to lower to. For every type but a float those are the same order, so one trait
carrying both went unnoticed.

A float separates them, and the return type is what forces it. `cmp` answers
`Ordering`, which has three cases; an IEEE comparison has four answers, the
fourth being that there is none. So `cmp` cannot express IEEE, and as long as it
answers a bare `Ordering` it must be the total order — which then disagrees with
`<` on a NaN and on the two zeroes.

The operators are IEEE on every float today, and `Ord` is the total order. What
is left is that one name covers both jobs, so which one a call reads is not
visible where it is written:

```wado
fn max<T: Ord>(a: T, b: T) -> T { if a > b { return a; } return b; }
```

At `T = f32` that `>` has no instruction to reach, so it reads `cmp` and gives
the total order. The same expression written at `f32` gives IEEE. A generic body
and a concrete body disagree about the same operator on the same type, and
nothing in either source says which order is in play.

### Rust's answer, and what its users say about it

Rust splits the two, and puts floats on the partial side only: `f64` implements
`PartialOrd` but not `Ord`. Three complaints follow it, and all three are
long-standing.

`vec.sort()` does not compile on a `Vec<f64>`. The workaround taught everywhere
is `sort_by(|a, b| a.partial_cmp(b).unwrap())`, which panics on a NaN; the
correct `total_cmp` arrived years later and is still the less familiar of the
two.

One float field disqualifies a whole struct. It cannot derive `Eq` or `Ord`, so
it cannot be a `BTreeMap` key or a `HashSet` member, however unrelated the rest
of its fields are to the comparison.

The names read backwards. `Eq` carries no methods and means "equality here is an
equivalence relation", while `PartialEq` is the one that supplies `==`. A reader
expects the plain name to be the ordinary one.

Wado does not have the first two. A float implements `Ord`, so `List<f32>`
sorts and a struct holding one is a `TreeMap` key. Keeping that is not in
question here.

## Decision

The comparison operators are IEEE on every float, and `Ord` is the total order.
Both are implemented.

Where a type's two orders differ, the operators read `OperatorOrd` — four
`bool` methods, `internal` to `core:prelude`, written for `f16` and `bf16`,
which are the only types with no instruction and two orders to choose between.
`f32` and `f64` reach the same answers through their instructions.

How the split should be named in the public API is not decided. `OperatorOrd`
is a private stopgap and its name forecloses nothing.

## Roadmap

Nothing committed. The problem is recorded; the shape of the answer is open.

## Known gaps

One name covers both orders, so a `T: Ord` bound does not say which its body
reads, and `max<T: Ord>` above answers a NaN differently from the same
expression at a concrete float.

`min` and `max` take `T: Ord`, and each caller means one order or the other.
Which they should take is unsettled.

A float's total equality — `cmp` answering `Equal` — is not `==`: it separates
`-0.0` from `0.0` and calls a NaN equal to itself. A `TreeMap<f32, V>` orders by
the first and has no way to say so, so nothing states which equality a keyed
lookup owes.

`OperatorOrd` is reachable by method syntax nowhere, since a call site cannot
name it, but `wado doc` lists its impls all the same. That gap is recorded in
[WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).
