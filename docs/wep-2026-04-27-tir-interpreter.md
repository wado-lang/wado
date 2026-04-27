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

### Stage 0 — split & rename (this commit)

- `wado_compiler::tiri` exists with `Value`, `Interpreter`,
  `reduce(&TirExpr) -> TirExpr`, `reduce_local(&mut TirExpr) -> bool`,
  and `reduce_to_value(&TirExpr) -> Option<Value>`.
- `const_folding.rs` is a 30-line visitor.
- Identity simplification for `&&` / `||` lives in tiri, not the visitor.
- Integration tests at `wado-compiler/tests/tiri.rs` cover the four
  arithmetic ops on i32/i64/u8/u32/f32/f64 plus `reduce`-API contracts
  (repr preservation, short-circuit, binary collapse).

Out of scope at Stage 0 (matches the previous behaviour, deferred to
later stages): float-to-int and int-to-float casts (only int-to-int
casts fold), heap-allocated `Value` payloads, and any cross-function
reasoning. Stage 1 and onward extend the engine; Stage 0 is purely a
relocation + API reshape with zero behaviour change.

### Stage 1 — local environment + memoization

- Add `env: HashMap<LocalId, Value>` to `Interpreter` for `let`-bound
  constants.
- Add `memo: HashMap<ExprKey, Option<Value>>` so each TIR node is
  evaluated at most once per reduce call. Mirrors rustc's `dataflow_const_prop`
  (`PLACE_LIMIT = 100`) and LLVM's `ValueTracking` per-call recursion
  cap (`MaxAnalysisRecursionDepth = 6`).
- Subsumes part of `const_propagation` for in-function constants.

### Stage 2 — `if` / `match` reduction

- Track `block_executable: BitSet` and `feasible_edge` à la SCCP so
  `if true { … } else { … }` collapses without a separate pass.
- Reduce both arms when the condition is non-constant; if both arms
  reduce to structurally-equal values, drop the branch.
- Subsumes part of `const_branch_prune`.

### Stage 3 — pure call inlining (in-process)

- When all args of a call reduce to constants and the callee is pure
  (effect set ⊆ pure), recursively reduce the callee body in a child
  interpreter with a fresh `env` and a `step_budget: u32`.
- Bail to the original `Call` expression on out-of-budget.
- Mirrors rustc's CTFE step counter (`LINT_TERMINATOR_LIMIT`).

### Stage 4 — bounded loop unrolling

- For `while` / `loop` with a constant trip count, unroll within the
  shared `step_budget`. Expressions that don't terminate within the
  budget are left as residuals.

### Stage 5 — wasm-CTFE backend (via `CompilerHost`)

- `wado-compiler` itself must compile to `wasm32-unknown-unknown` (CI
  enforces this), so it cannot link `wasmtime` directly. The CTFE
  backend follows the existing Kiln pattern instead: extend
  `CompilerHost` with a `run_compile_time_eval(component_wasm, args)`
  hook (mirroring today's `run_generator`). Hosts that have a Wasm
  runtime — `wado-cli` via `wasmtime` — implement it; LSP and
  browser hosts return `Unsupported` and tiri stays in-process.
- tiri compiles a pure callee to wasm using the existing pipeline,
  caches the resulting component bytes per
  `(FunctionKey, monomorph_args)`, and hands them to the host with
  the constant args. The host runs the component (with fuel /
  resource limits of its choice) and returns the result.
- Triggered when in-process reduction exceeds `step_budget` but the
  callee is still pure-by-effects.
- Result is decoded back into a `Value` and used as if the in-process
  evaluator had produced it.
- Enables `compute_lookup_table(256)` and similar workloads at compile
  time without re-implementing every TIR construct, and without
  breaking wasm32 compatibility of `wado-compiler`.

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
and instantiation overhead (~µs-ms). Both amortize with the per-`(fn,
monomorph)` `Module` cache, so a `fib(_)` invoked 100 times incurs one
codegen.

## Determinism

- Float NaN bits are nondeterministic in wasm; `non_nan_float` already
  refuses to fold NaN-producing arithmetic. Keep that for both backends.
- `v128` / relaxed-SIMD has implementation-defined corner cases;
  defer SIMD CTFE to a later WEP.
- Integer wrapping / signed `MIN / -1` semantics match Wasm.

## Consequences

- `const_folding.rs` shrinks to a thin glue file, and stays that way.
- `tiri` may eventually subsume `const_propagation`, `const_branch_prune`,
  and parts of `inline` — to be evaluated when each stage lands.
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

- Should `tiri` own the lattice value type (`Bottom`/`Const`/`Top`) or
  keep returning `Option<Value>` and let callers track lattice state?
  The Stage 1 prototype will inform this.
- Where does the wasm CTFE module cache live — per `compile` invocation
  or per process? Per-invocation is simpler; per-process speeds up
  watch-mode workflows but needs eviction.
