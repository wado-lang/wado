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
wherever the copy is provably unnecessary. But a copy is not always removed _or_
left intact: `value_copy_demote` rewrites a deep copy whose elements are never
mutated into a shallow spine copy, lowered as a `builtin::array_clone` /
`array_clone_shallow` call on the backing array. So a value copy that survives
optimization appears in the final NIR as one of: a remaining `$value_copy$T(...)`
call, or an `array_clone` / `array_clone_shallow` / `copy_value` call. The remark
collects all of these in entry-module functions. (`array_copy` is excluded — it
is bulk buffer movement inside stdlib helpers like `String::push`, not a
value-semantic copy.) Actual shipped output, where `b` is a `List<i32>` copied
then mutated:

```
src.wado:5:5: info: remark: a copy of `List<i32>` survives optimization
```

Why the final NIR and not the final WIR: the WIR is the last IR before codegen
and would be the most authoritative "this copy executes" signal, but `WirInstr`
carries no per-instruction span (only function-level `WirMeta` does), so a
WIR-sourced remark could not point at the source. The optimized NIR is the last
IR with per-expression spans, and `wir_build` lowers these copies one-to-one, so
NIR is both span-accurate and faithful. Even so the synthesized copy nodes
(`array_clone`, demoted spine copies) carry no user span themselves, so the
remark is anchored to the enclosing statement — the `let mut b = a;` that performs
the copy. The reference escape hatch (`&T` / `&mut T`) is the only construct that
shares rather than copies (spec.md); naming it as a suggestion is deferred to the
read-only-vs-required classification below.

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
- Surfaced as an info-level `remark:` diagnostic through the existing logger, not
  a dedicated flag. It rides `--log-level`, which is the opt-in mechanism: the
  CLI is quiet by default (default `warn`), so remarks appear under
  `wado compile --log-level info`. This resolves the original opt-in goal without
  a new configuration surface — the remark keeps its semantically-correct `Info`
  severity, and the standard verbosity knob gates it. The never-vague principle
  still holds: a remark that cannot supply why + where is not emitted.
- Never vague: a remark that cannot supply why + where stays silent, since the
  paper shows vague remarks are net-negative.

### MVP scope

Shipped: surviving value copies. `remarks::collect_value_copy_remarks` walks the
optimized NIR's entry-module functions, collects residual `$value_copy$T` calls
and `array_clone` / `array_clone_shallow` / `copy_value` calls, and emits one
info-level remark per survivor anchored to the enclosing statement.
`logger.remark` carries the span and the entry filename; tests
(`wado-compiler/tests/remarks.rs`) pin the behavior at `-O2` — a `List<i32>`
copied then mutated fires one remark on the copy line; a `Point` that SROA
scalarizes fires none. No size threshold yet: a synthesized copy helper only
exists for non-trivial aggregates, so survivors are already non-trivial.

Next: failed-SROA remarks, reusing the SROA passes' existing hard-escape
classification for the _why_ and the escape site for a second span.

### Reusing existing infrastructure

Remarks are diagnostics with spans, so they reuse the rendering path the
Diagnostic Reason Chains WEP already exercises (headline + indented `note:` /
`suggestion:` lines). That WEP's open follow-up — structured, independently-
spanned notes on `Diagnostic` — is a shared dependency: a failed-SROA remark
naming both the allocation and its escape site needs two spans, the same
capability the type/trait reason chains want. Building it once serves both.

## Consequences

Shipped:

- [x] Remark surface: info-level `remark:` diagnostic via `logger.remark`, gated
      by `--log-level` (no dedicated flag). `Code::Remark`.
- [x] Final-NIR walk (`remarks::collect_value_copy_remarks`) over entry-module
      functions, detecting `$value_copy$T` and `array_clone` / `array_clone_shallow`
      / `copy_value` survivors, anchored to the enclosing statement span.
- [x] Surviving value-copy remarks with why + where, plus the survives / elided
      test pair at `-O2` (`wado-compiler/tests/remarks.rs`).

Next:

- [ ] Failed-SROA remarks: surface the surviving allocation and reuse the SROA
      passes' hard-escape classification for the cause and escape-site span.
- [ ] Classify each survivor's actionability (read-only copy → suggest `&`;
      removable escape → suggest restructuring; otherwise explain why the cost is
      required) so a suggestion is offered only when a fix would actually help.
- [ ] Broaden beyond the entry module to all user (non-stdlib) modules.

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
