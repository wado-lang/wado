# Research: Super Traits

Background for a Wado WEP on super traits (`trait Ord: Eq`). Covers Rust's exact
specification, Wado's current state, and what super traits are expected to buy.

## Wado Today

Wado has no super trait mechanism at any layer:

- `ast::TraitDecl` (`wado-compiler/src/ast.rs:3422`) has `type_params`,
  `associated_types`, and `methods` — no bounds field.
- `parse_trait_decl` (`wado-compiler/src/parser.rs:5708`) expects `{` straight
  after the generic parameter list. `trait A: B { … }` is a parse error
  (`expected LBrace, found Colon`).
- Bounds are stored as `Vec<TraitBound>` (name + associated-type constraints)
  and checked by name at instantiation sites via
  `type_implements_trait` (`elaborator/trait_query.rs:578`). There is no
  elaboration step: `T: Ord` never yields `T: Eq`.
- `impl Trait for Type` carries no obligation beyond the trait's own methods.

Two consequences are observable today.

### 1. Bounds must be spelled out redundantly

`core:prelude/range.wado:195` already writes what a super trait would imply:

```wado
impl<T: Eq + Ord> Eq for RangeExclusive<T> { … }
```

Empirically, dropping `Eq` breaks compilation. Given
`fn eqq<T: Ord>(a: &T, b: &T) -> bool { return *a == *b; }`, `wado compile`
reports, at the `==` in the generic body:

```
error: type `P` does not implement trait `Eq`
```

even for a struct whose fields are all structurally `Eq`. The on-demand
`Eq` derivation is driven by the declared bound, so `T: Ord` alone does not
request it. Writing `T: Ord + Eq` compiles, and correctly rejects an
ineligible type with a reason chain.

### 2. Missing prerequisites surface as post-monomorphization errors

Wado resolves trait method calls against the concrete type after
monomorphization, so a trait default body can already call a _different_
trait's method on `Self` with no super trait declared:

```wado
trait Shape { fn area(&self) -> i32; }

trait Circle {
    fn radius(&self) -> i32;
    fn describe(&self) -> String { return `r=${self.radius()} a=${self.area()}`; }
}
```

This compiles and runs for any type implementing both. But for a type
implementing only `Circle`, the diagnostic lands on the trait's default body:

```
t6.wado:7:1: error: no method 'area' found on type 'Blob'
```

The impl that is actually wrong (`impl Circle for Blob`) and the call site are
both unmentioned. This is the C++-template failure mode: the error is reported
in library code, against a type the library never named.

So Wado already has the _capability_ half of super traits for free
(monomorphization gives supertrait-item access without declaring anything).
What is missing is the _contract_ half: declaring the requirement, checking it
at the impl site, and propagating it through bounds.

## Rust's Specification

### Definition

