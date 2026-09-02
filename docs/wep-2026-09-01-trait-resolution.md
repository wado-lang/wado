# Trait Resolution — One Order, Written Down

## Context

`docs/spec.md` says coherence guarantees that "for every `(Trait, Type)` pair,
there is at most one `impl Trait for Type` that can apply."

That is false, and the orphan rules do not make it true. Both impls below are
legal, local, and apply to `Point`:

```wado
impl<T: Limit> Describe for T { … }
impl<T: ReflectStruct> Describe for T { … }

struct Point { x: i32 }
impl Limit for Point { … }          // Point satisfies both bounds
```

Several impls applying is not a defect. A trait is meant to carry several value
blankets: `Inspect`, `Serialize` and `Deserialize` each derive over the four
reflection kinds. What the language needs instead is a _selection order_. One
exists. It was never written down.

Its rules were spread across four WEPs, with one recorded in none of them:

| Rule                                        | Recorded in    |
| ------------------------------------------- | -------------- |
| Non-variadic beats variadic                 | WEP 2026-03-14 |
| Variadic overlap is a definition-time error | WEP 2026-03-14 |
| A newtype's own impls select for it first   | WEP 2026-06-25 |
| Two traits, one method name, is ambiguous   | WEP 2026-07-31 |
| A `Reflect*` bound needs visible members    | WEP 2026-06-13 |
| Locality — a local impl beats a foreign one | **nowhere**    |

Locality is the one this WEP removes: see the order below.

Until now the only statement of the order was `select_trait_match`'s `sort_by`.
Issue #1932 shows what that costs. A rank was missing, so a newtype took its
base's blanket. Finding the fix meant reading the sort, because no document
said what the order was. A new rule about bare-`T` blankets then went into
"Variadic Type Parameters", the WEP that happened to hold the nearest rules.

## Decision

One order, stated here. Every other WEP that owns a rule keeps it and links here
for its place in the sequence. The implementation cites this document instead of
being the only place the order is written.

### Where the order applies

The order governs the trait-impl step of a _method call_ `recv.m(args)`. Method
lookup reaches it after two steps that are not part of it, and three other
dispatch paths select without it:

| Step or path                                  | What selects                                                                |
| --------------------------------------------- | --------------------------------------------------------------------------- |
| Inherent method (`impl Type { fn m }`)        | Shadows every trait method of that name, along the whole newtype chain      |
| A reference receiver's `&T` impls             | Ranked ahead of the base type's, concrete before blanket                    |
| Trait impls on the receiver                   | The order below                                                             |
| A type parameter's bounds (`T: Tr` in a body) | The first bound declaring the method; two or more is an ambiguity error     |
| `Type::m(args)` and other associated calls    | Current-module-first scan, first hit; value blankets on a separate fallback |
| Operators and indexing                        | The operand's type, unique-or-error (WEP 2026-07-31)                        |

Those paths agreeing with this order is a goal, not a fact: see Known gaps.

### The candidates

A call's candidates come from three places.

First, the impls whose target matches the receiver anywhere along its newtype
chain — impls written for the receiver's own type, for one instantiation of it
(`impl Tag for Box_<i32>`), or for its head (`impl<T> Tag for Box_<T>`).

Second, every _value blanket_ whose receiver-parameter bounds the receiver
satisfies. A value blanket is `impl<T: Bound> Tr for T`, and its receiver
parameter carries at least one bound: an unbounded `impl<T> Tr for T` names no
condition that could select it, so it is rejected where it is written rather
than accepted and never reached.

Third, every _reference blanket_ (`impl<T: Bound> Tr for &T`) whose bound a
reference receiver's referent satisfies. It ranks under a concrete `&T` impl
and over the base type's, which is rank 2 read one level up: written for the
reference, it is more specific than a blanket over it, and less specific than an
impl naming the container.

All three lists are then gated on scope, below.

Two impls of one `(Trait, Type)` pair are rejected where the second is written.
Coherence claims the pair cannot exist, no rank distinguishes them, and a
package compiles whole, so the check needs no open-world reasoning: the two are
the same key, in one module or in two of one package.

