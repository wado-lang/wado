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

The measured prize is compile-time string formatting. Every `${}` over
constants, every `to_string()` on a literal and every constant `assert` message
used to reach the end of the optimizer as a buffer allocation, a `Formatter` and
a digit-count and division loop per evaluation, plus the formatting code kept
alive in the binary — 877 bytes at `-Os` for one constant integer interpolation
over the same program written with the literal. Integers, `bool`, width,
zero-pad, radix and `Inspect` now fold to that literal and cost about 20 bytes;
`char` and floats do not yet. What the roadmap does with the rest is ordered
against a census of the corpus, not against this document's guesses.

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

Ordered by what the census counts, not by what this document guessed.

### 1. Say why a fold did not happen

- [x] A remark, over the [remarks](./wep-2026-06-03-optimizer-remarks.md)
      infrastructure, naming a block that computes a constant at run time and
      what stopped it — the calls that survived inside it, or the fact that
      refused it as a frame. Read off the final IR, so it retires itself as the
      fold reaches each shape.
- [x] `WADO_TRACE=ctfe_call`, which names what each declined call wanted that the
      frame could not give, and `ctfe_stmt`, which names the statement a frame
      abandoned at. The remark reports the region; the traces report the call and
      the statement inside it.
- [x] A census over the benchmark and `wasm-size` corpora and the Wado packages
      (`mise run report-const-regions`).

The census found 2 788 surviving regions across 20 files, and it reorders
everything below:

| Cause                                     | Regions |
| ----------------------------------------- | ------- |
| it writes a global                        | 1 353   |
| it calls a function the engine cannot run | 1 074   |
| it writes a place no local roots          | 246     |
| a named call still runs (`panic`, others) | 113     |
| it calls through a closure or CM          | 4       |

Two files of Gale-generated code carry 92 % of the total, so the ordering was
checked against hand-written corpora separately: `json_twitter`, `zlib_bench`
and `sqlite_parse` are dominated by the global write, `package-marl` by the
unrunnable call. The two together are about 95 % everywhere.

What that overturns: the format-coverage work this WEP was ordered around is the
fourth-largest bucket, at roughly 4 %. The value model is not what most regions
wait on. The two mechanisms below are, and neither was in this document.

### 2. A materializing global write is not a write

Half the surviving regions contain a `GlobalVarSet`, and a region carrying one
is refused whole. Almost all of them are the lazy initializer
[constant-object globalization](./wep-2026-05-31-const-object-globalization.md)
leaves behind: the region assigns a constant to a global and reads it straight
back, which is a materialization the frame could read through rather than a
write to program state. `` `v=${true}` `` is the smallest instance — `"true"`
is globalized, and that alone stops the fold.

- [x] Read such a store through instead of refusing it. The condition is a
      property, not a count: every mention of the global in the package is one
      half of a `{ G = v; G }` pair. Folding a region carrying one then deletes
      the store and the only read it serves together, and no read anywhere is
      left depending on a store that went away. A count would have been wrong
      twice over — inlining copies the pair, so two sites are as safe as one,
      and a global with a single store and a distant read is not safe at all.
- [x] The answer lives in `niri`, since the globalization pass cannot know
      whether the region around its store will fold, and the engine already
      reads a global out of the assignment that fills it.
- [x] A materialization is not itself a region. `{ G = v; G }` is two statements
      ending in an expression, so it answers the region shape, and admitting the
      store made the pair fold to the literal — writing the constant back over
      the naming construct and undoing the sharing globalization had arranged.
      What the pair is has one recognizer, in `region`, and both consumers ask
      it: the store inside a larger region is readable, the pair alone is not a
      region. A folded template is globalized afterwards, so it is still built
      once at instantiation.
- [ ] Recount the corpus, now that the bucket has moved.

Done when the corpus is recounted and the bucket has moved.

### 3. Which functions the engine cannot run, and why

A third of the surviving regions call something outside the callee map: impure,
generic, async, or bodiless. Which of those it is decides whether there is
anything to do — a genuinely impure callee is a correct refusal, a
still-generic one after monomorphization is a bug, and `panic` (79 regions) is a
cold path a region could carry rather than be refused for.

- [ ] Break the bucket down by why the callee is out: the trace names the call,
      but a region refused before `run_call` never reaches it.
- [ ] `panic` on a statically dead path, which is what a bounds check inside a
      folded region leaves.

Done when the bucket is split into what is a correct refusal and what is work,
and the work is a stage of its own.

### 4. The frame owns storage

