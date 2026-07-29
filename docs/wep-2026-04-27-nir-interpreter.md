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

What the line does not forbid is a store over the values the engine itself
constructed. Inside a frame `niri` already executes statements in order, so a
value built there — and reachable from nothing the frame did not build — can be
written through and read back without asking which definition reaches a use.
The program's heap stays the `ValueGraph`'s; the engine's own is the engine's.

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
  global, or a compile-time call result. An aggregate leaves the engine only
  where it has a literal shape to be written as; otherwise what reaches the IR
  is the scalars projected out of it.
- A three-state lattice — unevaluated, constant, non-constant — with a join,
  so an unreachable branch contributes nothing to the result and a trapping
  arm does not contaminate a fold.

Bindings:

- Immutable locals whose initializer is constant read back as that constant.
  Mutable and assigned locals are non-constant.
- Immutable globals whose initializer is constant fold at every read, as do
  the known constant fields of a global (a sequence global's length).
- A global whose value is not in its slot but in the assignment that fills it —
  what an extracted initializer and body globalization both leave behind —
  reads back from that assignment. A global assigned anything that does not
  reduce, or two disagreeing constants, stays non-constant. Reads inside module
  initialization need no exception: initializers are ordered by dependency, so
  a read there already follows the assignment it would fold from.
- Nothing is read off a global something writes through. `global` without `mut`
  forbids reassignment, not in-place mutation, so a `&mut self` method, a
  mutable borrow, or a store to a projection of one makes both its value and
  its known fields stale — the same discipline a local carrying an aggregate is
  held to.
- An aggregate constant binds to a local only when every mention of that local
  merely reads it. `niri` models whole values, not the heap, so a local
  another handle can write through would go stale.
- A borrow denotes what it points at rather than operating on it, so a local
  bound to `&CONST` carries the referent and reads project out of it.

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
  non-async, monomorphic, and returning something runs at compile time. A call
  that yields nothing has no value to substitute, and handing one back would
  leave a value where the program expects none. The body executes statement
  by statement, so a `let` sequence, assignment to a local, an `if` whose
  condition decides, an early `return`, a labeled block completed by its
  `break`, and a loop all reach a value. Recursion and total work are bounded.
  A body that does not produce a constant leaves the call in place, so a
  runtime trap inside it stays observable.
- A statement counts as executed only when everything it evaluates lands on a
  constant. Reducing an expression is not performing it: an unfolded call, a
  global write, or an operation that would trap all leave work undone, and
  stepping past them would drop it.
- A loop needs no constant trip count. The work budget bounds it, so one that
  does not finish in time abandons the evaluation rather than guessing, and
  what an iteration derived does not survive into the next. The budget is per
  function, so what one spends cannot decide whether the next one folds.
- A local the frame cannot track — one a mutable borrow, a mutable argument, a
  method receiver, a store through a projection, or an assignment buried inside
  a larger expression can write — carries no value, so a stale constant cannot
  outlive the write.
- A string literal's `len()` folds, as a consequence of the generic
  struct-field projection rather than any string-specific rule.

Sequences:

- The array backing a `String` or a `List` is a value, built from a byte-string
  literal or a fully-constant array literal, bounded by a maximum length past
  which building one would cost more than any fold it enables. `String` and
  `List` need no case of their own: each is an aggregate whose backing field is
  a sequence and whose length field is an integer — and an array literal
  denotes that whole container, since that is what it lowers to.
- Whether a local may carry an aggregate is decided from the reachable body. A
  node an earlier rewrite orphaned cannot run, so it must not disqualify one:
  inlining `t.len()` leaves the original method call behind, and counting its
  receiver would refuse every list the caller then reads.
- An element or length read folds through the array builtins the read lowers
  to, not through an index node. Both the generic builtin and the `u8`
  specialization are recognized. A read past the end is left alone, since it
  traps at run time.
- A shared borrow reads as the constant it points at, which is what makes a
  backing array reachable at all — it reaches the builtin as `&arr`. Only a
  shared one: a write goes through a mutable borrow, which stays unmodelled.
- A constant list's length folds, so a constant-index bounds check on it does
  too, and so does the element that check guards — out of a local literal, and
  out of a global, whose container the engine recovers from the assignment that
  fills its slot. Only a scalar element reaches the IR; an aggregate one stays
  inside the engine, as every aggregate does.
- An element write lands. `array_set` through a `&mut` reaching a place the
  frame owns — a local it bound to a constant, plus the field path into it —
  updates that local's value, so a later read sees what was written. The write
  is performed, not folded: it only counts at statement position, where the
  executor runs it. A place rooted anywhere else — a parameter, a global,
  anything the frame did not build — has no current value to update and
  abandons the evaluation rather than being stepped past.
- A borrow handed to a sequence builtin does not make its root stale: the
  executor performs the write itself, and a read cannot write at all. Every
  other borrow still does.
- A byte-sequence container a compile-time call produced is written back as the
  literal the lower phase emits for a source string — a struct over a packed
  byte array and its length. The bytes are the container's first `used`, since
  a grown container's capacity outruns what it holds and capacity is not
  observable. Only a call is rewritten this way: the literal denotes the value
  it replaced, so materializing one again would report a change at every visit
  and the worklist would never settle.

## TODO

### Values the engine cannot represent

- Enum and variant values with their payloads. Today an enum or variant
  pattern cannot be decided, so an `Option` / `Result` accessor exposed by
  inlining leaves a residual match the engine walks past.
- An aggregate that is not a byte sequence has no way back into the IR. A
  `List<T>` of scalars would want the `ArrayLiteral` shape, and a plain struct
  a `StructLiteral` over its materialized fields; both are exits to add beside
  the byte-sequence one, and both inherit its `Call`-only restriction until
  something establishes the value did not come from the node being rewritten.
- Comparing two literal strings. A string pattern reaches the engine as a
  guard, and deciding it means running the comparison — which is a method call
  taking references, so it waits on the two entries below rather than on the
  value model.

### Calls

- A destructuring `let`, which binds a pattern rather than a name; a body
  containing one is abandoned.
- Method calls, excluded because a `&mut self` receiver mutates through the
  call. Worth revisiting now that mod-ref and alias analysis can prove a
  receiver is not written — and unnecessary for a receiver the frame owns, which
  the store below can simply update.
- A call that returns nothing. Eligibility asks for a value to substitute, which
  is the right question for replacing a call and the wrong one for running it:
  a call whose every write lands in a place the frame owns is executable as a
  statement whatever it returns. Splitting the two — value-CTFE and
  frame-executable — is what lets a builder-style helper run.
- Closure calls: an indirect call whose closure is known at the call site is
  never resolved to a direct call, so neither inlining nor CTFE can reach
  through it.
- Recursion beyond a base case. The wasm-CTFE backend below is the intended
  answer.

### Control flow

- A `switch` with a constant scrutinee is not folded, although `if` and
  `match` are. Since a switch is formed before inlining, a scrutinee that
  inlining makes constant survives to the end untouched.
- Unrolling a loop in place in the caller. Distinct from running one during a
  compile-time call, which is done: this is a code-size trade needing a cost
  model, not an evaluation capability.
- Guards decided when the engine is only asked what an expression denotes;
  today an arm's bindings are only in scope on the rewriting path.

### Sequences

- The rest of the spine. Element and field writes land, an allocation denotes a
  zero-filled sequence and a copy a spliced one; what remains is a `&mut`
  argument writing back into the caller frame's place on return. Without it a
  buffer that grows — which is what `String` does the moment it outruns its
  capacity — still abandons the evaluation, because `grow` reshapes the
  caller's container from a frame of its own. What will not fit even then — a
  table past the length cap, a fill loop past the step budget — stays the
  wasm-CTFE backend's case.

### Regions

- A closed block is not evaluated, only a call is. A block that builds a value
  in locals of its own, writes only to those locals, and yields one of them is
  as self-contained as a call body and needs no more machinery to run — the
  difference is that the caller wrote it inline. Recognizing that shape is what
  turns the string-template case below from a call-level problem, which needs a
  contract about the caller's buffer, into a frame the engine starts from
  scratch.

### Compile-time string formatting

A template whose interpolations are all constant still formats at run time.
`` `n=${42}` `` reaches the end of the optimizer as a buffer allocation, two
byte pushes, a `Formatter` literal, and a call to `i32::fmt_decimal`, paying a
digit-count loop and a division loop per evaluation — and keeping the
formatting code alive in the binary — for four bytes decided at compile time.
The same string written `"n=42"` folds to a deduplicated constant global.
Every `${}` over constants, every `to_string()` on a literal, every constant
`assert` message, and every constant `${x:?}` pays this.

Nothing here waits on trait dispatch: `Display::fmt` is monomorphized and
devirtualized to a free call before the optimizer runs. What it waits on is the
four entries above — the aggregate exit, the store, the frame-executable call,
and region recognition. Together they fold the region to the literal the source
could have written, after which constant-object globalization deduplicates it
and DCE drops the formatting functions no live call reaches.

Fold the region, not the call. A region constructs its own buffer, so every
value inside it is concrete and nothing is assumed about it.

The call-level fold — rewriting one `fmt` over a constant into
`push_str(<literal>)`, which is what a template mixing constant and runtime
interpolations needs — claims more than one concrete evaluation shows: that the
callee appends the same bytes to every buffer, not just to the one it ran
against. `#[compiler_item]` is where that comes from, as it already does for
`push_str` — the rewrite expanding `buf.push_str("abc")` into per-byte `push`
is licensed by the marker, not by an analysis of either body.

Mark `Formatter`'s write primitives, not `Display::fmt`. Marking the trait
would extend the trust across every user-written impl, where nothing is
checkable; a primitive is one small stdlib function a reader can confirm, the
same obligation `push_str` already carries.

What a marked primitive declares is a region append: everything it does to the
buffer happens at or above the length the buffer had on entry, and what lies
below is neither read nor moved. That is the stdlib's formatting idiom as
written — `prepare_int_write` reserves a region the digit writers fill
backwards, `mark` / `apply_padding` appends content and then shifts it to make
room for alignment, and `fpfmt`'s writers reserve and slide a fractional tail
to insert the point. A strictly-append contract, where bytes land on the end
and are never revisited, is not the design: a padded float cannot learn its
length before appending, so reaching it would mean formatting through an
intermediate buffer, which costs more at run time than the contract is worth.
Region append covers all three idioms unchanged and is still one sentence.

What any particular `fmt` body does is then derived rather than declared: run it
against a buffer the engine constructed, and admit the result when every buffer
access either went through a marked primitive or landed inside a region one of
them just returned. A body that reads the buffer's prior length for its own
purposes, or reaches `f.buf` outside that, is refused — a condition the engine
checks rather than an invariant it hopes for.

The markers also keep the buffer plumbing out of the interpreter: a marked
`push_str` is applied by its declared meaning, so `grow`'s undecidable capacity
test and `realloc_to`'s prefix copy are never interpreted. The reserved region a
primitive hands back is the same place the frame store hands out, so the two
capabilities want the same representation.

Milestones, each red/green with the fixture first:

- [x] The aggregate exit. A compile-time call returning a constant `String`
      folds. No cap of its own is needed: `MAX_SEQ_ELEMENTS` already bounds what
      becomes a sequence value, and a payload past the inline threshold reaches
      the binary as a data segment rather than as code. A container the frame
      never filled stays as the source wrote it: an empty one is a reservation
      rather than a result, and a literal cannot carry the capacity it asked
      for.
- [x] Places and the frame store. Element writes, field stores, allocations and
      copies all land through a frame-owned place.
- [x] Frame-executable calls. A call writing through a `&mut` parameter runs and
      writes back into the caller frame's place, so a compile-time call that
      fills and returns a `List<u8>` folds — growth included. The write-back is
      not separable: what fills a container is `push`, which returns nothing, so
      the caller's place is the only thing the run produces.

      The write-back is confined to statement and `let` position, where the
      executor runs a call exactly once. The lattice projection is re-entrant,
      so a mutating call is refused there outright: a write applied twice is
      worse than a fold missed.

      Which places stay trackable divides on what reaches them, not on where
      the question is asked. A shared receiver, a by-value argument and a
      builtin's source cannot be written through, so they are exempt wherever
      they appear — that is what lets a container survive the `&self` reads
      `push` makes of its own capacity. A `&mut` one is exempt only inside a
      frame, which performs the write or abandons the evaluation; an ordinary
      walk performs nothing, and a write it steps over leaves its target stale.
- [ ] Region recognition. `` `ab` `` and `` `a${"b"}` `` fold to one literal.
- [ ] Coverage, in order of engine cost: `bool` / `char` / `String`, then
      integers, then width / zero-pad / radix specs, then `Inspect`, then
      floats. Each step measures what it spends against the step budget:
      integer formatting is two short loops and fits, and `fpfmt` is the
      candidate for overrunning it — if it does, floats are the wasm-CTFE
      backend's case rather than a reason to raise the budget.
- [ ] A remark for a region that nearly folded — one runtime interpolation, or
      an exhausted budget — so a missed fold is visible instead of silent, plus
      a `wasm-size` and `benchmark` run to record what the whole thing bought.

A template with any runtime interpolation keeps today's imperative form,
including the loop-buffer reuse `tmpl_hoist` gives it.

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
- Which primitives make the marked set. Every buffer touch in the formatting
  path has to reach one, so the set is whatever `write_str`, `write_char`,
  `pad`, `mark` / `apply_padding`, `prepare_int_write`, and `fpfmt`'s reserving
  writers turn out to factor into.
