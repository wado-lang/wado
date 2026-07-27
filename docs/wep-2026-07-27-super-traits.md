# WEP: Super Traits

## Context

A trait cannot state that implementing it requires implementing another. Every
prerequisite is therefore unwritten, and Wado's monomorphize-everything model
turns each one into a post-monomorphization error reported in library code:

```wado
trait Circle {
    fn radius(&self) -> i32;
    fn describe(&self) -> String { return `r=${self.radius()} a=${self.area()}`; }
}
// A type implementing only `Circle`:
//   error: no method 'area' found on type 'Blob'   ← at Circle::describe
```

Neither the offending `impl Circle for Blob` nor the call site is named. The
same gap makes bounds redundant: `T: Ord` does not imply `T: Eq`, so
`core:prelude/range.wado` writes `impl<T: Eq + Ord> Eq for RangeExclusive<T>`,
and a generic body comparing with `==` under a bare `T: Ord` fails even when
every field of `T` is structurally `Eq` — the on-demand derivation is driven by
the declared bound, which never mentions `Eq`.

`Ord` and `Eq` are also independently implementable today, so a type can have a
working `<` and a missing `==`.

Background, including Rust's exact specification and the reflection-side uses:
[Research: Super Traits](./research-super-traits.md).

## Decision

### Syntax

Rust-compatible, minus `where`:

```wado
trait Ord: Eq {
    fn cmp(&self, other: &Self) -> Ordering;
}

trait Circle: Shape + Display {
    fn radius(&self) -> i32;
}
```

The clause sits between the trait's generic parameters and its block, and reuses
the existing `parse_trait_bounds` production, so `+` lists and associated-type
constraints (`trait A: B<Item = i32>`) come for free.

Wado has no `where` clause and does not gain one here. It costs no expressive
power, because every case Rust needs `where` for is either already spelled
inline or absent from the language:

| Rust `where` form                       | Wado                                              |
| --------------------------------------- | ------------------------------------------------- |
| `trait C where Self: S`                 | `trait C: S` — the two are equivalent in Rust     |
| `trait F<T> where T: Display`           | `trait F<T: Display>` — already supported         |
| bound on a trait's own associated type  | `type Item: Display;` — already supported         |
| bound on a supertrait's associated type | `trait A: B<Item: Display>` — supertrait position |
| `fn m(&self) where Self: Sized`         | dyn-compatibility escape hatch; Wado has no `dyn` |
| `where T: 'a`                           | Wado has no lifetimes                             |

A `fn(...)` / `fn mut(...)` bound is rejected in supertrait position: a callable
signature is not a trait a type can be required to implement.

### Obligation at the impl site

`impl Sub for T` requires `T: Super` for every direct supertrait of `Sub`,
reported at the impl block with the existing reason chains
([Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)). The
check runs in the phase that already hosts the orphan and sealed-trait checks,
and reuses `type_implements_trait`, which recognizes explicit impls, blanket
impls, and structural on-demand derivation alike. A plain struct therefore
satisfies `Ord: Eq` without an `impl Eq` being written.

This is what moves a missing prerequisite from a post-mono error in library code
to a declaration-site error naming the type at fault.

### Elaboration of declared bounds

A declared bound `T: Sub` expands to the transitive closure
`{Sub} ∪ supertraits*(Sub)` when generic parameters are registered, so every
downstream consumer sees the same set: bound checking, on-demand derivation
demand, and method-lookup candidates. Registering the derivation demand is what
closes the `Ord` / `Eq` gap above — `T: Ord` alone becomes sufficient for `==`.

Associated-type constraints written in supertrait position are implied, matching
Rust ≥ 1.72: `trait A: B<Item = C>` implies both `Self: B` and the projection
constraint. Deciding this now is deliberate — reversing it later is a
compatibility break.

Nothing else is implied. A bound on a generic parameter other than `Self` is not
a supertrait and does not elaborate.

### Cycles

The closure is computed once when the trait environment is built. A trait that
reaches itself is an error at its declaration.

### Name collisions

A subtrait method whose name collides with a supertrait method is an error at
the subtrait declaration. Rust instead defers to the use site and requires
`<T as Trait>::m` to disambiguate; Wado has no such syntax — `Trait::<Type>::m()`
resolves only for the sealed `Reflect*` traits, which are intercepted by name.
Rejecting at the declaration keeps the rule total without inventing syntax, and
can be relaxed to Rust's behaviour once a qualified form exists.

### Standard library adoption

`Ord: Eq` only, in this WEP. Every `impl Ord` in the stdlib already has a
matching `Eq`, including the variadic tuple impls, so the obligation is
satisfied everywhere on arrival; the redundant `Eq` in `impl<T: Eq + Ord>`
(`range.wado`) comes out.

The format-trait pairs (`InspectAlt: Inspect`, `DisplayAlt: Display`, the
`*Alt` hex/octal/exp family), `Fn: FnMut`
([Closure Implementation Internals](./wep-2026-01-25-closure-implementation-internals.md)
deferred it for want of this syntax), and the reflection uses in
[Research: Super Traits](./research-super-traits.md) are follow-ups, kept out so
the first landing stays reviewable.

## Consequences

Removing a supertrait from a published trait becomes a breaking change for
downstream code, because the bound was implied. Adding one is breaking for
implementors. This is the same trade Rust makes, and the reason a non-implied
bound is not.

`Iterator` becomes decomposable — `ExactSizeIterator` / `DoubleEndedIterator`
style splits are expressible for the first time — but nothing is split here.

Purely a front-end concept: no NIR, WIR, codegen, runtime, or code-size effect.

## Tasks

- [ ] `supertraits: Vec<TraitBound>` on `ast::TraitDecl`; parse the `:` clause in
      `parse_trait_decl`; reject `fn` bounds in that position
- [ ] `unparse` and the formatter round-trip the clause; formatter fixtures
- [ ] Supertrait closure + cycle detection when the trait environment is built
- [ ] Impl-site obligation check alongside the orphan / sealed checks, with a
      reason chain
- [ ] Bound elaboration at generic-parameter registration
- [ ] Subtrait / supertrait method-name collision error
- [ ] `trait Ord: Eq`; drop the redundant `Eq` from `impl<T: Eq + Ord>` sites
- [ ] `wado doc` / `query hover` surface the clause
- [ ] VS Code grammar regenerated if the syntax module changes
