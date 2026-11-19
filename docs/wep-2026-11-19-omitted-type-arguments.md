# Omitted Type Arguments Are Inference Variables

## Context

`Type::method(…)` may name a generic type without its arguments: `Pair::m(&p)`,
`G::m(&g)` for a generic newtype `G`. Before its arguments are resolved, the
call has to decide whether `method` takes a receiver.

[WEP: Static Call Resolution](./wep-2026-09-06-static-call-resolution.md)
decided it by listing the instances of the type that some trait impl is written
for, and asking the resolution order at each. A static with nothing else to
settle its instance took the one listed instance its blanket answered at.

That list misses every instance the source does not spell out:

| Program                                                           | Result                    |
| ----------------------------------------------------------------- | ------------------------- |
| `type G<T> = Pair<T, i32>`, an impl written for `Pair<i32, i32>`  | `G::m(&g)` unknown        |
| `impl<T> AtBase for Box<T>`, a blanket over `AtBase`              | `Box::m(&b)` unknown      |
| `type G<T> = Pair<i32, i32>`, a parameter its base does not use   | `G::m(&g)` unknown        |
| `Box::<_>::m(&b)`                                                 | unknown, as the bare form |
| `type R<T> = &Pair<T, i32>`, an impl written for `Pair<i32, i32>` | `R::m(&r)` unknown        |

Each new shape of type would need its own extension of the list, and each
extension misses the next shape. Issue #2674 and the review of a fix for it
found them one at a time.

The list also acts at a distance. A static `Pair::make()` resolved through the
one impl written for `Pair<i32, i32>`, and a second impl written for another
instance anywhere in the program made the same call fail.

The same gap shows on a method call whose receiver still holds a variable.
`mk().eq(&other)`, where `mk<T>() -> Tree<T>`, reports that `Tree<T>` has no
method `eq`: the solver cannot be asked about a type holding a variable, so it
answers nothing, and nothing is read as "no such method".

Two designs that stopped short show what a fix must not do:

- Teaching the solver a type that may be anything, without identity, and
  reading its "may hold" through the same `Option<Holds>` as "holds". Each
  solver rule needed a second reading, the type did not say which rules had
  one, and review found the missing ones one at a time.
- Deciding the kind of an omitted spelling from the impls written for the type,
  apart from the order that decides a spelling with every argument. Two rules
  for one call drift apart by construction.

## Decision

### The rule

A type argument the spelling omits is an inference variable, as an omitted
argument of a generic function is. `Pair::m(…)` means `Pair::<_, _>::m(…)`.
Nothing lists the instances an impl is written for.

A call holding an inference variable resolves exactly as the spelling with every
argument would resolve at the instance inference settles. Where the call must be
resolved before inference settles it, and the impls that may apply would resolve
it in different ways, the call asks for a turbofish or an annotation. It never
resolves otherwise than the full spelling would.

This holds wherever a variable reaches trait resolution: a qualified call
spelling a type without its arguments, and a method call on a receiver whose
type still holds a variable.

### Three answers, as a type

Trait resolution answers "does this type satisfy this trait" with one of three
values, and the type of the answer says which:

- holds, with the derivations the answer owes;
- does not hold, at any instance the variables may become;
- not yet known: some instance would make it hold and another would not.

Every rule of the solver returns this type, so a rule that has not decided what
"not yet known" means for it does not compile. `Option<Holds>` is gone from the
solver's interface.

### Inference variables with identity

The solver has a type for the elaborator's inference variable, carrying its id.
Two occurrences of one variable are one type: matching `Pair<?1, ?1>` against
`impl<T> Tr for Pair<T, T>` binds `T` once.

Matching an impl's target against a type holding variables unifies the two
under a substitution local to the question, with an occurs check. Asking a
question never changes the elaborator's variables.

### Selection, commitment and the deferred check

Selection runs the order of
[WEP: Trait Resolution](./wep-2026-09-01-trait-resolution.md) over the
candidates that hold or are not yet known, and drops those that do not hold.

- One candidate is left: the call commits to it. A bound that was not yet known
  is recorded against the variables it reads, the same way the elaborator
  already records a bound on a variable, and checked when the module's
  inference finishes.
- Several are left: the call asks for a turbofish or an annotation.

The impl committed to settles nothing of the variables. Where only one impl may
apply, a variable the call does not otherwise settle is still reported as one
that cannot be inferred. Inferring it from the one impl would let an impl
written anywhere else later turn the call into an error, the action at a
distance this WEP removes from statics.

A numeric literal's variable is the exception, as it is in Rust: where one impl
alone could answer a method on a receiver holding it, that impl settles it
(`wrap(1).tag()` under `impl Tag for Box<u64>` takes `1` as `u64`). The
literal's type is a default left to settle last, and the impl only chooses it
earlier.

- None is left: the method does not exist at any instance, and the call says
  so.

When the module's inference finishes, every commitment made with variables
unsettled is resolved again at the instance they settled to, as the spelling
with every argument would be. The two must name the same impl. A difference is
an internal compiler error, so the rule above is checked on every fixture
rather than left for review to sample.

### The kind of a qualified call

The kind is read from the impls selection keeps at the spelled type, variables
included, by the same order as for a spelling with every argument:

- Every candidate takes a receiver: the call is a method call on its first
  argument, and selects what `recv.method(…)` selects. The receiver must be of
  the spelled type or a resource extending it.
- Every candidate is a static: the expected type or the arguments answer the
  variables. Where neither does, the call reports that the type's arguments
  cannot be inferred.
- Candidates of both kinds: the call cannot tell whether its first argument is
  a receiver, and says so, offering the trait-qualified form and the type's
  arguments as the two remedies.

The type's own declaration at the nearest link still answers before any trait,
and it decides the kind alone.

### What is given up

A static with neither an expected type nor an argument to settle its instance
no longer takes the one instance an impl is written for. A generic function
does not infer its parameter from the impls written for it either, and the
inference broke as soon as a second impl was written elsewhere.

## Roadmap

- [x] The specification states the rule in `spec-traits.md` § Qualified Calls,
  in place of the listed instances.
- [x] Fixtures for each row of the table in Context, and for a static whose
  instance nothing settles.
- [x] The solver's answer is the three-valued type, and every rule returns it.
  No program changes behavior: nothing yet asks with a variable.
- [ ] The solver's inference variable, its unification under a local
  substitution, and the bindings an answer implies. The lowering hands it the
  elaborator's variables instead of refusing them.
- [ ] Selection keeps the candidates not yet known, the elaborator commits to
  the one left, and a bound not yet known is checked when the module's
  inference finishes, through the elaborator's existing bound records.
- [ ] Every commitment made with variables unsettled is resolved again at the
  settled instance, and a difference is an internal compiler error.
- [ ] The kind of a qualified call is read from selection at the spelled type,
  and the reading from the impls written for the type is deleted.
- [ ] A method call on a receiver holding a variable resolves by the same
  selection, and reports a missing method only where no candidate is left.
- [ ] A check over the fixtures rewrites each qualified call that writes every
  argument into the spelling that omits them, and requires the same result or
  a request for an annotation.
