# WEP: TIR Interpreter (`tiri`) Evolution Plan

## Context

The Wado optimizer relies on constant folding to reduce literal-only
expressions to a single literal node. Until now the folding logic lived
inside `optimize/const_folding.rs` as a hand-rolled set of
operator helpers, mixed with the TIR visitor that drove it.

This commit splits the engine out into a new top-level module,
`wado_compiler::tiri` ("TIR Interpreter"), exposing
`Interpreter::reduce(&TirExpr) -> TirExpr` as the canonical API.
`const_folding.rs` becomes a thin visitor that delegates each visited
node's local rewrite to `Interpreter::reduce_local`.

This WEP records the planned trajectory so future contributors don't
have to re-litigate the design every time we want to fold a richer
expression.

## Decision

`tiri` evolves into a **partial evaluator** for TIR. The public surface
keeps a single shape — `reduce(&TirExpr) -> TirExpr` — and each stage
extends what kinds of expressions can be reduced. Beyond a single-process
interpreter, a complementary **wasm-CTFE backend** runs full pure
function calls through `wasmtime`, leveraging Wado's effect system as a
type-checked purity gate.

### Why two backends

|              | tiri (in-process)                                  | wasm execution                                          |
| ------------ | -------------------------------------------------- | ------------------------------------------------------- |
| Sweet spot   | `2+3 → 5`, identity simplification, branch pruning | `fib(20)`, lookup-table generation, full pure-call CTFE |
| Cost / call  | µs, memoizable                                     | ms (codegen + instantiate), amortized via module cache  |
| Partial eval | Yes (residuals)                                    | No (whole-call)                                         |
| Coverage     | Whatever we hand-write                             | All of Wado, for free                                   |

These are complementary, not alternatives. tiri stays sub-quadratic and
fine-grained; wasm execution covers anything tiri would balk at.

## Stages

### Stage 0 — split & rename

Status: done.

- [x] `wado_compiler::tiri` exists with `Value`, `Interpreter`,
      `reduce(&TirExpr) -> TirExpr`, `reduce_local(&mut TirExpr) -> bool`,
      and `reduce_to_value(&TirExpr) -> Option<Value>`.
- [x] `const_folding.rs` is a thin visitor.
- [x] Identity simplification for `&&` / `||` lives in tiri, not the
      visitor.
- [x] Integration tests at `wado-compiler/tests/tiri.rs` cover the four
      arithmetic ops on i32/i64/u8/u32/f32/f64 plus `reduce`-API
      contracts (repr preservation, short-circuit, binary collapse).

Out of scope at Stage 0 (matches the previous behaviour, deferred to
later stages): float-to-int and int-to-float casts (only int-to-int
casts fold), heap-allocated `Value` payloads, and any cross-function
reasoning. Stage 1 and onward extend the engine; Stage 0 is purely a
relocation + API reshape with zero behaviour change.

### Stage 1 — local environment + lattice

Status: done.

- [x] Replace `Option<Value>` with a 3-state lattice. `Option<Value>`
      conflated four distinct meanings (unevaluated, const, non-const,
      unsupported-op), which made memoization unsafe to add later —
      caching `None` couldn't distinguish "we know it's non-const" from
      "we haven't computed it yet". The lattice fixes this at the type
      level:

      ```rust
      pub enum Lattice {
          Unevaluated,    // = SCCP Bottom: not yet computed / unreachable
          Const(Value),   // provably this value
          NonConst,       // = SCCP Top: non-constant (or modelled-out op)
      }
      ```

      Names favour readability over the academic `Bottom` / `Top`.
      Comments in `tiri.rs` reference the SCCP lattice for readers
      familiar with the abstract-interpretation literature.

- [x] `reduce_to_lattice(&TirExpr) -> Lattice` is the canonical engine
      API. Callers that only need "is this a literal?" use
      `Lattice::as_const() -> Option<Value>` — kept as a _projection_,
      not a separate function, so the lattice is always the source of
      truth.
