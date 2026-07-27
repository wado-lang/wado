# WEP: NIR Interpreter (`niri`) Evolution Plan

## Context

The Wado optimizer needs to reduce expressions the source made constant —
literal arithmetic, a branch whose condition is known, a pure call with
constant arguments — to the value they denote. `niri` ("NIR Interpreter") is
the engine that answers what a NIR expression evaluates to at compile time.
Constant folding is its primary consumer; branch pruning, constant
propagation, and compile-time function evaluation reuse it.

This WEP records the trajectory so contributors don't re-litigate the design
each time we want to fold a richer expression. It states capabilities, not
mechanisms: what `niri` can and cannot evaluate. How it does so is the code's
business.

## Decision

`niri` is a **partial evaluator** for NIR: it reduces what it can and leaves
a residual otherwise. Beyond the in-process engine, a complementary
**wasm-CTFE backend** runs full pure calls through a real Wasm runtime, using
Wado's effect system as a type-checked purity gate.

### Why two backends

|              | niri (in-process)                                  | wasm execution                                          |
| ------------ | -------------------------------------------------- | ------------------------------------------------------- |
| Sweet spot   | `2+3 → 5`, identity simplification, branch pruning | `fib(20)`, lookup-table generation, full pure-call CTFE |
| Cost / call  | µs                                                 | ms (codegen + instantiate), amortized via module cache  |
| Partial eval | Yes (residuals)                                    | No (whole-call)                                         |
| Coverage     | Whatever we hand-write                             | All of Wado, for free                                   |

These are complementary, not alternatives. `niri` stays cheap and
fine-grained; wasm execution covers anything `niri` balks at.

### Scope boundary against the ValueGraph

`niri` evaluates **pure values**: given an expression and what is known about
its inputs, what does it denote. The engine's `ValueGraph` owns everything
**flow-sensitive**: reaching definitions, branch merges, loop and heap-write
invalidation, field store-to-load seeding.

The boundary is load-bearing. `niri` once carried its own per-local map of
known fields, with branch-merge and loop-invalidation logic; it was built and
then retired once the `ValueGraph` covered the same ground. Anything that
needs to know _which_ definition reaches a use belongs to the `ValueGraph`,
and a proposal to teach `niri` about control flow between statements should be
read as a sign the fact belongs on the other side of this line.

### Effects are the purity gate

CTFE soundness rests on effect inference: a function admitted for
compile-time evaluation is one the effect system called pure. A bug there
lets an impure function be evaluated at compile time. This is the same trust
already placed in effect checking elsewhere.

## Done

Value model:

- Integer, float, bool and char scalars, with the arithmetic, comparison,
  bitwise and unary operators over them, and casts between them.
- Structs and tuples whose every field is constant, and field reads
  projecting back out of them — out of a literal, a local, an immutable
  global, or a compile-time call result. Aggregates never leave the engine;
  what reaches the IR is the scalars projected out of them.
- A three-state lattice — unevaluated, constant, non-constant — with a join,
  so an unreachable branch contributes nothing to the result and a trapping
  arm does not contaminate a fold.

Bindings:

- Immutable locals whose initializer is constant read back as that constant.
  Mutable and assigned locals are non-constant.
