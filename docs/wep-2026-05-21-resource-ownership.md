# WEP: Resource Ownership — Move-Only Resources, Authoritative Cleanup, and Value-Copy Elision

## Context

Wado's `resource` is an opaque handle to a Component Model (CM) resource. CM
gives resources a precise ownership discipline — no double-drop, no
use-after-transfer, no leaked borrow — that Wado's surface language does not yet
model. This WEP makes resources **move-only** and builds the static analysis
that turns that discipline into compile-time guarantees plus deterministic
cleanup.

The core of that analysis answers one question — _is this the binding's last
use, or is the value still live afterward?_ — and that same question decides,
for a copyable value type, whether a materialization needs a physical deep copy
or can transfer storage in place. Value-copy elision is therefore a second
client of the ownership idea, and this WEP also records its implementation,
which retired the older "copy-everything-then-elide" machinery
(`optimize::escape` + `optimize::value_copy_elide`).

Two user-facing keywords — `move` and `unique` — were once reserved for this
work. They turned out to be unnecessary; the analysis needs move-only
_semantics_, not new syntax. The keywords are deferred (see the last section).

### Component Model resource ownership

CM defines a `resource` type and two handle types over it (Explainer, "Handle
types"):

| Handle      | Meaning                                                                         |
| ----------- | ------------------------------------------------------------------------------- |
| `own<R>`    | Unique owning handle. Dropping the last `own` runs the resource's `dtor`.       |
| `borrow<R>` | Non-owning handle, lent from an `own`. Must be dropped before the call returns. |

A handle is an opaque `i32` index into a per-component-instance table (CM-LM),
analogous to an OS file descriptor. The canonical ABI enforces the discipline
dynamically:

- `lift_own` removes the handle from the table — ownership is transferred. It
  traps if the handle is currently lent out (`num_lends != 0`) or is not
  actually an `own` handle.
- `lift_borrow` reads the representation and leaves the source handle in place,
  recording a lender so the source stays alive until the call ends.
- A task that returns with `num_borrows > 0` traps: borrows may not outlive the
  call that created them.
- `borrow` may not appear in `stream` / `future` element types.
- `resource.new` / `resource.drop` / `resource.rep` are usable only inside the
  component that defined the resource type.

In short, CM already enforces the discipline — but dynamically, as a trap, and
only at the component boundary. This WEP makes it static and guest-side.

### Wado today

- `resource` declarations exist (`pub resource Descriptor { ... }`). Methods
  take `self: &Descriptor` (a borrow) or the resource by value (a consuming
  method such as `Request::consume_body`). By-value `self` receivers are already
  supported for resources.
- CM-binding synthesis maps `&R` to `borrow<R>` and a by-value `R` to `own<R>`.
- A resource handle is lowered as `i32` (CM-LM) or `externref` (CM-GC); the
  value-copy machinery already treats resources as non-copyable (an identity
  copy, never a deep clone), so a resource never physically duplicates.
- `synthesis/resource_cleanup.rs` inserts `resource.drop` for every owned
  resource a body does not transfer, reconstructing ownership by dataflow.

### Two earlier WEPs, reconciled

- [Resource Lifecycle Management (RAII)](./wep-2026-01-12-resource-lifecycle.md)
  declares resources implicitly move-only with destructors and scope-based
  cleanup.
- [Resource Inheritance and Downcast](./wep-2026-04-28-resource-inheritance.md)
  states resource handles have "value semantics, no lifetimes, no borrow
  checker" and that "every `resource` method takes `&self`".

Read as language-wide claims these contradict each other. The tension resolves
by splitting the model by _resource kind_ (below); WEP 2026-04-28's wording is
narrowed to its actual scope (host-object resources) in the amendment section.

## Decision

### Split the ownership model by resource kind

A Wado `resource` is one of two kinds, and the ownership model follows the kind:

| Resource kind        | Backed by                                                                | Ownership model                           | Cleanup                                |
| -------------------- | ------------------------------------------------------------------------ | ----------------------------------------- | -------------------------------------- |
| Affine resource      | a CM `own`/`borrow` handle, a waitable handle (`i32`), or a guest handle | Move-only + a small resource-scoped check | Deterministic `resource.drop` / `dtor` |
| Host-object resource | a Wasm GC reference to a host object (no `dtor`)                         | Value semantics (copyable), no check      | Wasm GC                                |

The affine kind spans a CM `own`/`borrow` handle (WASI `Descriptor` etc. and
guest-defined resources exported across a CM boundary), a canonical-ABI waitable
handle (`stream`/`future` ends, `subtask`, `waitable-set`, `error-context`), and
a purely guest-internal handle (`AsyncCall<T>`). They differ in representation
but share one rule: they hold a one-shot, destructor-bearing thing, so copying
one would alias it — a double-`resource.drop`, a use-after-transfer, or a double
free. A host-object resource is a GC reference to a host-owned object with no
`dtor`; aliasing it is safe, and the GC reclaims it.

`Waitable` (the identity token from `join`) is _not_ affine: no `drop`, no
lifecycle — a copyable newtype over `i32`
([WEP 2026-03-01](./wep-2026-03-01-cm-resource-canonical-attrs.md)).

The wasm-level representation (`i32` vs `externref`, per
[GC in Components](./wep-2026-03-28-gc-in-components.md)) is orthogonal: a CM
handle lowered as `externref` in CM-GC mode is still affine. The rest of this
WEP concerns affine resources.

### Affine resources are move-only

An affine resource value is move-only, with **no keyword**:

- It cannot be copied. `let b = a;` moves `a` into `b`; `a` is then invalid.
- It transfers by value passing, returning, or placement into an aggregate,
  and by a by-value `self` receiver. After a transfer the source is invalid.
- Using a binding after it has been moved is a compile error.

Transfer is implicit at the point of consumption — Wado does not spell it with a
`move` operator. A resource type is implicitly move-only; it is not a language
that grows lifetimes, regions, or a shared-XOR-mutable exclusivity analysis.
spec.md's "no borrow checker" remains true for the value-semantics language at
large; resources are a single, contained exception.

#### Consuming `self` receivers are resource-only

A method may take its receiver by value — a bare `self`, which _consumes_ it —
only when the receiver type is a `resource`. On a copyable value-semantics type,
by-value `self` would hand the method a deep copy, observably identical to
`&self` plus a wasted copy, so it is rejected. This licenses `fn drop(self)` and
the consuming `fn wait(self)` / `fn cancel(self)` on `AsyncCall<T>`
([WEP 2026-04-22](./wep-2026-04-22-subtask-generic.md)).

### The move check

A single diagnostic pass gives resources their move-only semantics. It is
implemented in `resource_move_check.rs`.

Layer — it runs on the `Semantics` layer (the AST plus the type facts recorded
during `annotate`), as a sibling of `effect_check::check_semantics`. This is the
LSP + batch shared path: it has source spans and a diagnostic channel, sees
every function regardless of what reify emits, and surfaces errors both in
`wado compile`/`check` and in the editor. Post-monomorphize TIR was rejected as
the substrate because it has neither spans wired for diagnostics nor a
call-site origin.

Analysis — a forward walker over each body-bearing item (free function, `impl`
and `trait` methods, `test` blocks, and function-local items). For each resource
binding it holds one state: live, or moved-at-span. A value consumed by value
(a `let` initializer, a call / method / constructor argument, a `return`, an
aggregate element, or a discarded expression statement) records a move; a later
use of a moved binding is reported. Branch joins take the union of moves (moved
on any path → maybe-moved → an error to use), except a path that diverges
(`return` / `break` / `continue`) contributes nothing; a loop body is walked
twice so a value moved on one iteration is seen as moved on the next; a
re-assignment re-initialises the binding.

This walker is **independent** of the value-copy client's last-use liveness
(`lower/plan/value_copy/last_use.rs`, which runs post-monomorphize over TIR).
The two answer the same question at different pipeline stages; unifying them
onto one shared computation is future work, not a present fact.

Scope done vs. remaining:

- Done — use-after-move of a **bare** resource-typed local (`Resource` /
  `GenericResource`), including reassignment, branch divergence, and
  loop-carried moves.
- Remaining — resource-carrying **aggregates** (`Result<Fields, E>`,
  resource-bearing structs/tuples), by-value `self` receiver consumption, and
  the no-move-out-of-borrow rule below.

#### No move out of a borrow (planned)

Moving a resource out of a borrowed place (`*self`, `borrow.field`, a pattern
binding from `*borrow`) must be forbidden. This is what makes
`Result::unwrap(&self) -> T` illegal for a resource `T` and directs extraction
to `if let Ok(r) = …` (a move) instead. Because `unwrap`'s body is generic
(`match *self { Ok(v) => v }`, `v: T`), the check is a per-function summary
("returns a value rooted in a borrowed parameter"), computed on the generic body
and reported at each call site whose concrete return carries a resource — so the
error lands on the user's `.unwrap()`, not in the stdlib. A `&self` method that
returns a _freshly produced_ resource (`dir.open_at() -> Descriptor`) is not
rooted in the borrow and stays legal.

This rule is the prerequisite for retiring the cleanup heuristic (below): once
`&self` resource extraction is a compile error, an aggregate method call is
always a borrow, and cleanup never has to guess.

### Authoritative cleanup

`resource_cleanup.rs` inserts a `resource.drop` for every owned resource that a
body does not transfer, on every fall-through path (block end, early `return`,
the non-taken side of a branch), with structural drop of resource-carrying
aggregates via a synthesized `match`. Two properties are now correct:

- A `&self` method on a `Result<Resource, E>` (`is_ok`, …) is a borrow, not a
  consumption: the receiver is treated as consumed only when the call result
  itself carries a resource (an extracting `unwrap -> Fields`), so a plain
  inspector leaves the inner resource's structural drop intact (#1569).
- A resource-carrying value produced in a discarding statement position (a
  non-tail expression statement, or `let _ = …`, which lowers to the same) is
  dropped rather than leaked. A tail expression is the block's value and flows
  to its consumer, so it is left intact.

The remaining heuristic is `is_resource_aggregate`: a method call on a
resource-carrying aggregate is treated as extracting when its return carries a
resource. This is precise for every stdlib method but not principled — it cannot
distinguish extraction-from-self from a freshly produced return in general, and
`carries_resource` does not see a resource wrapped in a returned struct/tuple.
It is retired once the move check forbids `&self` extraction (above), after which
cleanup drops "owned and not moved at scope exit" with no aggregate guessing.

### Deterministic drop

A resource owned and not moved out is dropped when its binding leaves scope, on
every control-flow path; a move suppresses the drop at the source. The drop
action depends on the boundary:

- An imported handle held by the guest (`Descriptor`) → `resource.drop`.
- A guest-defined resource → its destructor, whether exported across a CM
  boundary or purely guest-internal (`AsyncCall<T>`). Destructor syntax and
  effect propagation are as in
  [WEP 2026-01-12](./wep-2026-01-12-resource-lifecycle.md).

A resource may also expose an explicit **consuming** drop method, `fn drop(self)`
— the form used by `Stream`, `WaitableSet`, and the other canonical resources
([WEP 2026-03-01](./wep-2026-03-01-cm-resource-canonical-attrs.md), amended
below). `r.drop()` consumes `r`, so the move records the transfer and the
scope-exit drop does not also fire — exactly one `resource.drop`. A `&self` drop
would be unsound: the binding would stay usable after its handle is dead, and the
scope-exit drop would double-free. (The stdlib `drop` methods still declare
`fn drop(&self)`; migrating them is the open R3 item below.)

A `struct` / `variant` / array containing a resource is itself move-only and
gets a synthesized destructor that drops its resource fields in declaration
order, with the union of the field destructors' effects. Panic / trap does not
unwind, so a drop is not guaranteed on panic — unchanged from WEP 2026-01-12,
revisited only when Wasm exception handling is adopted.

### `own` / `borrow` at the CM boundary

The move-only model maps directly onto CM handle types, so binding synthesis
needs no ownership heuristics:

| Wado position                      | CM handle                                    |
| ---------------------------------- | -------------------------------------------- |
| by-value `R` parameter / return    | `own<R>`                                     |
| `&R` parameter, `&self` receiver   | `borrow<R>`                                  |
| by-value `self` (consuming method) | `own<R>` — transfers / consumes the receiver |

Once the move check proves a by-value resource uniquely owned at the transfer
site with no live `&` to it, CM's `lift_own` preconditions hold statically and
its runtime trap becomes a guaranteed-unreachable backstop.

### Value-copy elision (the second client)

A copyable value type uses the same last-use question to decide, at each
consumption site, whether to move (last use → transfer storage, no copy), copy
(source still live → materialize an independent deep copy), or share (source and
destination both never mutate again → alias, no copy). This replaced the older
"copy-everything-then-elide" scheme.

As implemented, the value-copy client is caller-side and single-phase: the fold
(`lower::translate`) emits a `$value_copy$T` only where the analysis cannot prove
a move, share, or fresh value. There is no elision pass.

- Move — `last_use::compute_move_eligible` (backward liveness + freshness
  fixpoint over monomorphized TIR) plus source-level `moved_local_spans` from
  `elaborator::liveness`; the fold moves when either marks a consumption.
- Freshness — `ownership.rs` return conventions: a call is fresh iff its callee
  returns owned (the caller-side replacement for `optimize::escape`).
- Confinement — `confine.rs` per-parameter return/side-escape fixpoint; the fold
  skips a by-value argument's copy when the callee parameter is confined and the
  argument aliases no mutated sibling.
- Read-only-share — a read-only binding whose projected storage is never mutated
  while live shares its source (field-sensitive, gated on the source root being
  unconsumed).

A recursive value type's `$value_copy$T` helper is a true deep copy (a
mutually-recursive helper that copies through the indirection), replacing an
earlier identity fallback that silently shared storage for recursive types,
variants with value-typed payloads, and `List<variant>`.

