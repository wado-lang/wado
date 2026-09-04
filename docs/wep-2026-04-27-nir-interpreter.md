# WEP: NIR Interpreter (`niri`)

## Context

The optimizer reduces what the source made constant to the value it denotes.
Three mechanisms now do parts of that job, and they were one mechanism when this
WEP was first written.

- The hash-cons pool folds pure scalar arithmetic at intern time
  (`ValuePool::binary_folded` and its siblings), holding the invariant that no
  node in the pool is a foldable constant. It arrived with
  [the two-tier optimizer](./wep-2026-06-05-nir-optimizer-architecture.md).
- The `ValueGraph` carries everything flow-sensitive: reaching definitions,
  branch merges, loop and heap-write invalidation, field store-to-load seeding.
- `niri` runs bodies. A callee's body against constant arguments, or a
  self-contained region against an environment the walker can prove.

So `niri` is not "the engine that answers what a NIR expression evaluates to";
the pool answers that for arithmetic. `niri` is the compile-time executor, and
its unit of work is a frame with a heap of its own. Its `lattice` layer, which
re-derives scalar folding over skeleton `ExprId`s, is what remains of the older
framing and shrinks to a pool bridge as
[pure node kinds leave the skeleton](./wep-2026-06-05-nir-optimizer-architecture.md).

The measured prize is compile-time string formatting, and it is almost entirely
unclaimed. At `-O2`, `` `s=${"y"}` `` folds to a literal; `` `n=${42}` ``,
`` `b=${true}` ``, `` `c=${'x'}` ``, `` `${7:04}` `` and `(123).to_string()` all
reach the end of the optimizer as a buffer allocation, a `Formatter` and a
digit-count and division loop per evaluation. One constant integer interpolation
costs 599 bytes over the same program written with the literal — 2 012 to 2 611
at `-Os` on a hello-world — plus the formatting code kept alive in the binary.
Every `${}` over constants, every `to_string()` on a literal and every constant
`assert` message pays it.

## Decision

`niri` is a partial evaluator: it reduces what it can and leaves a residual
otherwise. Reduction is monotone and idempotent, and the optimizer's fixed-point
loop is the only fixed point — `niri` does not iterate internally. Each extension
keeps the engine's work bounded in the size of what it is asked about; a rule
whose cost is quadratic in body size, or that rebuilds a large value per query,
is the failure mode to watch for.

### Where the line falls

The pool folds pure values. The `ValueGraph` carries flow. `niri` executes
bodies.

The boundary against the `ValueGraph` is load-bearing: `niri` once carried its
own per-local field map and it was retired once the `ValueGraph` covered the same
ground. A proposal to teach `niri` about control flow between statements is a
sign the fact belongs on the other side of the line.

What the line does not forbid is a store over the values the engine itself
built. Inside a frame `niri` executes statements in order, so a value built
there — reachable from nothing the frame did not build — can be written through
and read back. The program's heap stays the `ValueGraph`'s; the engine's own is
the engine's.

### Effects are the purity gate

A function admitted for compile-time evaluation is one the effect system called
pure. A bug there lets an impure function run at compile time — the same trust
already placed in effect checking elsewhere.

### Determinism

- Float NaN bits are nondeterministic in Wasm, so NaN-producing arithmetic is
  never folded.
- `v128` and relaxed-SIMD have implementation-defined corner cases; SIMD CTFE is
  deferred to a later WEP.
- Integer wrapping and signed `MIN / -1` match Wasm. Division by zero and signed
  `MIN / -1` are left unfolded so the runtime trap survives.
- Float zero carries no sign through a fold: `-0.0` and `+0.0` are equal, so
  `if cond { -0.0 } else { 0.0 }` collapses to one of the two. A caller needing
  bit-precise zeros should get a per-operation equality predicate rather than a
  globally weakened fold.

### What the engine evaluates

Values: integer, float, bool and char scalars with their operators and casts;
structs and tuples whose every field is constant, and the field reads projecting
back out of one; enums, whose discriminant is the whole value, and variants,
which carry a case and its payload. A three-state lattice — unevaluated,
constant, non-constant — so an unreachable branch contributes nothing and a
trapping arm does not contaminate a fold. An aggregate leaves the engine only
where it has a literal shape to be written as; otherwise what reaches the IR is
the scalars projected out of it.

Bindings: immutable locals and globals whose initializer is constant, plus a
global's separately known fields, such as a sequence global's length; and a
global whose value is not in its slot but in the assignment that fills it, which
is what an extracted initializer and body globalization leave behind. Nothing is
read off a local or global something writes through — `global` without `mut`
forbids reassignment, not in-place mutation. A borrow denotes its referent, and
which locals carry one is decided once per body over the whole arena, so a read
folds through a binding an in-place rewrite displaced, and an index more than one
binder names carries nothing.

