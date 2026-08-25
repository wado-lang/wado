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
the analysis needs move-only semantics, not new syntax. They are deferred (last
section).

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

- `resource` methods take `self: &Descriptor` (a borrow) or the resource by
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

The `dtor` decides the kind, not the representation. A handle that owns one must
be move-only, because copying it aliases a destructor; without one there is
nothing to free, so the handle is an ordinary value. `i32`-vs-`externref` is
orthogonal to all three rows.

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
| `&self`     | shared borrow                 | `borrow<R>` |
| `&mut self` | mutable borrow                | `borrow<R>` |

`self: T` and `self: &T` are rejected. A copyable type has no use for a value
receiver (`&self` plus a `*self` deref-copy covers it), and a single borrow
spelling removes a needless choice. Bare `self` is legal on a resource or an
aggregate that carries one (its consuming method hands that resource off); on a
value type that owns no resource it is a diagnostic (`value types are not
move-only; use &self`) — this is how the value-semantics friction is mitigated,
not with a keyword. The rule licenses `fn drop(self)` and the consuming
`AsyncCall<T>` methods.

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
resource-carrying struct / tuple / `Result`, and no-move-out-of-borrow (a `&self`
return rooted in the borrow that carries a resource) — see below.

### Extraction is a by-value receiver (Rust-aligned)

Pulling a resource out of a `Result` / `Option` returns an owned handle, so the
source must relinquish ownership. `unwrap(&self)` cannot — a borrow keeps the
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

### No move out of a borrow

A hand-written `fn f(&self) -> Resource { return self.f; }` (or `match *self { …
=> interior }`, or a `let`-bound projection) moves a resource out of a borrowed
place: the borrow keeps the source owned, so the returned handle aliases it — a
double-free. `resource_move_check` rejects it. For each function it seeds the
borrowed parameters (`&self` / `&mut self` / `&T`), tracks bindings projected
from them (through `let`, field access, deref, and match-arm bindings whose
scrutinee is borrowed), and flags a `return` whose value is rooted in a borrow
and whose type carries a resource. A `&self` method returning a _freshly
produced_ resource (`dir.open_at()`) stays legal (its return roots in a fresh
allocation, not the borrow).

The gate is the concrete return type carrying a resource, so a generic body
(`T` abstract) is not flagged at its definition; the stdlib avoids the issue by
taking `self` by value, and a user-written generic `&self` extractor
instantiated with a resource is the remaining call-site case (deferred).

### Authoritative cleanup

`resource_cleanup.rs` drops every owned, untransferred resource on each
fall-through path, structurally for aggregates via a synthesized `match`. A value
is transferred when passed by value, returned, placed in an aggregate, or used as
the receiver of a by-value (`self`) method — so extraction (`unwrap`) is just a
by-value receiver transfer, with no aggregate-shape guessing. Two fixes landed:

- A `&self` inspector on a `Result<Resource, E>` (`is_ok`, …) is a borrow and
  leaves the structural drop intact (#1569).
- A resource value discarded in statement position (`Fields::new();`,
  `let _ = …`) is dropped, not leaked; a tail expression is left for its
  consumer.

The `is_resource_aggregate` heuristic is **retired**: cleanup now drops "owned and
not transferred at scope exit" directly.

### Deterministic drop

An owned, un-moved resource is dropped at scope exit on every path; a move
suppresses that drop. An imported handle drops via `resource.drop`; a
guest-defined one runs its destructor (WEP 2026-01-12). An explicit
`fn drop(self)` consumes the receiver, so exactly one drop fires; a `&self` drop
would be unsound (the binding outlives its handle and double-frees). An aggregate
containing a resource is move-only and gets a synthesized destructor in
declaration order. Panic does not unwind, so a drop is not guaranteed on panic.

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

- Move — `last_use::compute_move_eligible` plus `elaborator::liveness`'s
  `moved_local_spans`. This also covers a **place-level move**: at a struct /
  tuple literal, a whole-value or clean-field materialization aliases out of a
  _dead_ aggregate (the root is dead after the literal, sibling reads neither
  mutate nor move it, and the materialized fields are disjoint owners) instead of
  deep-copying — so `T { x: p.a, y: p.b }` over a dead `p`, and a shorthand store
  `S { data, n: data.len() }` past a read-only reuse, both move. A method
  receiver borrow escapes only if the callee stores its receiver specifically
  (`receiver_storing_methods`), so a `&mut self` mutation like `List::push`
  (which stores only its value) no longer pins the receiver local. A `let` is
  the same site: `let v = p.a` over a dead `p` moves, which is what keeps an
  unrolled variadic body from deep-copying the element it binds.
- Declared hand-over — a `let` marked `skip_value_copy` owns its storage by
  construction, so the freshness fixpoint seeds it instead of asking what the
  source expression looks like, and the ownership chain reaches the `match` arms
  and literals downstream of it. A tuple unroll marks its element bindings that
  way: the temp it reads is private to the unroll and each field has exactly one
  reader, so the field is the binding's to take.
- Freshness — `ownership.rs` return conventions (a call is fresh iff the callee
  returns owned), plus the literals that materialize their own storage: a string
  _and_ a bytes literal, both of which lower to a fresh aggregate over a packed
  array. An _indirect_ call is fresh when every closure `__call` of its
  return type returns owned: closure lowering rewrites every callable value —
  a closure literal and a bare `FuncRef` alike — into a functor whose `__call`
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
- Confinement — `confine.rs` per-parameter escape fixpoint.
- Read-only-share — a read-only binding whose storage is never mutated while
  live. See _Sharing_ below.

A recursive type's `$value_copy$T` is a true deep copy (a mutually-recursive
helper), replacing an identity fallback that silently shared storage.
`optimize::escape` and `optimize::value_copy_elide` are deleted.