### Scope

A trait's methods are candidates at a call site only where that trait's
_declaration_ is in scope. Where the impl was written does not matter; impls
stay globally visible, and one in a module the caller never named still answers
once the trait is imported.

A declaration is in scope when the calling module declares it, imports it by
name or by alias, imports it through a `pub use` re-export, or when it is one of
the prelude's. The prelude is the only exemption in the language: every other
top-level symbol a module names must be imported, and a trait is no different.
`Display`, `Eq`, `Inspect`, `IntoIterator` and the operator traits therefore
carry no import burden and nothing else is auto-used. Importing a _type_ brings
none of the traits its impls mention.

A bound is a name like any other, so `T: Sub` requires `Sub` imported and
reaches `Sub`'s own methods. A supertrait is a second name: a body calling
`Base`'s method through `T: Sub` imports `Base` as well, exactly as it would to
write `T: Base`. The bound states which contract `T` satisfies, not which names
the body may leave unwritten.

Explicitness is the reason. A method call whose meaning depends on a module the
reader cannot see in the imports is a call the reader cannot resolve either. The
cost the earlier design feared — that a library adding a blanket becomes a
breaking change downstream — lands the other way without this gate: the blanket
reaches every receiver in the program, so adding one changes what downstream
calls mean, with no diagnostic. Scope removes the reach and the tie together.

Lookup keeps searching outside the scope, for the diagnostic alone. When the
scoped candidates are empty and the unscoped ones are not, the call is an error
naming the trait that would have answered and the import that would enable it:

```text
no method 'shout' on 'String' in scope: 'Loud' declares it and is not
imported here; add `use { Loud } from "./lib_a.wado"`
```

The unscoped search never selects. It runs only where the scoped one found
nothing, so a call that resolves is never second-guessed, and its result is a
message rather than a candidate.

### The order

Candidates are ranked:

0. **Variadic yields to non-variadic.** Within one trait at one argument list, a
   variadic impl (`impl<..T> Tr for [..T]`) is dropped when a non-variadic one is
   present. The rule stays inside one trait, so a foreign blanket for trait `A`
   never displaces a local variadic impl of trait `B` (WEP 2026-03-14 §5 Rule 1).

1. **The newtype before its base.** The search runs down the newtype chain and
   stops at the first level that answers. A newtype is a type distinct from its
   base, so everything written for it selects before anything written for the
   base (WEP 2026-06-25), and no later rank can reach past a level that
   answered. Inherent lookup already reads the chain this way; this is the same
   order for trait impls.

   Depth is the level a candidate is selected _at_, so it covers both shapes: an
   impl whose target is the newtype sits at 0 and one targeting the base at 1,
   and a blanket sits at the level its bounds hold at.

   A blanket's depth is measured over the whole derivation, not just its first
   step. Take `impl<T: Base> Derived for T` answering `T: Derived`. That bound
   sits at the depth the blanket's own bound holds at. A chained blanket
   therefore does not report the base's bound as the newtype's.

   The walk keeps the same subject throughout. If it reaches a `(type, trait)`
   pair twice, that bound grounds nothing, so the walk answers no. Dispatch's
   query answers yes to the same repeat, because it descends into members, where
   a repeat is the well-founded structural case.

2. **A concrete impl beats a blanket.** Within one level: an impl written for
   the receiver defines the exact function the call names, and a blanket only
   covers the general case. A foreign `impl Tr for Point` therefore beats a
   blanket written here. It never reaches across levels — rank 1 has already
   chosen one.

3. **Anything left is ambiguous.** Two shapes are reported, described below.
   Every other tie is settled by the order the candidates were collected in,
   which is a gap rather than a rule.

Every rank asks how a candidate relates to the receiver. Where the impl was
written is not one of them: two candidates that tie at rank 2 are ambiguous
whether or not one of them is in the calling module. Letting the reader's
vantage decide a program's meaning is what ranks 1 and 2 exist to avoid, and the
escape — an impl written for the receiver — is one line and says which body runs
to every reader.

