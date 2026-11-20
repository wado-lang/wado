# One Inference Context: Equations and Obligations

## Context

[WEP: Omitted Type Arguments](./wep-2026-11-19-omitted-type-arguments.md) made
an omitted type argument an inference variable, and taught the trait solver to
answer "not yet known" about a type holding one. Three reviews of that work
then found defects one at a time, and each round found new ones of the same
kinds:

| Defect found                                                                      | What the site lacked                     |
| --------------------------------------------------------------------------------- | ---------------------------------------- |
| An impl chosen at an open receiver never had its bound checked                    | a place to leave the bound until settled |
| A subscript refused a bound on a variable instead of waiting for it               | the same                                 |
| A `_` in a turbofish reached an instance as `unknown`, and crashed monomorphizing | a variable for what the source wrote     |
| `Tree::eq(&Tree::Empty, &Tree::Empty)` reported its one unknown twice             | a way to join two variables              |
| `let n: i64 = b[0]` left `b`'s argument unsettled                                 | the same                                 |
| One fault was reported twice, or not at all once a variable was closed            | one report per unknown                   |

The cause is not any one site. The elaborator checks each expression where it
meets it, and a variable was added to that design afterwards. Every site that
reads a type must now pick, by itself, among five treatments: answer yes or
no, answer in three values, leave the check for later, settle the variable,
or close it behind a report. A site that picks wrong, or was written before
variables existed, is the next finding.

The machinery reflects this. `InferHoleTable` holds eight kinds of deferred
record (solutions, blames, bounds, mismatches, projection links, unwalked
defaults, commitments, deferred checks), each with its own rule for when it is
asked again. `unify` never fails: it keeps the first answer and drops a
conflict silently, so a conflict surfaces later as a deferred check. A call's
argument settles only the variables the callee itself minted, so two variables
that must be one type stay two.

## Decision

Each body is elaborated against one inference context. A site reaches the
context through three operations only, and the context, not the site, decides
what is known.

### Variables

A variable is created where the source leaves a type to inference: an omitted
type argument, a `_` the source writes, a numeric literal. It records its
origin, the span and the sentence it reports if never settled.

Variables are kept in a union-find. Two variables found equal become one, and
the root carries both origins. A variable is settled once; settling it again to
another type is a mismatch at the second cause.

A numeric literal's variable has a kind, integer or float. It joins only a
variable of its kind or a type of its kind. One left unsettled when the body
ends takes its kind's default type, `i32` or `f64`, as an unannotated literal
does today.

### `equate(a, b, cause)`

`equate` makes two types one. Where one side is a variable, it settles; where
both are, they join; where the heads differ, the cause reports a mismatch. It
replaces every way a site settles a variable today (`solve_infer_holes_against`,
`chain_infer_var`, `solve_infer_var`) and the deferred check: a check whose
sides hold variables is an equation, and holds or fails when they settle.

### `coerce(actual, expected, cause)`

A value flowing into a slot is coerced: an argument, an annotated `let`, an
assignment, a return, a field. The coercions are those the language defines
(`&mut T` to `&T`, `!` to anything, `Option<!>` widening, a literal into a
collection). A coercion is chosen by the heads of the two types. Where either
head is still a variable, no coercion can be chosen, and the site equates the
two, as Rust does.

### `require(obligation, cause)`

Every trait question is an obligation: a bound `T: Trait<Args>`, an associated
type `<T as Trait>::Assoc == U`, and the selection of the impl a method call, a
static, a subscript or an operator dispatches to. The context keeps them in one
queue and asks the solver about each in three values:

- holds: it is discharged. Selecting an impl equates what the impl's target
  writes, and requires the impl's bounds.
- fails: the cause reports it.
- not yet known: it stays queued.

The queue is asked again after each equation that settles a variable it reads,
until nothing changes. A selection commits under the rule of the previous WEP:
one candidate left, whose target meets the receiver at every instance.

### `resolve_head(ty, cause)`

Some sites need a type's head before they can go on: a method name is looked
up on it, a field is read from it. The site asks the context to resolve the
head. The context runs the queue first. A head still a variable after that is
reported at the cause as needing an annotation, and the variable is closed so
nothing reports it again.

### End of the body

When the body ends, the queue runs to its fixed point, the literal variables
take their defaults, and the queue runs again. Then:

- each obligation left is reported at its cause;
- each root variable left unsettled is reported once, by the origin of the
  earliest variable joined into it, unless a mismatch already covers it;
- every recorded fact is written back with its variables replaced, as
  `finalize_infer_holes` does today.

### What goes

The deferred machinery that exists only because sites decide alone:
`DeferredCheck`, `defer_awaited_impl_bounds` and the other deferred impl bound
paths, `Commitment` and `commit_to`, `close_infer_var` and `close_uninferred`,
and every branch that tests a type for `unknown` standing for an omitted
argument. `unknown` remains only as error recovery.

## Roadmap

Each step keeps every fixture passing and deletes what it replaces.

- [x] The union-find variable store with origins, behind the current API.
- [x] `equate` at every site that checks a value against a slot. Two variables
  found equal report once.
- [x] `equate` in place of the `solve_*` family.
- [x] One obligation queue holds bounds, projection links, commitments and the
  checks `equate` leaves undecided. It is asked again each time a variable
  settles, so a site that needs a head finds it at its fixed point.
- [ ] Method, static, subscript and operator selection are obligations; a
  selection not yet known stays queued until `resolve_head` or the body's end.
  Until then, a selection admits a variable awaiting its report as meeting
  every bound, and each selection site requires the committed impl's bounds
  itself (`defer_awaited_impl_bounds`); this step deletes both. Until then, a
  for-of that cannot select a walk while a literal's variable is open gives
  that variable its default (`for i of 0..<n`), where a later use could have
  settled it.
- [ ] A numeric literal's variable has a kind and a default. The open item of
  [WEP: Omitted Type Arguments](./wep-2026-11-19-omitted-type-arguments.md)
  closes with it, and so does `Settles::Nothing`, which until then keeps a
  literal's default type from settling a variable.
- [ ] A `_` the source writes in an expression's types is a variable. No
  inference path reads `unknown` for it.
- [ ] The context is per body rather than per module.

## Known gaps

- `InferCtx` in `infer.rs` infers a single call's type arguments with
  parameters as holes, apart from the context's variables.
