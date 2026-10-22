# WEP: Resource Ownership — Move-Only Resources, Authoritative Cleanup, and Value-Copy Elision

## Context

A Wado `resource` is an opaque handle to a Component Model (CM) resource. CM
already enforces an ownership discipline — no double-drop, no
use-after-transfer, no leaked borrow — but dynamically, as a trap, and only at
the component boundary. This WEP makes resources move-only and builds the static
analysis that turns that discipline into compile-time guarantees plus
deterministic cleanup.

The analysis answers one question — is this the binding's last use, or is the
value still live afterward? — which also decides, for a copyable value type,
whether a materialization needs a deep copy or can transfer storage in place. So
value-copy elision is a second client; this WEP records its implementation too.

The `move` and `unique` keywords once reserved for this work are unnecessary:
the analysis needs move-only semantics, not new syntax
([No `move` or `unique` keyword](#no-move-or-unique-keyword)).

[WEP: Affine Types](./wep-2026-10-17-affine-types.md#what-this-replaces)
replaces part of this one. A call through a reference no longer copies or
extracts, since the signature states it. Cleanup synthesis retires for the
scope-exit drop of what is not null. Most of the resource gaps below close.
The move check and value-copy elision stand.

### CM handle types

| Handle      | Meaning                                                                        |
| ----------- | ------------------------------------------------------------------------------ |
| `own<R>`    | Unique owning handle; dropping the last `own` runs the `dtor`.                 |
| `borrow<R>` | Non-owning handle lent from an `own`; must be dropped before the call returns. |

A handle is an opaque `i32` index into a per-instance table (or an `externref` in
CM-GC mode). `lift_own` transfers ownership and traps if the handle is lent out;
`lift_borrow` leaves the source in place; a task returning with a live borrow
traps.

### Wado today

- `resource` methods take `self: &Descriptor` (a reference) or the resource by
  value (a consuming method like `Request::consume_body`). By-value `self` is
  already supported.
- CM-binding synthesis maps `&R` → `borrow<R>` and by-value `R` → `own<R>`.
- The value-copy machinery already treats resources as non-copyable (an identity
  copy, never a deep clone), so a resource never physically duplicates.
- `resource_cleanup.rs` inserts `resource.drop` for every owned resource a body
  does not transfer, reconstructing ownership by dataflow.

## Decision

### Three resource kinds

| Kind                 | Backed by                                              | Ownership           | Cleanup                  |
| -------------------- | ------------------------------------------------------ | ------------------- | ------------------------ |
| Affine resource      | a CM `own`/`borrow` or waitable handle, or a guest one | Move-only + a check | `resource.drop` / `dtor` |
| Host-object resource | a Wasm GC reference to a host object (no `dtor`)       | Value semantics     | Wasm GC                  |
| Non-owning token     | an index naming something owned elsewhere (no `dtor`)  | Value semantics     | none of its own          |
| Unreclaimed handle   | an index into a host table nobody frees (no `dtor`)    | Value semantics     | none — it leaks          |

The `dtor` decides the kind, not the representation. A handle that owns one must
be move-only, because copying it aliases a destructor; without one there is
nothing to free, so the handle is an ordinary value. `i32`-vs-`externref` is
orthogonal to all four rows.

The three `dtor`-less rows are one linearity, which
[Resource Inheritance](./wep-2026-04-28-resource-inheritance.md) lets a
declaration write out: `#[cm(..., linearity = "unrestricted")]`. The other
spelling, `"affine"`, is the default and so goes unwritten. Only the web bindings'
resources declare it today; the non-owning tokens still take their value
semantics from the absence of a `dtor` alone.

### Non-owning tokens

A non-owning token names something whose lifetime another party already
guarantees. Two backings qualify:

- **An affine resource owns the referent.** `Waitable` is this: `Subtask::join`
  returns the subtask's own handle number as the identity to match a `WaitEvent`
  against, and the `Subtask` owns the drop.
- **An immortal table in a statically composed component.** `core:icu`'s
  interned handles are this: the component interns each object whose
  configuration the program bounds at compile time, so the table is finite by
  construction and never freed.

A CM `borrow<R>` cannot stand in for either: it must be dropped before the call
returns, where a token is compared long after, is copied across comparisons, and
travels inside a struct by value (`WaitEvent.handle`).

A referent with neither backing — allocated per call from unbounded runtime
input — is affine instead: a copyable index would leak it.

### Unreclaimed handles

The web bindings' browser handles break the rule above. They are indices into a host table,
allocated per call from unbounded runtime input, with no affine owner and no
immortal bound, so the rule makes them affine. They cannot be: an upcast in a
`resource extends` hierarchy copies the handle
([Resource Inheritance](./wep-2026-04-28-resource-inheritance.md)), and an
affine handle may not be copied.

They get value semantics instead, and pay for it. Nothing frees them, and each
one costs a table slot for the life of the instance. That is a known gap of that
WEP, not a fourth way to be safe.

The rest of this WEP concerns affine resources.

### Move-only resources

An affine resource is move-only, with no keyword:

- It cannot be copied; `let b = a;` moves `a`, which is then invalid.
- It transfers by value passing, returning, placement in an aggregate, or a
  by-value `self` receiver.
- Using a binding after it is moved is a compile error.

Transfer is implicit at the consumption site — there is no `move` operator, no
lifetimes, no exclusivity analysis.

The receiver has exactly three spellings and never a type annotation:

| Spelling    | Meaning                       | CM handle   |
| ----------- | ----------------------------- | ----------- |
| `self`      | by-value move — resource-only | `own<R>`    |
| `&self`     | shared reference              | `borrow<R>` |
| `&mut self` | mutable reference             | `borrow<R>` |

`self: T` and `self: &T` are rejected. A copyable type has no use for a value
receiver (`&self` plus a `*self` deref-copy covers it), and a single reference
spelling removes a needless choice. Bare `self` is legal on a resource or an
aggregate that carries one (its consuming method hands that resource off); on a
value type that owns no resource it is a diagnostic (`value types are not move-only; use &self`) — this is how the value-semantics friction is mitigated,
not with a keyword. The rule licenses `fn drop(self)`.

Because the annotated forms are gone, the receiver kind is carried entirely by
`SelfKind` (`Value` / `Ref` / `MutRef`); `SelfKind::None` means a genuine
non-method, and a call consumes its receiver iff the dispatch resolves to
`SelfKind::Value`.

### The move check

`resource_move_check.rs` gives resources their move-only semantics in one
diagnostic pass.

Layer — it runs on the `Semantics` layer (AST + type facts), a sibling of
`effect_check::check_semantics`, so it has source spans, surfaces in both
`wado compile`/`check` and the editor, and sees every function. Post-mono TIR was
rejected: it has neither diagnostic spans nor a call-site origin.

Analysis — a forward walker over each body (free / `impl` / `trait` / `test` /
function-local). Each resource binding is live or moved-at-span; a by-value
consumption records a move and a later use is reported. Branch joins union the
moves and drop a diverging path's; a loop body is walked twice to catch a
cross-iteration move; a `let` / `for-of` re-bind and a re-assignment clear state.

It is independent of the value-copy client's last-use liveness
(`lower/plan/value_copy/last_use.rs`, post-mono over TIR); unifying the two is
future work, not a present fact.

Done: use-after-move of a bare resource local, by-value `self` consumption (a
bare `self` receiver moves it, via a `consumes_self` dispatch fact — the
semantic-layer twin of `resource_cleanup`'s `owned_self`), and whole-move of a
resource-carrying struct / tuple / `Result`, and no-move-out-of-reference (a `&self`
return rooted in the reference that carries a resource) — see below.

### Extraction is a by-value receiver (Rust-aligned)

Pulling a resource out of a `Result` / `Option` returns an owned handle, so the
source must relinquish ownership. `unwrap(&self)` cannot — a reference keeps the
receiver owned, so the returned handle would _alias_ the one still inside the
container: two owners of one move-only resource, hence a double-free. The fix
mirrors Rust: the consuming accessors (`unwrap` / `expect` / `unwrap_err` /
`expect_err`) take `self` **by value**, so `res.unwrap()` consumes the whole
`Result` and moves the interior out with a single owner. For value (copyable)
types this is the usual value-semantics copy — elided to a move at last use — so
`.unwrap()` stays ergonomic and no rejection is needed. Inspectors (`is_ok`, …)
keep `&self`.

By-value `self` on a value type is otherwise a diagnostic
(`SelfByValueOnNonResource`); the generic `Result<T, E>` / `Option<T>` impls are
permitted because a generic self-type resolves to a `GenericInstance`, not a
concrete value type.

### No move out of a reference

A hand-written `fn f(&self) -> Resource { return self.f; }` (or `match *self { … => interior }`, or a `let`-bound projection) moves a resource out of a referenced
place: the reference keeps the source owned, so the returned handle aliases it — a
double-free. `resource_move_check` rejects it. For each function it seeds the
reference parameters (`&self` / `&mut self` / `&T`), tracks bindings projected
from them (through `let`, field access, deref, and match-arm bindings whose
scrutinee is a reference), and flags a `return` whose value is rooted in a
reference and whose type carries a resource. A `&self` method returning a
_freshly produced_ resource (`dir.open_at()`) stays legal (its return roots in a
fresh allocation, not the reference).

The gate is the concrete return type carrying a resource, so a generic body
(`T` abstract) is not flagged at its definition. Its call sites are, by the
rule below.

### A call through a shared reference copies

A generic body that returns a `T` it reads through a reference cannot be told
apart, by its signature, from one that hands back a fresh value. So the
signature decides, by the receiver kind: a generic function whose result names
a type parameter that some `&` parameter stores, and no `&mut` parameter does,
copies its result out of the referenced storage. `list[i]`, `List::last` and a
user's `fn first(&self) -> T` are such copies. A parameter stores a type
parameter through a field, an element, a payload or a reference, not through a
function type's result, so `AsyncCall<T>::wait(&self) -> T` hands back a fresh
`T`. The referenced storage keeps what was copied, so where the result holds a
resource, a position that takes it (a binding, an argument, a `return`) is
refused. Reading it where it stands (`list[0].has(name)`) leaves no second
owner:

```wado
let list: List<Fields> = [Fields::new()];
let f = list[0]; // error: cannot copy resource `Fields` out of a reference
```

The check runs where cleanup classifies the call, on the TIR before
monomorphization, so a call written in a body generic over `T` is checked
where that body's own signature is called, not inside it.

### A call through `&mut` extracts

A generic function whose result names a type parameter that a `&mut` parameter
stores takes it out of the referenced storage, as `List::pop` and
`List::remove` do. The caller owns the result whether or not a binding takes
it, so `while let Some(_) = list.pop() {}` drops each element.

A result whose type parameters no reference parameter stores is fresh
(`List::map`), and the caller owns it as any other call's.

### Authoritative cleanup

`resource_cleanup.rs` drops every owned, untransferred resource on each
fall-through path, structurally for aggregates via a synthesized `match`. A value
is transferred when passed by value, returned, placed in an aggregate, or used as
the receiver of a by-value (`self`) method — so extraction (`unwrap`) is just a
by-value receiver transfer, with no aggregate-shape guessing. Two fixes landed:

- A `&self` inspector on a `Result<Resource, E>` (`is_ok`, …) takes a reference and
  leaves the structural drop intact (#1569).
- A resource value discarded in statement position (`Fields::new();`,
  `let _ = …`) is dropped, not leaked; a tail expression is left for its
  consumer.
- A store into a binding or one of its fields drops the resource it replaces,
  after the stored value is computed, since computing it may read the old one.
  A binding whose resource was moved owns the one stored into it.
- Where paths join, a binding holds its resource after the join only where
  every path into it does, and a path holding more drops the surplus before it
  joins. The joins are an `if` or `match`, a loop's head (its entry, each
  iteration's end and each `continue`) and its exit (each `break`), and a
  labeled block's exit. The loop head is a fixpoint, since an iteration starts
  from it, so cleanup elaborates a loop body as many times as it takes to settle
  and a labeled block's body twice, throwing away all but the last.

The `is_resource_aggregate` heuristic is **retired**: cleanup now drops "owned and
not transferred at scope exit" directly.

Whether a type holds a resource is answered by one walk,
`resource_holding::holds_resource`, for the move check, a by-value `self` and
this pass alike. Each supplies a type's members typed at its instance, and the
walk cuts a declaration it reaches again with arguments that hold alike, which
ends on polymorphic recursion as well as on a type that holds itself.

### Deterministic drop

An owned, un-moved resource is dropped at scope exit on every path; a move
suppresses that drop. An imported handle drops via `resource.drop`; a
guest-defined one runs its destructor (WEP 2026-01-12). An explicit
`fn drop(self)` consumes the receiver, so exactly one drop fires; a `&self` drop
would be unsound (the binding outlives its handle and double-frees). An aggregate
containing a resource is move-only and gets a synthesized destructor in
declaration order. An aggregate that holds itself (`next: Option<Node>`) drops
through a function synthesized per type, `$drop$…`, since expanding its drop in
place never ends. Panic does not unwind, so a drop is not guaranteed on panic.

### CM boundary

| Wado position                      | CM handle                         |
| ---------------------------------- | --------------------------------- |
| by-value `R` parameter / return    | `own<R>`                          |
| `&R` parameter, `&self` receiver   | `borrow<R>`                       |
| by-value `self` (consuming method) | `own<R>` — transfers the receiver |

Once the move check proves a by-value resource uniquely owned with no live
borrow, CM's `lift_own` preconditions hold statically.

### Value-copy elision (the second client)

A copyable type uses the same last-use question to move (last use → no copy),
copy (source still live → deep copy), or share (neither side mutates again →
alias), replacing the old "copy-everything-then-elide" scheme. As implemented it
is caller-side and single-phase (`lower::translate` emits `$value_copy$T` only
where it cannot prove move / share / fresh; no elision pass):

- Move — `last_use::analyze_ownership`, whose backward walk over TIR decides a
  read is final and marks it on its node. It runs on the body lowering sees, so
  a read synthesis wrote is judged like one the source spelled. A final read
  moves unless a kept reference pins its local, its callers pass it uncopied,
  or it binds a match scrutinee's storage. This also covers a **place-level move**: at a struct /
  tuple literal, a whole-value or clean-field materialization aliases out of a
  _dead_ aggregate (the root is dead after the literal, sibling reads neither
  mutate nor move it, and the materialized fields are disjoint owners) instead of
  deep-copying — so `T { x: p.a, y: p.b }` over a dead `p`, and a shorthand store
  `S { data, n: data.len() }` past a read-only reuse, both move. A method
  receiver reference escapes only if the callee stores its receiver specifically
  (`receiver_storing_methods`), so a `&mut self` mutation like `List::push`
  (which stores only its value) no longer pins the receiver local. A `let` is
  the same site: `let v = p.a` over a dead `p` moves, which is what keeps an
  unrolled variadic body from deep-copying the element it binds. A call that
  takes its argument by `&` and returns a field of it (its `ReturnPath`)
  materializes that field as `p.a` does, so `Out { repr: b.raw() }` over a dead
  `b` moves too.
- Declared hand-over — a `let` marked `skip_value_copy` owns its storage by
  construction, so the freshness fixpoint seeds it instead of asking what the
  source expression looks like, and the ownership chain reaches the `match` arms
  and literals downstream of it. A tuple unroll marks its element bindings that
  way: the temp it reads is private to the unroll and each field has exactly one
  reader, so the field is the binding's to take.
- Freshness — `ownership.rs` return conventions (a call is fresh iff the callee
  returns owned), plus the literals that materialize their own storage: a string
  _and_ a bytes literal, both of which lower to a fresh aggregate over a packed
  array. Where a result comes from is one fact: a body is read for it, a
  `core:builtin` declares it. `#[result(owned)]` says the result is a fresh
  place; `#[result(part_of = p)]` says it names a component of parameter `p`.
  The declaration is mandatory. A bodyless declaration reading through a
  reference and returning storage is refused without one, at the declaration,
  where there is a span to point at. What is left
  undeclared cannot hand storage out at all, so reading it as fresh is a fact
  rather than a guess. The obligation is checkable from the
  signature because only a reference argument can carry storage out: a by-value
  one is copied at the call. Link also snapshots what it read, since
  monomorphization drops the generic declarations and materializes an instance
  only for the few a later phase rewrites.

  A function that returns a projection rather than a fresh place is fresh
  exactly when the argument it projects is, and `returns_self_projection`
  records which parameter that is. The caller tests that argument, not the
  receiver. `VariantCase::extract(&self, w)` returns a component of `w`; testing
  `self` asks about a member descriptor the walk built fresh, and calls the
  result fresh while `w` is still held. A `core:builtin` answers the same
  question from `#[result(part_of = p)]`, which names `p` outright.

  An _indirect_ call is fresh when every closure `$call` of its
  return type returns owned: closure lowering rewrites every callable value —
  a closure literal and a bare `FuncRef` alike — into a functor whose `$call`
  is an ordinary function, so those are the complete set of targets, and the
  call's own signature narrows them to one return type. The verdict is derived
  from the return-convention fixpoint and so is computed after it, never inside
  it.
- Representation-preserving casts — a newtype shares its base type's
  representation (WEP 2026-01-29), so `bytes as ByteArray` hands over the same
  storage and the move side reads through it, as freshness always did. Without
  that, `String { repr: bytes as ByteArray, used: len }` — how the CM string
  lift and `String::substring` build their result — deep-copies an array the
  caller just allocated.
- Branch results — a block's value is its tail expression and an `if`'s is the
  tail of whichever branch runs, so each is fresh exactly when those tails are,
  the rule `match` arms already followed. A router dispatch — a response bound
  from an `if let` over the matched route — needs both this and the
  indirect-call rule, or every later field read of the binding is deep-copied.
- Component-wise return conventions — a call-graph component settles with its
  own members assumed owned and the ones a walk refutes dropped. Grown from
  nothing, a cycle whose members return each other's result (`lower_element`
  yields `lower_group`'s value and `lower_group` yields `lower_element`'s) has
  each reading the other's unproven "shared", and every result the family
  builds is deep-copied.
- Transient references — a referent is pinned only at a move where a holder of a
  reference to it is still readable. When a holder is readable is stated in
  [WEP: Reference Liveness and Assignment to a Referenced Place](./wep-2026-10-13-referenced-places.md#a-reference-lasts-while-its-holder-is-readable).
- Confinement — `confine.rs` per-parameter escape fixpoint. A builtin declares
  no function, so it is absent from the table that walk builds; reading absence
  as an opaque callee made every parameter handed to one escape. `a[i]` is a
  builtin call, so that was every function that indexes an array — including
  `String::cmp`, and so `TreeMap::find_index`, whose key every lookup then
  deep-copied.

  What a builtin keeps of what it was passed is read from its `#[retain(...)]`,
  which only a body-less declaration carries. `array_set` and `array_fill` store their
  `value` into the array; nothing else a builtin is handed outlives the call.
  Link snapshots the positions alongside the return convention, since
  monomorphization drops the declarations either fact would be read from.
- Where a stored reference lands — `stores.rs` separates the position a callee
  routes into its _return value_ from one that reaches a global or is written
  through a reference the caller owns. A caller must assume the union, but the
  fixpoint must not: an iterator holds a reference to what it walks, so one set
  makes every `&List` parameter stored the moment the body iterates it, while a
  `collect()` that drops the iterator stores nothing. A `#[retain(p)]` with no
  `into` names no destination: a value-returning body is read as handing it out with
  the result, a void one (`List::push`, storing through a builtin) has nowhere
  visible to put it and keeps the strong reading. A builtin has no body to read
  either, so a reference-typed call result carries every argument —
  `array_get_ref` aliases its container, and `List::index_ref` is that call.
- Read-only-share — a read-only binding whose storage is never mutated while
  live. See _Sharing_ below.

A recursive type's `$value_copy$T` is a true deep copy (a mutually-recursive
helper), replacing an identity fallback that silently shared storage.
`optimize::escape` and `optimize::value_copy_elide` are deleted.

A function that calls itself is assumed to return owned while that is being
proved: the fixpoint only adds, so its own call would otherwise read back the
verdict being computed and pin it at shared. The returns that do not go through
the recursive call are still checked on their own, and those base cases are what
a recursive result is built from.

### Sharing

A read-only binding may alias its source's storage when nothing writes that
storage _while the binding is live_. Both halves are readings of one backward
walk: what is live at each point, and what each point writes. A write conflicts
when the binding is live there and the two places may alias; the same write
elsewhere in the body does not. Without liveness a deserializer deep-copies a
field it read before any `&mut self` call runs. Who owns the storage a place
names, what is live at a point, how a write is placed and what a call writes are
stated in
[WEP: Reference Liveness and Assignment to a Referenced Place](./wep-2026-10-13-referenced-places.md).

A move also hands storage on, so a binding the fold will move out of cannot share
it. Two reads are such a move: a value read of the binding, and a place-level
move of a field out of it. A binding with neither is handed nowhere, so refusing
it a share on move-eligibility alone costs a copy that defends nothing.

A share licenses the binding and nothing built out of it. `let mut b = a` and
`Wrapper { inner: a }` mint a second owner, which outlives the binding and may be
written through, so each owes its copy though `a` shares. Only a move exempts
them, and a move is proved of the binding itself, never inherited from what the
binding was read out of.

A `match` over a writable place is hoisted into a temp, and the fold decides that
temp's copy for every binding under it. One wrap site answers for the whole arm:
the bindings read the temp, so each is as defended as the temp is. A place nothing
can write is matched where it lies, its bindings costing nothing.

That temp dies at the match. Reading it alone therefore calls the place it was
hoisted out of dead while that place is still live, which moves an arm binding
out from under its holder. A source root stands on its own chain, and the whole
chain answers instead. The share side always closed its live set that way, and
the move side now reads the same closure. Both sides close over places rather
than root locals, so two components of one aggregate are not readers of each
other.

Handing the source root to a new owner costs the binding its share on the same
terms. That owner may write what the binding aliases, so the read matters where
the binding's storage is still read afterwards, and nowhere else.

A `List` or a `String` copy right-sizes its backing storage to the current
length, which a share skips. Capacity is not part of the value, but `capacity()`
reads it, so a binding whose own capacity is read keeps its copy. One that never
asks observes no difference and shares.

### Which helpers exist

`$value_copy$T` is additive synthesis, so the helpers are created in `plan` —
before the fold that decides where to call them, and before pattern lowering
mints the temps some of those calls land on (WEP 2026-05-11). The seed must
therefore be complete without predicting what a later pass writes: it reads the
types a program declares, since no expression rewrite introduces a type the
program did not already name. Over-synthesis costs nothing — `dce` removes an
unused helper — while a miss leaves the fold no helper to call.

### No `move` or `unique` keyword

Move-only semantics need no syntax: transfer is implicit, a use-after-move
diagnostic replaces a `move` operator's local visibility, and the consuming
receiver is spelled bare `self` (scoped to resources, so it never contradicts
value semantics — see [Move-only resources](#move-only-resources)). `unique`
as a `struct` modifier only matters for a move-only type that carries no
resource — a resource-bearing aggregate is already move-only by composition, and
no use case has appeared. Current state: neither `move` nor `unique` is a
keyword. If revived, `unique struct`
reuses the same move-check machinery, resources being its first client.

## Amendments to earlier WEPs

- WEP 2026-04-28 (Resource Inheritance): its "value semantics, no borrow
  checker" and "every `resource` method takes `&self`" claims are narrowed to
  host-object resources; affine resources additionally have by-value consuming
  methods. Applied there.
- WEP 2026-03-01 (CM Canonical Attributes): the resource `drop` method is
  amended from `fn drop(&self)` (unsound under move-only) to `fn drop(self)`; the
  call syntax is unchanged. Applied there.

## Consequences

- One story: move-only affine resources, value semantics for the two kinds that
  own nothing. Double-drop / use-after-transfer / leaked borrow become compile
  errors.
- No lifetimes, no annotations, no keyword. The cleanup heuristic and
  `optimize::escape` + `optimize::value_copy_elide` shrink or disappear.
- Move-only is a new concept for one type category; an element cannot be moved
  out of a resource-bearing aggregate (restructure around iteration); a resource
  API returns owned handles, not `&R`.

## Roadmap

Verified against the tree.

### Move check

- [x] `resource_move_check.rs` on `Semantics`, wired into batch and LSP; covers
  free / `impl` / `trait` / `test` / function-local bodies.
- [x] Use-after-move of a bare resource local (branch-join, divergence-aware,
  loop-carried, rebind clears).
- [x] By-value `self` consumption via the `consumes_self` dispatch fact.
- [x] Resource-carrying aggregates (struct / tuple / `Result`) are move-only,
  kept in step with cleanup's `carries_resource` (variant / `Option` /
  `List` deferred with their destructors); whole-aggregate move only.
- [x] No-move-out-of-reference: a `&self` / `&T`-param return rooted in the
  reference whose concrete type carries a resource is rejected.
- [x] Taking a resource a generic call copies out of a `&` parameter is
  refused at its call site; one through `&mut` extracts, and its caller owns the
  result. The check is cleanup's, on TIR, so the editor does not show it.
- [ ] Unify with the value-copy last-use liveness.

### Cleanup

- [x] `&self` aggregate call classified by return type (#1569).
- [x] Discarded resource value is dropped, not leaked.
- [x] A temporary in a referencing position (receiver, `&` argument, field read,
  operand, condition, scrutinee) is dropped at the end of its statement,
  and `break` / `continue` drop what their target encloses.
- [x] `is_resource_aggregate` retired: extraction accessors take `self` by
  value, so the receiver transfer is read directly (no shape guessing).

### Receiver grammar (`self` / `&self` / `&mut self`)

- [x] `SelfKind::Value`; parser accepts bare `self`, `consumes_self` is
  `self_kind == Value`, and `self` on a free function is a parse error.
- [x] A `Value` receiver is a diagnostic (`SelfByValueOnNonResource`) only on a
  value type that provably carries no resource; a resource-carrying aggregate
  (and any generic self-type) may consume with `self`.
- [x] stdlib `self: &R` → `&self` (via `wado-from-idl`); every `self:`
  annotation rejected, fixtures migrated, formatter normalization dropped.
- [ ] `syntax.rs` grammar + VS Code grammar for the new receiver forms.

### Consuming drop

- [x] `types.wado` drops migrated to `fn drop(self)`; `owned_self` keeps exactly
  one drop, and a use after `.drop()` is a move error.
- [x] Compositional destructor for struct / tuple aggregates (field-projected
  drops; `Result` keeps its `match`); variant / `Option` / `List` deferred.

### Value-copy client

- [x] `$value_copy$T` at `copy` sites only; read-only-share; recursive-type deep
  copy; `optimize::escape` / `value_copy_elide` deleted.
- [x] Place-level move at struct / tuple literals (whole-value / clean-field
  materialization out of a dead aggregate); receiver-storing precision so a
  value-storing `&mut self` method does not pin its receiver.
- [x] Freshness through an indirect call and through block / `if` results.
- [x] Move through a newtype cast (`bytes as ByteArray`), which the freshness
  side already read through.
- [x] A bytes literal counts as fresh, like a string literal.
- [x] A reference root answers for the place it refers to — in the immutable-root
  rule, in the freshness seed, and in path disjointness. Before, each read it
  as an owner: a `let` out of a `&`- or `&mut`-held struct kept seeing the
  source's later writes.
- [x] A self-recursive function can prove it returns owned, so `?` on one stops
  deep-copying the error it propagates.
- [x] A repoint of a place costs a binding that read it nothing: the binding
  keeps its share across it. Handing that binding on to a new owner is a
  separate claim, and
  [the release that lets a binding leave the function](#the-release-that-lets-a-binding-leave-the-function)
  says what proving it would take.
- [x] Whether a binding aliases storage something still reads is asked of the
  whole chain its source stands on, so a match temp standing between the
  binding and the holder does not read as the holder's death.
- [x] A place scrutinee nothing can write is matched where it lies, and a
  receiver-aliasing call counts as a place, so `match *r` and `match xs[0]`
  decide as `match r` does. A writable one is hoisted into a temp.
- [x] A closure costs its captures their move, their share and their
  confinement, not its whole frame's.
- [x] A projection to a scalar keeps its root live without consuming it, so
  `if c.pos == 0 { … } else { out.push(c) }` still moves `c`.
- [x] Representative move / copy / share decisions pinned as e2e fixtures
  (`pattern_temp_no_alias`, over syntactic position × writability × binding
  kind; `closure_capture`, `scalar_read_before_move`).
- [x] Sharing is keyed on liveness rather than on the whole body, as _Sharing_
  states: share collection runs inside the move analysis's backward walk, so
  a write refuses a binding only where that binding's storage is still
  readable. The live set is closed over what each live local took its value
  out of, an arm binding being its scrutinee's storage under a second name.
- [x] A `&mut` to a variant or a primitive is a `Box<T>` by the time pattern
  lowering runs, so the writability question a by-value arm binding asks goes
  to `place::is_reference`, which reads both spellings. A raw `Ref` / `MutRef`
  test minted no owned scrutinee temp and the binding aliased the caller's
  payload.
- [x] The helper seed is driven by the types a program names rather than by the
  expressions it writes, so it predicts none of the temps pattern lowering
  mints after it. A miss left the fold no helper to call, which
  `wrap_value_copy` asserts on.
- [x] One walk decides a read final: the TIR walk lowering runs. The
  elaborator's source-level last-use pass reached lowering keyed by span, which
  every synthesized node shares, so it moved a synthesized read final or not.
  A read synthesis wrote is now judged like any other, and the pass is gone.
- [x] A `break LABEL` resumes where its labeled block ends, at both levels that
  compute last-use liveness. Each kept a stack of exit sets that only loops
  pushed, so every such break fell back to _every_ local live. A template
  string lowers to a labeled block, so one interpolation anywhere in a
  function retired its whole last-use set. That showed most often as a
  whole-array copy of a `String` handed to a callee that keeps it. The
  source-level walk had
  the same fallback, and is fixed here rather than left. No fixture changes
  with that half fixed: a template is not a labeled block where that walk
  runs.
- [x] A caller takes the whole `&mut` handle as written when the callee names a
  write it cannot re-root there. Re-rooting kept only the writes whose owner
  was the handle's own type and dropped the rest, so a callee writing through
  a variant payload reported nothing to its caller at all. Harmless only
  while a defensive copy stood in the way; the item above removed that copy
  and `value_copy_nested_write_through_payload` miscompiled.
- [x] A wrap site skips its copy for a move, never for a share. The two were one
  set, so a second owner minted out of a share — `let mut b = a`,
  `Wrapper { inner: a }` — aliased the place the binding was read out of, and a
  write through it landed there.
- [x] An arm binding aliases live storage when the scrutinee is live where _that
  arm_ reads it, decided from the live set the arm's own walk produced. One set
  merged over every arm made a sibling arm's read refuse this arm's share.
- [x] Storage is handed over only when no other chain reaches it. One predicate
  answers at every hand-over site, and closes the chain on both sides: the
  storage being taken, and everything live where it is taken. Closing only
  the taker's own side left a sibling binding read out of the same place
  invisible, since it reaches the place from beside the taker rather than
  above it. Two bindings then both skipped their copy, and a write through
  one was observed through the other. Both sides are compared as places
  rather than as root locals: asking only whether two chains share a root
  made each component of a destructured tuple a reader of its siblings,
  which cost the corpus five copies in `httpbin_*`. Carrying the selectors
  gives those back. One copy elsewhere stays, in
  `parser_synth_id_collision_test`, where two bindings read the whole of one
  place and one is still live. That is the shape the rule is for, and the
  only elision in the corpus it takes.
- [ ] Let the fold decide a match arm's binding for itself, so the answer stops
  depending on the temp pattern lowering hoists the scrutinee into. Which
  syntactic position a `match` sits in is part of whether the binding is
  defended, which it should not be. Finishing means the binding carries its
  own path and the fold wraps it, with the corpus no worse off.

      What blocks it is a write reached through a variant payload: `modref` keys
      it by the payload's type while the call site keys by the handle's, so
      giving the binding the deeper path files the write where no caller looks.
      Two pieces close that — `Selector::Variant` carrying its owner, so a
      variant step is a key as a field is, and `modref` speaking addressable
      steps rather than fields, the vocabulary `disjoint` already uses.
      
      An or-pattern is the trap for any attempt: its alternatives bind one local
      but project different cases, so the local names only what they agree on —
      the value the whole pattern matched. Reading it as one alternative's
      payload lets a write to another's be called disjoint.
- [x] A referenced projection returned behind a variant construction. `return`
  is not a wrap site, so `return place` hands a reference out for the caller to
  materialize; `analyze::returned_value` makes `return Some(place)` do the
  same, and `ownership` judges that payload for the convention that tells
  callers so.

      Only where the caller can name what it got, because one copy in a callee
      is shared by every call site and moving it out puts a copy on each of them
      instead. Naming it means a `place::ReturnPath` that stays inside one
      parameter's own storage; a path leaving through a `&` field lands where the
      caller cannot follow.
      
      The path records _which_ parameter, not just the selectors. An accessor
      need not hand out its receiver: `StructField::get(&self, v: &T)` returns a
      field of `v`. Reading every path as the receiver's drops those, and a
      dropped path leaves a place nothing can name. That is what made
      `core:serde` deep-copy every field of every value it serialized.
      
      Whether a place is reached through a reference is decided by the walk
      that resolves it, never re-derived from an expression's syntax. The walk
      follows local bindings and callee return paths that the syntax does not,
      so the two disagree, and a caller told a referenced place is owned shares
      across a write.

- [ ] Follow a reference field back to what it refers to. This is what the item
  above does _not_ reach, and it is the by-value `for` binding: a
  `SliceValueIter` holds `repr: &Array<T>`, so the element it hands back is
  a projection of the _list_, not of the iterator, and `ReturnPath`'s
  `through_ref` closes the gate rather than claim a place it cannot
  justify. The walk already records which parameter a callee persists;
  what is missing is _into which field_, which is what would let a caller
  re-root `it.repr[i]` at the list `into_iter` was handed.
  On gale-gen the copy inside `SliceValueIter::next` is 7.9% of the run, ~5%
  of which writing the loops over `&` recovers. Narrow,
  though — the same measurement over `syntax_highlight`, `json_catalog` and
  `sqlite_parse` finds 0%. Only a program passing deeply nested aggregates by
  value pays it.

- [ ] Say which _field_ a stored parameter is stored into. The item above needs
  it to re-root an iterator's element read at the list, and `array_copy`
  needs it because its elements reach `dst`. Recorded with the rest of
  what the facts cannot yet say, in
  [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

- [ ] Read a call through `projection_param`, and `&fresh` through
  `is_owned_value`. Together, not separately.
  `projection_param` matches a syntactic chain, so a wrapper that returns a
  call gets no verdict: `VariantCase::extract`, `TemplateHole::get` and
  `StructField::get` each `return builtin::<part_of builtin>(v, …)`.
  Resolving the callee through `self_projection_param` is sound and gives
  `extract` the verdict "projects `v`".
  It buys nothing alone. `is_owned_value` has no `TirUnaryOp::Ref` arm, so
  `&fresh_local` is never owned, and the caller that would cash the verdict
  asks exactly that question about its argument. The resolution on its own
  leaves every WIR golden byte-identical, and a case written to exercise it —
  `wrap(h: &Holder) -> List<i32> { return get_items(h); }` called with a fresh
  `Holder` — is byte-identical at `-O0` too.

## Known gaps

### The release that lets a binding leave the function

`let v = self.held; self.held = null;` gives `v` what the place held. In
principle `v` may then be handed to a new owner with no copy, which is the
`take` / `drain` / `snapshot` idiom. The compiler copies instead.

A repoint of the place is not the proof this needs, for two reasons.

It says nothing about _when_ it runs, so a repoint under an `if` that never
executes would license the elision. `value_copy_release_is_not_a_proof` is that
program.

It says nothing about a _second_ binding read out of the same place, where a
write through one is observed through the other. `value_copy_new_owner_needs_a_proof`
is that one, and this half is answered: a hand-over is refused when another chain
reaches the same place (`value_copy_two_readers_of_one_place`). That states the
sibling case as a rule instead of counting bindings.

The first half stands. The backward walk records a live set per write and no
position, so it cannot say today which repoints dominate a site.

### A returned projection behind a `let`

`projection_param` matches a syntactic deref / field / index / payload / cast
chain rooted at a parameter. Returned as written, such a chain is handed out as
a reference, inside a variant construction too: `return Option::Some(h.item)`
copies nothing. Bound by a `let` first, it is not recognized.
`let item = h.item; return item;` returns owned, and the fold deep-copies
`h.item` into `item`.

`SliceValueIter<T>::next` has that shape:

```wado
let item = builtin::array_get_value(self.repr, self.index);
return Option::Some(item);
```

So every by-value `for x of list` over a `List` of aggregates pays a copy of
each element, even when the loop body only reads it. The element is also reached
through the reference field `repr`, which the Roadmap item "Follow a reference
field back to what it refers to" covers. Nothing about the copy is required: the
same walk written over `&list`, or as an index loop in one body, reads the
element in place under the read-only share.

Neither the inliner nor any NIR pass can stand in: the copy is chosen before NIR
exists, and `#[inline(always)]` on `next` leaves the expanded clone in the
caller's loop untouched even with the cloned array provably unread.

### A field moved out of a live aggregate

A place-level move out of an aggregate happens at a struct or tuple literal,
for a value type that owns no resource. Moving a field out of an aggregate that
is still live, or through a deref or an index, copies.

### A declared retention the walk does not confirm

Only a body-less declaration carries retention now
([Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md)),
and it has no body for the walk to confirm it against. A `#[retain(p)]` is taken
at its word on both channels, so a caller stays conservative whether or not the
declaration was right; what nothing can catch is a declaration that understates
what the callee keeps.

`carries` drops at a projection whose type only _holds_ a reference — `w.h`
where `Held { r: &Rep }` — since the arm keys on the projection being a
reference itself. A parameter that only holds one is seeded, through
`RefCarrying`: the memoized "can a value of this type hold a reference"
predicate, which is what keeps `List::push(Sink { r: &it })` retaining `it` now
that no frontend obligation makes the caller declare it. The projection arm is
still the narrower reading: the walk loses a reference at the first projection
rather than following it through an adapter chain.

### A generic resource a user module declares

The move check reads the type a binding resolves to, and a generic resource a
user module declares resolves to no resource type at all. `Stream<u8>` and
`Future<T>` resolve to `GenericResource` and are checked; a program's own
`resource Handle<T>` does not, so `consume(h); consume(h);` on a `Handle<i32>`
compiles with no diagnostic while the same code on a non-generic `Handle` is
rejected.

Nothing can be built through the gap today: a `#[cm]` resource a user module
declares has no import binding, so no program can obtain such a handle.

### A `Stream` or `Future` handle nothing drops

"Deterministic drop" says an owned, un-moved resource is dropped at scope exit
on every path. Cleanup does not do that for `GenericResource`, meaning
`Stream<T>` and `Future<T>`: it excludes them, so every path out of a function
holding one has to call `drop` itself, and nothing diagnoses a path that does
not. The handle leaks with no trace.

A `?` or an early `return` inside the region holding the handle is the shape
that admits it. `core:fs` drains the stream and drops before it decides what to
return. `core:kiln` lends the handle to a helper, so the body that owns it
cannot return early at all.

### What cleanup does not reach

Cleanup walks function bodies only. A resource a closure body binds, or a
temporary it references, is never dropped.

Moving one resource field out of a temporary aggregate (`make().a`) drops
nothing else the temporary owns, so a second resource field leaks.

A store through a reference (`*r = x`) or into an element (`list[i] = x`) drops
nothing it replaces.

An aggregate that holds itself and names a type parameter (`Node<T>` in a body
generic over `T`) is not dropped past its first level: cleanup runs before
monomorphization, and its `$drop$…` function would have to be generic.

A panic drops nothing the program owns.

### Freshness does not read the fold's own wraps

A copy hands its target storage nothing else reaches. Ownedness is computed from
a local's source before any wrap site is chosen, so a wrap the fold has just
decided does not count as making its target fresh.

No program reaches the imprecision. For a second read to pay a copy its move must
be refused, and a copied local aliases nothing, so only a read that is not the
last one refuses — and there the second copy is a second live object, which is
needed.

### Some resource holders are not move-checked or dropped

Every value holding an affine resource is move-only and dropped, but only a
struct, a tuple and a variant (`Option` and `Result` among them) are checked. A
`List` holding one can be moved twice with no diagnostic, and is not dropped at
scope exit.

### A call through `&mut` that copies

The receiver kind decides whether a call copies or extracts
([A call through `&mut` extracts](#a-call-through-mut-extracts)), and a `&mut`
function that copies breaks the rule unseen. The caller then owns a value the
storage still holds.

- `HashMap::get_or_insert` and `TreeMap::get_or_insert` return the value they
  keep. Neither can return a reference instead: `&V` needs `V: Ref` and
  `&mut V` needs `V: RefMut`, and a resource is neither.
- An iterator that walks a borrowed container by value (`SliceValueIter`,
  `MapValuesValueIter`) copies each element out through its `&mut self`. So
  `for x of list` over a `List` holding a resource hands each element to the
  loop while the list keeps it.

### A generic body copies a type parameter

The copy check reads a call's signature where the call is written. A body
generic over `T` that copies a `T` out of a reference
(`fn dup<T>(list: List<T>) -> T { return list[0]; }`) takes its arguments by
value, so its own call sites are not refused. Instantiated with a resource, it
returns a second owner.

### A resource field moves out of its holder

`let c = h.c;` over a struct `h` holding a resource compiles, and `h` stays
usable. Moving `h` afterwards leaves two owners of one handle.

### An integer casts to an affine resource

`5 as Counter` compiles for an affine `Counter` in any module. The result is a
handle nothing minted, which the program then owns and drops.

## References

- [CM Explainer — Handle types](../vendor/component-model/design/mvp/Explainer.md)
- [CM Canonical ABI — `lift_own` / `lift_borrow`](../vendor/component-model/design/mvp/CanonicalABI.md)
- [Resource Lifecycle Management (RAII)](./wep-2026-01-12-resource-lifecycle.md)
- [Redesign Wasm CM Builtins as Resource Canonical Attributes](./wep-2026-03-01-cm-resource-canonical-attrs.md)
- [Resource Inheritance and Narrowing](./wep-2026-04-28-resource-inheritance.md)
- [Migration to GC in Components](./wep-2026-03-28-gc-in-components.md)
- [Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md)
- [`core:icu`](./wep-2026-08-09-core-icu.md) — the non-owning token's second
  backing.
- [NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md)