The pattern this costs is "the library derives it, I override it for my types",
written as a second blanket rather than as an impl. Under this rule it reports
for exactly the types that satisfy both bounds. That is the strict side taken on
purpose, and it is not a permanent refusal: a concrete consumer of the pattern
is reason to reopen it.

Specificity is not a rank either. One blanket's bounds implying another's —
`impl<T: A + B>` beside `impl<T: A>`, or `impl<T: Ord>` beside `impl<T: Eq>`
under `Ord: Eq` — is rank 3, not a reason to prefer the narrower one. Which
associated-type bindings a caller sees when a narrower impl binds them
differently is the specialization soundness question, and answering it with a
rank would decide it by accident. The escape is the impl rank 2 puts above both.

One filter sits beside the order rather than in it. Arguments: one trait
declaration at several argument lists forms an overload set, and the call's
arguments choose (WEP 2026-07-31). Distinct traits never form one.

Two same-named trait declarations from different modules are distinct traits, so
Scope above settles them: each module's `s.shout()` dispatches to the `Loud`
imported there (`cross_module_same_name_foreign_impl.wado`), and a module
importing both gets the two-trait ambiguity below.

### The two ambiguities

The two are separate diagnostics because the programmer fixes them differently.

#### Two traits, one method name

The receiver has `impl Alpha for Item` and `impl Beta for Item`, both declaring
`describe`. They share no contract, so nothing selects. The call names the
trait: `Alpha::describe(&it)` (WEP 2026-07-31).

Blankets join the collision like any other candidate. Two traits' blankets
sharing a method name and both applying is the same question with no impl to
name, and Scope is what keeps it rare: both traits have to be imported here for
both to compete.

#### Two blankets of one trait

The receiver satisfies both bounds and nothing above ranks them. A blanket has
no name, so the call _cannot_ pin one. The only answer is an impl written for
the receiver, which rank 2 puts above both:

```text
ambiguous blanket impls of 'Describe' for 'Point': 'T: Limit' and
'T: ReflectStruct' apply, and nothing ranks them;
write 'impl Describe for Point'
```

The compiler reports this at the use site, the one place the question can be
answered. Rejecting the overlap where the impls are written would mean deciding
whether two bounds can both hold. An open world cannot decide that, since
another module may write `impl Limit` for a struct at any time. So nothing is
rejected at definition time. The standard library never reaches this rule: the
four reflection kinds are mutually exclusive, so no receiver satisfies two.

The report covers one trait declaration at one argument list. Two blankets of
_different_ traits are the two-trait ambiguity above.

### What eligibility is, and is not

Ranking runs over candidates, and a bound that does not hold produces no
candidate. Two gates decide that. They are not ranking rules:

- A `Reflect*` bound holds only where every member of the receiver is visible at
  the use site (WEP 2026-06-13). This keeps `TreeMap` out of a downstream
  `T: ReflectStruct` without naming it.
- A newtype inherits its base's impls for dispatch. So a blanket keyed by a bound
  only the base carries is still a candidate for the newtype. Rank 1 places it at
  depth 1 rather than excluding it.

A binder belongs to one item. Asking whether a name reaches a blanket's receiver
is therefore not the same as asking whether a name is spelled like it. A method
parameter may shadow the receiver's letter, and inside that method the letter is
the method's binder. Separately, all `impl` blocks in one module that bind a
parameter of one spelling share an index bucket, which is neither a declaration
nor a shape.

## How the order is guaranteed

The order above is a specification, and nothing holds the implementation to it
but a corpus of `.wado` fixtures. A fixture pins what a program prints, so it
proves the whole pipeline agreed on an answer; it does not pin the rule. A rank
that never fires, a candidate list that quietly loses a shape, a cycle rule only
one receiver kind exercises — none has a test that can name it. The rules
therefore move to where they can be stated and checked directly.

### The solver's input

Selection reads `TypeId`s, `DefId`s, `ImplHeader` AST nodes and the annotate-time
`Scope`. Every one is an index into something only the pipeline builds, so a test
that wants to ask a question has to write Wado source and run the compiler to ask
it. The solver takes one self-contained value instead:

```text
Program {
  types:  TypeDeclId  -> { kind, members: [(name, type, visibility)], module }
  traits: TraitDeclId -> { supertraits, methods, assoc_types, compiler_item }
  impls:  ImplId      -> { trait_, trait_args, target, params: [{ bounds }],
                           module, methods }
  scopes: ModuleId    -> { traits_in_scope }
}

SolverType = Decl(TypeDeclId, [SolverType]) | Param(u32) | Ref { mut, inner }
           | Tuple([SolverType])
```

`kind` names the shape a structural rule reads: struct, variant, enum, flags,
newtype over a base, primitive. Every id is a plain index, so a test writes
`TypeDeclId(0)` and the program around it, with no source and no pipeline.

A query carries an environment beside the program: the bounds in force where the
question was asked. A generic body's `T: Tr` holds because its own signature says
so, not because any impl exists, so no query can answer from `Program` alone.
This is rustc's `ParamEnv`, and it is a parameter from the start — the
annotate-time `Scope` field it replaces is mutable state threaded through the
elaborator, which is the shape hardest to retrofit later.

A query also carries the asking module. A `Reflect*` bound holds only where the
receiver's members are visible at the use site, and scope gates candidates by
what that module imported, so `Program` carries member visibility and no query is
a function of types alone.

### What `holds` answers from

Answering a bound is not an impl lookup. `T: Tr` holds nine ways: `Inspect`
holds for everything; a plain `enum` derives `Display`; a generic body's
parameter holds by its own signature; a reference satisfies the reference
identities; a primitive carries `Eq` / `Ord` and the operator items; a struct or
variant satisfies `Eq` / `Ord` / `Default` / serde when its members do; a
declaration satisfies its own reflection kind; an impl is written; or a blanket
answers and its bound is asked in turn.

The last two are the only recursive ones, and they are the solver's. The rest
are read off a type and are resolved by the lowering into facts the `Program`
carries — "this type satisfies `Eq` structurally", "this type is a
`ReflectStruct` when asked from this module".

The split is where the recursion is, not where the difficulty is. A fact is a
property of one type; an impl and a blanket are a search whose steps are other
questions of the same shape, and that search is what needs the cycle rule below.
Precomputing it is not an option either: whether a blanket applies is the
question, so a lowering that answered it would be running the solver.

A fact carries the derivation requests its answer depends on. "This struct
satisfies `Eq`" is also "emit `Eq` for this struct", and an answer that arrives
without the request loses the body.

Two things a bound can say are not carried yet: a trait's own arguments — no
bound can spell them (WEP 2026-07-31) — and an associated-type constraint
(`T: Mul<Output = T>`), which today gates a blanket separately.

### The four questions

| Function                                            | Rules it owns                                                                          |
| --------------------------------------------------- | -------------------------------------------------------------------------------------- |
| `coherence_errors(program)`                         | duplicate `(Trait, Type)` pairs, an unbounded value blanket, variadic overlap, orphans |
| `holds(program, env, ty, trait_)`                   | bound satisfaction, supertraits, the cycle rule                                        |
| `candidates(program, env, receiver, method, scope)` | the three candidate lists, the scope gate, each candidate's depth                      |
| `rank(candidates)`                                  | ranks 0-3 and both ambiguities                                                         |

Two disciplines keep them functions rather than passes:

- A diagnostic is returned, never emitted. `rank` answers with a winner or a
  description of the ambiguity by candidate, and the elaborator turns that into a
  message with the names only `DefTable` knows. An ambiguity becomes testable for
  the same reason.
- A derivation request is returned, never recorded. Answering a bound today
  writes "synthesize `Eq` for this type" into the `TypeTable` on the way past.
  The function reports the requests its answer depends on and the caller records
  them; a caller that drops one silently loses a derived body.

### Landing it

Nothing flips at once. The fixture corpus is the drift detector, as
`verify_arg_synthesis` already uses it for argument synthesis (WEP 2026-07-31):

