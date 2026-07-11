# Iterator Reference Model

Align `List` iteration with Rust's `iter` / `iter_mut` / `into_iter`.

## Context

The friction driving this WEP is that Wado's iterator methods diverge from
Rust's, so Rust muscle memory misfires:

- `List::iter()` returns `Item = T` — it copies every element — the opposite of
  Rust, where `iter()` yields `&T`. So `list.iter().any(...)` silently copies.
- There is no `iter_mut`: `&mut List` yields `&T`, and elements are mutated by
  index instead.

Reference iteration exists only via `for-of &list` (`Item = &T`), not through a
named method a Rust reader reaches for. The fix is to match Rust's surface.
Inventing a _new_ divergence (e.g. making owned `for-of list` yield `&T`) would
only move the friction elsewhere, so it is out of scope.

## Decision

Match Rust's conventions exactly:

- `iter()` over `List<T>` yields `&T` (return `ArrayRefIter`, not `ArrayIter`).
- `iter_mut()` yields `&mut T` (new), enabling in-place mutation
  (`for let x of xs.iter_mut() { *x = f(*x); }`) — sound with no borrow checker,
  riding on the write-back model
  ([WEP: Reference Representation](./wep-2026-06-13-reference-representation.md)).
- `into_iter()` and owned `for let x of list` keep `Item = T` (by value),
  matching Rust's owned iteration. Unchanged.
- `for let x of &list` keeps `&T`. Unchanged.
- `copied()` for iterating a reference list by value: `nums.iter().copied()`.
  Implemented as an inherent `ArrayRefIter::copied() -> ArrayIter<T>` (both share
  the same backing), not a generic `Iterator` adaptor — a generic
  `impl<I: Iterator<Item = &T>, T>` can't yet resolve `I::Item` to `&T` in its
  body (associated-type-equality bounds aren't propagated), so `copied()` chains
  only directly off `iter()` today, not mid-chain after `filter()`/`map()`.

Rejected — flipping owned `for-of list` to `&T` (and dropping the value
iterator): Rust's owned `for x in v` yields values, so this would _add_ a new
Rust divergence, the opposite of the goal. It also turns snapshot-copy iteration
into live-reference iteration, a silent hazard when the body mutates the list.

Rejected — operator auto-deref: making `&T` read as `T` in operators is unsafe,
because `==` on `&T` today compares reference identity, not value (`&a == &b` is
`false` for distinct variables both holding `5`). Auto-deref would silently turn
every `&T == &T` into a value comparison and remove identity testing via `==`.
Primitives use `copied()` or `*x` instead. Reference value-vs-identity is a
separate proposal.

## Consequences

- Removes the `iter()`-copies footgun; `.iter()` now matches Rust (`&T`), and
  `iter_mut` / `copied` fill the remaining Rust-shaped gaps.
- The only breaking change is `iter()` copy → reference (~136 call sites; the
  subset with `|x: T|` closures or value-consuming bodies needs `|x: &T|` / `*x`,
  the rest survive via field/method auto-deref). Owned `for-of` is untouched, so
  no wide migration and no iterate-while-mutate hazard.
- Operator semantics untouched (`==` stays reference identity).

## Status

- [x] `iter()` yields `&T` (`List::iter -> ArrayRefIter`).
- [x] `copied()` (inherent on `ArrayRefIter`); regression fixture
      `iter_ref_adapter_monomorph.wado`.
- [x] Migrated the breaking `iter()` call sites (stdlib tests, fixtures,
      `package-gale`).
- [x] `&mut` iteration for in-place element types. `&mut List<T>` /
      `&mut Array<T>` yield `&mut T` via `ArrayRefMutIter` (`next` returns
      `&mut self.repr[index]`, the element's shared GC handle). Mutation lands on
      the backing list for `struct` / `List` / `String` / `i128` elements. Every
      write is immediate, so `break` / `continue` / `return` are sound with no
      write-back epilogue. Fixture `iter_mut_inplace.wado`.
- [x] Reject `&mut` iteration over a replace-on-assign element type (`primitive`
      / `enum` / `flags` / `variant` / `fn`) at the `for ... of &mut xs` site,
      naming the index-assignment workaround — otherwise the write is silently
      dropped (WEP-2026-06-13 D1). Fixture `iter_mut_forbidden.wado`.
- [x] Fixed a latent P0 exposed by `Item = &mut T`: `Fn<N,Ret>^Inspect::inspect`
      was synthesized once per return-type `TypeId`, but `&T` and `&mut T` mangle
      to the same `Fn` name, so the two collided post-monomorphization.
      `collect_canonical_fn_signatures` now dedups by the canonical mangled name.

## TODO

- [ ] `&mut` iteration for replace-on-assign element types (`primitive` / `enum`
      / `flags` / `variant` / `fn`): needs the reference write-back model
      (write-back to `xs[i]` on every loop-exit edge — WEP-2026-06-13); rejected
      for now rather than silently dropped.
- [ ] Public `iter_mut()` method: redundant with `for ... of &mut xs` until
      adapter chaining over `&mut T` composes; add it then.
- [ ] Generic `copied()` on any `Iterator<Item = &T>`: blocked on propagating
      associated-type-equality bounds (`Item = &T`) into the impl body.
