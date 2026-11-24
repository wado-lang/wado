# For-Of Walks an Iterator Itself

## Context

An iterator is a position, but Wado copies it like any other value.
`IntoIterator`'s blanket `impl<I: Iterator> IntoIterator for I` returned a copy,
and `for-of` called `into_iter()` on every operand. So `for let x of it` walked
a copy, and so did `for let x of &mut it`: a loop that stopped early left `it`
where it started, and the next read of `it` saw elements the loop had already
taken.

`core:http_client` met this first. A caller reads part of a body, stops, and
reads `response()` or the rest. It worked around it with `IterRefMut`, a
wrapper over `&mut I`, and an `impl IntoIterator for &mut X` for each iterator
type. The general `impl<I: Iterator> IntoIterator for &mut I` could not be
written, because a blanket impl over a reference does not dispatch
([WEP: Trait Resolution](./wep-2026-09-01-trait-resolution.md), "A ref blanket
never dispatches"). A user's own iterator had no workaround at all.

Rust avoids the copy by moving. `for x in it` moves `it` and a later read is an
error; `for x in &mut it` advances it in place, through
`impl Iterator for &mut I`. Rust also keeps its iterators from being `Copy`:
`Range` is only `Clone`, and clippy's `copy_iterator` lint flags the rest. The
new range types of RFC 3550 split the copyable range from its iterator for the
same reason. A copyable iterator is the trap this WEP closes.

## Decision

`for-of` walks an operand that is an `Iterator` as the loop's own state, with no
`into_iter()`:

- `for let x of it` consumes `it`, copyable or not. Reading `it` after the
  loop, or in its body, is a compile error that points at `&mut it`. Only a
  local the body owns can be consumed. A global, a place read through a
  reference, or a capture is refused, with the same pointer.
- `for let x of &mut it` advances `it` in place. A loop that stops early leaves
  the rest in `it`. A `&mut I` parameter walks the caller's iterator.
- `for let x of &it` is a compile error: a shared reference cannot advance an
  iterator.

An iterable that is not an iterator, a collection such as `List<T>`, is walked
through `IntoIterator` as before. Each loop starts from the beginning and leaves
the collection as it was.

The rule matches Rust where the two can be compared. It also makes affine and
copyable iterators behave alike: the affine one already moved into the loop.
`impl<affine I: Iterator> ForOf for I` is gone, since the loop walks an affine
iterator itself. `ForOf` stays for affine collections.

The loop only changes the caller's iterator through a `&mut` the caller wrote.
`let mut it` alone does not make `for let x of it` advance `it`. Mutating a
value without `&mut` would break that rule.

`IterRefMut` and the `&mut BodyChunks` and `&mut EventStream` impls are
removed.

## Known gaps

- A method that takes an iterator by value still copies a copyable one.
  `it.take(3)` followed by `it.next()` sees the elements `take` already read.
  Rust writes `it.by_ref().take(3)`, which rests on
  `impl Iterator for &mut I`, which in turn needs a blanket over a reference to
  dispatch.
- `for let [i, x] of it.enumerate()` passes `it` to `enumerate` by value, so it
  is the same gap as `take`, not a consumption the loop sees.