- [x] `env: IndexMap<LocalId, Lattice>` on `Interpreter` for `let`-bound
      constants. Immutable bindings are captured as `Const(v)` when the
      RHS reduces; `let mut` and assignments invalidate to `NonConst`.
      Reads of `TirExprKind::Local` consult `env`.
- [x] Subsumes part of `const_propagation` for in-function constants.

### Stage 1.5 — memoization (deferred)

Status: deferred to Stage 3.

- [ ] Add `memo: HashMap<ExprKey, Lattice>` once cross-context
      re-evaluation actually exists (pure call inlining). In Stage 1's
      pure bottom-up walk every node is visited exactly once, so the
      memo carries no payoff and risks invariant drift.
- [ ] When introduced, only `Const(v)` and structurally-final `NonConst`
      results are cached. `Unevaluated` is the cache-miss sentinel by
      construction. "Unsupported-op `NonConst`" is **not** memoized so
      model extensions don't leave stale entries — `NonConst` is cheap
      to recompute (one op-match), so the precision gain outweighs the
      cache hit.

### Stage 2 — `if` reduction

Status: done. `match` reduction is split into its own Stage 2.5 below;
payload-aware variant matching is deferred to Phase B/C of that stage,
since the lattice work for variant payloads is more involved than scalar
`if`.

- [x] `Lattice::join` (SCCP join over the chain
      `Unevaluated ⊑ Const(v) ⊑ NonConst`):

      ```text
      Unevaluated ⊔ x       = x        (infeasible-edge identity)
      Const(v)    ⊔ Const(v) = Const(v) (arms agree)
      Const(a)    ⊔ Const(b) = NonConst (a ≠ b)
      NonConst    ⊔ _        = NonConst (Top is absorbing)
      ```

      Commutative, associative, idempotent. Tests verify each property.

- [x] Constant-condition expr-form `if` (`if true { A } else { B }` →
      `Block(A)`, `if false { A } else { B }` → `Block(B)`,
      `if false { A }` no-else → `Unit`). The unreachable arm is treated
      as an SCCP infeasible edge: its lattice value never enters the
      join, so a trapping `else { panic(…) }` does not contaminate the
      result.
- [x] Constant-condition stmt-form `if` (block-level splice via new
      `Interpreter::reduce_local_block`).
- [x] Both-arms-equal collapse: when the condition is non-constant but
      effect-free (`is_speculatable`) and both arms reduce to the same
      `Const(v)`, the `if` is rewritten to that literal. The
      "effect-free" gate is conservative — literals, locals, captures,
      arithmetic / comparison / bitwise binary, non-trapping unary,
      casts, and field accesses on the above. Calls / division / deref
      / mutation are excluded.
- [x] `expr_to_lattice(If { … })` returns the SCCP lattice value of the
      `if`: chosen-arm value when the condition is constant, joined arm
      values when not. Crucially, `Unevaluated` arms in the
      non-constant-condition path are promoted to `NonConst` before the
      join — under a non-constant condition the arm IS reachable, so
      "we don't know its value" is SCCP-Top, not infeasible.
- [x] Subsumes the constant-condition cases of `const_branch_prune`
      (both expr-form and stmt-form). The legacy pass now only handles
      trivial-block / labeled-block simplifications and keeps a doc
      pointer at the top of `const_branch_prune.rs` redirecting future
      `if`-related rewrites to tiri. Removing those branches and
      observing every existing fixture still pass is the equivalence
      proof.
- [x] `wado-compiler/tests/tiri.rs` covers `Lattice::join`, the
      feasible-edge constant-true / constant-false / no-else cases, the
      both-arms-equal collapse, the structurally-unequal-arms negative
      case, the Unevaluated-arm regression, and the stmt-form splice
      (true / false-no-else / non-const-untouched).

### Stage 2.5 — `match` reduction

