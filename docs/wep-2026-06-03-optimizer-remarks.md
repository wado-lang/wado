# WEP: Optimizer Remarks for Residual Value-Copy Costs

## Context

Wado source — including this compiler — is increasingly written and repaired by
AI coding agents. [WEP: Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)
acted on this for _correctness_ feedback: it adopted the finding (Krishnamurthi
and Flatt, "Type-Error Ablation and AI Coding Agents", arXiv:2606.01522) that
detailed, causal, source-located error messages measurably raise an agent's
first-try fix rate, and made type/trait diagnostics richer by default.

A second paper extends the same lesson to a different compiler subsystem.
"AI Coding Agents Need Better Compiler Remarks" (arXiv:2604.13927, 2026) studies
_optimization remarks_ — the compiler's feedback about a desired optimization —
rather than hard errors. Driving an agent to restructure C loops for
auto-vectorization on the TSVC suite, it found:

- Remarks are the bottleneck, not the model. Adding the compiler's stock
  vectorization remarks raised the success rate ~3.3x for the same small model;
  hand-written remarks that name the exact obstacle and prescribe a fix added a
  further 40–59 points.
- The split is precise-vs-vague, and vague is _worse than nothing_. A remark
  like "unsafe dependent memory operations in loop" triggered semantic-breaking
  hallucinations; a precise remark naming the dependence kind and both source
  locations let the agent fix the source.
- The three ingredients of a useful remark: the _why_ (the fact that blocked the
  optimization), _where_ (file:line:col for each participant), and a
  _prescriptive suggestion_ (the source change that would help).

This is the same meta-principle as the Diagnostic Reason Chains WEP — precise,
causal, prescriptive feedback by default; vague feedback is actively harmful —
applied to a subsystem that WEP does not touch. Type/trait diagnostics report
_errors_: the program is rejected. Optimizer remarks report _costs_: the program
is correct but pays a runtime price the source structure could avoid. Different
audience need (performance, not correctness), different subsystem (the optimizer,
not the type checker), so a separate WEP.

## What transfers, and the design principle it forces

The paper's literal task does not transfer. It coaxes a compiler into
auto-vectorizing; Wado's optimizer philosophy is to leave low-level work like
SIMD to the runtime JIT (see [optimizer.md](./optimizer.md)). What transfers is
the _remark mechanism_, and choosing where to point it surfaces a sharp design
constraint, because Wado's optimizer is still developing and remarks must not be
written to fit its current internals.

Inlining was the obvious first candidate and is rejected. It fails three tests a
remark must pass:

- _Durable_ — it must not bind to optimizer heuristics that are in flux.
  Inlining is governed by a cost-vs-threshold heuristic that will keep changing.
- _Actionable_ — there must be a clear source change. "Too big to inline" leaves
  the author with no reliable move.
- _Reliable_ — applying the change must reliably help. An inline hint exists
  already and does not reliably make code faster.

Inlining misses all three. The principle that survives the bar:

> A remark should bind to the language's semantic cost model, not to an optimizer
> heuristic, and it should fire from a fact observable in the final IR, not from a
> pass's internal decision.

Binding to the final IR is what decouples the remark from the immature
optimizer. The remark machinery reads the optimizer's _output_ — a stable
interface — never its pass internals. As the optimizer improves and removes a
cost, the fact disappears from the final IR and the remark self-retires. That is
the correct behavior, and it directly answers "don't write code to fit the
current optimizer."

## The flagship case: residual value-copy costs

Value semantics is Wado's defining feature: assignment, parameter passing, and
return all deep-copy the value; references (`&T`, `&mut T`) are the only escape
hatch (spec.md). A large part of the optimizer exists precisely to remove these
hidden copies, and most of them are in fact removed. Concretely, a deep copy is
a call to a synthesized `$value_copy$T` helper (`FunctionKind::ValueCopy`), and
the `value_copy_elide` pass strips the wrapper wherever the copy is provably
unnecessary.

The problem is observability. While coding, you cannot see which copies the
optimizer eliminated and which survived. `f(x)` looks free; whether it
deep-copies a whole aggregate at runtime is invisible at the source. This is the
gap an agent (or human) tuning a hot Wado function falls into — not "I wrote a
copy" (the optimizer usually erases that) but "I have no way to know which copies
actually remain."

