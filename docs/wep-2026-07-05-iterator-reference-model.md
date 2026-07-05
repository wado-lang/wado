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

### Read-context auto-deref (the enabling decision)

To keep primitive iteration ergonomic, extend the existing `&T` auto-deref
(already applied at field access and method calls) to read positions in
operators and comparisons, so `&i32` reads as `i32`:

```wado
for let x of nums { sum += x; }   // x: &i32, reads as i32
```

Without this, reference iteration would sprinkle `*x` through every numeric
loop. Auto-deref applies to reads only; a write target (`x = …`) still requires
an explicit `*x`. This is safe absent a borrow checker and reduces `*` noise
language-wide.

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

## TODO

- [ ] Confirm `&mut T` iteration composes with the reference write-back model
      (`next() -> Option<&mut T>`), or scope `iter_mut` to a later step.
- [ ] Pin the exact read-context auto-deref rule (which operator/comparison
      positions; interaction with reference identity) before implementation.
- [ ] Decide whether `into_iter` stays a nameable method or becomes
      desugar-only.
- [ ] Migrate stdlib and `package-gale` call sites; drop the value `ArrayIter`
      for `List` once no public use remains.
