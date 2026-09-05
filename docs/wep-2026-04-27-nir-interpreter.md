# WEP: NIR Interpreter (`niri`)

## Context

The optimizer reduces what the source made constant to the value it denotes.
Three mechanisms share that job.

- The hash-cons pool folds pure scalar arithmetic at intern time, holding the
  invariant that no node in the pool is a foldable constant.
- The `ValueGraph` carries everything flow-sensitive: reaching definitions,
  branch merges, loop and heap-write invalidation, field store-to-load seeding.
- `niri` runs bodies. A callee's body against constant arguments, or a
  self-contained region against an environment the walker can prove.

`niri` is the compile-time executor, and its unit of work is a frame with a heap
of its own. It is not the engine that answers what a NIR expression evaluates to;
the pool answers that for arithmetic. Its lattice layer re-derives scalar folding
over skeleton expressions. That is what remains of an older framing, and it
shrinks to a pool bridge as
[pure node kinds leave the skeleton](./wep-2026-06-05-nir-optimizer-architecture.md).

The prize is compile-time string formatting. An interpolation that does not fold
costs a buffer allocation, a `Formatter`, and a digit-count and division loop on
every evaluation, and it keeps the formatting code alive in the binary. Every
`${}` over constants, every `to_string()` on a literal and every constant
`assert` message pays that. Measured at `-Os` on a program that prints one
interpolation, against the same program printing the literal, which costs
1 997 bytes:

| Interpolation                | Cost   | Over the literal |
| ---------------------------- | ------ | ---------------- |
| `${"y"}`, `${42}`, `${42:?}` | ~2 015 | ~18              |
| `${7:04}`, `${true}`         | ~2 020 | ~24              |
| `${'x'}`                     | 2 340  | 343              |
| `${255:x}`                   | 5 197  | 3 200            |
| `${3.5}`                     | 6 411  | 4 414            |

The first two rows fold to the literal. `char`, `{:x}` and floats do not. The
roadmap is ordered against a census of the corpus rather than against this
document's guesses.

## Decision

`niri` is a partial evaluator: it reduces what it can and leaves a residual
otherwise. Reduction is monotone and idempotent, and the optimizer's fixed-point
loop is the only fixed point; `niri` does not iterate internally. Each extension
keeps the engine's work bounded in the size of what it is asked about. Watch for
a rule whose cost is quadratic in body size, or that rebuilds a large value on
every query.

### Where the line falls

The pool folds pure values. The `ValueGraph` carries flow. `niri` executes
bodies.

The boundary against the `ValueGraph` is load-bearing: `niri` once carried its
own per-local field map, and that map was retired once the `ValueGraph` covered
the same ground. A proposal to teach `niri` about control flow between
statements is a sign the fact belongs on the other side of the line.

The line does not forbid a store over the values the engine itself built. Inside
a frame `niri` executes statements in order, so a value the frame built, which
nothing outside it reaches, can be written through and read back. The program's
heap stays the `ValueGraph`'s; the engine's own is the engine's.

### Effects are the purity gate

A function admitted for compile-time evaluation is one the effect system called
pure. A bug there lets an impure function run at compile time, which is the same
trust already placed in effect checking elsewhere.

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
which carry a case and its payload. The lattice has three states: unevaluated,
constant, non-constant. So an unreachable branch contributes nothing and a
trapping arm does not contaminate a fold. An aggregate leaves the engine only
where it has a literal shape to be written as; otherwise what reaches the IR is
the scalars projected out of it.

Bindings: immutable locals and globals whose initializer is constant, plus a
global's separately known fields, such as a sequence global's length; and a
global whose value is not in its slot but in the assignment that fills it, which
is what an extracted initializer and body globalization leave behind. Nothing is
read off a local or global something writes through, since `global` without `mut`
forbids reassignment, not in-place mutation. A borrow denotes its referent,
whether the binding is mutable or not: what makes an alias safe is that nothing
displaces it, which holds of a `let mut` nothing reassigns. Which locals carry
one is decided once per body over the whole arena, so a read folds through a
binding an in-place rewrite displaced, and an index more than one binder names
carries nothing.