- Immutable globals whose initializer is constant fold at every read, as do
  the known constant fields of a global (a sequence global's length).
- An aggregate constant binds to a local only when every mention of that local
  merely reads it. `niri` models whole values, not the heap, so a local
  another handle can write through would go stale.

Control flow:

- `if`, expression and statement form: a constant condition collapses to the
  chosen arm; a side-effect-free condition whose arms denote the same constant
  collapses to that constant.
- `match`: a constant scrutinee selects the first arm that provably matches;
  a provably exhaustive match whose every arm denotes the same constant
  collapses to it. The exhaustiveness requirement is what keeps the implicit
  no-match trap alive.
- Patterns decided: wildcard, binding, integer / bool / char literal, range,
  or-patterns, a constant-valued pattern, struct patterns, and exact-arity
  tuple patterns. A definite field mismatch rules an arm out even when a
  sibling field binds.
- An arm's guard and body are evaluated with that arm's bindings in scope.
- `match X { Enum::Case => true, _ => false }` — the shape
  `X matches { Case }` desugars to — collapses to a discriminator comparison
  before the match ever reaches pattern lowering.

Calls:

- A free call whose arguments are all constant and whose callee is pure,
  non-async and monomorphic evaluates at compile time when the callee's body
  is a single expression. Recursion and total work are bounded. A body that
  does not reduce to a constant leaves the call in place, so a runtime trap
  inside it stays observable.
- A string literal's `len()` folds, as a consequence of the generic
  struct-field projection rather than any string-specific rule.

## TODO

### Values the engine cannot represent

- Enum and variant values with their payloads. Today an enum or variant
  pattern cannot be decided, so an `Option` / `Result` accessor exposed by
  inlining leaves a residual match the engine walks past.
- Sequence values — the array backing `String` and `List`. Without them an
  element cannot be projected out of an array literal or an indexed global
  read, and two literal strings cannot be compared (a string pattern reaches
  the engine as a guard, so deciding it is the same problem).

### Calls

- Multi-statement bodies: a `let` sequence, an early return. Only a
  single-expression body folds today, which excludes most functions outright
  and every body that pattern lowering desugared into statements — aggregate
  parameters included.
- Method calls, excluded because a `&mut self` receiver mutates through the
  call. Worth revisiting now that mod-ref and alias analysis can prove a
  receiver is not written.
- Closure calls: an indirect call whose closure is known at the call site is
  never resolved to a direct call, so neither inlining nor CTFE can reach
  through it.
- Recursion beyond a base case. The wasm-CTFE backend below is the intended
  answer.

### Control flow

- A `switch` with a constant scrutinee is not folded, although `if` and
  `match` are. Since a switch is formed before inlining, a scrutinee that
  inlining makes constant survives to the end untouched.
- Loops with a constant trip count, unrolled within the work budget.
  Expressions that don't terminate within it stay residual.
- Guards decided when the engine is only asked what an expression denotes;
  today an arm's bindings are only in scope on the rewriting path.

### Aggregate globals and element projection

Motivated by
[Constant Object Globalization](./wep-2026-05-31-const-object-globalization.md),
which turns read-only constant aggregates into immutable module-scope globals
and needs `niri` to see through them so the fold cascades.

- Elements of array literals, and an indexed read of a constant global.
- A global whose value is visible only as the inline store globalization
  emits, rather than as an initializer.
- The cascade this is for: a globalized constant's field and element reads
  fold to scalars module-wide, the folding and branch-pruning passes reduce
  further, and the now-unread global drops by DCE. This is the
  cross-function constant propagation intra-function SROA cannot reach.

### wasm-CTFE backend

- `wado-compiler` must compile to `wasm32-unknown-unknown`, so it cannot link
  a Wasm runtime. Compile-time evaluation of a whole call therefore routes
  through the compiler host, as generator execution already does. Hosts with
  a runtime implement it; the LSP and browser hosts decline and `niri` stays
  in-process.
- Triggered when in-process reduction gives up on a callee that is still
  pure by its effects; the result is used as if the in-process engine had
  produced it.
- Enables `compute_lookup_table(256)` and similar workloads at compile time
  without re-implementing every NIR construct in the interpreter.

## Complexity

Reduction is monotone — expressions only move toward literal form — and
idempotent, and the optimizer's fixed-point loop is the only fixed point:
`niri` does not iterate internally. Each extension should keep the engine's
work bounded in the size of what it is asked about; a rule whose cost is
quadratic in body size, or that rebuilds a large value per query, is the
failure mode to watch for. Everything else about speed is a profiling
question, not a design-time one.

## Determinism

- Float NaN bits are nondeterministic in Wasm, so NaN-producing arithmetic is
  never folded. This holds for both backends.
- `v128` and relaxed-SIMD have implementation-defined corner cases; SIMD CTFE
  is deferred to a later WEP.
- Integer wrapping and signed `MIN / -1` semantics match Wasm. Division by
  zero and signed `MIN / -1` are left unfolded so the runtime trap survives.
- Float zero carries no sign distinction through a fold: `-0.0` and `+0.0`
  are equal, so `if cond { -0.0 } else { 0.0 }` collapses to one of the two
  and an operation that observes the sign of zero sees the chosen
  representative. This matches IEEE 754 equality. A caller needing
  bit-precise zeros should get a per-operation equality predicate rather than
  a globally weakened fold.

## Out of scope

- User-facing CTFE syntax (`#[const_eval]`, `const fn`). Decide once real
  demand shows up.
- Salsa-style demand-driven reanalysis across compiler runs.

## Open questions

- Where does the wasm-CTFE module cache live — per `compile` invocation or per
  process? Per-invocation is simpler; per-process speeds up watch-mode
  workflows but needs eviction.
