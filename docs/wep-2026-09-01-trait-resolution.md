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
| A reference receiver's concrete `&T` impl     | Adopted ahead of the base type's impl; a _blanket_ `&T` impl never is       |
| Trait impls on the receiver                   | The order below                                                             |
| A type parameter's bounds (`T: Tr` in a body) | The first bound declaring the method; two or more is an ambiguity error     |
| `Type::m(args)` and other associated calls    | Current-module-first scan, first hit; value blankets on a separate fallback |
| Operators and indexing                        | The operand's type, unique-or-error (WEP 2026-07-31)                        |

Those paths agreeing with this order is a goal, not a fact: see Known gaps.

### The candidates

A call's candidates come from two places.

First, the impls whose target matches the receiver anywhere along its newtype
chain — impls written for the receiver's own type, for one instantiation of it
(`impl Tag for Box_<i32>`), or for its head (`impl<T> Tag for Box_<T>`).

Second, every _value blanket_ whose receiver-parameter bounds the receiver
satisfies. A value blanket is `impl<T: Bound> Tr for T`: its receiver parameter
must carry at least one bound. `impl<T> Tr for T` binds nothing that could
select it, so it is not a value blanket and reaches no receiver.

A blanket over a reference (`impl<T: Bound> Tr for &T`) is a third shape and is
on neither list. The prelude's `Inspect` and `Eq` reach one through the
compiler's own bound handling; nothing else does.

### The order

Candidates are ranked:

0. **Variadic yields to non-variadic.** Within one trait at one argument list, a
   variadic impl (`impl<..T> Tr for [..T]`) is dropped when a non-variadic one is
   present. The rule stays inside one trait, so a foreign blanket for trait `A`
   never displaces a local variadic impl of trait `B` (WEP 2026-03-14 §5 Rule 1).

1. **A concrete impl beats a blanket.** An impl written for the receiver defines
   the exact function the call names. A blanket only covers the general case.
   This rank outranks locality: a foreign `impl Tr for Point` beats a local
   `impl<T: B> Tr for T`. It also outranks rank 2, so the base's own impl beats a
   blanket the newtype satisfies.

2. **A shallower bound beats a deeper one.** This ranks _blankets_ on a newtype
   receiver: a blanket whose bounds hold at the newtype itself beats one that
   holds only after peeling to the base. A newtype is a type distinct from its
   base, so its own impls select for it first (WEP 2026-06-25). This rank
   outranks locality, because it asks which type selected the impl rather than
   where the impl was written.

   The depth is measured over the whole derivation, not just its first step.
   Take `impl<T: Base> Derived for T` answering `T: Derived`. That bound sits at
   the depth the blanket's own bound holds at. A chained blanket therefore does
   not report the base's bound as the newtype's.

   The walk keeps the same subject throughout. If it reaches a `(type, trait)`
   pair twice, that bound grounds nothing, so the walk answers no. Dispatch's
   query answers yes to the same repeat, because it descends into members, where
   a repeat is the well-founded structural case.

   Depth is a property of a blanket's bounds, so every non-blanket candidate
   sits at depth 0 and this rank does not separate two of them. Which of a
   newtype's and its base's concrete impls a call takes is therefore decided by
   ranks 3 and 4 instead — a gap, not the intent.

3. **A local impl beats a foreign one.** When candidates are otherwise equal, the
   one in the calling module wins. This is the last tie-break, and it is weaker
   than the rest. Every rank above asks how the impl relates to the receiver.
   This one asks only where the reader is standing.

   It separates local from foreign and goes no finer. Two blankets in two foreign
   modules stay tied and fall to rank 4. Ordering them by which module wrote them
   would just be declaration order under another name.

4. **Anything left is ambiguous.** Two shapes are reported, described below.
   Every other tie is settled by the order the candidates were collected in,
   which is a gap rather than a rule.

Two filters sit beside the order rather than in it:

- **Scope.** Two same-named trait declarations from different modules are
  distinct traits. When the concrete candidates name several declarations and
  exactly one of them is in scope at the call site, the others are dropped: each
  module's `s.shout()` dispatches to the `Loud` in scope there
  (`cross_module_same_name_foreign_impl.wado`). With none or several in scope the
  filter does not fire and the ambiguity rules see every candidate.
- **Arguments.** One trait declaration at several argument lists forms an
  overload set, and the call's arguments choose (WEP 2026-07-31). Distinct
  traits never form one.

### The two ambiguities

The two are separate diagnostics because the programmer fixes them differently.

#### Two traits, one method name

The receiver has `impl Alpha for Item` and `impl Beta for Item`, both declaring
`describe`. They share no contract, so nothing selects. The call names the
trait: `Alpha::describe(&it)` (WEP 2026-07-31).

Counted over concrete candidates only. A blanket does not join the collision,
because a foreign `impl<T: Bound> Foreign for T` reaches every receiver in the
program and counting it would make adding a blanket to a library a breaking
change for every downstream method of that name.

#### Two blankets of one trait

The receiver satisfies both bounds and nothing above ranks them. A blanket has
no name, so the call _cannot_ pin one. The only answer is an impl written for
the receiver, which rank 1 puts above both:

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
_different_ traits sharing a method name fall under neither diagnostic.

### What eligibility is, and is not

Ranking runs over candidates, and a bound that does not hold produces no
candidate. Two gates decide that. They are not ranking rules:

- A `Reflect*` bound holds only where every member of the receiver is visible at
  the use site (WEP 2026-06-13). This keeps `TreeMap` out of a downstream
  `T: ReflectStruct` without naming it.
- A newtype inherits its base's impls for dispatch. So a blanket keyed by a bound
  only the base carries is still a candidate for the newtype. Rank 2 places it at
  depth 1 rather than excluding it.

A binder belongs to one item. Asking whether a name reaches a blanket's receiver
is therefore not the same as asking whether a name is spelled like it. A method
parameter may shadow the receiver's letter, and inside that method the letter is
the method's binder. Separately, all `impl` blocks in one module that bind a
parameter of one spelling share an index bucket, which is neither a declaration
nor a shape.

## Consequences

Selection is one ordered list. A new rule becomes a rank in that list, and a
missing rank is visible. Two calls that used to differ only by declaration order
now either agree or report, and the report names an impl the programmer can
write.

This document states the order and `select_trait_match` implements it. The two
must not diverge, so the sort cites this document instead of restating it.

## Known gaps

Four of these are one defect in several shapes: a candidate the order cannot
rank is dropped without a word, so which impl runs depends on load order,
declaration order, or which module the reader is in.

### Duplicate impls of one `(Trait, Type)` pair are accepted

Two `impl Tr for S` blocks in one module, or in two modules of one package, both
compile. Nothing ranks them — they agree at every rank — so collection order
decides, and swapping two `use` statements changes which body runs. This is the
pair spec.md's coherence claim says cannot exist, and a package compiles whole,
so rejecting it needs no open-world reasoning. Closing it is a definition-time
check, not a rank.

### Two traits' blankets sharing a method name are not reported

`impl<T: Limit> Alpha for T` beside `impl<T: Limit> Beta for T`, both declaring
`describe`, both applying to the receiver: rank 3 answers when exactly one is
local, and otherwise collection order does. Excluding blankets from the
cross-trait count is what keeps adding a blanket from breaking downstream code
(WEP 2026-07-31), and it is also what makes this tie silent. Reporting it needs
the two rules reconciled: either the tie is an error with `Alpha::describe(&x)`
as the escape, or candidates are scoped by import and the tie stops arising.

### Rank 2 does not reach concrete impls

A newtype's own `impl Tr for W` and its base's `impl Tr for Inner` both sit at
depth 0, so rank 3 decides: written in one module the newtype's wins (the chain
is walked nearest-first), but a local impl on the base beats a foreign one on the
newtype. WEP 2026-06-25's "a newtype most of all" is therefore true only for
blankets. Closing it means measuring depth over the impl's target as well as
over a blanket's bounds.

### An unbounded value blanket applies to nothing

`impl<T> Tr for T` compiles and dispatches to no receiver — the call reports the
method as missing. The orphan rule forbids the shape for a foreign trait, so only
a local trait can reach it, which is why it has gone unnoticed. Either the
receiver parameter's bound is required where the impl is written, or the impl
applies to every type.

### A ref blanket never dispatches

`impl<T: Bound> Tr for &T` is accepted and reaches no call: the reference step
adopts only concrete `&T` impls, and the blanket collection takes only value
blankets. The prelude's `Inspect for &T` works because the compiler answers that
bound itself. Either the shape is rejected where it is written, or the reference
step ranks blanket `&T` impls below concrete ones and above the base type's.

### Specificity is not ranked

Sometimes one blanket's bounds imply another's. An order exists there and goes
unused. `impl<T: Ord>` is strictly narrower than `impl<T: Eq>` when `Ord: Eq`
(WEP 2026-07-27), and `impl<T: A + B>` is narrower than `impl<T: A>`. Both are
decidable from the trait declarations. Today such a pair is reported ambiguous.

This is left open on purpose, for two reasons. First, the rule would not fire:
no standard-library trait carries two blankets with comparable bounds, because
all three that carry several split on the mutually exclusive reflection kinds.
Second, a blanket may bind associated types, as in
`ReflectStruct<FieldTypes = [..F]>`. Deciding which binding a caller sees when a
narrower impl binds them differently is the specialization soundness question,
and it needs its own WEP. Rank 4 will surface a concrete case if one appears.

### An ungrounded bound cycle satisfies its own bounds

Write `impl<T: A> B for T` beside `impl<T: B> A for T` and neither trait is
grounded. The dispatch query still answers yes to a repeated `(type, trait)`
pair, so every type satisfies both, and a blanket keyed on either applies to
every receiver in the program. Rank 2's walk refuses the repeat, so a newtype is
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

### Rank 3 is undocumented elsewhere

Nobody ever proposed locality. It is recorded here as the behaviour that exists.
Whether a tie should instead be an error is worth asking separately: letting the
reader's vantage decide a program's meaning is exactly what ranks 1 and 2 avoid.

### `spec.md` overstates coherence

Its "at most one impl can apply" describes what the orphan rules guarantee about
_where impls may be written_. It does not describe how many apply to a call. Its
"Method Resolution" section lists two steps of the six above. The selection order
is language semantics and belongs in the spec. This WEP records the decision;
writing the spec section is the follow-up.

## Related WEPs

What each contributes to the order above.

- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md) — rank 0
- [Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md) — rank 2
- [Overload Resolution](./wep-2026-07-31-overload-resolution.md) — arguments, and the two-trait ambiguity
- [Visibility](./wep-2026-06-25-visibility-internal-pub-export.md) — the `Reflect*` eligibility gate
- [Super Traits](./wep-2026-07-27-super-traits.md) — what a specificity rank would read