Control flow: a constant `if` condition collapses to the chosen arm, and a
side-effect-free condition whose arms denote the same constant collapses to it. A
constant `match` scrutinee selects the first arm that provably matches. A
provably exhaustive match whose arms all denote one constant collapses to it,
since exhaustiveness is what keeps the implicit no-match trap alive. Patterns
decided: wildcard, binding, integer / bool / char literal, range, or-patterns,
constant-valued, struct, and exact-arity tuple.

Calls: a free call runs at compile time when its arguments are all constant and
its callee is pure, non-async and monomorphic. The body executes statement by
statement, so `let` sequences, assignments, decided branches, early returns,
labeled blocks and loops all reach a value; a loop needs no constant trip count,
since the work budget bounds it. A statement counts as executed only when
everything it evaluates lands on a constant, so an unfolded call, a global write
or a would-be trap leaves work undone and stepping past it would drop it. A
result that may carry the caller's storage is withheld, but the writes still
land. A unit-returning call runs for its writes, and a `&mut` argument is written
back on return.

Sequences: the array backing a `String` or a `List` is a value, built from a
byte-string literal or a fully-constant array literal, up to a maximum length
past which building one costs more than any fold it enables. A write goes into
the value where it lies rather than rebuilding the container around it, so
filling a sequence is not quadratic in its length, and the backing is shared
until something writes it.

### Regions

A block is self-contained when it builds its value in locals of its own, reads
and writes only those, and yields the result. Such a block runs as a frame the
engine starts from scratch, and that is what folds a constant string template to
the literal the source could have written.

Self-containment is decided before the run, since the run copies the enclosing
body while the checks only walk the block. That costs one thing: a mention on a
statically dead path counts, because no scan can tell it from a live one. An
outer local the region only reads is seeded from the walker's environment when
it is constant there, and a write position or a reference-typed mention refuses
the region instead.

Two shapes are not regions, and both look like one.

- A block that leaves nothing on the Wasm stack. Unit is the inlined statement
  call, whose result stands where the program expects none. Never is the `else`
  of a `let ... else { panic("…") }`, which builds a constant message and then
  diverges. Neither has a value to fold to.
- The `{ G = <const>; G }` pair
  [constant-object globalization](./wep-2026-05-31-const-object-globalization.md)
  leaves where it names a constant at a use site. Folding it writes the literal
  back over the naming construct and undoes the sharing globalization arranged.

A region that contains such a store can still fold. The condition is that every
mention of the global in the package is one half of such a pair. Folding the
region then deletes the store and the only read it serves together, so no read is
left depending on a store that went away. A count of stores would not do:
inlining copies the pair, so two sites are as safe as one, while a global with a
single store and a distant read is not safe at all. The check lives in `niri`
rather than in the globalization pass, which cannot know whether the region
around its store will fold.

### Fold the region, not the call

A region constructs its own buffer, so every value inside it is concrete, and
folding it claims nothing beyond one concrete evaluation. The call-level fold
claims more: that the callee appends the same bytes to every buffer. It rewrites
one `fmt` over a constant into `push_str(<literal>)`, which is what a template
mixing constant and runtime interpolations needs. That is a separate mechanism
with a separate trust story, so it is staged after the region fold rather than
under it.

Where that claim is needed, it comes from `#[compiler_item]`, as it already does
for `push_str`. Mark `Formatter`'s write primitives, not `Display::fmt`: marking
the trait would extend the trust to every user-written impl, where nothing is
checkable. A marked primitive declares a region append: everything it does to the
buffer happens at or above the length the buffer had on entry, and what lies
below is neither read nor moved. That covers the stdlib's three
formatting idioms (`prepare_int_write`, `mark` / `apply_padding`, `fpfmt`'s
reserving writers) unchanged, where a strictly-append contract would not. What a
particular `fmt` body does is then derived: run it against a buffer the engine
built, and admit the result when every buffer access went through a marked
primitive or landed inside a region one just returned.

### How a missed fold is found

There are three instruments, and the roadmap is ordered by what they count rather
than by what looks important.

