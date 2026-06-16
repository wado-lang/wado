# WEP: Turbofish Omission via Type Inference

## Context

Wado's turbofish (`f::<T>(...)`) is Rust-compatible, and so is its omission:
a type argument may be left out when inference can recover it. Inference draws
type arguments from three sources:

- forward, from the call's argument types;
- backward, from an LHS annotation / expected return type;
- from an associated-type-equality bound (`I: Iterator<Item = T>`).

The elaborator (`InferCtx`, `wado-compiler/src/elaborator/infer.rs`) already
covers these across free functions, static methods, instance methods, variant
constructors, and builtins. Coverage was confirmed empirically: the basic
matrix passes; the gaps are all backward-flow cases where a sub-expression is
resolved _before_ the expected type that would pin its type parameter becomes
available.

### What works

- [x] forward inference from arguments (all call kinds)
- [x] backward inference from an LHS annotation when the call is the RHS root
- [x] partial turbofish (explicit prefix, inferred tail)
- [x] associated-type-bound-driven inference
- [x] backward inference through the `?` operator (this WEP, below)
- [x] method-chain receiver — `let x: i32 = p.get().unwrap();` (this WEP, below)
- [x] `match` scrutinee — `match m.get() { Some(x) => x, None => 0 }` (method
      scrutinee; this WEP, below)
- [x] binary-op operand — `m.num() + 1` (method operand; this WEP, below)

### Remaining gaps

- [ ] free-function scrutinees / operands — `match none_of() { … }`,
      `make() + 1`. The `match` / binary-op solve points already fire; what is
      missing is _free-function_ deferral (today only generic _method_ calls
      defer). Free-function deferral needs care the method path did not: a
      deferred call's trait-bound check (`make<T: Producer>()`) must be
      re-verified once the hole is solved, and the holey `type_args` vec must
      align with the by-index return-type substitution.
- [ ] deep method chains whose _intermediate_ call does not pin the parameter —
      `gen().filter(..).unwrap()` — currently a clean "cannot infer" error
      (see the taint rule below), not yet inferred

Two look similar but are out of scope:

- `def() as Meters` — Rust does not infer an operand's type _through_ an `as`
  cast either (the source type must be known independently), so erroring here
  is Rust-compatible, not a gap.
- `collect()` into a non-`List` (e.g. `TreeMap`) — `collect` is hard-typed to
  `List<Self::Item>`, a missing generic-`collect` / `FromIterator` feature, not
  an inference gap.

## Decision

### Shipped: inference through `?`

