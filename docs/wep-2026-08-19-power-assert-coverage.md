# WEP: Power-Assert Coverage

## Context

`assert` exists to show the values behind a failed condition, and it cannot be
disabled, so a Wado program's assertion diagnostic is the one debugging aid that
is always present. Its instrumentation is a read-only scanner picking the
sub-expressions worth quoting, plus a capture mechanism that must not disturb
what the condition does.

Both halves are easy to get wrong in ways nothing reports. A capture hoisted
ahead of the condition changes what the condition evaluates; a shape the scanner
was never taught renders no operand, and a condition that shows nothing is
indistinguishable from one that has nothing to show.

## Decision

Three rules, in priority order. Each is a property of the whole `assert`
statement, testable per condition form.

### 1. The condition runs as if the `assert` were not there

`assert e` evaluates `e` exactly as `if !e` written by hand would: the same
sub-expressions, on the same objects, in the same order, no more and no fewer.
It admits no exception and outranks every rendering concern below: an operand
that cannot be captured without breaking it is one whose capture is not yet
implemented.

Two corollaries:

A capture may not reorder. Binding an operand ahead of the condition moves it
ahead of everything the instrumentation left in place — a method call's receiver
is evaluated before its arguments, and a subscript's receiver before its index,
so hoisting either sibling inverts that pair. An operand may be bound ahead of
the condition only when nothing evaluated before it stays behind; otherwise the
capture is taken where the operand sits.

The scanner walks the condition in evaluation order carrying one fact: whether
everything so far is bound ahead of the condition. While it holds, the next slot
binds ahead too, as `let __vK = …;` — a scope the failure branch reaches, and an
ordinary binding to the optimizer. The first fragment left behind clears it, and
every later slot is captured where it sits; a receiver and a callee clear it for
the operands under them, though the node containing them may still bind ahead,
since that moves the whole group and nothing within it. A place passes the fact
through — the failure branch re-reads it — save against an operand that may
write through it: a `&mut` borrow, a method call (`&mut self` is not knowable
this early), or a subtree the scan does not enter.

That is also what lets a receiver render. Binding one copies, and value
semantics would leave the call's own mutation on the copy; re-reading a place
copies nothing, so a place receiver is captured as any other place. An argument
is the same: a bare identifier there is a function-reference coercion site the
scan cannot recognise and a binding would lose, while a place leaves it as
written. Only `&<ident>` keeps the `&` itself unbound, the place under it
rendering instead.

A capture may not copy. Value semantics deep-copy on binding, so a captured
receiver would take the copy's mutation and leave the original untouched; the
answer is to capture in place, or by re-reading a binding the failure branch
reaches.

Short-circuits are the same corollary seen from the other side. A capture is
**unconditional** when no short-circuit lies between it and the condition root. A
capture below one is **conditional** and is taken where the operand sits, so the
short-circuit still decides whether it runs. The boundaries are the right operand
of `&&` and `||`, and every operand of a comparison chain past the first
comparison (`a < b < c` runs as `(a < b) && (b < c)`).

### 2. Rendering reports reach, not just value

A conditional slot the run never reached renders `<label>: <not evaluated>`, not
silence. Where evaluation stopped answers why the assert failed as often as any
value does, and an omitted line would be indistinguishable from an operand the
instrumentation cannot see — which rule 3 forbids.

Each conditional slot's text is chosen in the failure branch, from a flag saying
its capture site ran. The choice is on the cold path, so a passing assert pays
nothing for it.

### 3. No silent degradation

Every operand position is captured, save what _Known gaps_ lists as not yet
reached. A literal needs no slot: its value is its source text, which the
`condition:` line already shows, and a cast or negation of one renders that text
back. `&<ident>` earns none for the `&` itself, the place under it rendering
instead. Nor is there a second outcome to report for a type: `Inspect` is total
(WEP-2026-06-25), so every operand has a rendering, and one the compiler
declines to inspect is a bug in `Inspect` derivation — never a power-assert
degradation to document.

So one failure mode remains: the scanner does not descend into a shape. Two
things keep it from recurring silently. The scanner's match is exhaustive over
`Expr`, so a new variant is a compile error at the decision point rather than a
fall into a leaf arm. And the plan is dumpable — `wado dump --assert-plan`
prints, for every `assert`, which operands it captures and whether a
short-circuit can skip each one:

```
6: assert i < list.len() && list[i] == 1
  __v0  always       re-read   i
  __v1  always       re-read   list
  __v2  always       hoisted   list.len()
  __v3  always       hoisted   i < list.len()
  __v4  conditional  in-place  i
  __v5  conditional  in-place  list[i]
  __v6  conditional  in-place  list[i] == 1
```

`tests/integration/assert_capture_plan.rs` reads that back for one `assert` per
condition shape, so what each shape covers is a test rather than a paragraph
that goes stale.

### The `condition:` line is source, not a paraphrase

It is rendered by the formatter's `Unparser`, not by `unparse_expr_simple`,
whose readability-over-fidelity trade stays right for its own callers. A
block-carrying condition's line breaks collapse to single spaces, so the quote
is one line. Where the formatter drops parentheses — around an `if` used as an
operand — both spellings parse to the same tree, so no fidelity is lost.

## Known gaps

Each entry is the mechanism failing rule 1, or an operand position rule 3 does
not yet reach — a defect or an open question, never a boundary.

### Rule 1: the mechanism changes evaluation

- [ ] **Capture a receiver that is not a place.** A place receiver renders
      today, the failure branch re-reading it. Every other receiver — `f().m()`,
      `(a..<b).contains(&a)` — renders nothing, and needs the value kept without
      a copy. Unpinned; the fixtures that pinned it now pin the place half:
      `assert_method_receiver`, `assert_subscript_receiver`,
      `assert_matches_scrutinee`.

### Rule 3: operand positions that render nothing

- [ ] **Render a `WithHandler` or `Resume` operand.** A closure renders its
      signature now (`assert_closure_operand`); these two render nothing. Their
      _children_ stay unwalked for a reason that does hold: a sub-expression of
      a body has no value at the moment the condition failed. Unpinned.

- [ ] **Capture the operands inside the branch a run took.** The scan stops at
      an `If` / `Match` branch body and at a block's statements, since the
      enclosing node's capture already renders what the run produced — which
      holds only for a single-leaf body. Measured: `assert (if c { f() + g() }
      else { 0 }) == 99` renders `c` and the `if`'s value `5`, nothing for
      `f()`, `g()` or `f() + g()`. What a compound body should show is
      undecided, so this stays unpinned.

      A `Spread` needs nothing: it only ever sits inside a literal the scan
      already walks.

## Consequences

A slot whose operand is a plain binding gets none of its own: the failure branch
re-reads it. A conditional slot costs one `bool` flag, cleared ahead of the
condition and set at the capture site, so nothing has to synthesize a zero value
for an arbitrary `T`. A slot captured where it sits writes a function-scoped
slot the optimizer cannot fold through, so such a condition survives to run time
even when its value is constant; one bound ahead folds. `-Os` still drops the
whole expansion through `bare-asserts`.

Rule 2 adds one `String` binding per conditional slot to the cold branch. Rule 3
costs a dump surface and its tests, and returns a compile error or a test
failure where an assert would otherwise quietly report less.

Rule 1 is observable: an assert whose right operand has a side effect does not
run it when the left operand short-circuits.
