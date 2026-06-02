# WEP: Diagnostic Reason Chains for Type and Trait Errors

## Context

Wado source — including this compiler — is increasingly written and repaired
by AI coding agents. The error messages the compiler emits are therefore read
by agents as much as by humans, and the two audiences have different needs.

Krishnamurthi and Flatt, "Type-Error Ablation and AI Coding Agents"
(arXiv:2606.01522, 2026), ran a controlled experiment on an ML-style language
and found:

- More detailed type-error messages measurably improve an agent's ability to
  fix the error. Their four reporting modes ordered strictly by fix rate:
  `untyped` (test failures only) < `min` (conflicting types only) <
  `proximate` (one blame location) < `all` (the full unification stack, which
  is more likely to contain the true cause).
- Type-system feedback beats test-only feedback: a type error is a causal
  signal ("these types conflict, here is why") while a test failure is only a
  symptomatic one.
- The benefit lands on the agent's first hypothesis, not on iterative retries —
  so the cause should be present in the initial message.

An inventory of Wado's diagnostics placed them at the paper's `proximate`
level: a single span plus both the expected and found type names, but no chain
of expressions or causes. Two concrete gaps stood out, both confirmed by
writing and compiling Wado programs:

- Binary-operator type errors were routed through `TypeError::InvalidPattern`,
  which mislabeled them as "invalid pattern:", and were asymmetric — `a - lst`
  reported a `TypeMismatch` while `lst - a` reported an operator error, so the
  same defect read differently depending on operand order (the paper's
  "symmetric chaff" effect).
- Trait-bound failures (`T: Ord` unsatisfied) named only the type and trait,
  never the field whose type breaks the auto-derive. The blame sits at the
  call site, far from the cause.

Unlike the paper's `proximate`-vs-`all` framing, Wado anchors types at function
signatures (parameters require annotations), so ordinary type mismatches are
already local. The places where Wado's cause is genuinely far from the blame
are trait resolution and generic inference — the same territory as
interactive Rust trait-error debuggers.

## Decision

Make diagnostics richer by default. We explicitly reject a separate
"AI mode": a single detailed default serves both audiences and avoids a
configuration surface that agents would have to discover. The MVP ships two
changes that move trait/operator diagnostics from `proximate` toward `all`.

### Operator errors

Add `TypeError::OperatorNotApplicable { op, operands, note }` and route every
operator type error through it. The requires-trait check inspects both
operands, so a pair that cannot combine under the operator produces one
symmetric message naming both types regardless of operand order, e.g.
`` operator `-` cannot be applied to types `i32` and `Array<i32>` ``. The
misleading "invalid pattern:" prefix is gone.

### Trait-bound reason chains

Add `trait_unimpl_reason_chain(type_id, trait_name)`, which walks the `Eq`/`Ord`
auto-derive structure (struct fields, generic-struct fields with type-argument
substitution, and variant payloads), finds the first member that breaks the
trait, and recurses into it. The result is a step-by-step chain, depth-bounded
and cycle-guarded, carried on `TraitBoundNotSatisfied` and rendered by default
as indented `note:` lines beneath the headline at every bound-check site:

```
error: type 'Outer' does not implement trait 'Ord' required by bound on 'T'
  note: `Outer` does not implement `Ord` because field `inner` of type `Inner` does not implement `Ord`
  note: `Inner` does not implement `Ord` because field `f` of type `fn() -> i32` does not implement `Ord`
```

### Rendering mechanism

The MVP folds the chain into the existing `Diagnostic.message` string (the
same pattern as `OperatorNotApplicable`'s `note`). `Diagnostic` is constructed
in ~85 places and carries a single optional span; adding structured
secondary-span notes would touch all of them, so it is deferred until the value
is proven. Substring-matched `compile_error` fixtures keep working because the
headline is unchanged and notes are appended.

## Consequences

Shipped and validated:

- [x] `OperatorNotApplicable`: symmetric, correctly-labeled operator errors.
- [x] Trait-bound reason chains for `Eq`/`Ord` auto-derive (struct, generic
      struct, variant), recursive and on by default at all five bound-check
      sites.
- [x] Unit tests plus E2E fixtures (`operator_not_applicable_*`,
      `trait_bound_reason_*`); no regressions in the `wado-compiler` suite.

Trade-offs and known limits:

- Notes are strings without their own spans; they explain the cause but do not
  point the editor at the offending field's definition.
- Only the `Eq`/`Ord` auto-derive rules are unfolded. A missing user-written
  `impl Trait for T` is not explained beyond "does not implement".
- Generic inference provenance is not tracked: when `T` is inferred to a
  concrete type from an argument, the diagnostic cannot yet say where that
  binding came from (`infer.rs` records `bindings` without source origin).
- `Diagnostic` remains single-span; this WEP does not change that model.

Next steps, in priority order toward the paper's `all` level:

- [ ] Give `Diagnostic` structured notes with their own spans (builder or
      `Default` to avoid editing all construction sites), so a note can point
      at the field definition.
- [ ] Track inference provenance through unification so a `TypeMismatch` from
      generic inference can show where a type variable was bound.
- [ ] Explain a missing user-written `impl` (candidate impls, near-misses).
- [ ] Extend reason chains beyond `Eq`/`Ord` to other structural bounds.
