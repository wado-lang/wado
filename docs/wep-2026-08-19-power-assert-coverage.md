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

### What is covered today

Read from the scanner at `3de0afb35` and checked by running one program per
form:

| Condition form                                                                    | Operand lines rendered                                           |
| --------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| `Ident`, `Binary`, `Unary`                                                        | captured; operands recursed                                      |
| `Call`, `StaticMethodCall`                                                        | captured whole; arguments recursed, except a bare-ident argument |
| `MethodCall`                                                                      | captured whole; arguments recursed; **receiver not**             |
| `FieldAccess`                                                                     | captured whole; **receiver not recursed**                        |
| `Index`                                                                           | captured whole; **receiver and index operand not recursed**      |
| `TemplateString`                                                                  | captured whole                                                   |
| `ComparisonChain`                                                                 | **nothing** — `0 <= index < used` renders no operand             |
| `Cast`                                                                            | **nothing** — `x as i64 == y` renders `y` and loses `x`          |
| `TupleLiteral`                                                                    | **nothing** — `[a, b] == [3, 4]` renders no operand              |
| `StructLiteral`                                                                   | **nothing** — `P { x: x } == P { x: 2 }` renders no operand      |
| `Matches`                                                                         | **nothing** — `s matches { Point }` loses the scrutinee          |
| `If`, `Match`, `Block`, `LabeledBlock`, `Range`                                   | **nothing**                                                      |
| `TryOp`, `Spread`, `Closure`, `WithHandler`, `Resume`, `Assign`, `CompoundAssign` | **nothing**                                                      |

Six forms render no operand line at all when one of them is the condition:
`ComparisonChain`, `TupleLiteral`, `StructLiteral`, `Matches`, `If` and `Match`.
`Cast` drops the operand on its side of a comparison and prints the
other. The three receiver rows drop a value that is in scope and inspectable.

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
comparison chain after the first, and the arms of an `if` / `match` used as an
expression.

### 2. Rendering reports reach, not just value

A conditional slot the run never reached renders `<label>: <not evaluated>`,
not silence. Where evaluation stopped is the answer to why the assert failed as
often as any value is, and an omitted line would be indistinguishable from an
operand the instrumentation cannot see — which rule 3 exists to forbid.

This makes the failure message a sequence of per-slot statements rather than one
template. That path is cold; the cost is irrelevant and the constraint that
forced a single template goes away with it.

### 3. No silent degradation

Every operand position in a condition is either captured, or named by the
compiler as not captured with the reason. A form the scanner has not been taught
must not reach an opaque-leaf arm by default.

Two causes are distinguished, because they need different answers:

- **Structural** — the scanner does not descend into the shape. This is a bug
  against this WEP, closed by teaching the scanner, not by reporting.
- **Type** — the operand's type has no `Inspect` (`Fn<…>`, a CM resource
  handle). Nothing can be rendered, and rejecting the assert would be worse than
  the missing line. The slot is dropped and an optimizer-style remark
  (WEP-2026-06-03) names the operand, its type, and the trait that is missing.
  A remark, not a diagnostic: the stdlib asserts on receivers routinely, and a
  warning on every one of them would train the reader to stop looking.

The enumeration in the table above is a compiler artefact, not folklore: the
capture plan for a condition is dumpable, so the covered-forms list is a test
rather than a paragraph that goes stale.

### The `condition:` line is source, not a paraphrase

It is rendered by a fidelity-preserving unparse — parentheses where the parse
needed them, no statement punctuation invented inside an expression — separate
from `unparse_expr_simple`, whose readability trade stays right for its own
callers.

## Roadmap

Ordered by yield per cost, and rules 1–3 are why: a wrong-code bug outranks a
missing line, and a missing line outranks a misrendered one.

- [ ] **P0 — short-circuit preservation.** Conditional slots per rule 1, for
      `&&` and `||`. Closes the wrong-code bug; nothing below is safe to build
      on an expansion that moves operands. Fixture: the guarded-index assert
      above must report the assertion, not trap in `List::index_value`.
- [ ] **Comparison chains** ([#1855](https://github.com/wado-lang/wado/issues/1855)).
      Scan `first` and each comparison's `right`; operands after the first are
      conditional slots. Unblocks the uniform `0 <= index` bound on the index
      traits that WEP-2026-06-02 Phase D reverted.
- [ ] **`<not evaluated>` rendering** (rule 2). Needed to read the output of the
      two items above; folded in with them if the message rebuild lands first.
- [ ] **Structural leaves that lose operands already in scope**: `Cast`,
      `TupleLiteral`, `StructLiteral`, `Matches` (the scrutinee), and the index
      operand of `Index`. Each is a recursion the scanner does not do; no new
      machinery.
- [ ] **Condition-line fidelity.** A fidelity-preserving unparse for the
      `condition:` line, with the parenthesised and block-expression cases as
      format fixtures.
- [ ] **Receivers** of `MethodCall`, `FieldAccess` and `Index`. Deliberately
      skipped today because capturing one forces `Inspect` on the receiver's
      type; gated on the type-cause remark below, which is what makes the
      skip visible instead of silent.
- [ ] **Type-cause remark** (rule 3). Names the operand, its type and the
      missing trait.
- [ ] **Branch-shaped conditions**: `If`, `Match`, `Block`, `LabeledBlock`.
      Arms are conditional slots, so this rests on the P0 item.
- [ ] **Dump the capture plan** so the covered-forms table is generated and
      tested rather than written down.

### Deliberately out of scope

`Closure`, `WithHandler` and `Resume` capture nothing and stay that way: their
children are not values in isolation, and a slot for one would report a
sub-expression that never had a value at the moment the condition failed.

## Consequences

Rule 1 costs an `Option<T>` slot and an in-place write per conditional capture.
Unconditional captures — the whole of the stdlib's asserts, and most asserts
anywhere — keep the hoisted `let` and pay nothing new. `-Os` still drops the
whole expansion through `bare-asserts`.

Rule 2 replaces the single panic template with a statement sequence in the cold
branch. That is more TIR per assert, all of it behind the failure branch that
`builtin::cold_path()` already marks, and it is what makes conditional slots
reportable at all.

Rule 3 makes the covered-forms table a compiler artefact. The cost is a dump
surface and its tests; the return is that the next unhandled AST shape is a test
failure rather than an assert that quietly reports less.

Fixing rule 1 changes observable behaviour of existing programs: an assert whose
right operand has a side effect stops running it when the left operand
short-circuits. That is the point — the current behaviour is the bug — but it
means a program that relied on the eager evaluation changes, and e2e fixtures
that print from inside an assert condition move with it.
