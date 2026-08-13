# WEP: NIR Interpreter (`niri`) Evolution Plan

## Context

The optimizer needs to reduce what the source made constant — literal
arithmetic, a branch whose condition is known, a pure call with constant
arguments — to the value it denotes. `niri` is the engine that answers what a
NIR expression evaluates to at compile time. Constant folding is its primary
consumer; branch pruning, constant propagation and compile-time function
evaluation reuse it.

This WEP states capabilities, not mechanisms: what `niri` can and cannot
evaluate. How it does so is the code's business.

## Decision

`niri` is a partial evaluator: it reduces what it can and leaves a residual
otherwise. A complementary wasm-CTFE backend runs whole pure calls through a
real Wasm runtime, using Wado's effect system as a type-checked purity gate.

### Why two backends

|              | niri (in-process)                                  | wasm execution                                          |
| ------------ | -------------------------------------------------- | ------------------------------------------------------- |
| Sweet spot   | `2+3 → 5`, identity simplification, branch pruning | `fib(20)`, lookup-table generation, full pure-call CTFE |
| Cost / call  | µs                                                 | ms (codegen + instantiate), amortized via module cache  |
| Partial eval | Yes (residuals)                                    | No (whole-call)                                         |
| Coverage     | Whatever we hand-write                             | All of Wado, for free                                   |

`niri` stays cheap and fine-grained; wasm execution covers anything it balks at.

### Scope boundary against the ValueGraph

`niri` evaluates pure values: what an expression denotes, given what is known
about its inputs. Everything flow-sensitive — reaching definitions, branch
merges, loop and heap-write invalidation, field store-to-load seeding — belongs
to the `ValueGraph`. The boundary is load-bearing: `niri` once carried its own
per-local field map and it was retired once the `ValueGraph` covered the same
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

## Done

Value model:

- Integer, float, bool and char scalars, with their arithmetic, comparison,
  bitwise and unary operators and the casts between them.
- Structs and tuples whose every field is constant, and the field reads
  projecting back out of a literal, a local, an immutable global or a
  compile-time call result.
- Enums, whose discriminant is the whole value, and variants, which carry a case
  and its payload. Both construction forms fold and both pattern kinds decide.
- An aggregate leaves the engine only where it has a literal shape to be written
  as. Otherwise what reaches the IR is the scalars projected out of it.
- A three-state lattice — unevaluated, constant, non-constant — so an
  unreachable branch contributes nothing and a trapping arm does not contaminate
  a fold.

Bindings:

- Immutable locals and globals whose initializer is constant, plus a global's
  separately known fields, such as a sequence global's length.
- A global whose value is not in its slot but in the assignment that fills it,
  which is what an extracted initializer and body globalization leave behind.
  Two disagreeing assignments make it non-constant.
- Nothing is read off a local or global something writes through: `global`
  without `mut` forbids reassignment, not in-place mutation. An aggregate binds
  to a local only when every mention of that local merely reads it.
- A borrow denotes its referent, so a local bound to `&CONST` carries it. Which
  locals those are is decided once per body over the whole arena, so a read
  folds through a binding an in-place rewrite displaced — and an index more than
  one binder names carries nothing, since such a scan cannot say which binding
  governs a read.

Control flow:

- `if`: a constant condition collapses to the chosen arm, and a side-effect-free
  condition whose arms denote the same constant collapses to it.
- `match`: a constant scrutinee selects the first arm that provably matches, and
  a provably exhaustive match whose arms all denote one constant collapses to
  it. Exhaustiveness is what keeps the implicit no-match trap alive.
- Patterns decided: wildcard, binding, integer / bool / char literal, range,
  or-patterns, constant-valued, struct, and exact-arity tuple. An arm's guard and
  body are evaluated with that arm's bindings in scope.
- `X matches { Case }` collapses to a discriminator comparison before the match
  reaches pattern lowering.

Calls:

- A free call runs at compile time when its arguments are all constant and its
  callee is pure, non-async and monomorphic. The body executes statement by
  statement, so `let` sequences, assignments, decided branches, early returns,
  labeled blocks and loops all reach a value.
- A statement counts as executed only when everything it evaluates lands on a
  constant. An unfolded call, a global write or a would-be trap leaves work
  undone, and stepping past it would drop it.
- A loop needs no constant trip count: the work budget bounds it, and the budget
  is per function. Recursion is bounded too, and a body that does not produce a
  constant leaves the call in place, so a runtime trap inside it stays
  observable.
