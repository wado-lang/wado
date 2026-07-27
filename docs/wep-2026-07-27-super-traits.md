# WEP: Super Traits

## Context

A trait cannot state that implementing it requires implementing another. Every
prerequisite is therefore unwritten, and since Wado monomorphizes everything,
each one surfaces as an error inside the library rather than at the impl that is
actually wrong:

```wado
trait Circle {
    fn radius(&self) -> i32;
    fn describe(&self) -> String { return `r=${self.radius()} a=${self.area()}`; }
}
// A type implementing only `Circle`:
//   error: no method 'area' found on type 'Blob'   ← reported at Circle::describe
```

The same gap makes bounds redundant — `T: Ord` does not imply `T: Eq`, so
`impl<T: Eq + Ord> Eq for RangeExclusive<T>` spells both — and lets `Ord` and
`Eq` be implemented independently, so a type can have a working `<` and a
missing `==`.

## Decision

### Syntax

Rust-compatible:

```wado
trait Ord: Eq {
    fn cmp(&self, other: &Self) -> Ordering;
}

trait Circle: Shape + Display {
    fn radius(&self) -> i32;
}
```

The clause sits between the trait's generic parameters and its block, and takes
the same bound list generic parameters do, so `+` lists and associated-type
constraints (`trait A: B<Item = i32>`) are included.

There is no `where` form. It would add no expressive power: `trait C where
Self: S` is Rust's own synonym for `trait C: S`, and every other `where` case is
either already spelled inline in Wado (bounds on generic parameters, on a
trait's own associated type, on a supertrait's) or concerns a feature Wado does
not have (`dyn`, lifetimes).

A `fn(...)` / `fn mut(...)` bound is rejected here: a callable signature is not a
trait a type can be required to implement.

### Obligation

`impl Sub for T` requires `T: Super` for every direct supertrait of `Sub`,
reported at the impl block with a reason chain. Structural on-demand derivation
satisfies the obligation, so a plain struct gets `Ord: Eq` without an `impl Eq`
being written.

### Elaboration

A declared bound `T: Sub` expands to the transitive closure of `Sub` and its
supertraits, feeding bound checking, on-demand derivation, and method lookup
alike. The derivation half is what makes `T: Ord` alone sufficient for `==`.

Associated-type constraints written in supertrait position are implied, matching
Rust ≥ 1.72: `trait A: B<Item = C>` implies both `Self: B` and the projection
constraint. Nothing else is — a bound on a generic parameter other than `Self`
is not a supertrait.

### Cycles

A trait that reaches itself through supertraits is an error at its declaration.

### Name collisions

A subtrait method whose name collides with a supertrait method is an error at
the subtrait declaration. Rust instead defers to the use site and requires
`<T as Trait>::m`; Wado has no qualified form, so rejecting at the declaration
keeps the rule total. It can be relaxed once such syntax exists.

### Standard library

`trait Ord: Eq` only. Every stdlib `impl Ord` already has a matching `Eq`, so the
obligation holds on arrival, and the redundant `Eq` in `impl<T: Eq + Ord>` comes
out.

The format-trait pairs (`InspectAlt: Inspect`, `DisplayAlt: Display`, and the
rest of the `*Alt` family), `Fn: FnMut` — which
[Closure Implementation Internals](./wep-2026-01-25-closure-implementation-internals.md)
deferred for want of this syntax — and a shared face over the `Reflect*` traits
are follow-ups.

## Consequences

Removing a supertrait from a published trait breaks downstream code that relied
on the implied bound; adding one breaks implementors. This is the trade Rust
makes, and the reason a non-implied bound carries neither risk.

`Iterator` becomes decomposable — `ExactSizeIterator` / `DoubleEndedIterator`
style splits are expressible for the first time — though nothing is split here.

Front-end only: no NIR, WIR, codegen, runtime, or code-size effect.

## Tasks

- [x] Parse the clause onto `ast::TraitDecl`; reject `fn` bounds in it
- [x] `unparse` and the formatter round-trip it; formatter fixtures
- [x] Supertrait closure and cycle detection
- [x] Impl-site obligation check with a reason chain
- [x] Bound elaboration
- [x] Subtrait / supertrait method-name collision error
- [x] `trait Ord: Eq`; drop the redundant `Eq` from `impl<T: Eq + Ord>` sites
- [x] `wado doc` / `query hover` surface the clause

Bounds elaborate where they are read, not where they are registered: a type
parameter's declared bounds stay as written, and the question "does `T`
implement `Eq`?" expands through the closure. The requirement side is left
alone — the impl obligation already carries supertraits transitively.