Control flow: a constant `if` condition collapses to the chosen arm, and a
side-effect-free condition whose arms denote the same constant collapses to it. A
constant `match` scrutinee selects the first arm that provably matches, and a
provably exhaustive match whose arms all denote one constant collapses to it —
exhaustiveness is what keeps the implicit no-match trap alive. Patterns decided:
wildcard, binding, integer / bool / char literal, range, or-patterns,
constant-valued, struct, and exact-arity tuple.

Calls: a free call runs at compile time when its arguments are all constant and
its callee is pure, non-async and monomorphic. The body executes statement by
statement, so `let` sequences, assignments, decided branches, early returns,
labeled blocks and loops all reach a value; a loop needs no constant trip count,
since the work budget bounds it. A statement counts as executed only when
everything it evaluates lands on a constant, so an unfolded call, a global write
or a would-be trap leaves work undone and stepping past it would drop it. A
result that may carry the caller's storage is withheld, though the writes still
land.

Sequences: the array backing a `String` or a `List` is a value, built from a
byte-string literal or a fully-constant array literal, up to a maximum length
past which building one costs more than any fold it enables. A write goes into
the value where it lies rather than rebuilding the container around it, so
filling a sequence is not quadratic in its length, and the backing is shared
until something writes it.

Regions: a self-contained block — one that builds its value in locals of its own,
reads and writes only those, and yields the result — runs as a frame the engine
starts from scratch. Self-containment is decided before the run, since the run
copies the enclosing body while the checks only walk the block; what that gives
up is a mention on a statically dead path, which no scan can tell from a live
one. An outer local the region only reads is seeded from the walker's environment
when it is constant there; a write position or a reference-typed mention refuses
the region instead.

### Fold the region, not the call

A region constructs its own buffer, so every value inside it is concrete, and
folding it claims nothing beyond one concrete evaluation. The call-level fold —
rewriting one `fmt` over a constant into `push_str(<literal>)`, which a template
mixing constant and runtime interpolations needs — claims more: that the callee
appends the same bytes to every buffer. That is a separate mechanism with a
separate trust story, and it is staged after the region fold rather than under
it.

Where that claim is needed, it comes from `#[compiler_item]`, as it already does
for `push_str`. Mark `Formatter`'s write primitives, not `Display::fmt`: marking
the trait would extend the trust to every user-written impl, where nothing is
checkable. What a marked primitive declares is a region append — everything it
does to the buffer happens at or above the length the buffer had on entry, and
what lies below is neither read nor moved. That covers the stdlib's three
formatting idioms (`prepare_int_write`, `mark` / `apply_padding`, `fpfmt`'s
reserving writers) unchanged, where a strictly-append contract would not. What a
particular `fmt` body does is then derived: run it against a buffer the engine
built, and admit the result when every buffer access went through a marked
primitive or landed inside a region one just returned.

## Roadmap

Ordered. Each stage says what finishing it means.

### 1. Say why a fold did not happen

- [ ] A remark, over the [remarks](./wep-2026-06-03-optimizer-remarks.md)
      infrastructure, naming why a region or a call did not fold: a runtime
      interpolation, an exhausted budget, a refused aggregate, an unowned place,
      an undecided pattern.
- [ ] A census of the benchmarks, `wasm-size` corpus and stdlib by that reason.

Done when the refusal reasons are counted, and every stage below is ordered
against those counts rather than against this document.

### 2. The frame owns storage

One capability with three faces, and the measured blocker for every constant
interpolation that is not already a `String`. `` `c=${'x'}` `` leaves a
`Formatter { …, buf: __local }` literal in the WIR because a field holding a
place is not a value the engine can represent; `` `b=${true}` `` leaves
`Formatter::pad`, a unit-returning `&mut self` method; `` `n=${42}` `` leaves
both, through `fmt_decimal`'s `&mut Formatter` parameter.

- [ ] A place-valued field, so an aggregate can carry a reference. Today such an
      aggregate is not a constant, since a field holding the referent's value
      would take a write meant for the referent; what it needs to hold is the
      place the frame already names elsewhere. The refusal is whole-value, so a
      scalar field naming no storage is refused with the rest, which is why
      `Array::slice`'s computed bounds stop folding.
- [ ] A call that returns nothing but whose every write lands in a place the
      frame owns, which is what lets a builder-style helper run.
