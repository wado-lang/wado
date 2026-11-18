# Omitted Type Arguments Are Inference Variables

## Context

`Type::method(…)` may name a generic type without its arguments: `Pair::m(&p)`,
`G::m(&g)` for a generic newtype `G`. The call then has to decide two things
before its arguments are resolved: whether `method` takes a receiver, and which
impls may answer.

[WEP: Static Call Resolution](./wep-2026-09-06-static-call-resolution.md)
answered both by listing the instances of the type that some trait impl is
written for, and asking the resolution order at each. A static with nothing
else to settle its instance took the one listed instance its blanket answered
at.

That list misses every instance the source does not spell out:

| Program                                                           | Result                    |
| ----------------------------------------------------------------- | ------------------------- |
| `type G<T> = Pair<T, i32>`, an impl written for `Pair<i32, i32>`  | `G::m(&g)` unknown        |
| `impl<T> AtBase for Pair<T, i32>`, a blanket over `AtBase`        | `Pair::m(&p)` unknown     |
| `type G<T> = Pair<i32, i32>`, a parameter its base does not use   | `G::m(&g)` unknown        |
| `Pair::<_, _>::m(&p)`                                             | unknown, as the bare form |
| `type R<T> = &Pair<T, i32>`, an impl written for `Pair<i32, i32>` | `R::m(&r)` unknown        |

Each new shape of type would need its own extension of the list, and each
extension misses the next shape. Issue #2674 and the review of a fix for it
found them one at a time.

The list also acts at a distance. A static `Pair::make()` resolved through the
one impl written for `Pair<i32, i32>`, and a second impl written for another
instance anywhere in the program made the same call fail.

## Decision

A type argument the spelling omits is an inference variable, as an omitted
argument of a generic function is. `Pair::m(…)` means `Pair::<_, _>::m(…)`.
Nothing lists the instances an impl is written for.

### One order, with variables where the spelling wrote none

The resolution order runs at the spelled type. A spelling that writes every
argument is the case with no variables, so it resolves as before.

With variables, an impl is a candidate when it may apply:

- An impl written for a link of the chain may apply when its target can match
  the link, a variable matching anything.
- A blanket may apply when each of its bounds may hold. A bound may hold unless
  no impl of its trait can match the type it is asked of.

The candidates decide the kind of the call. Their order is then confirmed at
the instance the arguments settle.

### The kind of the call

- Every candidate takes a receiver: the call is a method call on its first
  argument. That argument is resolved once, its type settles the variables,
  and the call selects what `recv.method(…)` selects. The receiver must be of
  the spelled type or a resource extending it.
- Every candidate is a static: the variables are answered by the expected type
  or the arguments. Where neither answers them, the call reports that the
  type's arguments cannot be inferred, and a turbofish or an annotation
  supplies them.
- Candidates of both kinds: the call is ambiguous, as two traits at one link
  are whichever kind each declares. The trait-qualified form names the one
  meant.

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
- [x] The receiver-taking call decides its kind from candidates that may apply,
  and the code listing written instances is deleted.
- [x] A static reports uninferred arguments where it took the one listed
  instance.

## Implementation

The trait solver has a type `Var`. It matches any impl target, binding what the
target leaves open to `Var` in turn, and every bound holds of it. The order run
at a type holding one therefore names the impls that may apply, through blankets
as deep as they go. An inference variable lowers to `Var` only for the question
that decides the kind of a qualified call. Every other question waits for the
variable to settle, as before.
