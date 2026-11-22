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
context through four operations only, and the context, not the site, decides
what is known.

### Variables

A variable is created where the source leaves a type to inference: an omitted
type argument or a `_` the source writes. It records its origin, the span and
the sentence it reports if never settled.

A `_` may stand at any depth of any type an expression writes: a turbofish, a
struct literal's, a `let` annotation, an `as` target. This is Rust's reach. It
widens what the language accepted before, where only a whole turbofish slot
took one.

Variables are kept in a union-find. Two variables found equal become one, and
the root carries both origins. A variable is settled once; settling it again to
another type is a mismatch at the second cause.

### Numeric literals

A numeric literal is not one of the body's variables. It takes its type within
the expression it is written in, and its default where nothing there answers
it, as
[the specification](./spec-literals.md#where-a-literals-context-ends) states. A
later statement never retypes it.

Rust leaves a literal open until the body ends, so a later use settles it. That
was this WEP's first design, and it was dropped for three reasons:

- A later line would change what an earlier one computes. In
  `let b = put(none(), 2_000_000_000)`, then `b.v.unwrap() + b.v.unwrap()`, then
  `take64(b)`, the last line makes the sum `i64` and stops it overflowing. An
  omitted type argument has no default, so settling it late only accepts a
  program that was refused. A literal has one, so settling it late changes a
  program that already compiled.
- What it buys is an annotation saved: `let mut xs: List<i64> = []` in place of
  `let mut xs = empty()` before `xs.push(5)`. Nothing in the repository needed
  it.
- Every site that reads a type would have to meet an open literal, and each
  would choose alone between waiting and defaulting. That is the defect this
  WEP removes. Leaving literals open broke 114 fixtures at sites that read a
  head (a `for-of` over `0..<n`, `==` on `Option<T>`, an inherent method, a
  subscript, a template hole). Rust's users meet the same thing as "can't call
  method on ambiguous numeric type", reported far from the literal.

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

A selection not yet known types its site by the trait's declaration of the
method, with `Self` the receiver. Once the impl is selected, its signature is
held to those types. A comparison operator answers `bool` whichever impl it
selects. Any other operator and a subscript answer the trait's `Output`, a
variable until the impl is selected. A static call on a receiver spelled
without its arguments (`Bx::make()`) is asked at the instance whose arguments
are variables. A site waits only where:

- the receiver's head is known: a bare variable, or a projection off an open
  variable, needs its head resolved first;
- every candidate is of one trait;
- a method call's or a static call's declaration reads no associated type and
  no type parameter of its own;
- a static call omits no default.

Elsewhere an undecided selection reports at once that the receiver needs an
annotation.

### `resolve_head(ty, cause)`

Some sites need a type's head before they can go on: a method name is looked
up on it, a field is read from it. The site asks the context to resolve the
head. The context runs the queue first. A head still a variable after that is
reported at the cause as needing an annotation, and the variable is closed so
nothing reports it again.

### End of the body

When the body ends, the queue runs to its fixed point. Then:

- each obligation left is reported at its cause;
- each root variable left unsettled is reported once, by the origin of the
  earliest variable joined into it, unless a mismatch already covers it;
- every recorded fact is written back with its variables replaced, as
  `finalize_infer_holes` does today.

### What goes

The deferred machinery that exists only because sites decide alone:
`DeferredCheck`, the deferred impl bound paths, `Commitment` and `commit_to`, `close_infer_var` and `close_uninferred`,
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
- [x] Method, static, subscript and operator selection are obligations. A
  selection not yet known stays queued until the body's end
  (`Obligation::Pending`), so a later use settles which impl a call on
  `Box<_>` reaches, and `a.eq(&a)` and `a == a` select alike.
- [x] A selection asks an impl's bounds in three values (`Unsettled::MayHold`):
  one an open variable may yet meet keeps the candidate, and the selected impl
  requires it (`require_selected_impl_bounds`). This replaces standing a
  variable awaiting its report in as the error type, which met every bound.
- [x] A numeric literal keeps its type within its expression (see Numeric
  literals). Within a call, a literal at a slot that an associated-type
  equality on another slot names answers last, so `f(1, 2)` for
  `fn f<A: Src<Item = B>, B>(a: A, b: B)` takes `B` from `i32`'s `Src::Item`.
  A call links its slots' equalities (`Obligation::Projection`) before its
  arguments resolve, and settles a variable one of them answers after the rest,
  whatever the argument order. An operation over literals waits as a literal
  does.
- [x] A `_` below a call's turbofish slot, and any `_` in a struct literal's
  turbofish, is a variable (`InferHoleTable::written`), minted afresh each time
  its expression is elaborated. A slot meets it as it meets a written type, and
  an unsettled one reports as its slot would. The call counts it among its own
  variables, so a literal under it waits for the expected type. A struct
  literal's turbofish names its slots, as a call's does.
- [x] A `_` standing for a whole turbofish slot, or anywhere in a path's type
  argument, is a variable. A call's slot meets it, and the solve counts it
  among its own variables. A static call's receiver and a case path meet the
  expected type before their arguments are typed, so a literal under a `_`
  waits for it. A case path's `_` for a whole slot is that slot omitted
  (`omit_bare_written_holes`), which the construction infers and reports.
- [ ] A `_` in a `let` annotation and after `as` is a variable.
- [ ] The context is per body rather than per module.

## Known gaps

- `InferCtx` in `infer.rs` infers a single call's type arguments with
  parameters as holes, apart from the context's variables.
- A mismatch at a later statement does not name the literal whose default
  caused it. In `let b = put(none(), 5)` followed by `take64(b)`, the report
  reads `expected 'Bx<i64>', found 'Bx<i32>'` at `take64(b)`. By then `b`'s
  type is `Bx<i32>`, which no longer says where its `i32` came from.
