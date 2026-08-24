# WEP: Literal Coercion as `From<Array<…>>`

## Context

Wado coerced `[…]` and `{…}` literals to collection types through a four-trait
builder protocol — `SequenceLiteralBuilder` / `SequenceLiteral` and
`KeyValueLiteralBuilder` / `KeyValueLiteral`, plus two blanket impls
([Literal-to-Collection Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md)).
The compiler expanded a literal into `new_literal(capacity)`, N ×
`push_literal` / `insert_literal`, and `build()`, synthesizing the TIR for that
block by hand.

Three things have changed since that design.

- `Array<T>` became a value type in its own right
  ([Sequence Family](./wep-2026-06-02-sequence-family.md)) and gained a
  first-class NIR node
  ([`NirExprKind::ArrayLiteral`](./wep-2026-05-31-nir-array-literal.md)). A
  fixed-length array _is_ what a literal denotes, and the optimizer already
  deduplicates it, folds indexing into it, and promotes a constant one to the
  data section.
- `From<T>` landed ([Conversion Traits](./wep-2026-03-16-conversion-traits.md)),
  with a compiler-provided reflexive `impl From<T> for T`.
- The builder protocol never grew heterogeneous elements. `Element` / `Value` was
  a single type, so `core:value::Value` implemented neither trait and
  `let v: Value = { name: "Alice", scores: [10, 20] }` did not compile — the
  literal shape [JSON Literal Compatibility](./wep-2026-01-18-json-literal-compatibility.md)
  was written for.

The protocol was also the largest piece of hand-written lowering in the
elaborator — the coercions, the TIR synthesis, the coercion-fact records and
their name remangling, the remangle sweep, and the impl search: about 1,100
lines that existed because a literal was not an expression the rest of the
pipeline could see.

## Decision

### A literal's natural type is an `Array`

| Literal       | Natural type    |
| ------------- | --------------- |
| `[e0, e1, …]` | `Array<E>`      |
| `{k0: v0, …}` | `Array<[K, V]>` |

A key-value literal is an array of pairs — the same shape `TreeMap`'s entry
iterator already yields (`type Item = [K, V]`). Two parallel arrays would be
cheaper and are rejected for it: the pair array is the honest denotation, and it
is what lets the whole mechanism be `From`, whose `from` takes one argument.

This is what a coercion materializes, not what a literal is everywhere. Tuple
and struct interpretations keep their existing priority: `[…]` against a tuple
type — or against no type at all, as in `let t = [1, 2, 3]` — is a tuple
literal, and `{…}` against a nominal struct with matching fields is a struct
literal. Coercion is attempted only when neither applies.

### Coercion is `From`, applied at literal positions

There is no literal-specific trait. A literal in a position of known type `T`
elaborates to `T::from(natural)`:

```wado
let a: List<i32>            = [1, 2, 3];      // List::from(Array<i32>)
let m: TreeMap<String, i32> = { x: 1, y: 2 }; // TreeMap::from(Array<[String, i32]>)
let s: Array<i32>           = [1, 2, 3];      // reflexive From<T> for T
```

One rule governs implicit conversion in the language:

> A literal is implicitly converted to its target type through `From`. No other
> expression is implicitly converted.

"Literal" means a number, string, char, `bool`, `null`, or byte literal
(`b'x'`, `b"…"`), and an `[…]` or `{…}` literal. A template string, a
variable, and a call are not literals.

A bare `null` is an `Option<!>`: a value of every `Option<T>` and of no other
type. That is what a target converts from to accept it, so `null` reaches a
literal slot exactly where an `Option` does or where the slot's type writes
`impl From<Option<!>>` — which is how `core:value::Value` takes JSON's `null`.
Typing it `Unknown` instead would defer every check it meets, which is how a
`null` used to reach a slot whose type is not nullable.

```wado
let v: List<Value> = [1, "x"];   // OK — every element is a literal
let v: List<Value> = [a, b];     // ERROR — write [Value::from(a), Value::from(b)]
let v: List<i64>   = [x];        // ERROR — no implicit widening of `x: i32`
```

An element that reaches no conversion says which of the two rules refused it —
the element is not a literal, or the slot's type has no `From` from what the
element is.

Existing literal typing ([Literal Type Conversion Rules](./wep-2026-01-12-literal-type-conversion.md))
runs first: an unsuffixed `42` against `i64` _is_ an `i64`, not an `i64::from`.
`From` applies only where literal typing cannot reach the target, so `{n: 1}`
against `Value` is `Value::from(1i32)` — the literal takes its default type,
then converts.

### Impl selection

The element type is read off the selected impl, and drives the elaboration of
every element (and key).

