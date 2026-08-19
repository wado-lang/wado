# WEP: Iterator Reference Model

What a sequence iterator yields — values, shared references, or mutable ones —
and which of the three a given syntax or method selects.

## Context

`List::iter()` returned `Item = T`, copying every element, so
`list.iter().any(…)` copied silently. Reference iteration existed only through
`for-of &list`, reachable by no named method, and there was no way to mutate in
place except by index.

## Decision

Iteration comes in three axes, and every iterator yields exactly one of them:

| Axis    | Item     | Method           | `for-of` form      |
| ------- | -------- | ---------------- | ------------------ |
| value   | `T`      | `iter_value()`   | `for x of xs`      |
| shared  | `&T`     | `iter_ref()`     | `for x of &xs`     |
| mutable | `&mut T` | `iter_ref_mut()` | `for x of &mut xs` |

Reading a sequence yields references by default — `for x of &xs` and
`iter_ref()` — and a caller wanting owned elements says so. The names spell the
axis out rather than following Rust's `iter` / `iter_mut` / `into_iter`, because
Wado's semantics differ enough that the Rust names would mislead: see
[The Sequence Family](./wep-2026-06-02-sequence-family.md), which owns the
naming rule and the iterator type names.

`SliceRefIter::iter_value()` drops a reference iterator to values, standing in
for Rust's `copied()`. It is inherent rather than a generic `Iterator` adaptor —
a generic `impl<I: Iterator<Item = &T>, T>` cannot yet resolve `I::Item` to `&T`
in its body — so it chains only directly off `iter_ref()`, not after a
`filter()` or `map()`.

`&mut T` iteration is sound without a borrow checker because it rides the
write-back model
([Reference Representation](./wep-2026-06-13-reference-representation.md)). It
is available only for elements mutated in place: a replace-on-assign element
(`primitive` / `enum` / `flags` / `variant` / `fn`) has no addressable cell, so
a write through the reference would be lost, and the compiler rejects it.

Rejected — making owned `for x of xs` yield `&T` and dropping the value
iterator. It turns snapshot-copy iteration into live-reference iteration, a
silent hazard when the body mutates the sequence.

Rejected — operator auto-deref. `==` on `&T` compares reference identity, so
auto-deref would silently turn every `&T == &T` into a value comparison and
remove identity testing through `==`. Use `*x`. Reference value-vs-identity is a
separate proposal.

## Consequences

- Which iterator a call site gets is legible from its name, and the copying
  default is gone.
- No unmarked spelling survives, so every migration is mechanical but wide.
- Operator semantics are untouched: `==` stays reference identity.

## TODO

- [ ] `&mut` iteration for replace-on-assign element types: needs write-back to
      `xs[i]` on every loop-exit edge (WEP-2026-06-13). Rejected for now rather
      than silently dropped; fixture `iter_mut_forbidden.wado` pins the error.
- [ ] Generic `iter_value()` on any `Iterator<Item = &T>`: blocked on
      propagating associated-type-equality bounds (`Item = &T`) into the impl
      body.