> Supertraits are traits that are required to be implemented for a type to
> implement a specific trait. Furthermore, anywhere a generic or trait object is
> bounded by a trait, it has access to the associated items of its supertraits.
> — [Rust Reference, Traits](https://doc.rust-lang.org/reference/items/traits.html)

A trait with a supertrait is a **subtrait** of it. Formally, supertraits are
**trait bounds on the `Self` type of a trait**, plus transitively the supertraits
of those traits.

### Declaration forms

Two syntaxes, exactly equivalent:

```rust
trait Circle: Shape { fn radius(&self) -> f64; }
trait Circle where Self: Shape { fn radius(&self) -> f64; }
```

Only bounds on `Self` count. A `where T: Bound` clause on some other parameter
is not a supertrait and is not elaborated.

Anything that can bound `Self` participates: other traits, `Sized`, lifetime
bounds (`trait Foo: 'static`), and generic trait applications
(`trait Foo: Into<String>`).

### Obligation on implementors

Implementing the subtrait requires a separate impl of every supertrait. There is
no inheritance of items — `impl Circle for UnitCircle` does not provide
`Shape::area`; a distinct `impl Shape for UnitCircle` must exist. A missing
supertrait impl is `E0277` reported **at the subtrait impl**, not at a use site.

The compiler proves the trait reference is well-formed by discharging all direct
super-bounds; `WellFormed` is co-inductive for traits, so cycles through
associated-type constraints are accepted.

### Elaboration and implied bounds

Rustc **elaborates** an environment: from `T: C`, with `trait C: B` and
`trait B: A`, it derives `T: B` and `T: A` to a fixed point. So `fn f<T: Ord>()`
may rely on `T: PartialOrd`, `T: Eq`, `T: PartialEq` without restating them.

Chalk models the same thing as global clauses
(`forall<T> { FromEnv(T: A) :- FromEnv(T: B) }`) that fire only against
`FromEnv` facts, keeping the rule local and non-circular.

Boundaries of what is implied:

- **Implied:** direct and transitive supertrait bounds (bounds on `Self`).
- **Implied since Rust 1.72** ([PR 112629](https://github.com/rust-lang/rust/pull/112629)):
  associated-type bounds written in supertrait position. `trait A: B<Assoc: C>`
  now implies both `Self: B` and `<Self as B>::Assoc: C`.
- **Not implied:** ordinary `where` clauses that do not bound `Self`; bounds on
  associated types written in a separate `where` clause rather than in
  supertrait position ([issue 85978](https://github.com/rust-lang/rust/issues/85978)).
- Implied bounds are still not fully general in rustc: only outlives
  requirements, supertrait bounds, and associated-type bounds participate
  ([RFC 2089](https://rust-lang.github.io/rfcs/2089-implied-bounds.html) remains
  unimplemented in full).

A deliberate consequence: because a supertrait bound is implied, **removing** it
from a trait is a breaking change for downstream code, whereas removing a
non-implied bound is not.

### Item access and ambiguity

A generic bounded by the subtrait can call supertrait methods and name
supertrait associated types/consts:

```rust
fn print_area_and_radius<C: Circle>(c: C) {
    println!("{} {}", c.area(), c.radius());   // area() comes from Shape
}
```

If subtrait and supertrait declare the same method name, the call is ambiguous
and needs a qualified path: `<C as Shape>::area(&c)`. Rust's fully qualified
syntax `<Type as Trait>::item` is the general disambiguator and also the
canonical path for trait impl items.

### Restrictions

- **No cycles.** A trait may not be its own supertrait, directly or
  transitively (`E0391`).
- **Dyn compatibility.** `dyn Sub` requires every supertrait to be
  dyn-compatible too. A trait cannot use `Self` as a type argument to a
  supertrait (`trait WithSelf: Super<Self>`) in a context requiring dyn
  compatibility.
- **Trait upcasting** (`&dyn Sub` → `&dyn Super`, and the same through `Box`,
  `Arc`, `*const`) was stabilized in
  [Rust 1.86](https://blog.rust-lang.org/2025/04/03/Rust-1.86.0/) (2025-04-03).
  Before that, traits needed a hand-written `fn upcast(&self) -> &dyn Super`.

### Rust's own uses

`Ord: Eq + PartialOrd<Self>`, `Eq: PartialEq<Self>`, `Copy: Clone`,
`ExactSizeIterator: Iterator`, `DoubleEndedIterator: Iterator`,
`Error: Debug + Display`, `Fn: FnMut: FnOnce`.

## Expected Gains for Wado

### 1. Impl-site checking instead of post-mono errors

The strongest win, and the one that does not overlap with anything Wado has.
`trait Circle: Shape` moves the diagnostic from "no method `area` on `Blob`",
reported inside `Circle::describe`, to "`impl Circle for Blob` requires
`impl Shape for Blob`", reported at the impl. Wado already builds reason chains
for structural derivation failures (`elaborator/trait_query.rs:631`,
`docs/wep-2026-06-02-diagnostic-reason-chains.md`); super traits extend the same
locality guarantee to hand-written traits.

This matters more in Wado than in Rust, because Wado monomorphizes everything
and has no separate generic type-check pass — today _every_ prerequisite
violation is a post-mono error.

### 2. Bound elaboration removes redundant `+` lists

`impl<T: Eq + Ord>` becomes `impl<T: Ord>` once `Ord: Eq` holds. Concrete sites
in the stdlib today: `range.wado:195`, `range.wado:201`, `collections.wado:550`
and `:569` (`K: Ord + Inspect`, where only `Ord` is semantically load-bearing
for the map and `Inspect` for the element), `slice.wado` (`Step + Ord`).

More importantly, elaboration is what makes on-demand derivation fire: a `T: Ord`
bound would register the `Eq` demand automatically, closing the gap in §1 of
"Wado Today" without the author noticing there was one.

### 3. `Ord` and `Eq` stop being independent

Today `impl Ord for Q` compiles for a type that can never be `Eq` (e.g. one with
a closure field). The result is a type where `<` works and `==` does not — a
partial comparison surface with no diagnostic until someone writes `==`. With
`Ord: Eq`, the incoherent state is unrepresentable, matching the `Ordering`
contract the trait already documents.

Same argument, smaller stakes, for the format-trait pairs
(`InspectAlt`/`Inspect`, `DisplayAlt`/`Display`, `LowerHexAlt`/`LowerHex`, …) and
for `LenientFromStr`/`FromStr` if the lenient grammar is meant to be a superset.

### 4. Decomposing the monolithic `Iterator`

`core:prelude/traits.wado`'s `Iterator` is ~350 lines with one required method
and everything else defaulted. Rust splits off `ExactSizeIterator`,
`DoubleEndedIterator`, and `FusedIterator` as subtraits; Wado cannot express
that shape at all. Super traits are the prerequisite for any such split, and for
letting adapters (`rev`, `len`) require the extra capability in their bound
rather than trapping or being absent.

### 5. Unblocks the deferred `Fn`/`FnMut` traits

`docs/wep-2026-01-25-closure-implementation-internals.md` explicitly deferred
re-introducing prelude `Fn`/`FnMut` because "Wado's parser does not yet support
a supertrait clause on trait declarations", and shipped a direct
`check_assignable` rule instead. Super traits let `Fn: FnMut` be stated where it
belongs, which is what user-defined callable types would need.

### 6. Library-author expressiveness and documentation

`trait X: Y` is the single most compact way to say "Y is part of X's contract".
It flows into `wado doc`, `wado query hover` (which already lists a type's impl
blocks), and LSP completion — a `T: Circle` receiver can be offered `Shape`'s
methods with a declared justification rather than by guessing.

## Non-Gains

Being explicit about what super traits will _not_ deliver in Wado:

- **No dyn upcasting.** Wado has no dynamic dispatch (`dyn Trait` is listed under
  "Not Yet Implemented" in `docs/spec.md`), so the entire Rust 1.86 upcasting
  story, and the dyn-compatibility restrictions around `Super<Self>`, are
  out of scope.
- **No new call capability.** Monomorphization already lets default bodies and
  bound-generic code call methods that a super trait would have authorized.
  Super traits make those calls _checked_, not _possible_.
- **No runtime or code-size effect.** Purely a front-end concept; NIR/WIR and
  codegen are untouched.

## Open Design Questions

For the WEP, not settled here:

1. **Syntax.** `trait A: B` (Rust) versus `extends`, which Wado already uses for
   resource inheritance (`docs/wep-2026-04-28-resource-inheritance.md`, which
   anticipates "a separate WEP could later introduce trait supertraits with a
   different syntax, e.g. `:`"). The `:` form collides with nothing in
   `parse_trait_decl` today.
2. **Where the obligation is discharged.** At `impl Sub for T`, requiring
   `impl Super for T` to exist — including via structural on-demand derivation
   (`Eq`) and blanket impls.
3. **Elaboration scope.** Whether `T: Sub` elaborates for (a) bound checking
   only, (b) on-demand derivation demand registration, (c) method lookup
   candidate sets. (b) is where the current `Ord`/`Eq` gap lives.
4. **Retrofitting `Ord: Eq`.** Whether every existing `impl Ord` in the stdlib
   and in fixtures has a corresponding `Eq`, and whether the auto-derive path
   satisfies the new obligation.
5. **Effects.** `docs/wep-2026-01-27-effect-system-design.md` calls effect
   propagation "an automatic, signature-derived form of supertrait". Whether
   `effect` declarations should get an explicit super-effect clause with the
   same machinery, or stay signature-derived.
6. **Cycle detection.** Where in the pipeline (`TraitEnv::build`) the supertrait
   graph is closed and checked for cycles.
7. **Ambiguity rule.** Wado currently makes a same-named method in two traits a
   compile error and has no `<Type as Trait>::method` syntax. A subtrait
   shadowing a supertrait method needs either a resolution rule or the qualified
   syntax.

## Sources

- [Rust Reference — Traits](https://doc.rust-lang.org/reference/items/traits.html)
- [Rust Reference — Qualified paths](https://doc.rust-lang.org/reference/paths.html#qualified-paths)
- [Chalk Book — Implied bounds](https://rust-lang.github.io/chalk/book/clauses/implied_bounds.html)
- [RFC 2089 — Implied bounds](https://rust-lang.github.io/rfcs/2089-implied-bounds.html)
- [rust-lang/rust#112629 — Associated type bounds in supertrait position are implied](https://github.com/rust-lang/rust/pull/112629)
- [rust-lang/rust#85978 — Bounds on associated types of supertraits are not implied](https://github.com/rust-lang/rust/issues/85978)
- [Announcing Rust 1.86.0 — trait upcasting](https://blog.rust-lang.org/2025/04/03/Rust-1.86.0/)