Status: Phase A done. Phases B and C are scoped but not implemented.

Match reduction is split into three phases by pattern shape, in
increasing order of representation cost. The split lets us land
scalar / payload-free matching today without committing to a heap-aware
[`Value`] type that the WEP otherwise reserves for Stage 4+.

#### Phase A — payload-free patterns (done)

- [x] `Interpreter::match_lattice` — for a constant scrutinee, walk
      arms in source order; the first definite-`Yes` arm contributes
      its body's lattice (chosen-arm, infeasible-edge for later arms).
      An earlier `Unknown` arm (unmodelled pattern, guarded arm,
      unanalyzable `ConstantValue`) cannot be ruled out, so every arm
      from that point on participates in the join. Non-constant
      scrutinee → join all arm bodies, with `Unevaluated → NonConst`
      promotion (same fix as the `if` non-const-condition path).
- [x] `Interpreter::pattern_matches` — three-state
      `PatternMatch { Yes, No, Unknown }`. Phase A handles `Wildcard`,
      `Literal(I128 | U128 | Bool | Char)`, `Or`, `Range` (signed and
      unsigned, integer and char), and `ConstantValue` whose inner
      expression reduces to a primitive `Value`. `Binding`, `Tuple`,
      `Variant`, `Enum`, `Struct`, and string / null literal patterns
      report `Unknown` so they never wrongly commit a match (`Yes`)
      and never wrongly drop a later arm (`No`).
- [x] `rewrite_match_expr` — two rewrites:
      1. **Const scrutinee**: replace the `Match` with
      `Block { stmts: [Expr(arm.body)] }` for the first definite-`Yes`
      arm (mirrors the `if true` → `Block(then_branch)` shape so the
      outer visitor walks the residual normally). Bails on any
      earlier `Unknown` so the original trap-on-no-match behaviour
      is preserved.
      2. **All-arms-equal collapse**: when the scrutinee is non-constant
      but speculatable (same `is_speculatable` gate as the `if`
      rule), every arm has no guard, every arm body reduces to the
      same `Const(v)`, **and the match is provably exhaustive**
      (`is_provably_exhaustive`: an unguarded `Wildcard` / `Binding`
      arm, or an `Or` containing one), rewrite the whole match to
      that literal. Without the exhaustiveness gate the rewrite
      would drop the lowering's implicit `Unreachable` fallback
      trap — Wado's resolver checks exhaustiveness for
      `bool` / `enum` / `variant` / range-covered `int` but skips
      `struct` / `string` / `tuple`, so the gate is load-bearing.
- [x] `match_lattice` applies the same exhaustiveness gate before
      returning a non-`NonConst` lattice for a non-const scrutinee,
      and bails to `NonConst` when no definite-`Yes` arm is found
      under a const scrutinee. Without these, an enclosing `if`
      both-arms-equal collapse would pick up the `Const(v)` and
      erase the trap on the lattice's behalf.
- [x] `reduce_in_place` recurses into the scrutinee, every arm guard,
      and every arm body so the visitor-driven path sees fully-folded
      operands at each match node.
- [x] Unit tests at `wado-compiler/tests/tiri.rs` cover: literal-arm
      first-match selection, wildcard fallthrough, char and range
      patterns (inclusive / exclusive bounds, signed / unsigned mix,
      char codepoint ordering), or-patterns (match / no-match / mixed),
      `ConstantValue` (definite Yes / No / Unknown), guard handling
      (no-fold under const scrut, no-pickup of later arm), unmodelled
      patterns (Tuple → Unknown → Match left intact),
      Unevaluated-arm regression under non-constant scrutinee,
      env-resolved local scrutinee, first-match wins on overlap, and
      visitor-driven `reduce_local` rewrites. Single e2e fixture
      `tiri_match_const_fold.wado` checks observable end-to-end fold
      (constant-scrutinee match's chosen arm body survives at -O2,
      non-chosen arms are gone).

