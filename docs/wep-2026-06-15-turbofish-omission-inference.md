# WEP: Turbofish Omission via Type Inference

## Context

Wado's turbofish (`f::<T>(...)`) is Rust-compatible, and so is its omission:
a type argument may be left out when inference can recover it. Inference draws
type arguments from three sources:

- forward, from the call's argument types;
- backward, from an LHS annotation / expected return type;
- from an associated-type-equality bound (`I: Iterator<Item = T>`).

The elaborator (`InferCtx`, `wado-compiler/src/elaborator/infer.rs`) already
covers these across free functions, static methods, instance methods, variant
constructors, and builtins. Coverage was confirmed empirically: the basic
matrix passes; the gaps are all backward-flow cases where a sub-expression is
resolved _before_ the expected type that would pin its type parameter becomes
available.

### What works

- [x] forward inference from arguments (all call kinds)
- [x] backward inference from an LHS annotation when the call is the RHS root
- [x] partial turbofish (explicit prefix, inferred tail)
- [x] associated-type-bound-driven inference
- [x] backward inference through the `?` operator (this WEP, below)

### Remaining gaps

Each is the same shape: an inner generic call whose type parameter appears
_only_ in its return type is resolved with no expected type, so it cannot be
inferred and demands a turbofish.

- [ ] method-chain receiver — `let x: i32 = p.get().unwrap();`
      (`p.get(): Option<T>`, `T` uninferable)
- [ ] `match` scrutinee — `match none_of() { Some(x) => x, None => 0 }`
- [ ] binary-op operand — `make() + 1`
- [ ] `as`-cast operand — `def() as Meters`

`collect()` into a non-`List` (e.g. `TreeMap`) looks similar but is a separate
concern: `collect` is hard-typed to `List<Self::Item>`, so it is a missing
generic-`collect` / `FromIterator` feature, not an inference gap.

## Decision

### Shipped: inference through `?`

`resolve_question_mark` resolved its operand with `expected = None`, dropping
the LHS type. It now reconstructs the operand's expected type from the
`?`-stripped payload `U` and the enclosing function's return shape — `Option<U>`
or `Result<U, F>` (`F` = the function's return error type) — and threads it into
the operand. The `?` relationship is fixed (it always strips `Option`/`Result`),
so the wrapper is reconstructible without knowing anything method-specific.

Fixture: `tests/fixtures/infer_type_arg_through_question_mark.wado`.

### Deferred: the other backward-flow gaps

The method-chain / `match` / binary-op / `as`-cast gaps cannot reuse the `?`
trick, because there the relationship between the inner result and the outer
expected type is not fixed:

- The outer operator (`.unwrap()`, an arm body, `+`, `as`) determines the
  inner expected type, but identifying that operator's effect on types
  requires the inner type — which is exactly what we are trying to infer.
  Resolving the inner expression first to break the cycle then needs a second,
  expected-aware pass.
- Re-resolving the inner expression is unsafe: the elaborator is eager and
  single-pass, and `annotate` allocates locals in walk order for parity with
  `reify`. A second walk shifts that order.
- The premature "cannot infer" diagnostic is emitted _during_ the inner
  resolution, so even a post-hoc patch of the recorded type arguments would
  still surface the error.

A sound fix is therefore a deferred-constraint layer, not a local patch:

1. Resolve the inner generic call to a _partial_ type carrying an inference
   variable for the unbound parameter, and record the unbound parameter as a
   pending obligation instead of erroring immediately.
2. Let the enclosing expression (method call, match, binary op, cast) add a
   constraint relating the inner result to the now-known expected type.
3. Solve pending obligations at a well-defined point; emit "cannot infer" only
   for obligations still unsolved.

This is a meaningful change to the elaborator's resolution model and is tracked
here rather than attempted piecemeal — a compiler miscompilation is P0, so the
backward-flow cases wait for the deferred layer rather than ad-hoc per-operator
back-propagation.

## Consequences

- The most common real-world omission, `let v: T = call()?`, works today.
- The method-chain case (`.unwrap()` / `.expect()` after a generic call) and the
  `match` / binary-op / `as` cases still require a turbofish or an intermediate
  annotated `let`; both are documented workarounds until the deferred layer
  lands.
- Stdlib turbofishes such as `seq.next_element::<Value>()?` are _not_ removable
  by the `?` fix: they bind to a `let` with no annotation, so the turbofish is
  the only place the element type is named. They remain correct as written.