A function that calls itself is assumed to return owned while that is being
proved: the fixpoint only adds, so its own call would otherwise read back the
verdict being computed and pin it at borrowed. The returns that do not go through
the recursive call are still checked on their own, and those base cases are what
a recursive result is built from.

### What a root owns

Every rule above asks who owns the storage a place names, and answers from the
place's root local. A `&` / `&mut` local is not that owner: its immutability says
nothing about whether the storage is written, its death frees nothing, and a
write the owner makes never mentions it. A root reached through a reference
resolves to the place it borrows (`let r = &a.b` answers at `a.b`); one this body
cannot resolve — a parameter, or a reference a call returned — names storage the
body does not own and answers nothing, leaving it to sharing.

### Sharing

A read-only binding may alias its source's storage when nothing writes that
storage _while the binding is live_. Both halves are readings of one backward
walk: what is live at each point, and what each point writes. A write conflicts
when the binding is live there and the two places may alias; the same write
elsewhere in the body does not. Without liveness a deserializer deep-copies a
field it read before any `&mut self` call runs.

Two kinds of write reach different storage. `p.f = x` points `p.f` elsewhere, so
a reference already taken out of `p.f` keeps what it has, and only a write
_inside_ that storage disturbs it. And a place repointed after a binding read it
hands that binding the only reference to what the place held — the `take` /
`drain` / `snapshot` idiom — so the binding may leave the function though it was
read out of a place the caller still owns.

A `match` over a place needs no temp of its own: the arms project the place
where it lies and each binding asks the fold for itself. Only a non-place
scrutinee is hoisted for `labeled_block_fusion`, whose temp the fold defends.

What a call writes is read off the callee rather than assumed: `modref.rs`
collects each function's writes as fields of the type carrying them and closes
them over the call graph, so a read of one field survives a `&mut self` call
that writes another. A callee with no body reaches only the `&mut` arguments it
is handed; anything this cannot name writes everything.

Every analysis here asks one resolver what an expression names (`place.rs`).
Its answer separates a value of its own from a place the walk cannot follow, so
a shape no arm covers costs an elision rather than becoming a write nobody
records.

### Which helpers exist

`$value_copy$T` is additive synthesis, so the helpers are created in `plan` —
before the fold that decides where to call them, and before pattern lowering
mints the temps some of those calls land on (WEP 2026-05-11). The seed must
therefore be complete without predicting what a later pass writes: it reads the
types a program declares, since no expression rewrite introduces a type the
program did not already name. Over-synthesis costs nothing — `dce` removes an
unused helper — while a miss leaves the fold no helper to call.

### Known gap: a borrowed projection behind a variant

`is_projection_of_param` matches a syntactic deref / field / index / payload /
cast chain rooted at the first parameter, so `build(&self) -> List { return
*self }` is self-projecting but `SliceValueIter<T>::next` is not:

```wado
let item = builtin::array_get_value(self.repr, self.index);
return Option::Some(item);
```

`array_get_value` is already a container alias read, and `Option<T>` over a reference
type lowers to a bare nullable ref, so the borrowed element could be returned as
it stands. Two things hide it: the projection is behind a `let` binding, and it
is wrapped in a variant construction. So `next` returns owned, and the fold
deep-copies the element into the payload — every `for x of list` over a `List` of
aggregates pays a copy of each element even when the loop body only reads it.
Nothing about the copy is required: the same walk written over `&list`, or as an
index loop in one body, reads the element in place under the read-only share.

Closing it needs both halves, in the same fixpoint: the recognizer must see the
projection through the binding and the variant, _and_ the fold must then treat
the materialization into that payload as a share rather than a copy. Neither the
inliner nor any NIR pass can substitute — the copy is chosen before NIR exists,
and `#[inline(always)]` on `next` leaves the expanded clone in the caller's loop
untouched even with the cloned array provably unread.

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
- Open: guaranteed drops on panic (needs Wasm EH). Place-level moves out of
  aggregates now land at struct / tuple literals for non-resource value types;
  moving a field out of a _live_ aggregate, or through a deref / index, still
  copies.

## Implementation status

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
- [x] No-move-out-of-borrow: a `&self` / `&T`-param return rooted in the borrow
      whose concrete type carries a resource is rejected (generic-instantiation
      call sites deferred).
- [ ] Unify with the value-copy last-use liveness.

### Cleanup

