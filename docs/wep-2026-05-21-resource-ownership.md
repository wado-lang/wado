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

### Two resource kinds

| Kind                 | Backed by                                              | Ownership           | Cleanup                  |
| -------------------- | ------------------------------------------------------ | ------------------- | ------------------------ |
| Affine resource      | a CM `own`/`borrow` or waitable handle, or a guest one | Move-only + a check | `resource.drop` / `dtor` |
| Host-object resource | a Wasm GC reference to a host object (no `dtor`)       | Value semantics     | Wasm GC                  |

An affine resource holds a one-shot, destructor-bearing thing, so copying it
would alias it — a double-drop, a use-after-transfer, or a double free. A
host-object resource is a GC reference with no `dtor`, so aliasing is safe. The
`i32`-vs-`externref` representation is orthogonal; the kind decides the model.
`Waitable` (from `join`) has no `drop` and is a copyable newtype, not affine.
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
- A resource value discarded in statement position (`Fields::new();`, `let _ =
…`) is dropped, not leaked; a tail expression is left for its consumer.

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
  (which stores only its value) no longer pins the receiver local.
- Freshness — `ownership.rs` return conventions (a call is fresh iff the callee
  returns owned). An _indirect_ call is fresh when every closure `__call` of its
  return type returns owned: closure lowering rewrites every callable value —
  a closure literal and a bare `FuncRef` alike — into a functor whose `__call`
  is an ordinary function, so those are the complete set of targets, and the
  call's own signature narrows them to one return type. The verdict is derived
  from the return-convention fixpoint and so is computed after it, never inside
  it.
- Branch results — a block's value is its tail expression and an `if`'s is the
  tail of whichever branch runs, so each is fresh exactly when those tails are,
  the rule `match` arms already followed. A router dispatch — a response bound
  from an `if let` over the matched route — needs both this and the
  indirect-call rule, or every later field read of the binding is deep-copied.
- Confinement — `confine.rs` per-parameter escape fixpoint.
- Read-only-share — a read-only binding whose storage is never mutated while live.

A recursive type's `$value_copy$T` is a true deep copy (a mutually-recursive
helper), replacing an identity fallback that silently shared storage.
`optimize::escape` and `optimize::value_copy_elide` are deleted.

## Amendments to earlier WEPs

- WEP 2026-04-28 (Resource Inheritance): its "value semantics, no borrow
  checker" and "every `resource` method takes `&self`" claims are narrowed to
  host-object resources; affine resources additionally have by-value consuming
  methods. Applied there.
- WEP 2026-03-01 (CM Canonical Attributes): the resource `drop` method is
  amended from `fn drop(&self)` (unsound under move-only) to `fn drop(self)`; the
  call syntax is unchanged. Applied there.

## Consequences

- One story: move-only affine resources, GC value semantics for host-object
  ones. Double-drop / use-after-transfer / leaked borrow become compile errors.
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

### Value-copy client (done)

- [x] `$value_copy$T` at `copy` sites only; read-only-share; recursive-type deep
      copy; `optimize::escape` / `value_copy_elide` deleted.
- [x] Place-level move at struct / tuple literals (whole-value / clean-field
      materialization out of a dead aggregate); receiver-storing precision so a
      value-storing `&mut self` method does not pin its receiver.
- [x] Freshness through an indirect call and through block / `if` results.
- [ ] Pin representative move / copy / share decisions as e2e fixtures.

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
- [The Live ValueGraph](./wep-2026-06-15-live-value-graph.md)
