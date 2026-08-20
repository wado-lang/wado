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

| Condition form                                                                    | Operand lines rendered                                           |
| --------------------------------------------------------------------------------- | ---------------------------------------------------------------- |
| `Ident`, `Binary`, `Unary`                                                        | captured; operands recursed                                      |
| `Call`, `StaticMethodCall`                                                        | captured whole; arguments recursed, except a bare-ident argument |
| `MethodCall`                                                                      | captured whole; receiver and arguments recursed                  |
| `FieldAccess`                                                                     | captured whole; receiver recursed                                |
| `Index`                                                                           | captured whole; receiver and index operand recursed              |
| `TemplateString`                                                                  | captured whole                                                   |
| `ComparisonChain`                                                                 | captured; operands recursed                                      |
| `Cast`                                                                            | captured; operand recursed                                       |
| `TupleLiteral`, `StructLiteral`                                                   | not captured (shape comes from the expected type); elements recursed |
| `Matches`                                                                         | captured; scrutinee recursed                                     |
| `If`, `Match`, `Block`, `LabeledBlock`, `Range`                                   | **nothing**                                                      |
| `TryOp`, `Spread`, `Closure`, `WithHandler`, `Resume`, `Assign`, `CompoundAssign` | **nothing**                                                      |

The table is the state after this WEP's roadmap; the rows still reading
**nothing** are what it has left to do.

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

The scanner's own comments claim otherwise, and that is what kept receivers
uncaptured: they say `Fn<…>` and CM resource handles have no `Inspect`. The
claim is stale — `${f:?}` on a closure prints `|i32| -> i32` today.

So one cause remains: the scanner does not descend into a shape. That is a bug
against this WEP, closed by teaching the scanner, not by reporting. The
enumeration in the table above is a compiler artefact, not folklore: the capture
plan for a condition is dumpable, so the covered-forms list is a test rather
than a paragraph that goes stale.

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
missing line, and a missing line outranks a misrendered one.

- [x] **P0 — short-circuit preservation.** Conditional slots per rule 1, for
      `&&` and `||`. Closed the wrong-code bug; nothing below is safe to build
      on an expansion that moves operands.
- [x] **`<not evaluated>` rendering** (rule 2), landed with it: without it a
      short-circuited slot would have quoted the value it never took.
- [x] **Comparison chains** ([#1855](https://github.com/wado-lang/wado/issues/1855)).
      Unblocks the uniform `0 <= index` bound on the index traits that
      WEP-2026-06-02 Phase D reverted.
- [x] **Structural leaves that lose operands already in scope**: `Cast`,
      `TupleLiteral`, `StructLiteral`, `Matches` (the scrutinee), and the index
      operand of `Index`.
- [x] **Condition-line fidelity.** The line is rendered with the formatter's
      `Unparser`.
- [x] **Receivers** of `MethodCall`, `FieldAccess` and `Index`. The `Inspect`
      claim that had held them back was stale, and nothing else stood behind
      it.
- [ ] **Branch-shaped conditions**: `If`, `Match`, `Block`, `LabeledBlock`.
      Arms are conditional slots, so this rests on the P0 item.
- [ ] **Dump the capture plan** so the covered-forms table is generated and
      tested rather than written down.

### Deliberately out of scope

The children of `Closure`, `WithHandler` and `Resume` are not captured: they
are not values in isolation, and a slot for one would report a sub-expression
that never had a value at the moment the condition failed. The closure itself is
an operand like any other and is captured where it appears.

## Consequences

Rule 1 costs one `bool` flag per conditional capture, cleared ahead of the
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
surface and its tests; the return is that the next unhandled AST shape is a test
failure rather than an assert that quietly reports less.

Fixing rule 1 changes observable behaviour of existing programs: an assert whose
right operand has a side effect stops running it when the left operand
short-circuits. That is the point — the current behaviour is the bug — but it
means a program that relied on the eager evaluation changes, and e2e fixtures
that print from inside an assert condition move with it.
