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

Several impls applying is not a defect to be legislated away — a trait carrying
several value blankets is the point of `Inspect`, `Serialize` and `Deserialize`,
each of which derives over the four reflection kinds. So a _selection order_ is
needed, and one exists. It just was not written anywhere.

Its rules were spread across four WEPs, with one recorded in none of them:

| Rule                                        | Recorded in    |
| ------------------------------------------- | -------------- |
| Non-variadic beats variadic                 | WEP 2026-03-14 |
| Variadic overlap is a definition-time error | WEP 2026-03-14 |
| A newtype's own impls select for it first   | WEP 2026-06-25 |
| Two traits, one method name, is ambiguous   | WEP 2026-07-31 |
| A `Reflect*` bound needs visible members    | WEP 2026-06-13 |
| Locality — a local impl beats a foreign one | **nowhere**    |

The single authority on the order was `select_trait_match`'s `sort_by`.
Issue #1932 is what that costs: a missing rank let a newtype take its base's
blanket, and the fix had to be derived by reading the sort rather than a
document. A rule about bare-`T` blankets then had to be filed under
"Variadic Type Parameters", because that is where its siblings happened to live.

## Decision

One order, stated here. Every other WEP that owns a rule keeps it and links
here for its place in the sequence; the implementation cites this document
rather than being it.

### The order

A call's candidates are the impls whose target matches the receiver, along its
newtype chain, plus every value blanket whose receiver-parameter bounds the
receiver satisfies. They are ranked:

0. **Variadic yields to non-variadic.** Within one trait at one argument list, a
   variadic impl (`impl<..T> Tr for [..T]`) is dropped when a non-variadic one is
   present. Scoped to one trait in both directions, so a foreign blanket for
   trait `A` never displaces a local variadic impl of trait `B`
   (WEP 2026-03-14 §5 Rule 1).

1. **A concrete impl beats a blanket.** An impl written for the receiver defines
   the very function the call names; a blanket is the general case. This outranks
   locality: a foreign `impl Tr for Point` beats a local `impl<T: B> Tr for T`.

2. **A shallower bound beats a deeper one.** Where the receiver is a newtype, a
   blanket whose bounds hold at the newtype itself beats one that holds only
   after peeling to its base. A newtype exists to be a type distinct from its
   base, so its own impls select for it first (WEP 2026-06-25). This outranks
   locality, which is about where an impl is written rather than which type
   selected it.

   The level is asked of the whole derivation, not just its first step. A bound
   satisfied through another blanket (`impl<T: Base> Derived for T` answering
   `T: Derived`) is at the level that blanket's own bound holds at, so a
   chained blanket does not report the base's bound as the newtype's.

3. **A local impl beats a foreign one.** Between candidates otherwise equal, the
   one in the calling module wins. This is the tie-break of last resort, not a
   substantive rule: everything above it is about the impl's relation to the
   receiver, and this is about the reader's vantage.

   It splits local from foreign and no finer. Two blankets in two foreign
   modules are tied, and fall to rank 4 — ordering them by which module wrote
   them would be declaration order wearing a rank.

4. **Anything left is ambiguous**, and is reported rather than resolved. See
   below for the two shapes.

Two filters sit beside the order rather than in it:

- **Scope.** Two same-named trait declarations from different modules are
  distinct traits. One the calling module never imported is not a competitor:
  each module's `s.shout()` dispatches to the `Loud` in scope there
  (`cross_module_same_name_foreign_impl.wado`). Only declarations in scope reach
  the ambiguity rules.
- **Arguments.** One trait declaration at several argument lists is an overload
  set, and the call's arguments choose (WEP 2026-07-31). Distinct traits never
  form one.

### The two ambiguities

They differ in what the programmer can do about it, which is why they are
separate diagnostics.

**Two traits, one method name.** The receiver has `impl Alpha for Item` and
`impl Beta for Item`, both declaring `describe`. They share no contract, so
nothing selects. The call names the trait: `Alpha::describe(&it)`
(WEP 2026-07-31).

**Two blankets of one trait.** The receiver satisfies both bounds and nothing
above ranks them. A blanket has no name, so the call _cannot_ pin one. The only
answer is an impl written for the receiver, which rank 1 puts above both:

```text
ambiguous blanket impls of 'Describe' for 'Point': 'T: Limit' and
'T: ReflectStruct' apply, and nothing ranks them;
write 'impl Describe for Point'
```

Reported at the use site because that is the one place the question is
decidable. Rejecting the overlap where the impls are written would need to know
whether two bounds can both hold, which an open world does not answer: another
module may write `impl Limit` for a struct at any time. Nothing is rejected at
definition time, and the standard library's blankets never reach this rule —
the four reflection kinds are mutually exclusive, so no receiver satisfies two.

### What eligibility is, and is not

Ranking runs over candidates, and a bound that does not hold produces none. Two
gates decide that and are not ranking rules:

- A `Reflect*` bound holds only where every one of the receiver's members is
  visible at the use site (WEP 2026-06-13). This is what keeps `TreeMap` out of
  a downstream `T: ReflectStruct` without naming it.
- A newtype inherits its base's impls for dispatch, so a blanket keyed by a
  bound only the base carries is a candidate for the newtype — at rank 2's
  depth 1, not excluded.

A binder is one item's parameter, so a name that reaches a blanket's receiver
is not the same question as a name spelled like it. A method parameter may
shadow the receiver's letter, and inside that method the letter is the method's
binder; the `impl` blocks of one module that bind a parameter of one spelling
share an index bucket, which is neither a declaration nor a shape.

## Consequences

Selection is one ordered list, so a new rule is a rank in it and its absence is
visible. Two calls that used to differ only by declaration order now either
agree or report, and the report names an impl the programmer can write.

The order is stated here and implemented in `select_trait_match`; the two must
not diverge, which is why the sort cites this document instead of restating it.

## Known gaps

**Specificity is not ranked.** Where one blanket's bounds imply another's, an
order exists and is unused: `impl<T: Ord>` is strictly narrower than
`impl<T: Eq>` when `Ord: Eq` (WEP 2026-07-27), as is `impl<T: A + B>` against
`impl<T: A>`. Both are decidable from the trait declarations. Such a pair is
reported ambiguous today.

Left open deliberately. No standard-library trait carries two blankets with
comparable bounds — all three that carry several split on the mutually exclusive
reflection kinds — so the rule would not fire. And a blanket may bind associated
types (`ReflectStruct<FieldTypes = [..F]>`); which binding a caller sees when a
narrower impl binds them differently is the specialization soundness question,
and wants its own WEP. Rank 4 surfaces the demand with a concrete case when one
appears.

**Rank 3 is undocumented elsewhere.** Locality was never proposed; it is
recorded here as the behaviour that exists. Whether a tie should instead be an
error — the reader's vantage deciding a program's meaning is exactly what
ranks 1 and 2 avoid — is worth asking separately.

**`spec.md` overstates coherence.** Its "at most one impl can apply" describes
the orphan rules' guarantee about _where impls may be written_, not about how
many apply to a call. The selection order belongs in the spec as language
semantics; this WEP records the decision, and the spec section is the follow-up.

## Related WEPs

What each contributes to the order above.

- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md) — rank 0
- [Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md) — rank 2
- [Overload Resolution](./wep-2026-07-31-overload-resolution.md) — arguments, and the two-trait ambiguity
- [Visibility](./wep-2026-06-25-visibility-internal-pub-export.md) — the `Reflect*` eligibility gate
- [Super Traits](./wep-2026-07-27-super-traits.md) — what a specificity rank would read
