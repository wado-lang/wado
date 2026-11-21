# Effect Rows in the Inference Context

## Context

[WEP: Effects Through Closures](./wep-2026-10-11-effects-through-closures.md)
gave a closure literal's type the effects its body performs. The elaborator
leaves an effect variable where a row is not known yet, and the effect check
solves the variables on the whole program after reify.

So the elaborator decides everything that reads a row before any row is
solved, and reads a variable as the empty row:

- An impl selected for a closure literal's type is the one for the empty row.
  Where `Name` has an impl for `fn() -> i32` and one for
  `fn() -> i32 with Stdout`, `(|| loud()).name()` reaches the pure one
  (`fn_type_impl_effectful_closure_picks_effectful_impl.wado`).
- An associated type reached through such an impl is resolved from the empty
  row. Where only the effectful impl answers, `T::Out` stays unresolved
  (`fn_type_impl_effectful_closure_assoc_type.wado`).

A bound asked of a type holding a variable is already deferred: the effect
check records it as a `DeferredBound`, and `unmet_deferred_bounds` asks it again
once the rows are solved. That repairs a yes-or-no question after the fact. It
cannot repair a question whose answer the elaborator has already built on: a
selected impl, or a type it resolved.

This is the defect class
[WEP: One Inference Context](./wep-2026-11-20-inference-context.md) names for
type variables: a site decides alone what is known. Effect rows are the one
kind of variable that WEP leaves outside the context.

## Decision

An effect row variable is a variable of the body's inference context, and the
context solves it while the body is elaborated.

### Rows are sets

A row is a set of effects, so a row variable is solved by inclusion, not by
equality. A variable holds a lower bound: the effects shown to flow into it.
The solution is the least row that meets every inclusion. Inclusions only add
effects, so the solution only grows, and it exists.

- A closure literal's row includes the row of each call in its body, less the
  effects a `with … do` around the call handles.
- A callee's effect parameter at a call includes the row of each argument
  that carries it.
- A value flowing into a slot whose row holds a variable widens that variable.
  Into a row without one, the value's row must be included in it; what the
  value performs beyond it is an error at the flow.
- `equate` on two function types makes each row include the other.

### Inference is body-local

A function signature declares its effects with `with`, so nothing a body
infers flows into another body's rows. Each body's rows are solved by the end
of the body, as its type variables are.

### Obligations wait for rows

An obligation that reads a row stays queued until the row settles: a row
settles once every inclusion that can still widen it has been recorded. A
selection between impls that differ only in their rows, and an associated
type read through one, then sees the solved row.

A site that needs the answer before the row settles (`resolve_head`, or an
obligation the row's own inclusions wait on) closes the row at what it holds.
A later flow that would widen a closed row is an error at that flow, naming
the site that closed it.

### What goes

The effect check verifies rows it is given; it no longer solves any.
`DeferredBound`, `unmet_deferred_bounds`, `write_back_effect_rows` and the
effect check's fixed point over the program (`solve_rows`) are deleted. Every
type the elaborator writes back holds solved rows, so reify and the TIR never
see an effect variable.

## Roadmap

Each step keeps every fixture passing and deletes what it replaces.

- [ ] The prerequisites in
  [WEP: One Inference Context](./wep-2026-11-20-inference-context.md):
  selection as obligations, and a context per body.
- [ ] Row variables live in the context, with the inclusions above recorded as
  each body is elaborated. The effect check reads the solution, and agrees
  with its own on every fixture.
- [ ] Selection and associated types wait for the rows they read.
  `fn_type_impl_effectful_closure_picks_effectful_impl.wado` and
  `fn_type_impl_effectful_closure_assoc_type.wado` pass, and
  `DeferredBound` with its recheck is deleted.
- [ ] A row closed early reports a widening flow.
- [ ] The effect check solves nothing: `solve_rows` and the write-back are
  deleted.
