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
_optimization remarks_ — the compiler's feedback about why a desired
optimization did **not** fire — rather than hard errors. Driving an agent to
restructure C loops for auto-vectorization on the TSVC suite, it found:

- Remarks are the bottleneck, not the model. Adding the compiler's stock
  vectorization remarks raised the success rate ~3.3x for the same small model;
  hand-written remarks that name the exact data-flow obstacle and prescribe a
  fix added a further 40–59 points.
- The split is precise-vs-vague, and vague is _worse than nothing_. A remark
  like "unsafe dependent memory operations in loop" triggered
  semantic-breaking hallucinations — the agent mangled program logic trying to
  satisfy a constraint it could not see. A precise remark —
  "assumed write-after-read dependence between `a[i+1]` and `a[i]`; suggestion:
  use a temporary" — carries the dependence kind, both source locations, and an
  actionable transformation, and the agent fixes the source.
- The three ingredients of a useful remark: the **why** (the analysis fact that
  blocked the pass), **where** (file:line:col for each participant), and a
  **prescriptive suggestion** (the source change that would unblock it).

This is the same meta-principle as the Diagnostic Reason Chains WEP — precise,
causal, prescriptive feedback by default; vague feedback is actively harmful —
applied to a subsystem that WEP does not touch. Type/trait diagnostics report
_errors_: the program is rejected. Optimizer remarks report _missed wins_: the
program compiles and runs correctly but slower or larger than the source
structure could allow. Different audience need (performance, not correctness),
different subsystem (the NIR/WIR optimizer, not the type checker), so a separate
WEP rather than an extension of the existing one.

### Where Wado stands today

The optimizer (see [optimizer.md](./optimizer.md)) runs a 23-pass NIR
fixed-point loop plus WIR-level passes, every one of which can silently decline
to fire on a given construct: a function over the inline threshold, a value copy
not elided because an alias guard tripped, LICM refusing to hoist a possibly
mutated field, SROA giving up on an escaping aggregate. None of this is
observable. The compiler emits no remark for any missed optimization; the only
trace of the concept in the codebase is one incidental `// missed optimisation`
comment in `licm.rs`. An agent (or human) tuning a hot Wado function has no
signal for _why_ a pass did not engage or _what to change_, which is exactly the
gap the paper measures.

Wado's optimizer philosophy ("prefer a native Wasm instruction over a complex
transformation; let the runtime JIT do low-level work like vectorization")
means the paper's literal task — coaxing the compiler to auto-vectorize — does
not transfer: Wado deliberately leaves SIMD to the JIT. What transfers is the
_remark mechanism_ applied to the passes Wado does own, the ones whose firing is
a direct function of source structure and value-semantics shape: inlining, value-
copy elision/demotion, SROA / container SROA, LICM, dead-argument/return
elimination. These are precisely the passes where a small source rewrite flips
the outcome, so a prescriptive remark has a concrete fix to name.

## Decision

Introduce **optimizer remarks**: an opt-in channel in which a pass that declines
a profitable transformation emits a structured, source-located, prescriptive
note. Adopt the paper's three principles as hard requirements — every remark
carries the blocking analysis fact (why), a span for each participant (where),
and a concrete source-level suggestion (how) — and the corollary: a pass that
cannot meet all three stays silent rather than emit a vague remark, because the
paper shows vague remarks are net-negative.

### Opt-in, unlike error diagnostics

The Diagnostic Reason Chains WEP made errors rich _by default_ and rejected an
"AI mode". Remarks take the opposite default: **off unless requested**. The
reasoning is not inconsistent — it follows the same goal of maximizing signal.
An error names one rejected program; a missed-optimization remark could fire on
many call sites of every compile, and on-by-default noise would bury the rare
actionable remark. LLVM gates the analogue behind `-Rpass`; Wado gates it behind
an explicit surface (proposed: a `--remarks[=pass,...]` flag on
`wado compile`, and exposure through `wado dump`). Within that surface, the
default-rich, never-vague principle holds in full.

### MVP scope (proposed)

Prove the mechanism on one pass before generalizing. Function inlining is the
best first target: its decision is a single legible predicate (callee cost vs.
the level's inline threshold), the fix is unambiguous, and the threshold is
already opt-level-driven. A declined inline would emit, for example (proposed
output, not yet implemented):

```
remark: `parse_header` was not inlined into `decode` [hot.wado:42:5]
  note: callee cost 21 exceeds the -O2 inline threshold (13)
  suggestion: split `parse_header`, raise the level to -O3, or mark it for inlining
```

The remark names the callee and call site (where), the cost-vs-threshold fact
that blocked it (why), and the levers that would change the outcome (how) —
never a bare "could not inline".

### Reusing existing infrastructure

Remarks are diagnostics with spans, so they should reuse the rendering path the
Diagnostic Reason Chains WEP already exercises (headline + indented `note:` /
`suggestion:` lines) rather than invent a parallel one. That WEP's open
follow-up — give `Diagnostic` structured, independently-spanned notes — is a
shared dependency: a remark that points at both the call site and the callee
definition needs multi-span notes, the same capability type/trait reason chains
want for pointing at an offending field. Building it once serves both.

## Consequences

This WEP is design-stage; nothing below is implemented yet.

- [ ] Decide the remark surface: `--remarks[=pass,...]` flag on `wado compile`,
      `wado dump` exposure, and/or `--log-level` integration.
- [ ] Define a `Remark` representation (pass name, participant spans, the
      blocking fact, the suggestion) and where passes emit it without threading
      it through every transformation.
- [ ] MVP: inlining-declined remarks with the three required ingredients,
      plus E2E fixtures asserting remark text at a fixed `-Ox`.
- [ ] Extend to the source-structure-sensitive passes (value-copy elision,
      SROA / container SROA, LICM) once the inlining MVP validates the shape.

Trade-offs and boundaries:

- The paper's specific task (compiler auto-vectorization) is intentionally out
  of scope: Wado leaves SIMD to the runtime JIT. Only the remark _mechanism_
  transfers, applied to Wado's own passes.
- Off-by-default is a deliberate divergence from the errors-by-default stance of
  the Diagnostic Reason Chains WEP, justified by remark volume; the never-vague
  principle is shared, not divergent.
- Multi-span structured notes on `Diagnostic` are a shared prerequisite with the
  Diagnostic Reason Chains WEP's next steps; this WEP does not duplicate that
  work, it depends on it.
- Risk: a remark that misstates why a pass declined is worse than none (the
  paper's central finding). Each remark must be derived from the pass's actual
  decision predicate, not a heuristic guess, and is gated on a test asserting it
  fires exactly when the pass declines for that reason.