`optimize::escape` and `optimize::value_copy_elide` are deleted; `build_param_mut`
moved to `optimize::value_copy::mutation` (used by `copy_prop`);
`value_copy_demote` is independent and survives.

## Amendments to earlier WEPs

### WEP 2026-04-28 (Resource Inheritance)

Its value-semantics statements are correct for host-object resources (the only
kind `extends` admits) but phrased as language-wide claims. Narrowed, with the
substance unchanged, applied directly in that WEP:

- "In Wado, resource handles are themselves immutable values … value semantics,
  no lifetimes, no borrow checker." → scoped to host-object resources, with a
  cross-reference here for affine resources.
- "Across the language, every `resource` method takes `&self`." → scoped to
  host-object resources; affine resources additionally have by-value consuming
  methods.

### WEP 2026-03-01 (CM Resource Canonical Attributes)

Its resource `drop` method is declared `fn drop(&self)`. A `&self` receiver is a
non-consuming borrow, so under the move-only model `r.drop()` would leave `r`
usable after its handle is dead and double-drop against the scope-exit drop. The
`drop` method is amended to a consuming `fn drop(self)`; the `r.drop()` call
syntax is unchanged. Applied directly in that WEP.

## Consequences

### Positive

- One coherent story: move-only ownership for affine resources, GC value
  semantics for host-object resources.