A residual-copy remark closes exactly that gap. After optimization, a deep copy
that survived is literally a remaining `$value_copy$T(...)` call in the final
NIR. The remark walks the final NIR, collects those survivors for aggregate types
above a size threshold, and maps each back to its source span. The design is
self-justifying:

- Low-noise by construction — the optimizer has already erased the easy copies,
  so a survivor is rare and meaningful.
- Self-decoupling — fewer survivors as the optimizer matures; the remark count
  tracks genuine residual cost rather than today's heuristics.
- Wado-specific — it teaches the cost model agents most often mispredict when
  they carry priors from borrow-checked or reference-default languages.

Its primary value is observability: the residual-copy set is information
otherwise unobtainable during coding. Where the copied value is only read, the
remark can also prescribe the reference escape hatch.

Example (proposed output, not yet implemented):

```
remark: a deep copy of `Matrix` survives optimization here [stats.wado:12:18]
  note: `frobenius` receives `Matrix` by value; the copy could not be elided
  suggestion: take `m: &Matrix` if the callee only reads it
```

This carries the why (a deep copy of `Matrix` remains), the where (the call
site), and a how that applies when the value is read-only — never a bare "copy
not elided".

## Decision

Introduce optimizer remarks, with residual value-copy remarks as the first kind.

- Bind to the semantic cost model; derive firing from facts observable in the
  final NIR — residual `$value_copy$T` calls — not from any pass's internals.
- Opt-in, not on by default. Errors name one rejected program and are rich by
  default (the Diagnostic Reason Chains stance); a cost remark is advisory and
  could be plentiful, so it is gated behind an explicit surface (proposed:
  `--remarks` on `wado compile`, plus `wado dump` exposure). Within that surface
  the default-rich, never-vague principle holds in full.
- Never vague: a remark that cannot supply why + where stays silent, since the
  paper shows vague remarks are net-negative.

### MVP scope (proposed)

- Walk the final NIR; collect residual `$value_copy$T(...)` calls for aggregate
  types above a size threshold; emit one remark per survivor with its source
  span. Surfacing the survivor set with spans is already the valuable
  information; this is the MVP.
- E2E fixtures pin the behavior at a fixed `-Ox`: one fixture where a copy
  survives asserts the remark fires, one where the optimizer elides it asserts
  it does not.

### Reusing existing infrastructure

Remarks are diagnostics with spans, so they reuse the rendering path the
Diagnostic Reason Chains WEP already exercises (headline + indented `note:` /
`suggestion:` lines). That WEP's open follow-up — structured, independently-
spanned notes on `Diagnostic` — is a shared dependency: a remark that points at
both the call site and the parameter definition needs multi-span notes, the same
capability the type/trait reason chains want. Building it once serves both.

## Consequences

This WEP is design-stage; nothing below is implemented yet.

- [ ] Decide the remark surface: `--remarks` on `wado compile`, `wado dump`
      exposure, and/or `--log-level` integration.
- [ ] Define how the final-NIR walk collects residual `$value_copy$T` calls with
      source provenance and a size threshold.
- [ ] MVP: residual value-copy remarks with why + where, plus the survives /
      elided fixture pair at a fixed `-Ox`.
- [ ] Classify each survivor (read-only → suggest `&`; mutated / stored /
      returned → explain why the copy is required) so the suggestion is offered
      only when a reference would actually do.

Trade-offs and boundaries:

- Pure observability is valuable even without a suggestion: knowing which copies
  remain is information the source cannot otherwise reveal.
- Self-retiring as the optimizer matures is a feature, not a regression: the
  remark reports residual cost, so its disappearance means the cost is gone.
- Not every survivor is reference-fixable; some copies are semantically required
  (an independent mutable value, a stored or returned aggregate). The MVP
  surfaces the fact; the read-only-vs-required classification is the follow-up
  above, and until it lands the suggestion is withheld rather than guessed.
- The paper's specific task (auto-vectorization) is intentionally out of scope:
  Wado leaves SIMD to the JIT. Only the remark mechanism transfers.
- Rejected alternatives, recorded for design history: inlining remarks (fail
  durable / actionable / reliable, above); loop-allocation hoisting remarks
  (bind to the LICM pass rather than a language-level semantic cost, so they
  violate the principle).