- A remark, over the [remarks](./wep-2026-06-03-optimizer-remarks.md)
  infrastructure, names a block that computes a constant at run time and what
  stopped it: the calls that survived inside it, or the fact that refused it as a
  frame. It fires from the final IR, never from a refusal a pass recorded, so it
  retires itself as the fold reaches each shape.
- `WADO_TRACE=ctfe_call` names what each declined call wanted that the frame
  could not give, `ctfe_stmt` the statement a frame abandoned at, and
  `region_seed` every refusal along the fold path, under the function it walked.
  The remark reports the region, the traces the call and the statement inside it.
- `mise run report-const-regions` counts the remarks over the benchmark and
  `wasm-size` corpora and the Wado packages.

What the remark names is a call on the region's own path. A call under a
`cold_path` marker is not one: `push` carries `grow` behind its capacity check,
so a region filling a pre-sized container would name a call it never reaches and
bury the one it does. A remark that names nothing is itself an answer: the fold
is waiting on a value the engine cannot represent, not on a body it cannot run.
That is where the statement trace takes over.

A count is worth nothing until something independent agrees with it. The
formatting work is measured in bytes as well, which is what a census miscount
cannot reach.

## Roadmap

The census counts 1 198 surviving regions across 9 of 22 files:

| Cause                                | Regions |
| ------------------------------------ | ------- |
| no call on its path explains it      | 900     |
| it writes a global                   | 240     |
| `push_encoded_ranges` still runs     | 34      |
| it writes a place no local roots     | 13      |
| `union_char_ranges` still runs       | 4       |
| `binary_property_ranges` still runs  | 4       |
| `general_category_ranges` still runs | 2       |
| `i32::fmt_decimal` still runs        | 1       |

Two Gale-generated files hold 1 120 of them. That concentration is not a Gale
fact: what generated code does is call the same missing capability often.

The largest row is also the least specific. A region naming no call is waiting on
a value the engine cannot represent rather than a body it cannot run. It says
which instrument to reach for next, not which capability is missing, so only
`ctfe_stmt` turns those 900 into work.

### 1. The aggregate exit

Every callee the census names builds a `List<T>` from a constant:
`push_encoded_ranges` (34), `union_char_ranges` (4), `binary_property_ranges`
(4), `general_category_ranges` (2). A `List<T>` region is also what the no-call
row reports, so the census points at this one capability twice over, once by
name and once by the silence. A `List<T>` filled
by a loop and returned does not fold, whether `T` is a scalar or a struct, while
the same program over a `String` does. A `String`'s backing is a byte array the
engine represents and can write back, and a `List<T>`'s is not.

What stops is the list, not everything that touches one. A scalar the engine
projects out of it still folds, so `` `${table().len()}` `` reaches the IR as the
rendered literal while `` `${table():?}` ``, which needs the list itself, does
not.

- [ ] A `List<T>` of scalars written back as
      [`ArrayLiteral`](./wep-2026-05-31-nir-array-literal.md) and a plain struct
      as `StructLiteral`, each needing its own answer to "is this already the
      literal the rewrite writes" so the worklist still settles.
- [ ] A destructuring `let`; a body containing one is abandoned.

Done when a constant `List` result and a constant struct result reach the IR as
literals, `Array::slice`'s computed bounds fold, and the corpus is recounted.

### 2. The stores the engine will not read

- [ ] The 238 global writes, which are the stores that fail the
      materialization property: a global read somewhere that does not store it.
      Whether that set has a shape of its own, or is a tail of unrelated cases,
      decides whether there is a mechanism here at all.
- [ ] Whether an unrunnable callee still refuses a region anywhere: impure,
      generic, async or bodiless. The corpus now counts none, so this is a check
      that the refusal has no cases left rather than a set to work through. A
      genuinely impure callee is a correct refusal; a still-generic one after
      monomorphization is a bug.

### 3. The frame owns storage

`` `${'x'}` `` still leaves `Formatter::pad` standing, which the remark names,
where `` `${true}` `` folds and reports nothing. With `` `${255:x}` `` it is
what still pays for its formatter short of floats.

