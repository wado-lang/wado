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

## Decision

A type argument the spelling omits is an inference variable, as an omitted
argument of a generic function is. `Pair::m(…)` means `Pair::<_, _>::m(…)`.
Nothing lists the instances an impl is written for.

### The kind of the call

A spelling that writes every argument names one instance. The resolution order
runs at it, so a blanket answers only where its bounds hold. Such a spelling
resolves as before.

A spelling that omits an argument names an instance the call has yet to infer.
The impls that may answer it at some instance decide the kind, in the order a
method call ranks them:

- the type's own declaration at the nearest link;
- the impls written for the nearest link of the type's newtype chain, by value
  or by reference;
- where neither answers, every blanket, whether or not its bounds will hold.

Bounds do not take part, since only the settled instance can say whether one
holds. A blanket comes last because it is written for no type in particular:
counting it beside what is written for the type would let any blanket of the
same name, in any imported library, change the call.

- Every candidate takes a receiver: the call is a method call on its first
  argument. That argument settles the variables, and the call selects what
  `recv.method(…)` selects. The receiver must be of the spelled type or a
  resource extending it.
- Every candidate is a static: the expected type or the arguments answer the
  variables. Where neither does, the call reports that the type's arguments
  cannot be inferred, and a turbofish or an annotation supplies them.
- Candidates of both kinds: the call is ambiguous, as two traits at one link
  are. The trait-qualified form names the one meant.

The type's own declaration at the nearest link still answers before any trait,
and it decides the kind alone.

### What is given up

- A static with neither an expected type nor an argument to settle its
  instance no longer takes the one instance an impl is written for. A generic
  function does not infer its parameter from the impls written for it either,
  and the inference broke as soon as a second impl was written elsewhere.
- The kind is read from what is written, not from the instance. An impl whose
  bounds will not hold at the receiver's instance still counts, and where it
  is the other kind, the omitted spelling is ambiguous while the spelling with
  every argument resolves. A blanket whose bounds do hold there counts only
  where nothing written for the type answers.

The trait solver never sees an inference variable. An earlier design taught it
one, a type that may be anything. Each of its rules then needed a second,
"may hold" reading, and review kept finding rules without one. Deciding the
kind from the impls written, before any instance is known, keeps that question
out of the solver.

## Roadmap

- [x] The specification states the rule in `spec-traits.md` § Qualified Calls,
  in place of the listed instances.
- [x] Fixtures for each row of the table in Context, for a static whose
  instance nothing settles, and for candidates of both kinds.
- [x] The kind comes from the impls written for the chain and the blankets, and
  the code listing written instances is deleted.
