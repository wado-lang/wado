# WEP: Optimizer Remarks for Missed Optimizations

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
  hallucinations; a precise remark naming the obstacle and both source locations
  let the agent fix the source.
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

## The flagship costs: residual aggregates

Value semantics is Wado's defining feature: assignment, parameter passing, and
return all deep-copy the value, and aggregates (structs, tuples, `List<T>`) are
GC-managed heap objects (spec.md). Two of the optimizer's heaviest jobs exist to
remove the hidden cost of that model — redundant deep copies, and the
allocations themselves — and most of the time they succeed. The cost that
remains is invisible at the source: `f(x)` looks free, and a struct literal looks
like a register's worth of work, whether or not either survives as a runtime
copy or heap allocation.

Both costs share the structure the principle wants: the optimizer represents each
as a concrete construct in the IR and tries to remove it, so a _survivor_ is an
observable fact in the final NIR, rare because the easy cases are already gone,
and self-retiring as the optimizer matures. They become the first two remark
kinds.

### Surviving value copies

A deep copy is a call to a synthesized `$value_copy$T` helper
(`FunctionKind::ValueCopy`), and the `value_copy_elide` pass strips the wrapper
wherever the copy is provably unnecessary. A copy that survives optimization is
therefore a remaining `$value_copy$T(...)` call in the final NIR. The remark
collects those survivors for aggregate types above a size threshold and maps each
back to its source span (proposed output, not yet implemented):

```
remark: a deep copy of `Matrix` survives optimization here [stats.wado:12:18]
  note: `frobenius` receives `Matrix` by value; the copy could not be elided
  suggestion: take `m: &Matrix` if the callee only reads it
```

The _why_ is the surviving copy, the _where_ is the call site, and the _how_
applies when the value is read-only: the reference escape hatch (`&T` / `&mut T`)
is the only construct that shares rather than copies (spec.md).

### Surviving aggregate allocations (failed SROA)

Scalar Replacement of Aggregates (`sroa`, `container_sroa`, `field_scalarize`,
`sroa_param`) dissolves a struct or tuple into individual scalar locals, removing
the GC heap allocation entirely — by the pass's own note, "the single most
impactful optimization for WasmGC-targeting compilers." It is gated by escape
analysis: an aggregate used only for field access is scalarized; one with _soft_
escapes (call argument, return, nested literal) is scalarized with reconstruction
at the escape site; one with a _hard_ escape (address taken, closure capture,
bare local assignment, reference stored) is left as a heap allocation.

The observable survivor is an aggregate allocation that remains in the final IR.
The remark reports it with the hard-escape reason the pass already classified —
so the _why_ is the analysis fact, not a guess:

```
remark: `Point` stays heap-allocated here; SROA could not scalarize it [path.wado:30:13]
  note: the value escapes by being captured in a closure at path.wado:33:20
  suggestion: pass the fields the closure needs instead of capturing `Point`
```

Because SROA already distinguishes hard from soft escapes, the remark can name
the precise escape that blocked it, and can stay silent on soft escapes that were
scalarized anyway.

## Decision

Introduce optimizer remarks, with residual value-copy and failed-SROA remarks as
the first two kinds.

- Bind to the semantic cost model — Wado's aggregate copies and allocations —
  and derive firing from facts observable in the final NIR (residual
  `$value_copy$T` calls; aggregate allocations the SROA passes left in place),
  not from any pass's internals.
- Opt-in, not on by default. Errors name one rejected program and are rich by
  default (the Diagnostic Reason Chains stance); a cost remark is advisory and
  could be plentiful, so it is gated behind an explicit surface (proposed:
  `--remarks` on `wado compile`, plus `wado dump` exposure). Within that surface
  the default-rich, never-vague principle holds in full.
- Never vague: a remark that cannot supply why + where stays silent, since the
  paper shows vague remarks are net-negative.

### MVP scope (proposed)

- Start with surviving value copies: walk the final NIR, collect residual
  `$value_copy$T(...)` calls for aggregate types above a size threshold, emit one
  remark per survivor with its source span. Surfacing the survivor set with spans
  is already the valuable information.
- Then add failed-SROA remarks, reusing the SROA passes' existing hard-escape
  classification for the _why_ and the escape site for a second span.
- E2E fixtures pin each at a fixed `-Ox`: a fixture where the cost survives
  asserts the remark fires; a fixture where the optimizer removes it asserts it
  does not.

### Reusing existing infrastructure

Remarks are diagnostics with spans, so they reuse the rendering path the
Diagnostic Reason Chains WEP already exercises (headline + indented `note:` /
`suggestion:` lines). That WEP's open follow-up — structured, independently-
spanned notes on `Diagnostic` — is a shared dependency: a failed-SROA remark
naming both the allocation and its escape site needs two spans, the same
capability the type/trait reason chains want. Building it once serves both.

## Consequences

This WEP is design-stage; nothing below is implemented yet.

- [ ] Decide the remark surface: `--remarks` on `wado compile`, `wado dump`
      exposure, and/or `--log-level` integration.
- [ ] Define how the final-NIR walk collects residual `$value_copy$T` calls and
      surviving aggregate allocations with source provenance and a size threshold.
- [ ] MVP: residual value-copy remarks with why + where, plus the survives /
      elided fixture pair at a fixed `-Ox`.
- [ ] Failed-SROA remarks: surface the surviving allocation and reuse the SROA
      passes' hard-escape classification for the cause and escape-site span.
- [ ] Classify each survivor's actionability (read-only copy → suggest `&`;
      removable escape → suggest restructuring; otherwise explain why the cost is
      required) so a suggestion is offered only when a fix would actually help.

Trade-offs and boundaries:

- Pure observability is valuable even without a suggestion: knowing which copies
  and allocations remain is information the source cannot otherwise reveal.
- Self-retiring as the optimizer matures is a feature, not a regression: the
  remarks report residual cost, so their disappearance means the cost is gone.
- Not every survivor is fixable; some copies are semantically required, and some
  escapes are inherent. The MVP surfaces the fact; the actionability
  classification above withholds the suggestion rather than guessing.
- The paper's specific task (auto-vectorization) is intentionally out of scope:
  Wado leaves SIMD to the JIT. Only the remark mechanism transfers.
- Rejected alternatives, recorded for design history: inlining remarks (fail
  durable / actionable / reliable, above); loop-allocation hoisting remarks
  (bind to the LICM pass rather than a language-level semantic cost, so they
  violate the principle).
