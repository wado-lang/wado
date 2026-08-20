# WEP: Power-Assert Coverage

## Context

`assert` exists to show the values behind a failed condition, and it cannot be
disabled, so a Wado program's assertion diagnostic is the one debugging aid that
is always present. Its instrumentation (`elaborator/assert.rs`, replayed by
`reify_assert`) is a read-only scanner that picks the sub-expressions of the
condition worth quoting, plus a capture mechanism that must not disturb what the
condition does.

Both halves are easy to get wrong in ways nothing reports. A capture hoisted
ahead of the condition changes what the condition evaluates; a shape the scanner
was never taught renders no operand, and a condition that shows nothing is
indistinguishable from one that has nothing to show.

## Decision

Three rules, in priority order. Each is a property of the whole `assert`
statement, testable per condition form.

### 1. Instrumentation is evaluation-preserving

`assert e` evaluates `e` exactly as `let _ = e` would: the same sub-expressions,
in the same order, and no others. It is a correctness invariant, and it outranks
every rendering concern below.

A capture is **unconditional** when no short-circuit lies between it and the
condition root. Those are bound ahead of the condition, which is free.

A capture below a short-circuit is **conditional**, and is taken where the
operand sits rather than hoisted, so the short-circuit still decides whether it
runs. The boundaries are the right operand of `&&` and `||`, and every operand
of a comparison chain past the first comparison (`a < b < c` runs as
`(a < b) && (b < c)`).

### 2. Rendering reports reach, not just value

A conditional slot the run never reached renders `<label>: <not evaluated>`, not
silence. Where evaluation stopped answers why the assert failed as often as any
value does, and an omitted line would be indistinguishable from an operand the
instrumentation cannot see — which rule 3 forbids.

Each conditional slot's text is chosen in the failure branch, from a flag saying
its capture site ran. The choice is on the cold path, so a passing assert pays
nothing for it.

### 3. No silent degradation

Every operand position in a condition is captured, save what
_Deliberately out of scope_ names. There is no second outcome to report for a
type: `Inspect` is total (WEP-2026-06-25), so a `T: Inspect` obligation always
holds and every operand has a rendering. An operand the compiler declines to
inspect is a bug in `Inspect` derivation, at the priority every compiler bug
carries — never a power-assert degradation to document.

So one failure mode remains: the scanner does not descend into a shape. Two
things keep it from recurring silently. The scanner's match is exhaustive over
`Expr`, so a new variant is a compile error at the decision point rather than a
fall into a leaf arm. And the plan is dumpable — `wado dump --assert-plan`
prints, for every `assert`, which operands it captures and whether a
short-circuit can skip each one:

```
6: assert i < list.len() && list[i] == 1
  __v0  always       i
  __v1  always       list.len()
  __v2  always       i < list.len()
  __v3  conditional  list[i]
  __v4  conditional  list[i] == 1
```

`tests/integration/assert_capture_plan.rs` reads that back for one `assert` per
condition shape, so what each shape covers is a test rather than a paragraph
that goes stale.

### The `condition:` line is source, not a paraphrase

It is rendered by the formatter's `Unparser`, not by `unparse_expr_simple`,
whose readability-over-fidelity trade stays right for its own callers. A
block-carrying condition renders over several lines there, so the line breaks
collapse to single spaces — the quote is one line, and a string literal's
escapes are already 2-char sequences at that point, so no literal text folds.

The formatter drops the parentheses around an `if` used as an operand. That is
not a fidelity loss: both spellings parse to the same tree.

## Deliberately out of scope

The children of `Closure`, `WithHandler` and `Resume` are not captured: they are
not values in isolation, and a slot for one would report a sub-expression that
never had a value at the moment the condition failed. The same reasoning stops
the walk at the body of an `If` / `Match` branch and at the statements of a
block: the value of the branch the run took is what that node's own capture
renders, so descending would report it twice under a second name.

A `Literal` adds nothing the source text does not already show. An `Assign` or
`CompoundAssign` in a condition is a mutation rather than an operand. A `Spread`
only appears inside a literal the scanner already walks.

A method call's receiver is rule 1: value semantics make the capture a copy, so
a `&mut self` method would mutate the copy and leave the receiver untouched —
`assert p.next_if(..) matches { .. }` would stop advancing `p`. The scanner runs
before the method's `self` kind is known, so it cannot capture only the
non-mutating ones.

A projection receiver, a subscript receiver and a `matches` scrutinee wait on
one optimizer capability, and it is not a trade against diagnostic value.
Rendering an operand whose value is an aggregate is a _use_ of that aggregate in
the failure branch, and the escape and scalarization analyses count that use
even though `builtin::cold_path()` marks the branch. Measured: capturing
`List<T>::index_value`'s `self` stops const-object globalization, LICM and
array-append collapse; capturing the scrutinee of `assert ok matches { Ok(6) }`
stops variant-return scalarization. Binding is not what costs — the read is, so
re-reading in the failure branch instead of binding regresses identically.

- [ ] **Discount a use dominated by `builtin::cold_path()`** in the escape and
      scalarization analyses. All three land together behind it, and nothing
      else stands between this WEP and rendering every operand.

Two narrower exclusions are unresolved rather than principled. A bare identifier
in call-argument position stays uncaptured because it may be a
function-reference coercion site, and `&<ident>` likewise: the scanner runs
before types are known, so it cannot tell `&value` from `&fn_name`.
`assert takes(&a)` therefore does not show `a`.

## Consequences

A slot whose operand is a plain binding gets no binding of its own: the failure
branch re-reads it, which straight-line code makes exact. A conditional slot
costs one `bool` flag, cleared ahead of the condition and set at the capture
site; its value gets no binding either, so the failure branch reads the local
under the flag and nothing has to synthesize a zero value for an arbitrary `T`.
Unconditional captures — the whole of the stdlib's asserts, and most asserts
anywhere — keep the hoisted `let`. `-Os` still drops the whole expansion through
`bare-asserts`.

Rule 2 adds one `String` binding per conditional slot to the cold branch. Rule 3
costs a dump surface and its tests, and returns a compile error or a test
failure where an assert would otherwise quietly report less.

Rule 1 is observable: an assert whose right operand has a side effect does not
run it when the left operand short-circuits.
