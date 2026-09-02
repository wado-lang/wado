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

### The order

A call's candidates come from two places. First, the impls whose target matches
the receiver anywhere along its newtype chain. Second, every value blanket whose
receiver-parameter bounds the receiver satisfies. They are ranked:

0. **Variadic yields to non-variadic.** Within one trait at one argument list, a
   variadic impl (`impl<..T> Tr for [..T]`) is dropped when a non-variadic one is
   present. The rule stays inside one trait, so a foreign blanket for trait `A`
   never displaces a local variadic impl of trait `B` (WEP 2026-03-14 §5 Rule 1).

1. **A concrete impl beats a blanket.** An impl written for the receiver defines
   the exact function the call names. A blanket only covers the general case.
   This rank outranks locality: a foreign `impl Tr for Point` beats a local
   `impl<T: B> Tr for T`.

2. **A shallower bound beats a deeper one.** This applies when the receiver is a
   newtype. A blanket whose bounds hold at the newtype itself beats one that
   holds only after peeling to the base. A newtype is a type distinct from its
   base, so its own impls select for it first (WEP 2026-06-25). This rank also
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

3. **A local impl beats a foreign one.** When candidates are otherwise equal, the
   one in the calling module wins. This is the last tie-break, and it is weaker
   than the rest. Every rank above asks how the impl relates to the receiver.
   This one asks only where the reader is standing.

   It separates local from foreign and goes no finer. Two blankets in two foreign
   modules stay tied and fall to rank 4. Ordering them by which module wrote them
   would just be declaration order under another name.

4. **Anything left is ambiguous.** The compiler reports it instead of picking.
   The two shapes are described below.

Two filters sit beside the order rather than in it:

- **Scope.** Two same-named trait declarations from different modules are
  distinct traits. A declaration the calling module never imported does not
  compete. Each module's `s.shout()` dispatches to the `Loud` in scope there
  (`cross_module_same_name_foreign_impl.wado`). Only declarations in scope reach
  the ambiguity rules.
- **Arguments.** One trait declaration at several argument lists forms an
  overload set, and the call's arguments choose (WEP 2026-07-31). Distinct
  traits never form one.

### The two ambiguities

The two are separate diagnostics because the programmer fixes them differently.

#### Two traits, one method name

The receiver has `impl Alpha for Item` and `impl Beta for Item`, both declaring
`describe`. They share no contract, so nothing selects. The call names the
trait: `Alpha::describe(&it)` (WEP 2026-07-31).

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
five reflection kinds are mutually exclusive, so no receiver satisfies two.

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
pair, so every type satisfies both. Rank 2's walk refuses the repeat, so a
newtype is unaffected. A concrete receiver has no such walk and reaches the
shared answer.

Closing this needs a second recursion stack. One stack cannot separate the two
recursions: descent into members is well-founded and must answer yes, while a
cycle at a fixed subject must answer no. That is a change to dispatch, not to
selection, which is why it is recorded here rather than made.

### Rank 3 is undocumented elsewhere

Nobody ever proposed locality. It is recorded here as the behaviour that exists.
Whether a tie should instead be an error is worth asking separately: letting the
reader's vantage decide a program's meaning is exactly what ranks 1 and 2 avoid.

### `spec.md` overstates coherence

Its "at most one impl can apply" describes what the orphan rules guarantee about
_where impls may be written_. It does not describe how many apply to a call. The
selection order is language semantics and belongs in the spec. This WEP records
the decision; writing the spec section is the follow-up.

## Related WEPs

What each contributes to the order above.

- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md) — rank 0
- [Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md) — rank 2
- [Overload Resolution](./wep-2026-07-31-overload-resolution.md) — arguments, and the two-trait ambiguity
- [Visibility](./wep-2026-06-25-visibility-internal-pub-export.md) — the `Reflect*` eligibility gate
- [Super Traits](./wep-2026-07-27-super-traits.md) — what a specificity rank would read