`resolve_question_mark` resolved its operand with `expected = None`, dropping
the LHS type. It now reconstructs the operand's expected type from the
`?`-stripped payload `U` and the enclosing function's return shape — `Option<U>`
or `Result<U, F>` (`F` = the function's return error type) — and threads it into
the operand. The `?` relationship is fixed (it always strips `Option`/`Result`),
so the wrapper is reconstructible without knowing anything method-specific.

Fixture: `tests/fixtures/infer_type_arg_through_question_mark.wado`.

### Shipped: deferred inference via inference holes

The method-chain case (`let x: i32 = p.get().unwrap()`) cannot reuse the `?`
trick: the relationship between `get`'s result and the LHS `i32` runs through
`.unwrap()`, whose effect on types is only known once the receiver type is —
exactly what we are inferring. Re-resolving the receiver to break the cycle is
unsafe (the eager, single-pass elaborator allocates locals in walk order for
parity with `reify`; a second walk shifts that order). The fix is a small
deferred-inference layer (`elaborator::infer_hole`), built so that the recorded
facts a later phase consumes never embed an unknown.

The pieces:

1. Inference hole. A hole is a `TypeParam` minted with a reserved high index
   (`HOLE_INDEX_BASE`), so it reuses the existing unification / substitution
   machinery and never collides with a real type parameter. `reify` reads
   recorded facts rather than re-inferring, so concretising those facts after
   the fact is enough.
2. Deferral. When a generic _method_ call's type parameter stays unbound and no
   expected type is in hand — and the receiver/args are hole-free —
   `infer_method_type_args` mints a hole instead of erroring and lets the holey
   type flow up. The hole-free-receiver guard guarantees the call's recorded
   _mangled name_ carries no hole, so its facts are fixable by a `TypeId`
   substitution alone.
3. Solve before record. At an enclosing call with an expected type
   (`.unwrap()`'s `i32`), `resolve_method_call_with` unifies the holey return
   against the expected, then concretises the receiver/return _before_ the
   mangling + recording below — which embed the receiver type in name strings a
   later sweep could not fix.
4. Taint guard. If a hole still rides the receiver when a call records (its
   intermediate did not pin the parameter, e.g. `gen().filter(..).unwrap()`, or
   a holey binding is reused), the hole is _tainted_: it resolves to a clean
   "cannot infer" error rather than risk a stale mangled name reaching codegen.
5. Module-end finalize. `finalize_infer_holes` raises "cannot infer" for every
   unsolved / tainted hole (same message as the immediate diagnostic, only
   deferred) and substitutes all holes (solved → concrete, otherwise → `error`)
   through every recorded fact map that can carry a `TypeId`.

### Trait-bound enforcement (one rule, no drift)

A deferred parameter's bound is unknown until the hole is solved, so the
solution must be re-verified — `get<T: Producer>()` solved to a non-`Producer`
type would otherwise reach codegen and trap. Adding that check exposed a
pre-existing drift: the trait-bound + associated-type-registration loop was
duplicated across the free-function, static-method, and instance-method call
paths (and the instance path simply omitted it, so a bad method type arg
trapped WIR). All four paths — the three call kinds plus the deferred-hole
re-check — now funnel through one primitive, `enforce_single_bound`
(`type_implements_trait` → register assoc types, else `TraitBoundNotSatisfied`),
driven by the shared `enforce_type_arg_bounds`. Enforcement is _concrete-only_
at the call site: a still-parametric argument is a forwarded generic, verified
when its owner is monomorphized; a hole is verified at finalize. Routing every
path through one primitive is what makes the rule unable to diverge again.

Fixtures: `tests/fixtures/infer_type_arg_through_method_chain.wado`,
`tests/fixtures/infer_type_arg_match_and_binop.wado`.

### Shipped: match scrutinee and binary-op operand (additional solve points)

The same hole flows into a `match` scrutinee and a binary-op operand; two more
solve points pin it:

- `resolve_match_expr`: after the arms resolve, the scrutinee hole has flowed
  through the pattern bindings into the arm bodies (the arm-binding `x` is the
  same `TypeId` as the scrutinee's hole). Solving the holey arm bodies against
  the match's expected type — or a concrete sibling arm — pins it, then the
  arm / scrutinee types are concretised before the result-type selection.
- `resolve_binary`: a binary op's operands share a type, so a holey operand is
  solved against its concrete sibling before operator dispatch (which would
  otherwise mangle a trait-method name against the hole).

Both fire for any deferred hole, so they work today for method-call scrutinees
/ operands. The free-function forms (`match none_of() { … }`, `make() + 1`)
wait on free-function deferral (above).

## Consequences

- The most common real-world omissions work today: `let v: T = call()?`, a
  single-level method chain `let v: T = call().unwrap()` (and `.expect(..)`, or
  a call result passed directly as a typed argument), and a generic method call
  in `match`-scrutinee or binary-operand position.
- Free-function scrutinees / operands and deep chains whose intermediate call
  does not pin the parameter still require a turbofish or an intermediate
  annotated `let` — documented workarounds until the follow-up extends
  deferral to free-function calls.
- Stdlib turbofishes such as `seq.next_element::<Value>()?` are _not_ removable
  by the `?` fix: they bind to a `let` with no annotation, so the turbofish is
  the only place the element type is named. They remain correct as written.
