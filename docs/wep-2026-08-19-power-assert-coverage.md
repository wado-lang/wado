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
This is the whole of the rule. It admits no exception, and it outranks every
rendering concern below — an operand that cannot be captured without breaking it
is not thereby out of scope, it is an operand whose capture is not yet
implemented.

Two corollaries the mechanism keeps getting wrong, so both are stated:

A capture may not reorder. Binding an operand ahead of the condition moves it
ahead of everything the instrumentation left in place — a method call's receiver
is evaluated before its arguments, and a subscript's receiver before its index,
so hoisting either sibling inverts that pair. An operand may be bound ahead of
the condition only when nothing evaluated before it stays behind; otherwise the
capture is taken where the operand sits.

A capture may not copy. Value semantics deep-copy on binding, so a captured
receiver would take the copy's mutation and leave the original untouched. The
answer is to capture without copying — in place, or by re-reading a binding the
failure branch can reach — never to drop the operand and call the gap a
deliberate exclusion.

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

Every operand position in a condition is captured, save what _Known gaps_
lists as not yet reached. A `Literal` is the one position needing no slot of its
own: its value is its source text, which the `condition:` line already shows.
There is no second outcome to report for a
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
block-carrying condition's line breaks collapse to single spaces, so the quote
is one line. Where the formatter drops parentheses — around an `if` used as an
operand — both spellings parse to the same tree, so no fidelity is lost.

## Known gaps

Nothing here is settled by design. Each entry is the mechanism failing rule 1,
or an operand position rule 3 does not yet reach, or a cost not yet paid down —
a defect or an open question, never a boundary.

### Rule 1: the mechanism changes evaluation

- [ ] **Stop hoisting a capture past an operand left in place.** A method call's
      receiver is evaluated before its arguments and a subscript's receiver
      before its index, but neither receiver is captured, so binding the sibling
      ahead of the condition inverts the pair: `assert recv().take(arg())` runs
      `arg` first where `if !recv().take(arg())` runs `recv` first. An operand
      may be bound ahead of the condition only when nothing evaluated before it
      stays behind; otherwise it is captured where it sits, as a conditional
      slot already is. Red: `assert_eval_order_method_receiver`,
      `assert_eval_order_subscript_receiver`.

- [ ] **Capture a receiver and a `matches` scrutinee.** Value semantics copy on
      binding, so a bound receiver takes its own `&mut self` mutation and leaves
      the original untouched. Capturing in place, or re-reading a binding from
      the failure branch, changes no evaluation — the copy is the mechanism's
      choice, not the operand's nature. Deciding which method mutates needs the
      `self` kind, which the scan does not have because it runs ahead of
      resolution: an ordering this design chose and can revisit. Red:
      `assert_gap_method_receiver`, `assert_gap_subscript_receiver`,
      `assert_gap_matches_scrutinee`.

### Rule 3: operand positions that render nothing

- [ ] **Render a closure, `WithHandler` or `Resume` operand.** `Inspect` is
      total, so a closure value has a rendering — `assert apply(|x: i32| -> i32
      { return x * 2; }, n)` could show `|i32| -> i32` and shows nothing today.
      Their _children_ stay unwalked for a separate reason that does hold: a
      sub-expression of a closure body has no value at the moment the condition
      failed. Red: `assert_gap_closure_operand`, which pins the closure half
      only — `WithHandler` and `Resume` are unpinned.

- [ ] **Capture a bare identifier in call-argument position, and `&<ident>`.**
      Either may be a function-reference coercion site, and the scanner runs
      before types are known, so it cannot tell `&value` from `&fn_name`.
      `assert takes(&a)` therefore does not show `a`. Resolving it means
      deciding after types are known. Red: `assert_gap_call_arg_ident`,
      `assert_gap_ref_ident`.

- [ ] **Capture the operands inside the branch a run took.** The scan stops at
      an `If` / `Match` branch body and at a block's statements, on the argument
      that the enclosing node's own capture already renders what the run
      produced. That holds only when the body is a single leaf. Measured:
      `assert (if c { f() + g() } else { 0 }) == 99` renders `c` and the `if`'s
      value `5`, and nothing for `f()`, `g()` or `f() + g()`. What a compound
      body should show — every operand, or only the sub-expression that is the
      body's value — is undecided, so this stays unpinned.

      A `Spread` needs nothing: it only ever sits inside a literal the scan
      already walks.

### Cost

- [ ] **Rematerialize a cold-path use** in the escape and scalarization
      analyses. This one is cost, not correctness, and it is not a trade against
      diagnostic value either. Rendering an aggregate operand is a genuine _use_
      of that aggregate in the failure branch, and the analyses are right to
      count it — `builtin::cold_path()` produces no Wasm and changes no
      semantics, so an analysis that dropped a real use on its word would
      scalarize an aggregate the cold branch still has to read. Measured:
      capturing `List<T>::index_value`'s `self` stops const-object
      globalization, LICM and array-append collapse; capturing the scrutinee of
      `assert ok matches { Ok(6) }` stops variant-return scalarization. Binding
      is not what costs — the read is, so re-reading instead of binding
      regresses identically. What lifts it is letting the hot path scalarize as
      though the cold use were absent and reconstructing at the cold use from
      what survives; power assert makes that cheap, since the failure branch
      wants a rendering and scalarization leaves exactly the fields `Inspect`
      would walk. Unpinned by an e2e fixture: the regression it describes only
      appears once the receiver entry above lands.

## Consequences

A slot whose operand is a plain binding gets no binding of its own: the failure
branch re-reads it, which straight-line code makes exact. A conditional slot
costs one `bool` flag, cleared ahead of the condition and set at the capture
site; the failure branch reads its value local under that flag, so nothing has
to synthesize a zero value for an arbitrary `T`. Unconditional captures keep the
hoisted `let`. `-Os` still drops the whole expansion through `bare-asserts`.

Rule 2 adds one `String` binding per conditional slot to the cold branch. Rule 3
costs a dump surface and its tests, and returns a compile error or a test
failure where an assert would otherwise quietly report less.

Rule 1 is observable: an assert whose right operand has a side effect does not
run it when the left operand short-circuits.