- [ ] What distinguishes the `char` path from the `bool` one.
- [ ] A place-valued field, so an aggregate can carry a reference. Today such an
      aggregate is not a constant, since a field holding the referent's value
      would take a write meant for the referent; what it needs to hold is the
      place the frame already names elsewhere. The refusal is whole-value, so a
      scalar field naming no storage is refused with the rest, which is why
      `Array::slice`'s computed bounds stop folding. It is also what
      `` `${255:x}` `` waits on: `Formatter::prepare_int_write` survives as a
      call taking `&mut Formatter`, whose `buf` is such a field. A template
      folds today only where inlining and SROA dissolve its `Formatter` first,
      which the inliner's pricing decides, not the engine.
- [ ] `String::grow`, which reshapes the caller's container from a frame of its
      own and so abandons the evaluation whenever a buffer outgrows its
      reservation.

Upstream: the `stores`-gated temp and write-back carve-outs and divergences D1–D6
in [Reference Representation](./wep-2026-06-13-reference-representation.md). The
engine's notion of a place must be the one that WEP settles, not a second one.

Done when `${'x'}` folds.

### 4. Format coverage to the budget

- [ ] The step budget is per function, and a formatting region spends a large
      share of it: four in one body all fold, while seven exhaust the budget and
      five of them stop folding. Whether that is a budget to raise, a cost to
      cut, or a limit to document is what a recount answers.
- [ ] Floats. `fpfmt` is the largest size prize by an order of magnitude and the
      largest engine cost, so the order is the engine's, not the payoff's; if it
      overruns the budget it becomes a known gap rather than a reason to raise
      the budget.
- [ ] A `wasm-size` and `benchmark` run, recording what the folds buy on whole
      programs rather than on one interpolation.

Done when a recount shows no refusal reason left that the step budget does not
explain.

### 5. Mixed templates

- [ ] The marked region-append primitive set and the derived-`fmt` admission
      rule, per "Fold the region, not the call".

Done when a template mixing constant and runtime interpolations emits the
constant parts as literals. The census counts no such template, since a region
reading a runtime local is not reported; sizing this needs a count of its own,
and a small one demotes the stage to a known gap.

### 6. The remaining refusals

Each is a small, local refusal the census does not count, so each needs a reason
of its own to be worth the code.

- [ ] A `switch` with a constant scrutinee. A switch is formed before inlining.
      When inlining later makes its scrutinee constant, nothing revisits it.
- [ ] Closure calls: an indirect call whose closure is known is never resolved to
      a direct call, so neither inlining nor CTFE reaches through it.
- [ ] Guards decided when the engine is only asked what an expression denotes.

### Validation, alongside every stage

- [ ] A fold / no-fold differential oracle in the
      [fuzzer](./wep-2026-08-19-compiler-fuzzing.md): compile each corpus fixture
      with and without constant folding and compare observable behaviour.

A wrong constant is a silent miscompile: no trap, no diagnostic, a different
answer. Fixtures cover the shapes we thought of; the differential covers the ones
we did not, and it is the safety net every stage above leans on.

## Known gaps

### A wasm-CTFE backend

Whole pure calls run through a real Wasm runtime, with the effect system as the
purity gate. It would cover what the in-process engine balks at: recursion beyond
a base case, `fib(20)`-shaped work, lookup-table generation. The cost is ms per
call against `niri`'s µs, amortized through a module cache.

Closing it takes: a route through the compiler host, since `wado-compiler` must
compile to `wasm32-unknown-unknown` and cannot link a runtime, as Kiln generator
execution already does; a resolution of the async boundary, since the host's
generator entry point is async and the optimizer's fixed-point loop is not; an
answer to the circularity of needing a compiled module to evaluate a call made
while compiling; and a module-cache lifetime, per `compile` invocation or per
process.

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
constant string comparison in value position; the guard position does not reduce.

### Unrolling a loop in the caller

A code-size trade needing a cost model, not an evaluation capability.

### User-facing CTFE syntax

`#[const_eval]`, `const fn`. No demand has shown up; everything the roadmap
covers is implicit.

### Salsa-style demand-driven reanalysis across compiler runs

Named here so a proposal is measured against the optimizer's own caching
decisions rather than raised fresh.