- [ ] Method calls, excluded today because a `&mut self` receiver mutates through
      the call. Unnecessary for a receiver the frame owns, which the store can
      update.
- [ ] A `&mut` argument writing back into the caller frame's place on return.
      Without it a buffer that grows still abandons the evaluation, because
      `grow` reshapes the caller's container from a frame of its own.

Upstream: the `stores`-gated temp and write-back carve-outs and divergences D1–D6
in [Reference Representation](./wep-2026-06-13-reference-representation.md). The
engine's notion of a place must be the one that WEP settles, not a second one.

Done when `${true}`, `${'x'}` and `${42}` fold to literals at `-O2` and the 599
bytes go away.

### 3. The aggregate exit

- [ ] A `List<T>` of scalars written back as
      [`ArrayLiteral`](./wep-2026-05-31-nir-array-literal.md) and a plain struct
      as `StructLiteral`, each needing its own answer to "is this already the
      literal the rewrite writes" so the worklist still settles.
- [ ] A destructuring `let`; a body containing one is abandoned.

Done when a constant `List` result and a constant struct result reach the IR as
literals, and `Array::slice`'s computed bounds fold.

### 4. Format coverage to the budget

- [ ] Width, zero-pad and radix specs, then `Inspect`, then floats. Each step
      measures what it spends against the step budget; if `fpfmt` overruns it,
      floats become a known gap rather than a reason to raise the budget.
- [ ] A `wasm-size` and `benchmark` run recording what stages 2–4 bought.

Done when the corpus census from stage 1 shows no refusal reason left that the
step budget does not explain.

### 5. Mixed templates

- [ ] The marked region-append primitive set and the derived-`fmt` admission
      rule, per "Fold the region, not the call".

Done when a template mixing constant and runtime interpolations emits the
constant parts as literals. Sized by stage 1's count of such templates; if that
count is small, this stage is demoted to a known gap.

### 6. The remaining refusals

Each is a small, local refusal, and stage 1's census decides whether any is worth
the code.

- [ ] A `switch` with a constant scrutinee. A switch is formed before inlining,
      so a scrutinee inlining makes constant survives untouched.
- [ ] Closure calls: an indirect call whose closure is known is never resolved to
      a direct call, so neither inlining nor CTFE reaches through it.
- [ ] Guards decided when the engine is only asked what an expression denotes.

### Validation, from stage 2 onward

- [ ] A fold / no-fold differential oracle in the
      [fuzzer](./wep-2026-08-19-compiler-fuzzing.md): compile each corpus fixture
      with and without `nir/const_fold` and compare observable behaviour.

A wrong constant is a silent miscompile — no trap, no diagnostic, a different
answer. Fixtures cover the shapes we thought of; the differential covers the
ones we did not, and it is the safety net every stage above leans on.

## Known gaps

### A wasm-CTFE backend

Whole pure calls run through a real Wasm runtime, with the effect system as the
purity gate. It would cover recursion beyond a base case, `fib(20)`-shaped work
and lookup-table generation — everything the in-process engine balks at — at ms
per call against `niri`'s µs, amortized through a module cache.

Closing it takes: a route through the compiler host, since `wado-compiler` must
compile to `wasm32-unknown-unknown` and cannot link a runtime, as Kiln generator
execution already does; a resolution of the async boundary, since
`CompilerHost::run_generator` is async and the optimizer's fixed-point loop is
not; an answer to the circularity of needing a compiled module to evaluate a call
made while compiling; and a module-cache lifetime, per `compile` invocation or
per process.

Unowned because the demand is not visible. No benchmark, stdlib path or corpus
program exhibits recursion over constants, and
[compile-time data providers](./wep-2026-06-13-compile-time-data-providers.md)
and [Kiln](./wep-2026-04-12-kiln.md) already serve the "compute data at build
time" case with a user-visible, content-addressed contract that implicit CTFE
cannot match. If it is ever built it shares their host-delegated execution
facility rather than adding a third.

Downstream of the same gap: recursion beyond a base case stays unfolded, and a
runtime trap inside an unfolded body stays observable.

### Comparing two literal strings as a guard

It reaches the engine as a guard over a method call on references. `niri` folds a
constant string comparison in value position today; the guard position does not
reduce.

### Unrolling a loop in the caller

A code-size trade needing a cost model, not an evaluation capability.

### User-facing CTFE syntax

`#[const_eval]`, `const fn`. No demand has shown up; everything the roadmap
covers is implicit.

### Salsa-style demand-driven reanalysis across compiler runs

Named here so a proposal is measured against the optimizer's own caching
decisions rather than raised fresh.