- [x] `coherence_errors` and the impls half of `Program`, with unit tests.
- [x] Lower the impl headers into a `Program` and report what it finds. It has
      no path in use to differ against — the checks it answers did not exist —
      so it is authoritative from the start.
- [x] `holds`, with the impls, the traits' supertraits, the derivation facts and
      the bounds in force, and the cycle rule under unit test. Called by nothing
      yet.
- [ ] Lower the facts — every non-recursive way a bound holds, with the
      derivation requests each owes — and put `holds` under the differential
      against `type_implements_trait` over every fixture. Flip once they agree.
- [ ] `candidates`, then `rank`, the same way: unit tests first, then the
      differential, then the flip.

`coherence_errors` went first because it reads the shallowest part of `Program` —
impls alone, no bounds in force and no receiver — so it fixed the lowering's
skeleton at the smallest surface while closing two checks the Decision owed.

## Consequences

Selection is one ordered list. A new rule becomes a rank in that list, and a
missing rank is visible. Two calls that used to differ only by declaration order
now either agree or report, and the report names an impl the programmer can
write.

This document states the order and `select_trait_match` implements it. The two
must not diverge, so the sort cites this document instead of restating it.

## Known gaps

Three of these are one defect in several shapes: a candidate the order cannot
rank is dropped without a word, so which impl runs depends on load order,
declaration order, or which module the reader is in.

### Scope is decided and not implemented

Today a trait's methods are candidates wherever its impls are loaded. A trait
declaration's scope is consulted at one point only — several concrete candidates
naming several declarations, with exactly one of those in scope — so an
unimported trait's method still dispatches, an unimported blanket still reaches
every receiver, and two of them tie on collection order.

What the decision above needs:

- [ ] Gate both candidate lists on the trait declaration's scope at the call
      site, so a call in a module that imported nothing sees only the prelude.
- [ ] Keep the unscoped search as the recovery path: run it only where the
      scoped set came out empty, and turn its result into the "not imported
      here" message rather than a candidate.
- [ ] Gate the supertrait reach too: a body calling `Base`'s method through
      `T: Sub` resolves today with `Base` unimported, and must stop.
- [ ] Pin an aliased import (`use { Loud as L }`) and a `pub use` re-export as
      putting the trait in scope. Both follow from the rule and neither is
      exercised by a fixture.
- [ ] Pin that an impl in a module the caller never named still answers once
      the trait is imported. Impls stay global; scope gates the declaration.

An unused-trait-import warning belongs to
[Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md) rather than here,
with one constraint this decision imposes on it: a trait imported only to enable
a dispatch never appears in an expression, so the check must count enabling a
dispatch as a use. A check that reads the source alone will tell the programmer
to delete the import that makes the module compile.

### Two coherence rules still read the AST

`coherence_errors` owns four rules and answers two of them:

- [x] Two impls of one `(Trait, Type)` pair.
- [x] An unbounded `impl<T> Tr for T`.
- [ ] Variadic overlap, still `check_variadic_impl_overlap` over the AST. Moving
      it needs `Program` to carry a pack's bounds, which the key it compares on
      deliberately ignores (WEP 2026-03-14 §5 Rule 2).
- [ ] The orphan rule, still over the AST. Moving it needs each declaration's
      module and the package boundary, which `Program` does not carry yet.

### Rank 1 does not order the chain

Depth is read off a blanket's bounds alone, so every non-blanket candidate sits
at 0 and rank 1 separates none of them:

- [ ] A newtype's own `impl Tr for W` and its base's `impl Tr for Inner` tie, and
      the removed locality sort decides — written in one module the newtype's
      wins because the chain is collected nearest-first, but a local impl on the
      base beats a foreign one on the newtype
      (`trait_newtype_concrete_impl_outranks_foreign_base.wado`).
- [ ] A blanket satisfied at the newtype loses to a concrete impl on the base,
      because rank 2 runs first today
      (`trait_newtype_blanket_beats_base_concrete.wado`).

Closing both is one change: measure depth as the level a candidate is selected
at, over an impl's target as well as a blanket's bounds, and rank it above
concrete-over-blanket.

