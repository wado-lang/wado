# Iterator Reference Model

Reference-based iteration for storage-backed collections.

## Context

Rust's `iter` / `iter_mut` / `into_iter` split encodes ownership transfers
(borrow, mutable borrow, move) that Wado — value semantics, GC, no move, no
borrow checker — does not have. The only real axis left is share-a-reference vs
copy-a-value, a performance choice.

Today `List::iter()` returns `Item = T` (copies every element), inverting Rust's
convention where `iter()` yields `&T`. Reference iteration exists only via
`for-of &list` (`Item = &T`), and `&mut List` yields `&T`, not `&mut T`, so there
is no `iter_mut`. Iteration is small surface today, so this is a cheap time to
fix the model.

## Decision

Principle: borrow what exists in memory, produce by value what is synthesized. A
storage-backed collection yields `&T`; a computed sequence (`Range`, `chars()`)
yields `T`.

- `for-of` and `iter()` over `List<T>` yield `&T` (change `IntoIterator for
  List<T>` to `Item = &T`; `into_iter(&self)` already borrows, nothing is
  consumed). Drop the value-copying `List` iterator from the public API.
- `for let x of &mut list` / `iter_mut()` yield `&mut T`, adding in-place
  element mutation — sound with no borrow checker, riding on the write-back model
  ([WEP: Reference Representation](./wep-2026-06-13-reference-representation.md)).
- `next` / `find` / adaptor closures carry `&T`.
- Public surface: `for-of`, `iter()` (`&T`), `iter_mut()` (`&mut T`); `into_iter`
  is desugar-only.

Primitive ergonomics: struct iteration already works, since field access on `&T`
yields values (`x.field == target`). A primitive used directly in an operator
(`sum += x`, `x: &i32`) opts into value iteration via an explicit `copied()`
adaptor (`Item = &T` to `Item = T`), or `*x`.

Rejected — operator auto-deref: making `&T` read as `T` in operators is unsafe,
because `==` on `&T` today compares reference identity, not value (`&a == &b` is
`false` for distinct variables both holding `5`). Auto-deref would silently turn
every `&T == &T` into a value comparison and remove identity testing via `==`.
Reference value-vs-identity is a separate proposal, never a rider here.

## Consequences

- Removes the `iter()`-copies footgun; by-reference (cheap) is the default and
  matches a Rust reader's model of `.iter()`.
- Adds `iter_mut`. Operator semantics are untouched (`==` stays identity).
- Migration is small: `|x: T|` closures become `|x: &T|`; a loop body that
  rebinds the variable takes an explicit `*x` copy (same meaning); `iter()`
  changes copy → reference. Match ergonomics already handle `&T` scrutinees.

## TODO

- [ ] Confirm `&mut T` iteration composes with write-back (`next -> Option<&mut
      T>`), or defer `iter_mut`.
- [ ] Add `copied()`; check `String` / deep-copy element ergonomics.
- [ ] Migrate stdlib and `package-gale`; drop the value `ArrayIter` for `List`.