- Double-drop, use-after-transfer, and leaked borrows become compile errors
  rather than runtime traps at the CM boundary.
- No lifetimes, no annotations, no exclusivity analysis, and no new keyword.
- The heuristic ownership reconstruction in `resource_cleanup.rs` — a fragile
  spot — shrinks toward a trivial consumer of the move check, and
  `optimize::escape` + `optimize::value_copy_elide` are gone.
- Missed move/copy/share decisions are detectable as e2e `wir_expect` /
  `wir_not_expect` fixtures, without benchmarks.

### Negative

- Move-only is a new concept in an otherwise value-semantics language, applying
  to one type category.
- Binding-granular tracking forbids moving one element out of a resource-bearing
  aggregate; such code restructures around iteration.
- Borrows cannot escape a function, so a resource API returns owned handles or
  values rather than a `&R`.

### Open questions

- Should drops be guaranteed on panic once Wasm exception handling is adopted?
- Finer-grained (place-level) moves out of resource-bearing aggregates, if a use
  case appears.

## Implementation status

Verified against the tree. Checked = landed and tested.

### Move check (use-after-move)

- [x] `resource_move_check.rs` on the `Semantics` layer, wired into both the
      batch compile path and the LSP diagnostics path.
- [x] Use-after-move of a bare resource local: forward walker, branch-join
      union, divergence-aware, loop-carried (two-pass), reassignment clears.