### A ref blanket never dispatches

`impl<T: Bound> Tr for &T` is accepted and reaches no call: the reference step
adopts only concrete `&T` impls, and the blanket collection takes only value
blankets. The prelude's `Inspect for &T` works because the compiler answers that
bound itself.

- [ ] Collect reference blankets as a third candidate list, ranked under a
      concrete `&T` impl and over the base type's
      (`trait_ref_blanket_dispatch.wado`).

### Two traits' blankets sharing a method name are not reported

`impl<T: Limit> Alpha for T` beside `impl<T: Limit> Beta for T`, both declaring
`describe`, both applying to the receiver: the removed locality sort answers when
exactly one is local, and otherwise collection order does. Blankets are excluded
from the cross-trait count, which is what makes this tie silent. Scope removes
most of it — two foreign blankets only compete where both traits are imported —
and the rest is the count: a blanket must join the collision like any other
candidate.

### An ungrounded bound cycle satisfies its own bounds

Write `impl<T: A> B for T` beside `impl<T: B> A for T` and neither trait is
grounded. The dispatch query still answers yes to a repeated `(type, trait)`
pair, so every type satisfies both, and a blanket keyed on either applies to
every receiver in the program. Rank 1's walk refuses the repeat, so a newtype is
unaffected. A concrete receiver has no such walk and reaches the shared answer.

Closing this needs a second recursion stack. One stack cannot separate the two
recursions: descent into members is well-founded and must answer yes, while a
cycle at a fixed subject must answer no. That is a change to dispatch, not to
selection, which is why it is recorded here rather than made.

### The other dispatch paths do not share the order

`Type::m(args)` scans the receiver's trait impls current-module-first and takes
the first that declares the method, with value blankets on a separate fallback
behind it; operators and indexing filter by operand type and report
unique-or-error; a bound in a generic body takes the first bound declaring the
name. Ranks 0-3 exist on none of them. They agree with the order in the cases
tested so far because the scans happen to visit candidates in a compatible
order, which is not a guarantee. One selection function serving every path is the
fix; what stands in the way is that each path holds a different amount of the
call (a receiver type, an operand class, a bound list).

### Locality is still implemented

The sort still prefers a candidate in the calling module, and the blanket
ambiguity report still treats local and foreign candidates as separate groups,
so a tie the order calls ambiguous is answered instead:

- [ ] Drop the local-over-foreign comparison from the sort.
- [ ] Drop the locality grouping from the blanket ambiguity report, so two
      blankets tied at rank 2 report whichever modules wrote them
      (`trait_error_local_blanket_ties_foreign.wado`).

Only one shape reaches this once the rest lands — two value blankets of one
trait holding at the same level, one written in the calling module — because a
duplicate pair is rejected where it is written, a newtype and its base are
separated by rank 1, and two traits' blankets are the cross-trait ambiguity.

### The structural derivation rules stay outside the unit tests

`holds` reads the derivation facts rather than deriving them, so "a struct whose
members all satisfy `Eq` satisfies `Eq`" is the lowering's rule and the fixture
corpus is what holds it. Moving it in later is an addition to `Program`, not a
rewrite of the solver; what it would buy is those rules under unit test, and
what it costs is every type's members and their visibility in the
representation.

### `spec.md` overstates coherence

Its "at most one impl can apply" describes what the orphan rules guarantee about
_where impls may be written_. It does not describe how many apply to a call. Its
"Method Resolution" section lists two steps of the six above. The selection order
is language semantics and belongs in the spec. This WEP records the decision;
writing the spec section is the follow-up.

## Related WEPs

What each contributes to the order above.

- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md) — rank 0
- [Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md) — rank 1
- [Overload Resolution](./wep-2026-07-31-overload-resolution.md) — arguments, and the two-trait ambiguity
- [Visibility](./wep-2026-06-25-visibility-internal-pub-export.md) — the `Reflect*` eligibility gate
- [Super Traits](./wep-2026-07-27-super-traits.md) — what a specificity rank would read