What the format work waited on. A unit-returning call whose writes land in a
place the frame owns and a `&mut` argument written back on return were already
implemented; what was missing was that the buffer a template builds is threaded
through a `let mut` holding a `&mut` of itself, and neither the frame nor the
analysis deciding what a frame can track would call that an alias.

- [x] A `let mut` binding a borrow resolves to a place alias when nothing
      reassigns the local. An immutable binding always did; what makes it safe is
      that nothing displaces it, which is equally true of a mutable local nothing
      reassigns — and `sroa_param` spells its scalarized `&mut` field that way.
- [x] The same predicate in `Reached::record_alias_borrows`. The two must agree:
      a borrow the frame resolves to an alias but the walk counts as a clobber
      leaves the frame holding no value for the place it just aliased, which is
      what abandoned every template at its first append.

Measured at `-Os` against the same program written with the literal, which costs
3 312 bytes:

| Interpolation | Before | After  |
| ------------- | ------ | ------ |
| `${42}`       | 4 189  | 3 328  |
| `${7:04}`     | 4 455  | 3 337  |
| `${255:x}`    | 3 853  | 3 330  |
| `${42:?}`     | 4 189  | 3 328  |
| `${true}`     | 3 799  | 3 337  |
| `${'x'}`      | 3 724  | 3 724  |
| `${3.5}`      | 13 267 | 13 267 |

A constant integer interpolation costs 16 bytes over the literal, down from 877.
What is left:

- [ ] `char`, which still leaves `Formatter::pad` standing where `bool` no longer
      does, so the two differ in something the remark does not yet separate.
- [ ] Floats, whose `fpfmt` is the whole of the 10 000 bytes and is stage 6's
      budget question, not this stage's.
- [ ] A place-valued field, so an aggregate can carry a reference. Today such an
      aggregate is not a constant, since a field holding the referent's value
      would take a write meant for the referent; what it needs to hold is the
      place the frame already names elsewhere. The refusal is whole-value, so a
      scalar field naming no storage is refused with the rest, which is why
      `Array::slice`'s computed bounds stop folding.
- [ ] `String::grow`, which reshapes the caller's container from a frame of its
      own and so abandons the evaluation whenever a buffer outgrows its
      reservation.

Upstream: the `stores`-gated temp and write-back carve-outs and divergences D1–D6
in [Reference Representation](./wep-2026-06-13-reference-representation.md). The
engine's notion of a place must be the one that WEP settles, not a second one.

Done when `${'x'}` folds too.

### 5. The aggregate exit

- [ ] A `List<T>` of scalars written back as
      [`ArrayLiteral`](./wep-2026-05-31-nir-array-literal.md) and a plain struct
      as `StructLiteral`, each needing its own answer to "is this already the
      literal the rewrite writes" so the worklist still settles.
- [ ] A destructuring `let`; a body containing one is abandoned.

Done when a constant `List` result and a constant struct result reach the IR as
literals, and `Array::slice`'s computed bounds fold.

### 6. Format coverage to the budget

- [x] Width, zero-pad, radix and `Inspect` fold, and needed nothing of their own:
      the frame runs each spec's body once it can hold the buffer.
- [ ] The step budget is per function and a formatting region spends a large
      share of it. Seven constant templates in one body exhaust it and five of
      them stop folding; four all fold. Whether that is a budget to raise, a cost
      to cut, or a limit to document is what a corpus recount answers — real code
      spreads templates over functions, and the fixture had to.
- [ ] Floats. `fpfmt` is the largest size prize by an order of magnitude and the
      largest engine cost, so the order is the engine's, not the payoff's; if it
      overruns the budget it becomes a known gap rather than a reason to raise
      the budget.
- [ ] A `wasm-size` and `benchmark` run recording what stages 2–6 bought.

Done when a recount shows no refusal reason left that the step budget does not
explain.

### 7. Mixed templates

- [ ] The marked region-append primitive set and the derived-`fmt` admission
      rule, per "Fold the region, not the call".

Done when a template mixing constant and runtime interpolations emits the
constant parts as literals. The census counts no such template yet, since a
region reading a runtime local is not reported; sizing it needs a count of its
own, and a small one demotes this stage to a known gap.

### 8. The remaining refusals

Each is a small, local refusal the census does not count, so each needs a reason
of its own to be worth the code.

- [ ] A `switch` with a constant scrutinee. A switch is formed before inlining,
      so a scrutinee inlining makes constant survives untouched.
- [ ] Closure calls: an indirect call whose closure is known is never resolved to
      a direct call, so neither inlining nor CTFE reaches through it.
- [ ] Guards decided when the engine is only asked what an expression denotes.

### Validation, from the first engine change onward

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