#### Phase B — definite-no enum / variant tag pruning (deferred)

- [ ] Extend `pattern_matches` to handle `Enum { case_index }`
      patterns when the scrutinee is structurally an `EnumConstruct`
      (no [`Value`] enrichment needed — peek the TIR shape at match
      time). Same trick for `Variant { variant_name }` against a
      `VariantConstruct` scrutinee.
- [ ] Lets the engine drop arms that are definitely infeasible
      (`match Color::Red { Color::Green => …, … }`) without committing
      to a full payload model. Useful for inlining `Option::unwrap` and
      similar "scrutinee constructed locally" idioms.

#### Phase C — payload-aware variant matching (deferred)

- [ ] Add `Value::Enum { case_index, type_id }` (or equivalent) and
      `Value::Variant { tag, payload: Box<Value> }`, opening
      [`Value`] to a heap-aware shape. This crosses the "primitive
      only" line currently held through Stages 1-3, so it should be
      gated on a real consumer (Stage 3 inlining producing residual
      matches over `Option<i32>`).
- [ ] Add `Binding` pattern handling: introduce the bound name as a
      `Const(payload)` entry in a child `env` while reducing the arm
      body, then unbind on exit. Mirrors how Rust's MIR const-eval
      handles variant scrutinees.
- [ ] At this point `value_to_expr_kind` needs a fallible variant
      that can report "not representable as a primitive literal" when
      asked to materialize an `Enum` / `Variant` value back into TIR
      (we don't always have the case_name handy). The all-arms-equal
      collapse skips those cases.

### Stage 3 — pure call inlining (in-process)

- [ ] When all args of a call reduce to constants and the callee is
      pure (effect set ⊆ pure), recursively reduce the callee body in a
      child interpreter with a fresh `env` and a `step_budget: u32`.
- [ ] Bail to the original `Call` expression on out-of-budget. Mirrors
      rustc's CTFE step counter (`LINT_TERMINATOR_LIMIT`).

### Stage 4 — bounded loop unrolling

- [ ] For `while` / `loop` with a constant trip count, unroll within the
      shared `step_budget`. Expressions that don't terminate within the
      budget are left as residuals.

### Stage 5 — wasm-CTFE backend (via `CompilerHost`)

- [ ] `wado-compiler` itself must compile to `wasm32-unknown-unknown`
      (CI enforces this), so it cannot link `wasmtime` directly. Extend
      `CompilerHost` with `run_compile_time_eval(component_wasm, args)`
      (mirroring today's `run_generator`). Hosts with a Wasm runtime —
      `wado-cli` via `wasmtime` — implement it; LSP and browser hosts
      return `Unsupported` and tiri stays in-process.
- [ ] tiri compiles a pure callee to wasm using the existing pipeline,
      caches the resulting component bytes per
      `(FunctionKey, monomorph_args)`, and hands them to the host with
      the constant args. The host runs the component (with fuel /
      resource limits of its choice) and returns the result.
- [ ] Triggered when in-process reduction exceeds `step_budget` but
      the callee is still pure-by-effects. The result decoded back
      into a `Value` is used as if the in-process evaluator had
      produced it.
- [ ] Enables `compute_lookup_table(256)` and similar workloads at
      compile time without re-implementing every TIR construct, and
      without breaking wasm32 compatibility of `wado-compiler`.

## Cost model

The in-process engine stays sub-quadratic by following the patterns
production compilers use:

- **Sparse, on-demand evaluation** — `reduce` is called only on nodes
  the visitor touches; results are memoized per pass.
- **Finite-height lattice** — `Bottom` / `Const(v)` / `Top`. Each cell
  transitions a bounded number of times → `O(N × height)` total work.
- **Worklist with dependent re-queueing** — added in Stage 1; an
  expression re-evaluates when one of its inputs becomes constant,
  not on every fixed-point iteration. This is what lets LLVM's
  `InstCombine` settle in a single iteration (LLVM D154579).
- **No internal fixed-point loop in tiri** — each invocation is
  monotone (only `Top → Const` transitions). The optimizer's outer
  loop is the one fixed-point.

The wasm backend's cost is dominated by module compilation (~1-100 ms)
and instantiation overhead (~µs-ms). Both amortize with the
per-`(fn, monomorph)` `Module` cache, so a `fib(_)` invoked 100 times
incurs one codegen.

## Determinism

- Float NaN bits are nondeterministic in wasm; `non_nan_float` already
  refuses to fold NaN-producing arithmetic. Keep that for both backends.
- `v128` / relaxed-SIMD has implementation-defined corner cases;
  defer SIMD CTFE to a later WEP.
- Integer wrapping / signed `MIN / -1` semantics match Wasm.
- Stage 2 caveat — float signed-zero folding: the both-arms-equal
  `if`-collapse uses [`Lattice`]'s derived `PartialEq`, which delegates
  to f64's IEEE 754 `==`. That treats `-0.0` and `+0.0` as equal, so
  `if cond { -0.0 } else { 0.0 }` collapses to `-0.0` (the chosen
  representative is the then-arm's bit pattern). Operations that
  observe the sign of zero — `1.0 / x` (signed infinity) and explicit
  `f64::is_sign_positive` — see the folded representative rather than
  the cond-dependent value. This matches IEEE 754 equality semantics
  and the golden fixtures encode the resulting WIR. If a future caller
  needs bit-precise zero distinction here, add a per-op equality
  predicate to `Value` rather than weakening the fold globally.

## Consequences

- `const_folding.rs` shrinks to a thin glue file, and stays that way.
- `tiri` may eventually subsume `const_propagation`, `const_branch_prune`,
  and parts of `inline` — to be evaluated when each stage lands.
- Stage 2.5's match-fold is observable in the TIR optimize phase even
  though `lower_patterns` runs before `optimize`: not every match is
  desugared by `lower_patterns` (some shapes survive to optimize
  intact), and Stage 3's pure-call inlining will produce fresh match
  expressions when inlining `Option` / `Result` accessors. Phase A
  handles both paths today; Phase B/C extend coverage as those
  scenarios materialize.
- `wasmtime` is **not** linked by `wado-compiler` (the crate must build
  for `wasm32-unknown-unknown`). The wasm-CTFE backend instead routes
  through `CompilerHost`, just like Kiln generator execution does today.
  Hosts without a Wasm runtime (LSP, browser) decline the call and
  tiri falls back to in-process reduction — no `cfg`-gated backend in
  the compiler crate itself.
- Effect-system guarantees become load-bearing for CTFE soundness. A bug
  in effect inference could cause non-pure functions to be CTFE-evaluated.
  This is the same trust we already place in effect-check elsewhere.

## Out of scope

- User-facing CTFE syntax (`#[const_eval]`, `const fn`). Decide later
  once we see real usage demand.
- Heap-allocated values (`Array`, `String`) in the in-process `Value`
  type — Stages 1-3 are primitive-only. The wasm backend doesn't need
  this since it returns whatever wasm returns.
- Salsa-style demand-driven reanalysis across compiler runs.

## Open questions

- Where does the wasm CTFE module cache live — per `compile` invocation
  or per process? Per-invocation is simpler; per-process speeds up
  watch-mode workflows but needs eviction.

## Resolved questions

### Lattice ownership vs. `Option<Value>`

Resolved as part of Stage 1: tiri owns
`Lattice { Unevaluated, Const(Value), NonConst }`. `Option<Value>` is
dropped — it conflated four meanings, making memoization unsafe to add
later. Lattice is exposed via `reduce_to_lattice`; the
`Lattice::as_const()` projection covers the simple "is this a literal?"
case without re-introducing the ambiguity at the API surface.
