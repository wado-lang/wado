# Iterator Reference Model

Reference-based iteration for storage-backed collections.

## Context

Rust's `iter()` / `iter_mut()` / `into_iter()` trichotomy encodes three
ownership transfers: shared borrow, mutable borrow, and move. Wado has none of
these concepts — value semantics with GC, no move, no borrow checker — so the
trichotomy carries no meaning here. The only distinction left is _share a
reference_ vs _copy a value_, which is a performance choice, not a semantic one
(a copy is deep-equal to the original).

The current `List<T>` API imports the Rust shape without its rationale:

- `List::iter()` returns `ArrayIter<T>` with `Item = T` — it _copies_ every
  element. This inverts the Rust convention (`iter()` yields `&T`), so a Rust
  reader's `list.iter().any(...)` silently copies.
- Reference iteration exists only via `&List<T>` / `&mut List<T>`
  (`ArrayRefIter<T>`, `Item = &T`), reached through `for-of &list` — not through
  a named method a Rust reader would look for.
- `&mut List<T>` yields `&T`, not `&mut T`, so there is no `iter_mut`: elements
  are mutated by index. Under a borrow checker this restriction earns its keep;
  in Wado it is free to lift.

Iteration is small surface today (a handful of `.map` / `.filter` sites plus the
stdlib), so this is a cheap time to fix the model rather than the naming.

## Decision

### One principle

Borrow what exists in memory; produce by value what is synthesized. A
storage-backed collection (`List`, string bytes) yields `&T`; a computed
sequence with no backing storage (`RangeExclusive` / `RangeInclusive`,
`chars()`) yields `T`. This mirrors the rest of the language: references are the
sharing primitive, and a copy happens only at an assignment or call boundary,
written explicitly as `*x`.

### Protocol

- `for-of` and `iter()` over a `List<T>` yield `&T`, whether the source is
  `list` or `&list`. Implementation: change `IntoIterator for List<T>` from
  `Item = T` to `Item = &T` (reusing `ArrayRefIter`); `into_iter(&self)` already
  borrows `self`, so nothing is consumed. The value-copying `List` iterator is
  dropped from the public API.
- `for let x of &mut list` and `iter_mut()` yield `&mut T`, adding in-place
  element mutation (`for let x of &mut xs { *x = f(*x); }`). This is sound with
  no borrow checker; its representation rides on the reference write-back model
  ([WEP: Reference Representation and Mutation Write-Back](./wep-2026-06-13-reference-representation.md)).
- `next()` returns `Option<&T>`; `find()` returns `Option<&T>`; the closures of
  `map` / `filter` / `any` / `all` / `for_each` receive `&T`. `Option<&T>` is
  already produced by today's `ArrayRefIter`.
- Synthesized sequences keep `Item = T` unchanged.

### Primitive ergonomics

Struct iteration needs no special handling: field access on `&T` already yields
the field by value, so `x.field == target` and `x.field + 1` type-check under
`&T` iteration today. The only rough edge is a primitive element used _directly_
in an operator (`for let x of nums { sum += x }`, `x: &i32`), which needs `*x`.

Provide a `copied()` adaptor mapping `Iterator<Item = &T>` to
`Iterator<Item = T>`, so a caller opts into value iteration explicitly:

```wado
for let x of nums.copied() { sum += x; }             // x: i32
let hit = nums.copied().any(|x: i32| x == target);
```

`copied()` makes the copy visible at the call site, keeps the cheap path (`&T`)
the default, and needs no change to operator semantics. Explicit `*x` stays
available for a one-off read.

### Rejected: operator auto-deref

An earlier draft proposed auto-dereferencing `&T` in operator/comparison read
positions so primitives read as values. Measurement rejects it: `==` / `!=` on
`&T` today compare _reference identity_, not value — `&a == &b` is `false` for
two distinct variables both holding `5`, and `true` only for two references to
the same variable. Auto-deref would silently reinterpret every existing
`&T == &T` as a value comparison and remove the only way to test reference
identity with `==`. It would also entangle operator-overload resolution (a
reference's own `impl` vs the deref'd `impl`) and hide potentially expensive deep
comparisons behind an invisible `*`. Reference value-vs-identity is its own
proposal with its own migration, never a rider on iteration. (Arithmetic on `&T`
is a plain type error today, so it has no semantics to preserve, but bundling it
with the `==` change is what makes auto-deref a single unsafe lever — so the
whole lever is out of scope here.)

### Public surface

`for-of`, `iter()` (`&T`), `iter_mut()` (`&mut T`). `into_iter` is retained only
as the internal `for-of` desugar target.

## Consequences

- Removes the `iter()`-copies footgun; iteration is by reference (cheap) by
  default and matches a Rust reader's mental model of `.iter()`.
- Adds `iter_mut` — a capability the value-semantics-plus-index workaround only
  approximated.
- Migration (small surface): closures written `|x: T|` become `|x: &T|`; a loop
  body that reassigned the loop variable locally takes an explicit copy
  (`let mut y = *x;`, unchanged meaning since the binding was already a copy);
  `iter()` changes from copy to reference. The `for-of` desugar
  (`into_iter` / `next`) is retargeted, not restructured.
- Match ergonomics already handle `&T` scrutinees, so `for let { x, y } of pts`
  keeps working.
- Operator semantics are untouched: `==` on `&T` stays reference identity, and
  the primitive escape hatch is the explicit `copied()` adaptor, not a global
  auto-deref.

## TODO

- [ ] Confirm `&mut T` iteration composes with the reference write-back model
      (`next() -> Option<&mut T>`), or scope `iter_mut` to a later step.
- [ ] Add the `copied()` adaptor (`Iterator<Item = &T>` to
      `Iterator<Item = T>`); confirm `String`/deep-copy element ergonomics.
- [ ] Decide whether `into_iter` stays a nameable method or becomes
      desugar-only.
- [ ] Migrate stdlib and `package-gale` call sites; drop the value `ArrayIter`
      for `List` once no public use remains.
