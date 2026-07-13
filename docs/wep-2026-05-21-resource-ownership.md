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
lifetimes, no exclusivity analysis. A by-value `self` receiver is allowed only on
a resource (on a copyable type it would just deep-copy), which licenses
`fn drop(self)` and the consuming `AsyncCall<T>` methods.

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

Done: use-after-move of a bare resource local (`Resource` / `GenericResource`)
and by-value `self` consumption — a method call whose receiver is `self: R`
(not `&self`) moves the receiver, so a later use is reported. The receiver
convention rides a `consumes_self` fact recorded on the method dispatch (the
semantic-layer twin of `resource_cleanup`'s `owned_self`), read at the call
site via `Semantics::method_call_consumes_receiver`.
Remaining: resource-carrying aggregates (`Result<Fields, E>`, structs / tuples)
and the no-move-out-of-borrow rule below.

### No move out of a borrow (planned)

Moving a resource out of a borrowed place (`*self`, a pattern binding from
`*borrow`) is forbidden — this is what makes `Result::unwrap(&self) -> T` illegal
for a resource `T`, directing extraction to `if let Ok(r) = …`. Since `unwrap`'s
body is generic, the check is a per-function summary ("returns a value rooted in
a borrowed parameter") reported at each call site whose concrete return carries a
resource, so the error lands on the user's `.unwrap()`. A `&self` method
returning a freshly produced resource (`dir.open_at()`) stays legal. This rule is
the prerequisite for retiring the cleanup heuristic below.

### Authoritative cleanup

`resource_cleanup.rs` drops every owned, untransferred resource on each
fall-through path, structurally for aggregates via a synthesized `match`. Two
fixes landed:

- A `&self` method on a `Result<Resource, E>` (`is_ok`, …) is a borrow: the
  receiver is consumed only when the call result carries a resource (an
  extracting `unwrap`), so an inspector leaves the structural drop intact (#1569).
- A resource value discarded in statement position (`Fields::new();`, `let _ =
  …`) is dropped, not leaked; a tail expression is left for its consumer.

The `is_resource_aggregate` heuristic remains: it is precise for stdlib but
cannot in general tell extraction-from-self from a fresh return. The
no-move-out-of-borrow rule retires it, after which cleanup drops "owned and not
moved at scope exit" with no guessing.

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
  `moved_local_spans`.
- Freshness — `ownership.rs` return conventions (a call is fresh iff the callee
  returns owned).
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
- Open: guaranteed drops on panic (needs Wasm EH); place-level moves out of
  aggregates.

## Implementation status

Verified against the tree.

### Move check

- [x] `resource_move_check.rs` on `Semantics`, wired into batch and LSP.
- [x] Use-after-move of a bare resource local: forward walker, branch-join
      union, divergence-aware, loop-carried, reassignment/rebind clears.
- [x] Covers free / `impl` / `trait` / `test` / function-local bodies.
- [x] By-value `self` consumption via the dispatch `consumes_self` fact
      (`Semantics::method_call_consumes_receiver`).
- [ ] Resource-carrying aggregates (`Result<Fields, E>`, structs / tuples).
- [ ] No-move-out-of-borrow (rejects `Result::unwrap` on a resource).
- [ ] Unify with the value-copy last-use liveness.

### Cleanup

- [x] `&self` aggregate call classified by return type (#1569).
- [x] Discarded resource value is dropped, not leaked.
- [ ] Retire `is_resource_aggregate` once no-move-out-of-borrow lands.

### Consuming drop

- [ ] Migrate `types.wado`'s `drop` methods to `fn drop(self)`. These are
      `#[cm("…-drop…")]` attributes, so CM-binding synthesis must accept a
      by-value `self`; double-drop avoidance already holds via cleanup's
      `owned_self`.
- [ ] Compositional destructor synthesis for resource-bearing aggregates.

### Value-copy client (done)

- [x] `$value_copy$T` inserted only at `copy` sites; read-only-share;
      recursive-type deep copy; `optimize::escape` / `value_copy_elide` deleted.
- [ ] Pin representative move / copy / share decisions as e2e fixtures.

## Deferred: the `move` and `unique` keywords

Intentionally not implemented. Move-only semantics need no syntax: transfer is
implicit and a use-after-move diagnostic replaces a `move` operator's local
visibility. `unique` as a `struct` modifier only matters for a move-only type
that carries no resource — a resource-bearing aggregate is already move-only by
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
