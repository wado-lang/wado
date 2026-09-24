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

A `fn(...)` / `fn mut(...)` bound is rejected here: a callable signature is not a
trait a type can be required to implement.

### Obligation

`impl Sub for T` requires `T: Super` for every trait in `Sub`'s closure, not
only its direct supertraits — one satisfied structurally has no impl block of
its own to carry the rest of the chain. Reported at the impl block with a reason
chain. Structural on-demand derivation satisfies the obligation, so a plain
struct gets `Ord: Eq` without an `impl Eq` being written.

The impl answering that obligation is where the supertrait's members live, so
`impl Sub for T` neither owes nor may bind an associated type `Super` declares
([Associated Types](./wep-2026-01-20-associated-types.md)). A binding written in
the subtrait lands where no projection through `Super` reads it.

### Elaboration

A declared bound `T: Sub` expands to the transitive closure of `Sub` and its
supertraits, feeding bound checking, on-demand derivation, and method lookup
alike. The derivation half is what makes `T: Ord` alone sufficient for `==`.

The expansion happens where a bound is read, not where it is registered: a type
parameter's bounds stay as written, and the question "does `T` implement `Eq`?"
is what walks the closure. Registration sites are too many to keep in step.

A clause writes its arguments in the declaring trait's own parameter space, so
`trait Gauge<X>: Measure<X>` says `Measure` at whatever `Gauge` was asked at:
`T: Gauge<i32>` supplies `Measure<i32>`, and `impl Gauge<i32> for Ruler`
requires `Ruler: Measure<i32>`. A position the clause leaves out stands at the
supertrait's declared default, so a bound inherited through it arrives as a type
rather than as a parameter no later reader can resolve.

A clause may write `Self::Assoc` as an argument. `Self` there is the
implementing type, so `trait Constrained: Make<Self::Base>` asks each impl for
`Make` at the associated type that impl binds: an impl binding `Base` to
`String` owes `Make<String>`, and one binding it to `i32` owes `Make<i32>`. A
bound carries the projection with it, so `T: Constrained` requires
`T: Make<T::Base>`, and a call through it lands on the impl the projection
names ([Trait Resolution](./wep-2026-09-01-trait-resolution.md)).

The obligation an impl owes is read from what every impl on the target binds,
not from what the block under check happens to write. A clause reached through
another trait is therefore answered the same as one the block wrote itself: a
question about a type is answered by the type.

The closure is stored in the declaring trait's parameter space, which is not the
reading site's. Each clause therefore travels with the chain of clauses that
reaches it, every step written in the space of the step before. A reader carries
its own arguments down that chain, resolving each step in what the step before
it answered, and reads the clause in the space it arrives at. A reader that
takes a clause for one of its own bounds is the defect this forecloses. It fails
by resolving a parameter name the site never wrote, which nothing downstream can
detect.

Walking is what answers a projection, because no spelling denotes one:
`trait Sink<X: Src>: Collect<Item = X::Item>` read at `Sink<Feed>` binds `Item`
to what `Feed` binds it to. The walk settles `X` to `Feed` before anything reads
`X::Item`, so the base is a type by the time the projection is asked.

An associated-type constraint written in supertrait position
(`trait Sink: Collect<Item = i32>`) is checked: a type binding `Item = String`
is rejected wherever `T: Sink` is required, the same as if the constraint had
been written on the bound directly. It also answers the projection, so `T: Sink`
gives `T::Item = i32` with no annotation. A constraint naming the writer's own
parameter (`trait Sink<T>: Collect<Item = T>`) is read at the argument the bound
writes, `U: Sink<i32>` giving `U::Item = i32`.

### Cycles

A trait that reaches itself through supertraits is an error at its declaration.

### Name collisions

A method reachable through more than one of a receiver's bounds is an error
where it is called, not where the traits are declared — Rust's E0034. One rule
covers all three shapes: a subtrait shadowing a supertrait method, two
supertraits of a diamond sharing one, and a `<T: Left + Right>` written by hand.
Rejecting at the declaration would cover only the first, and leave the other two
resolving silently to whichever bound came first.

The escape is to name the trait: `Base::name(&x)`, the trait-qualified call
form from
[WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md).

### Standard library

`trait Ord: Eq` only. Every stdlib `impl Ord` already has a matching `Eq`, so the
obligation holds on arrival, and the redundant `Eq` in `impl<T: Eq + Ord>` comes
out.

## Known gaps

Two standard-library clauses are unwritten: a shared face over the `Reflect*`
traits, and the stdlib `Fn: FnMut` that
[Closure Implementation Internals](./wep-2026-01-25-closure-implementation-internals.md)
leaves open.

The trait solver states a clause's arguments only where it can name them as
types. A clause whose argument is the subtrait's own parameter states none
there, and the solver answers that edge at the supertrait's declared defaults.
It is lenient rather than wrong — the elaborator, which does carry the argument,
is what rejects a mismatch — so the two engines disagree on nothing a program
can reach. The solver's type language has no term for a trait's own parameter.

## Consequences

Removing a supertrait from a published trait breaks downstream code that relied
on the implied bound; adding one breaks implementors. This is the trade Rust
makes, and the reason a non-implied bound carries neither risk.

`Iterator` becomes decomposable — `ExactSizeIterator` / `DoubleEndedIterator`
style splits are expressible for the first time — though nothing is split here.

Front-end only: no NIR, WIR, codegen, runtime, or code-size effect.