- [x] Covers free functions, `impl` / `trait` method bodies, `test` blocks, and
      function-local items.
- [ ] Resource-carrying aggregates (`Result<Fields, E>`, resource-bearing
      structs / tuples) and by-value `self` receiver consumption.
- [ ] No-move-out-of-borrow rule (per-function summary reported at call sites) —
      rejects `Result::unwrap` on a resource.
- [ ] Unify with the value-copy client's last-use liveness onto one computation.

### Cleanup (drop insertion)

- [x] `&self` aggregate method call classified by whether its return carries a
      resource (#1569), so an inspector leaves the structural drop intact.
- [x] Discarded resource-carrying expression statement is dropped, not leaked.
- [ ] Retire `is_resource_aggregate` once the no-move-out-of-borrow rule lands,
      driving drop insertion off the move check's owned-and-not-moved set.

### Consuming drop (R3)

- [ ] Migrate the resource `drop` methods in `types.wado` from `fn drop(&self)`
      to `fn drop(self)`. These are `#[cm("…-drop…")]` canonical-attribute
      declarations, so the migration is not a rename: CM-binding synthesis must
      accept a by-value `self` receiver on a drop attribute. The double-drop
      avoidance itself already holds — cleanup's `owned_self` set consumes a
      by-value receiver, so `r.drop()` suppresses the scope-exit drop today; the
      move check would additionally reject a use after `r.drop()`.
- [ ] Compositional destructor synthesis for aggregates carrying resources, in
      declaration order.

### CM boundary

- [x] Wording amendments to WEP 2026-04-28 and WEP 2026-03-01.
- [ ] Confirm CM-binding synthesis maps by-value `R` → `own`, `&R` → `borrow`
      with no ownership heuristics left.

### Value-copy client (M5) — done

- [x] `lower::plan::value_copy` inserts `$value_copy$T` only at `copy` sites
      (freshness, last-use move, confinement decided at insertion).
- [x] Read-only-share refinement, pinned by `value_copy_elide_disjoint_field_mut`
      and `value_copy_share_root_escape`.
- [x] Recursive-type `$value_copy$T` is a true deep copy, pinned by
      `value_copy_variant_*`.
- [x] `optimize::escape` and `optimize::value_copy_elide` deleted.
- [ ] Pin representative move / copy / share decisions as e2e fixtures (serde
      `?`-chain, accumulator `push`, literal-into-field, `let b = a; b.mut; …a`).

## Deferred: the `move` and `unique` keywords

These were reserved for this work and are intentionally **not** implemented.
Move-only _semantics_ (above) need no syntax: transfer is implicit at the
consumption site, and a good use-after-move diagnostic replaces the local
visibility a `move` operator would add. `unique` as a `struct` modifier is only
meaningful for a move-only type that carries no resource; a resource-bearing
aggregate is already move-only by composition, and no concrete use case for a
keyword-declared `unique` has appeared. Introducing an affine-type surface into
an otherwise value-semantics, "no borrow checker" language is a real conceptual
cost paid only when a use case demands it.

Current lexer/parser state, for whoever picks this up:

- `move` is not tokenized.
- `unique` is lexed to `TokenKind::Unique` but the parser never consumes it.

If revived, `unique struct T { … }` would reuse the exact move-check machinery
(move-only, same use-after-move analysis) with no synthesized destructor unless
it carries a resource — resources are simply the first client of the same
mechanism.

## References

- [Component Model Explainer — Handle types and Resource built-ins](../vendor/component-model/design/mvp/Explainer.md)
- [Component Model Canonical ABI — `lift_own` / `lift_borrow`](../vendor/component-model/design/mvp/CanonicalABI.md)
- [Resource Lifecycle Management (RAII)](./wep-2026-01-12-resource-lifecycle.md)
- [Redesign Wasm CM Builtins as Resource Canonical Attributes](./wep-2026-03-01-cm-resource-canonical-attrs.md)
- [Resource Inheritance and Downcast](./wep-2026-04-28-resource-inheritance.md)
- [Migration to GC in Components](./wep-2026-03-28-gc-in-components.md)
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [The Live ValueGraph](./wep-2026-06-15-live-value-graph.md)
- [Optimizer Remarks for Missed Optimizations](./wep-2026-06-03-optimizer-remarks.md)