- `{…}` considers impls of the form `From<Array<X>> for T` where `X` is a
  two-element tuple. None is an error: _`T` is not constructible from an object
  literal_.
- `[…]` considers every `From<Array<X>> for T`. Where more than one matches, the
  one whose `X` is not a two-element tuple wins; still more than one is an error
  reporting how many, and `T::from(…)` written out resolves it by ordinary
  overload resolution — with a turbofish where the target is generic
  (`List::<i32>::from(…)`).

The key side is settled by what a literal can write: every key is a field name,
so the selected impl's `K` must accept a `String`. A target that builds from
another key type is rejected at the literal — computed keys are what would give
such a `K` something to be written from.

The rule exists for types that accept both literal forms. `core:value::Value`
is the case that motivates it:

| Literal      | `List<i32>`      | `TreeMap<String, i32>` | `Value`                        |
| ------------ | ---------------- | ---------------------- | ------------------------------ |
| `[1, 2]`     | ✓                | ✗ (elements not pairs) | `Value::List`                  |
| `{a: 1}`     | ✗ (no pair impl) | ✓                      | `Value::Object`                |
| `[["a", 1]]` | ✗                | ✓ (as a map)           | `Value::List` of `Value::List` |

The last column is JSON's own reading of those three documents, which is the
test this rule is meant to pass.

Where the target's type arguments are still open — a callee's slot the call site
instantiated — the elements decide them, as they do today.

Newtypes peel: a `newtype N = List<i32>` target coerces through `List`'s impl and
casts the result to `N`.

### Spread

`..base` inside a literal ([Literal Spread](./wep-2026-07-03-literal-spread.md))
cannot be a `From`, so it keeps a trait of its own:

```wado
/// `..base` inside a `{ … }` literal: merge `base` into `self`, last-wins.
internal trait LiteralSpread {
    fn spread_literal(&mut self, base: Self);
}
```

A literal with spreads is a left-to-right fold: a run of consecutive `k: v`
members becomes one `T::from([…])` call, and each `..base` becomes one
`spread_literal`. A leading spread seeds the accumulator directly.

```wado
let m: T = { ..base, "c": 3 };
// {
//     let mut __acc: T = base;
//     __acc.spread_literal(T::from([["c", 3]]));
//     break __kv_lit: __acc;
// }
```

The dead-write and duplicate-key checks stay in the compiler, at the literal,
unchanged.

### Standard library impls

```wado
impl From<Array<T>> for List<T> {
    fn from(elements: Array<T>) -> List<T> {
        return List { repr: elements, used: builtin::array_len(&elements) };
    }
}

impl<T: Ord> From<Array<T>> for TreeSet<T> {
    fn from(elements: Array<T>) -> TreeSet<T> {
        let mut s = TreeSet::<T>::new();
        for let e of elements { s.insert(e); }   // last duplicate ignored
        return s;
    }
}

impl From<Array<[String, V]>> for TreeMap<String, V> {
    fn from(entries: Array<[String, V]>) -> TreeMap<String, V> {
        let mut m = TreeMap::<String, V>::new();
        for let [k, v] of entries { m.insert(k, v); }
        return m;
    }
}

impl From<Array<i32>> for i32x4 {
    fn from(a: Array<i32>) -> i32x4 { … }   // four `replace_lane`s, no builder
}
```

`Array<T>` needs no impl: the reflexive one covers it, and the array the
compiler builds _is_ the result.

`List` takes ownership of the array it is handed, so a list literal costs one
`array.new_fixed` and a struct literal — no capacity check, no push loop.

The SIMD lane readers pad a short literal with zero (`if i < a.len() { a[i] }
else { 0 }`, folded away since the length is a constant), preserving today's
`[1, 2, 3] as i32x4` behaviour.

`core:value` gains the two array impls plus the leaf conversions
(`From<i32>`, `From<f64>`, `From<String>`, `From<bool>`, `From<Option<!>>` for
`null`, …), which is what makes a JSON-shaped literal compile.

### Compiler shape

The elaborator builds an `Array` value and emits an ordinary static call. That
deletes the coercion facts, the remangle sweep, and both hand-built TIR
expansions in `reify.rs`; what remains of the old paths is the element-type
inference, the newtype peel, the `&mut […] as T` borrow transparency, and the
duplicate-key and dead-write diagnostics.

`TirExprKind::ArrayLiteral` is added, and `lower` emits `NirExprKind::ArrayLiteral`
directly. `optimize::array_literal` — the peephole rule that reconstructed that
node from an inlined `array_new(N) + N × push` window — is removed: code
reducible to a literal should be written as a literal. Removing it leaves every
benchmark's `-O2` WIR byte-identical.