- A result that may carry the caller's storage is withheld, though the writes
  still land. Two parameter kinds reach that storage: a `&mut` one, whose
  referent the result may embed (`Formatter::new(&mut buf)`), and one `stores[p]`
  declares the callee keeps, whose alias leaves inside an ordinary aggregate that
  neither the return type nor the write targets name. Either result would stand
  as a snapshot the next write leaves stale. A scalar embeds nothing.
- A local the frame cannot track — one a mutable borrow, a mutable argument, a
  receiver or an assignment buried in a larger expression can write — carries no
  value. A shared borrow is not one of those channels.

Sequences:

- The array backing a `String` or a `List` is a value, built from a byte-string
  literal or a fully-constant array literal, up to a maximum length past which
  building one costs more than any fold it enables.
- Element and length reads fold through the array builtins they lower to, not
  through an index node. A read past the end is left alone to trap at run time.
- An element write lands, and a prefix clone folds — the latter being what a
  value copy of a sequence container lowers to.
- A write goes into the value where it lies rather than rebuilding the container
  around it, so filling a sequence is not quadratic in its length. The backing is
  shared until something writes it, so a value copied out beforehand keeps what
  it was given.
- A byte-sequence container the engine filled is written back as the literal the
  lower phase emits for a source string, over its first `used` bytes. One the
  frame never filled stays as the source wrote it: an empty container is a
  reservation rather than a result.

Regions:

- A self-contained block — one that builds its value in locals of its own, reads
  and writes only those, and yields the result — runs as a frame the engine
  starts from scratch. This is what folds a fully-constant string template to
  the literal the source could have written.
- Self-containment is decided before the run, since the run copies the enclosing
  body while the checks only walk the block. What that gives up is a mention on
  a statically dead path, which no scan can tell from a live one.
- A block yielding nothing denotes nothing, whatever its last statement computed.
- An outer local the region only reads is seeded from the walker's environment
  when it is constant there. A write position — an `Assign` target, a `&mut`
  borrow root, or an argument the callee's signature takes by `&mut` — or a
  reference-typed mention refuses the region instead.
- Inside a frame, a `let` binding a borrow of a local place resolves to an alias,
  and so does rebinding a local that already carries one: copying a reference
  copies the reference. An alias is resolved by a projection and by nothing else,
  so a capture never turns into a copy.
- A reference is recognized by shape rather than by spelling, since the boxing
  pass redefines `&T` into `Box<T>`. A cast between the same reference shape
  denotes its operand.

## TODO

Values the engine cannot represent:

- [ ] A place-valued field, so an aggregate can carry a reference. Today such an
      aggregate is not a constant, since a field holding the referent's value
      would take a write meant for the referent; what it needs to hold is the
      place the frame already names elsewhere. `Formatter { buf: &mut __r }`
      waits on this, and so does every result a `stores` callee hands back — and
      that refusal is whole-value, so a scalar field naming no storage is refused
      with the rest, which is why `Array::slice`'s computed bounds stop folding.
- [ ] An aggregate that is not a byte sequence has no way back into the IR. A
      `List<T>` of scalars wants the `ArrayLiteral` shape and a plain struct a
      `StructLiteral`, each needing its own answer to "is this already the
      literal the rewrite writes" so the worklist still settles.
- [ ] Comparing two literal strings, which reaches the engine as a guard and
      means running a method call over references.

Calls:

- [ ] A destructuring `let`; a body containing one is abandoned.
- [ ] Method calls, excluded because a `&mut self` receiver mutates through the
      call. Unnecessary for a receiver the frame owns, which the store can update.
- [ ] A call that returns nothing but whose every write lands in a place the
      frame owns, which is what lets a builder-style helper run.
- [ ] Closure calls: an indirect call whose closure is known is never resolved
      to a direct call, so neither inlining nor CTFE reaches through it.
- [ ] Recursion beyond a base case. The wasm-CTFE backend is the intended answer.

Control flow:

- [ ] A `switch` with a constant scrutinee. A switch is formed before inlining,
      so a scrutinee inlining makes constant survives untouched.
- [ ] Unrolling a loop in the caller — a code-size trade needing a cost model,
      not an evaluation capability.
- [ ] Guards decided when the engine is only asked what an expression denotes.

Sequences:

- [ ] A `&mut` argument writing back into the caller frame's place on return.
      Without it a buffer that grows still abandons the evaluation, because
      `grow` reshapes the caller's container from a frame of its own. What will
      not fit even then stays the wasm-CTFE backend's case.

### Compile-time string formatting

A template whose interpolations are all constant still formats at run time.
`` `n=${42}` `` reaches the end of the optimizer as a buffer allocation, two
byte pushes, a `Formatter` literal and a call to `i32::fmt_decimal` — a
digit-count loop and a division loop per evaluation, plus the formatting code
kept alive in the binary, for four bytes decided at compile time. Every `${}`
over constants, every `to_string()` on a literal and every constant `assert`
message pays it.

Nothing waits on trait dispatch: `Display::fmt` is devirtualized to a free call
before the optimizer runs. The aggregate exit, the store, the frame-executable
call and region recognition together fold the region to a literal, after which
globalization deduplicates it and DCE drops the unreached formatting functions.
What the remaining coverage waits on is the value model above: an interpolation
keeping its `Formatter` literal needs an enum value for the alignment field and
a place-naming value for the `&mut` buffer field.

Fold the region, not the call. A region constructs its own buffer, so every
value inside it is concrete. The call-level fold — rewriting one `fmt` over a
constant into `push_str(<literal>)`, which a template mixing constant and
runtime interpolations needs — claims more than one concrete evaluation shows:
that the callee appends the same bytes to every buffer. `#[compiler_item]` is
where that claim comes from, as it already does for `push_str`.

Mark `Formatter`'s write primitives, not `Display::fmt`: marking the trait would
extend the trust to every user-written impl, where nothing is checkable. What a
marked primitive declares is a region append — everything it does to the buffer
happens at or above the length the buffer had on entry, and what lies below is
neither read nor moved. That covers the stdlib's three formatting idioms
(`prepare_int_write`, `mark` / `apply_padding`, `fpfmt`'s reserving writers)
unchanged, where a strictly-append contract would not. What a particular `fmt`
body does is then derived: run it against a buffer the engine built, and admit
the result when every buffer access went through a marked primitive or landed
inside a region one just returned.

- [ ] Coverage, in order of engine cost: `bool` / `char` / `String`, then
      integers, then width / zero-pad / radix specs, then `Inspect`, then floats.
      A `String` interpolation folds already. Each step measures what it spends
      against the step budget; if `fpfmt` overruns it, floats are the wasm-CTFE
      backend's case rather than a reason to raise the budget.
- [ ] A remark for a region that nearly folded — one runtime interpolation, or an
      exhausted budget — plus a `wasm-size` and `benchmark` run to record what
      the whole thing bought.

### wasm-CTFE backend

- [ ] `wado-compiler` must compile to `wasm32-unknown-unknown`, so it cannot link
      a Wasm runtime. Whole-call evaluation therefore routes through the compiler
      host, as generator execution already does; hosts without a runtime decline
      and `niri` stays in-process.
- [ ] Triggered when in-process reduction gives up on a callee still pure by its
      effects, with the result used as if the in-process engine produced it.

## Complexity

Reduction is monotone and idempotent, and the optimizer's fixed-point loop is
the only fixed point: `niri` does not iterate internally. Each extension should
keep the engine's work bounded in the size of what it is asked about; a rule
whose cost is quadratic in body size, or that rebuilds a large value per query,
is the failure mode to watch for.

## Determinism

- Float NaN bits are nondeterministic in Wasm, so NaN-producing arithmetic is
  never folded, on either backend.
- `v128` and relaxed-SIMD have implementation-defined corner cases; SIMD CTFE is
  deferred to a later WEP.
- Integer wrapping and signed `MIN / -1` match Wasm. Division by zero and signed
  `MIN / -1` are left unfolded so the runtime trap survives.
- Float zero carries no sign through a fold: `-0.0` and `+0.0` are equal, so
  `if cond { -0.0 } else { 0.0 }` collapses to one of the two. A caller needing
  bit-precise zeros should get a per-operation equality predicate rather than a
  globally weakened fold.

## Out of scope

- User-facing CTFE syntax (`#[const_eval]`, `const fn`). Decide once real demand
  shows up.
- Salsa-style demand-driven reanalysis across compiler runs.

## Open questions

- Where does the wasm-CTFE module cache live — per `compile` invocation or per
  process? Per-invocation is simpler; per-process speeds up watch mode but needs
  eviction.
- Which primitives make the marked set. Every buffer touch in the formatting
  path has to reach one.
