# WEP: Power-Assert Coverage

## Context

`assert` exists to show the values behind a failed condition, and it cannot be
disabled, so a Wado program's assertion diagnostic is the one debugging aid that
is always present. Its instrumentation (`elaborator/assert.rs`, replayed by
`reify_assert`) grew case by case: a read-only scanner picks capturable
sub-expressions of the condition, and every AST shape it was not taught falls
into an opaque-leaf arm that captures nothing and recurses into nothing.

Nothing states which shapes those are. A condition that renders no operands is
indistinguishable from one that has none, so the writer of the assert gets no
signal that the form they chose reports less — the complaint in
[#1855](https://github.com/wado-lang/wado/issues/1855), where
`assert 0 <= index < used` drops both operand lines that `assert index < used`
prints. `List::insert`, `List::remove` and `List::swap` all assert that lower
bound, so their failures have never shown the index; WEP-2026-06-02 Phase D
wanted the same bound on the index traits and reverted it for exactly this
reason.

Measuring the current behaviour to size the gap turned up something larger than
missing output.

### The instrumentation changes what the condition evaluates

Every capture is emitted as a `let __vK = …;` hoisted to the top of the assert
block, ahead of `let __cond = …;`. A capture under a short-circuit therefore
runs unconditionally:

```wado
fn boom() -> i32 with Stdout { println("SIDE EFFECT RAN"); return 1; }
let ok = false;
assert ok && boom() == 2;
```

prints `SIDE EFFECT RAN` and reports `boom(): 1`. The same holds for `||` with a
true left operand — the assert passes, and the right operand ran anyway. The
consequence is not confined to side effects:

```wado
let list: List<i32> = [10, 20, 30];
let i = 99;
assert i < list.len() && list[i] == 1, "guarded";
```

traps inside `List::index_value` with `index out of bounds` instead of failing
the assertion it was written for. The guard idiom does not guard. This is a
wrong-code bug: instrumentation that is supposed to observe the condition
decides it instead.

### What the scanner covers

Read from the scanner and checked by running one program per form. The
**nothing** rows are what this WEP measured at `3de0afb35` and has not yet
closed; every other row is the state after it:

| Condition form                                                                      | Operand lines rendered                                               |
| ----------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| `Ident`, `Binary`, `Unary`                                                          | captured; operands recursed                                          |
| `Call`, `StaticMethodCall`                                                          | captured whole; arguments recursed, except a bare-ident argument     |
| `MethodCall`                                                                        | captured whole; arguments recursed; receiver not                     |
| `FieldAccess`                                                                       | captured whole; receiver recursed                                    |
| `Index`                                                                             | captured whole; receiver and index operand recursed                  |
| `TemplateString`                                                                    | captured whole                                                       |
| `ComparisonChain`                                                                   | captured; operands recursed                                          |
| `Cast`                                                                              | captured; operand recursed                                           |
| `TupleLiteral`, `StructLiteral`                                                     | not captured (shape comes from the expected type); elements recursed |
| `Matches`                                                                           | captured; scrutinee recursed                                         |
| `If`, `Match`                                                                       | captured; the condition / scrutinee recursed, bodies not             |
| `Block`, `LabeledBlock`                                                             | captured; statements not walked                                      |
| `Range`                                                                             | captured; bounds recursed                                            |
| `TryOp`                                                                             | captured; operand recursed                                           |
| `Literal`, `Closure`, `WithHandler`, `Resume`, `Spread`, `Assign`, `CompoundAssign` | not captured — see below                                             |

Every operand position in a condition is now captured. What the last row
leaves out, and why, is under _Deliberately out of scope_.

### The `condition:` line is not the condition

The line is produced by `unparse_expr_simple`, which documents that it drops
disambiguating parentheses because its callers "prioritise readability over
round-trip fidelity". For an error message naming a symbol that trade is fine.
For a line labelled `condition:` it prints a different expression than the one
that failed:

| Source                             | Printed                            |
| ---------------------------------- | ---------------------------------- |
| `(0..<5).contains(&i)`             | `0..<5.contains(&i)`               |
| `(if a > 0 { a } else { b }) == 5` | `if a > 0 { a; } else { b; } == 5` |
| `l: { break l: a == 5 }`           | `<labeled-block>`                  |

The formatter's own `Unparser` prints all three correctly, so the fix is to
render the line with it.

## Decision

Three rules, in priority order. Each is a property of the whole `assert`
statement, testable per condition form.

### 1. Instrumentation is evaluation-preserving

`assert e` evaluates `e` exactly as `let _ = e` would: the same sub-expressions,
in the same order, and no others. This is not a coverage goal — it is a
correctness invariant that today's expansion violates, and it outranks every
rendering improvement below.

A capture is **unconditional** when no short-circuit or branch boundary lies
between it and the condition root. Those keep today's hoisted `let __vK = …;`:
the common case, unchanged and free.

A capture below such a boundary is **conditional**. Its slot is declared
`let mut __vK: Option<T> = None;` beside the others, and the capture happens
where the operand sits, as a value-yielding block that records into the slot and
returns the value. Evaluation order and short-circuiting are then the source's,
because the compiler no longer moves the operand.

The boundaries are: the right operand of `&&` and `||`, every operand of a
comparison chain past the first comparison (`a < b < c` runs as
`(a < b) && (b < c)`), and the arms of an `if` / `match` used as an expression.

### 2. Rendering reports reach, not just value

A conditional slot the run never reached renders `<label>: <not evaluated>`,
not silence. Where evaluation stopped is the answer to why the assert failed as
often as any value is, and an omitted line would be indistinguishable from an
operand the instrumentation cannot see — which rule 3 exists to forbid.

Each conditional slot's text is chosen in the failure branch, from the flag
saying its capture site ran, and the template interpolates that text. The
choice is on the cold path, so it costs the passing assert nothing.

### 3. No silent degradation

Every operand position in a condition is captured. There is no second
outcome to report, because there is no type the diagnostic cannot render:
`Inspect` is total (WEP-2026-06-25), so a `T: Inspect` obligation always holds
and every operand has a rendering. An operand the compiler declines to inspect
is a bug in `Inspect` derivation, at the priority every compiler bug carries —
never a power-assert degradation to document.

The scanner's comments used to claim otherwise, and that is what had kept
receivers uncaptured: they said `Fn<…>` and CM resource handles have no
`Inspect`. Both halves were wrong — `${f:?}` on a closure prints `|i32| -> i32`,
and an assert on a `wasi:http` `Fields` receiver compiles. Capturing receivers
did surface a real `Inspect` gap, but a separate one, reachable with no `assert`
in sight: `${list_of_fns:?}` failed to resolve. Every closure of one arity and
return type shares a single `Fn<N,Ret>^Inspect` vtable — a representation key,
coarser than the type's own name (`crate::name::fn_type_arg_names`) — and
substituting a `fn(..)` type for a type parameter named the receiver by that own
name instead, so `T^Inspect::inspect` reached WIR build unresolved. Fixed in
`monomorphize`, where the substitution happens.

So one cause remains: the scanner does not descend into a shape. That is a bug
against this WEP, closed by teaching the scanner, not by reporting. Two things
keep it from recurring silently. The scanner's match is exhaustive over `Expr`,
so a new variant is a compile error at the decision point rather than a fall
into a leaf arm. And the plan itself is dumpable — `wado dump --assert-plan`
prints, for every `assert`, which operands it captures and whether a
short-circuit can skip each one:

```
6: assert i < list.len() && list[i] == 1
  __v0  always       i
  __v1  always       list
  __v2  always       list.len()
  __v3  always       i < list.len()
  __v4  conditional  list
  __v5  conditional  i
  __v6  conditional  list[i]
  __v7  conditional  list[i] == 1
```

`tests/integration/assert_capture_plan.rs` reads that back for one `assert` per
condition shape, so the table above is a test rather than a paragraph that goes
stale.

### The `condition:` line is source, not a paraphrase

It is rendered by the formatter's `Unparser`, not by `unparse_expr_simple`,
whose readability trade stays right for its own callers. A block-carrying
condition renders over several lines there, so the line breaks collapse to
single spaces — the quote is one line, and a string literal's escapes are
already 2-char sequences at that point, so no literal text folds.

The formatter drops the parentheses around an `if` used as an operand. That is
not a fidelity loss: both spellings parse to the same tree and evaluate the
same, which is why the formatter drops them.

## Roadmap

Ordered by yield per cost, and rules 1–3 are why: a wrong-code bug outranks a
missing line, and a missing line outranks a misrendered one. All of it has
landed; what the design leaves out on purpose is under _Deliberately out of
scope_.

- [x] **P0 — short-circuit preservation.** Conditional slots per rule 1, for
      `&&` and `||`. Closed the wrong-code bug; nothing below is safe to build
      on an expansion that moves operands.
- [x] **`<not evaluated>` rendering** (rule 2), landed with it: without it a
      short-circuited slot would have quoted the value it never took.
- [x] **Comparison chains** ([#1855](https://github.com/wado-lang/wado/issues/1855)).
      Unblocks the uniform `0 <= index` bound on the index traits that
      WEP-2026-06-02 Phase D reverted.
- [x] **Structural leaves that lose operands already in scope**: `Cast`,
      `TupleLiteral`, `StructLiteral`, and the index operand of `Index`.
- [x] **Condition-line fidelity.** The line is rendered with the formatter's
      `Unparser`.
- [x] **Receivers**, and the `matches` scrutinee with them. Measured, and not
      captured — see _Deliberately out of scope_. The `Inspect` claim that had
      held receivers back was stale; what actually holds all three is one
      optimizer capability.
- [ ] **Discount a use dominated by `builtin::cold_path()`** in the escape and
      scalarization analyses. The single item the three above wait on, and the
      only thing between this WEP and rendering every operand.
- [x] **Branch-shaped conditions**: `If`, `Match`, `Block`, `LabeledBlock`,
      plus `Range` and `TryOp`.
- [x] **Dump the capture plan** so the covered-forms table is tested rather
      than written down.

### Deliberately out of scope

The children of `Closure`, `WithHandler` and `Resume` are not captured: they
are not values in isolation, and a slot for one would report a sub-expression
that never had a value at the moment the condition failed. The same reasoning
stops the walk at the body of an `If` / `Match` branch and at the statements of
a block: the value of the branch the run took is what that node's own capture
renders, so descending would report the same value twice under a second name.

A `Literal` adds nothing the source text does not already show. An `Assign` or
`CompoundAssign` in a condition is a mutation rather than an operand. A `Spread`
only appears inside a literal this scanner already walks.

A method call's receiver is not captured, and this one is rule 1: value
semantics make the capture a copy, so a `&mut self` method would mutate the copy
and leave the receiver untouched — `assert p.next_if(..) matches { .. }` would
stop advancing `p`. The scanner runs before the method's `self` kind is known,
so it cannot capture only the non-mutating ones.

A projection receiver, a subscript receiver and a `matches` scrutinee are all
uncaptured for one further reason, and it is not a trade against diagnostic
value. Rendering an operand whose value is an aggregate is a _use_ of that
aggregate in the failure branch, and the escape and scalarization analyses count
that use even though `builtin::cold_path()` marks the branch. Measured:
capturing `List<T>::index_value`'s `self` stops const-object globalization, LICM
and array-append collapse; capturing the scrutinee of
`assert ok matches { Ok(6) }` stops variant-return scalarization.

Binding is not what costs — the read is. An intermediate design bound nothing
and re-read the operand in the failure branch instead; the aggregate cases
regressed identically, because the read is the use. One optimizer capability
lifts all three at once: discounting a use dominated by `cold_path()`.

Two narrower exclusions are worth naming because they are not principled, only
unresolved. A bare identifier in call-argument position stays uncaptured because
it may be a function-reference coercion site, and `&<ident>` likewise: the
scanner runs before types are known, so it cannot tell `&value` from `&fn_name`.
`assert takes(&a)` therefore still does not show `a`.

## Consequences

A slot whose operand is a plain binding gets no binding of its own: the failure
branch re-reads it, which straight-line code makes exact. Rule 1 costs one
`bool` flag per conditional capture, cleared ahead of the
condition and set at the capture site. The captured value itself gets no
binding: the failure branch reads its local under the flag, and until the
capture site assigns it the local holds the Wasm default for its type — so
nothing has to synthesize a zero value for an arbitrary `T`. Unconditional
captures — the whole of the stdlib's asserts, and most asserts anywhere — keep
the hoisted `let` and pay nothing new. `-Os` still drops the whole expansion
through `bare-asserts`.

Rule 2 adds one `String` binding per conditional slot to the cold branch, behind
the failure branch that `builtin::cold_path()` already marks.

Rule 3 makes the covered-forms table a compiler artefact. The cost is a dump
surface and its tests; the return is that the next unhandled AST shape is a
compile error or a test failure rather than an assert that quietly reports
less.

Fixing rule 1 changes observable behaviour of existing programs: an assert whose
right operand has a side effect stops running it when the left operand
short-circuits. That is the point — the current behaviour is the bug — but it
means a program that relied on the eager evaluation changes, and e2e fixtures
that print from inside an assert condition move with it.