- [x] `&self` aggregate call classified by return type (#1569).
- [x] Discarded resource value is dropped, not leaked.
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
- [x] A reference root answers for the place it borrows — in the immutable-root
      rule, in the freshness seed, and in path disjointness. Before, each read it
      as an owner: a `let` out of a `&`- or `&mut`-held struct kept seeing the
      source's later writes.
- [x] A self-recursive function can prove it returns owned, so `?` on one stops
      deep-copying the error it propagates.
- [x] A place repointed after a binding read it releases that binding.
- [x] A place scrutinee is matched where it lies, and a receiver-aliasing call
      counts as one, so `match *r` and `match xs[0]` decide as `match r` does.
- [x] A closure costs its captures their move, their share and their
      confinement, not its whole frame's.
- [x] A projection to a scalar keeps its root live without consuming it, so
      `if c.pos == 0 { … } else { out.push(c) }` still moves `c`.
- [x] Representative move / copy / share decisions pinned as e2e fixtures
      (`pattern_temp_no_alias`, over syntactic position × writability × binding
      kind; `closure_capture_move`, `closure_confinement`,
      `scalar_read_before_move`).
- [ ] Key sharing on liveness rather than on the whole body, as _Sharing_ states.
      The share analysis is a forward walk with no liveness and no control flow,
      so a write anywhere refuses the binding.
- [ ] Drive the helper seed from declared types rather than from expressions.
      Predicting the temps pattern lowering mints is what the current seed does,
      and each shape it misses is a copy the fold cannot emit.
- [ ] Decide a match arm's binding in the fold, by lowering it to an ordinary
      projection of the scrutinee as `let`-destructure already is. Deciding it in
      pattern lowering puts the copy on a temp that exists for
      `labeled_block_fusion`, so which syntactic position a `match` sits in
      changes whether the binding is defended.
- [x] Recognize a borrowed projection returned behind a variant construction
      (2026-08-25). `return` is not a wrap site, so `return place` already hands
      a borrow out for the caller to materialize; `analyze::returned_value` makes
      `return Some(place)` do the same, and `ownership` judges that payload for
      the convention that tells callers so.

      The gate is the feature: the callee's one copy is shared by every call
      site, so moving it out multiplies it unless the callers elide. Ungated on
      gale-gen it _added_ 192 residual copies (2774 → 2966) and 13 KB of Wasm.
      So the payload is handed out only where
      the caller can name what it got — the callee's result is a projection of
      its receiver (`place::ReturnPaths`) that stays inside the receiver's own
      storage. Gated: 2774 → 2765, and the Wasm shrank.

      Two precision fixes it needed, each standing on its own: `ReturnPaths` is
      now a least fixpoint (an accessor written over another accessor resolved to
      `Unknown` in a single pass), and a container-alias read (`array_get_value`)
      names its container's slot in the resolver as it already did in the
      ownership walk.

- [ ] Follow a borrowed field back to what it borrows. This is what the item
      above does _not_ reach, and it is the by-value `for` binding: a
      `SliceValueIter` holds `repr: &Array<T>`, so the element it hands back is
      a projection of the _list_, not of the iterator, and `ReturnPath`'s
      `through_borrow` closes the gate rather than claim a place it cannot
      justify. `stores[...]` already records which parameter a callee persists;
      what is missing is _into which field_, which is what would let a caller
      re-root `it.repr[i]` at the list `into_iter` was handed.
      Priced on gale-gen (2026-08-25): the copy inside `SliceValueIter::next` is
      7.9% of the run, ~5% recovered by writing the loops over `&`. Narrow,
      though — the same measurement over `syntax_highlight`, `json_catalog` and
      `sqlite_parse` finds 0%. Only a program passing deeply nested aggregates by
      value pays it.

## Deferred: the `move` and `unique` keywords

Intentionally not implemented. Move-only semantics need no syntax: transfer is
implicit, a use-after-move diagnostic replaces a `move` operator's local
visibility, and the consuming receiver is spelled bare `self` (scoped to
resources, so it never contradicts value semantics — see the receiver grammar
above). `unique` as a `struct` modifier only matters for a move-only type that
carries no resource — a resource-bearing aggregate is already move-only by
composition, and no use case has appeared. Current state: `move` is not
tokenized; `unique` is lexed to `TokenKind::Unique` but never parsed. If revived,
`unique struct` reuses the same move-check machinery, resources being its first
client.

## References

- [CM Explainer — Handle types](../vendor/component-model/design/mvp/Explainer.md)
- [CM Canonical ABI — `lift_own` / `lift_borrow`](../vendor/component-model/design/mvp/CanonicalABI.md)
- [Resource Lifecycle Management (RAII)](./wep-2026-01-12-resource-lifecycle.md)
- [Redesign Wasm CM Builtins as Resource Canonical Attributes](./wep-2026-03-01-cm-resource-canonical-attrs.md)
- [Resource Inheritance and Downcast](./wep-2026-04-28-resource-inheritance.md)
- [Migration to GC in Components](./wep-2026-03-28-gc-in-components.md)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [`core:icu`](./wep-2026-08-09-core-icu.md) — the non-owning token's second
  backing.
- [NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md)