Three consumers keyed on the shape the builder desugar used to leave, and read
the array literal directly instead: `container_sroa` recognizes `[]` as a
one-argument call handed an empty `ArrayLiteral`; niri values an
`Array<T>`-typed `ArrayLiteral` as the sequence it is rather than as a
`{ repr, used }` container, and sees through a block's constant `let`s so an
initializer that binds its parts still folds; and
`const_object_globalization` treats an element read as non-escaping whether the
spine is the local itself or a field of it.

The `sequence_literal_builder`, `sequence_literal`, `key_value_literal_builder`,
and `key_value_literal` compiler items are removed; `from` is the only one the
mechanism needs.

## Consequences

- One rule replaces two mechanisms. "Implicit conversion happens at literals,
  through `From`" is the whole story, and the explicit escape hatch
  (`T::from(arr)`) is the same code path rather than a parallel one.
- Four traits, two blanket impls, and the `Builder` / `Output` indirection are
  gone. The immutable-output case that needed a second trait and a builder
  struct — `Array<T>` and ten SIMD vectors — needs neither.
- A user makes a type literal-constructible by writing `impl From<Array<T>> for
  MyVec`, with no vocabulary specific to literals.
- Heterogeneous literals work, so `core:value::Value` is constructible from a
  JSON-shaped literal for the first time.
- `null` becomes a typed value rather than a deferral hole. Every place that
  used to skip a branch "still holding UNKNOWN" — the `if` / `match` /
  labeled-block result-type pick, the missing-return walk, reify's recorded-type
  read — now asks the same question of `Option<!>` too, and a `null` that fits
  nowhere is reported instead of reaching WIR.
- A list literal is cheaper at every optimization level: one `array.new_fixed`
  instead of a capacity allocation and N bounds-checked pushes.
- A key-value literal allocates one pair per entry where the builder allocated
  none. A constant one is expected to globalize whole, via the constant-tuple
  path of [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md);
  a non-constant one pays for the pairs.
- Because a literal is now a call, `wado query` resolves a definition from it and
  `wado dump tir` shows it, where before the expansion existed only inside the
  elaborator.
- `collect()`, `from_iter`, and hand-written push loops no longer collapse to an
  array literal now that `optimize::array_literal` is gone. This is intended,
  and it costs the benchmarks nothing: their `-O2` WIR is unchanged.
- The node's meaning narrows: it is the raw `Array<T>` a literal denotes, not
  the `List<T>` the retired pass materialized.
  [`NirExprKind::ArrayLiteral`](./wep-2026-05-31-nir-array-literal.md) is
  rewritten to match.

## Known gaps

- Sequence spread. `[..xs, 4]` is expressible under `LiteralSpread` but is not
  adopted here, and `Array<T>` could not implement it in any case — a fixed
  array does not grow, and `xs.len()` is not a compile-time constant. A spread
  in a coerced sequence literal is reported where it is written. Closing the gap
  means deciding whether the asymmetry with `Array` is acceptable.
- Computed keys. `{ [Color::Red]: 1 }` needs the key expression to convert to
  `K`, which `Array<[K, V]>` already carries positionally; only the syntax and
  its elaboration are missing.
- Passing a literal where the target type is a generic _parameter_
  (`fn f<E, C: From<Array<E>>>(xs: C)`). A bound carries associated-type
  bindings (`I: Iterator<Item = T>`) and no positional trait arguments — the
  AST has no field for one — so the bound itself is unwritable, in this or any
  other trait. A generic _instantiation_ is a different case and works: an open
  `Array<E>`, `List<T>` or `TreeMap<String, V>` slot is decided by the
  elements.
- Deep-copy elision on the array handed to `From` is left to `lower` and
  `value_copy_demote`. No benchmark's `-O2` WIR carries a `$value_copy$Array`
  at a literal site; if one appears, the copy analysis is extended rather than
  the design changed.

## Related WEPs

- [Literal-to-Collection Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md) — superseded by this WEP
- [Conversion Traits (From, TryFrom, `?` operator)](./wep-2026-03-16-conversion-traits.md)
- [Literal Type Conversion Rules](./wep-2026-01-12-literal-type-conversion.md)
- [Literal Spread (`..base`)](./wep-2026-07-03-literal-spread.md)
- [JSON Literal Compatibility](./wep-2026-01-18-json-literal-compatibility.md)
- [Sequence Family](./wep-2026-06-02-sequence-family.md)
- [`NirExprKind::ArrayLiteral` — a NIR-Materialized List Node](./wep-2026-05-31-nir-array-literal.md)
- [Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md)
