# WEP: Ownership Analysis — Affine Resources, the Borrow Checker, and Value-Copy Elision

## Context

Wado's `resource` is an opaque handle to a Component Model (CM) resource. CM
gives resources a precise ownership discipline that Wado's surface language
does not yet model. This WEP adopts that discipline as an affine type rule
plus a small, resource-scoped static check, and uses the result as the
foundation for the long-reserved `move` and `unique` keywords.

The move-tracking half of that check is not specific to affine types. It
answers one question — _is this the binding's last use, or is the value still
live afterward?_ — and that same question decides, for a copyable value type,
whether a materialization needs a physical deep copy or can transfer storage in
place. This WEP therefore also specifies value-copy elision as a third client
of one shared ownership analysis, retiring the separate
"copy-everything-then-elide" machinery (`optimize::escape` +
`optimize::value_copy_elide`). See "Generalization to copyable value types".

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

- `lift_own` removes the handle from the table — ownership is transferred.
  It traps if the handle is currently lent out (`num_lends != 0`) or is not
  actually an `own` handle.
- `lift_borrow` reads the representation and leaves the source handle in
  place, recording a lender so the source stays alive until the call ends.
- A task that returns with `num_borrows > 0` traps: borrows may not outlive
  the call that created them.
- `borrow` may not appear in `stream` / `future` element types — the
  call-scoping rule does not extend to them.
- `resource.new` / `resource.drop` / `resource.rep` are usable only inside
  the component that defined the resource type.

In short, CM already enforces "no double-drop, no use-after-transfer, no
leaked borrow" — but dynamically, as a trap, and only at the component
boundary.

### Wado today

- `resource` declarations exist (`pub resource Descriptor { ... }`). Methods
  take either `self: &Descriptor` (a borrow) or the resource by value (a
  consuming method such as `Request::consume_body`).
- CM-binding synthesis maps `&R` to `borrow<R>` and a by-value `R` to
  `own<R>`. `Own<T>` / `Borrow<T>` appear as builtin names in `cm_abi.rs`
  but are otherwise vestigial.
- `move` and `unique` are reserved but unimplemented: `unique` is lexed to
  `TokenKind::Unique` and never consumed by the parser; `move` is not even
  tokenized.
- `synthesis/resource_cleanup.rs` (`elaborate_resource_drops`) reconstructs
  ownership by heuristic dataflow and inserts `resource.drop` on every path
  where an owned resource is not transferred. Its own module documentation
  anticipates this WEP: once Wado distinguishes owned from borrowed handles
  with a `move` operator, "this pass collapses to 'drop every owned binding
  that was not moved out'".

### Two earlier WEPs in tension

- [Resource Lifecycle Management (RAII)](./wep-2026-01-12-resource-lifecycle.md)
  declares resources implicitly `unique` (move-only) with destructors and
  scope-based cleanup. None of `move` / `unique` / the cleanup model was
  implemented.
- [Resource Inheritance and Downcast](./wep-2026-04-28-resource-inheritance.md)
  states that "resource handles are themselves immutable values" with "value
  semantics, no lifetimes, no borrow checker", and that "every `resource`
  method takes `&self`".

Read as language-wide claims these contradict each other. This WEP resolves
the tension by splitting the model by _resource kind_ — affine resources
versus host-object resources — a distinction both earlier WEPs already draw.

## Decision

### Split the ownership model by resource kind

A Wado `resource` is one of two kinds, and the ownership model follows the
kind:

| Resource kind        | Backed by                                                                                             | Ownership model                                                | Cleanup                                |
| -------------------- | ----------------------------------------------------------------------------------------------------- | -------------------------------------------------------------- | -------------------------------------- |
| Affine resource      | a CM `own` / `borrow` handle, a canonical-ABI waitable handle (bare `i32`), or a guest representation | Affine (move-only) + resource-scoped borrow checker (this WEP) | Deterministic `resource.drop` / `dtor` |
| Host-object resource | a Wasm GC reference to a host object (no `dtor`)                                                      | Value semantics (copyable), no borrow checker — WEP 2026-04-28 | Wasm GC                                |

The affine kind spans three backings:

- a CM `own` / `borrow` resource handle — CM-imported resources (WASI
  `Descriptor` etc.) and guest-defined resources exported across a CM
  boundary;
- a canonical-ABI waitable handle — a bare `i32` index that is _not_ a CM
  `resource` type (`stream` / `future` ends, `subtask`, `waitable-set`,
  `error-context`), but still needs exactly-once `drop`, so Wado models it as
  an affine resource;
- a guest representation — guest-internal resources such as `AsyncCall<T>`
  that never cross a CM boundary.

They differ in representation but share one ownership model. The CM
classification — `resource` vs async value type vs waitable — is orthogonal:
what places a type in the affine kind is needing exactly-once cleanup, not
being a CM `resource`.

`Waitable` (the identity token returned by `join`) is _not_ affine: it has no
`drop` and no lifecycle, so it is a copyable newtype over `i32`, not a
resource — see
[WEP 2026-03-01](./wep-2026-03-01-cm-resource-canonical-attrs.md).

This is not a compromise; it follows from what an affine resource owns. It
holds a one-shot, destructor-bearing thing — a CM `own` / `borrow` handle into
a per-instance table, or a guest-owned allocation — so copying it (`let b = a;`)
would alias that thing: a double-`resource.drop` trap, a use-after-transfer
trap, or a double free. A host-object resource is a GC reference to an object
the host owns, with no handle table and no `dtor`; holding many references to
the same object is normal and safe, and the GC reclaims the object. So
WEP 2026-04-28's
value-semantics claims are correct _within its scope_ (`extends` is gated on
host-object resources, declared `type="extern-ref"`), but must not be read as
language-wide. The amendment below narrows that wording.

#### Representation is orthogonal to kind

The _ownership model_ is fixed by the resource kind. The _wasm-level
representation_ of a CM canonical handle is a separate, ABI-mode axis owned by
[GC in Components](./wep-2026-03-28-gc-in-components.md): the handle is lowered
as `i32` in CM-LM mode and as `externref` in CM-GC mode. A CM canonical
resource lowered as `externref` in CM-GC mode is still a CM `own` / `borrow`
handle with a `dtor` — it is affine, and this WEP applies to it unchanged. It
must not be confused with a host-object resource, whose `externref` points at
a host-owned GC object and has no `dtor`. "`i32` vs `externref`" is therefore
not the axis that decides the ownership model; the resource kind is.

The rest of this WEP concerns affine resources.

### Affine resources are move-only

An affine resource value is move-only:

- It cannot be copied. `let b = a;` where `a` is a resource binding moves `a`
  into `b`; `a` is then invalid. There is no implicit copy.
- It is transferred by value passing, returning, or placement into an
  aggregate. After a transfer the source binding is invalid.
- `move a` is the explicit transfer operator. It is required wherever a
  resource binding is consumed and the consumption would otherwise be easy to
  miss (value-argument positions, see below). `move` makes the loss of `a`
  syntactically visible.

A resource type is implicitly `unique`. `unique` is also a modifier on
`struct` (see "Generalization to `unique`").

This affine rule is the entire ownership model. It is not, by itself, a
borrow checker: there are no lifetimes, no regions, and no exclusivity
analysis. spec.md's headline "no borrow checker" remains true for the
value-semantics language at large; resources are a single, deliberately
contained exception.

#### Consuming `self` receivers are resource-only

A method may take its receiver by value — a bare `self`, which _consumes_ the
receiver — only when the receiver type is a `resource`. Non-resource methods
take `&self` or `&mut self`; a bare `self` receiver on a non-resource type is
a compile error.

A consuming receiver is only meaningful for an affine type. On a copyable
value-semantics type, by-value `self` would hand the method a deep copy of the
receiver — observably identical to `&self` plus a wasted copy. Restricting
`self` receivers to resources keeps the only by-value receiver the one that
carries real meaning: move the resource in and consume it.

This is what licenses `fn drop(self)` (see "Deterministic drop") and the
consuming `fn wait(self)` / `fn cancel(self)` on `AsyncCall<T>` — a
guest-defined resource owning an in-flight CM subtask, per
[WEP 2026-04-22](./wep-2026-04-22-subtask-generic.md). It does not license
consuming methods on a `unique struct`: a `unique struct` is affine, but it is
consumed by `move` into a by-value parameter or by scope-exit drop, not
through a `self` method.

### The resource-scoped borrow checker

Borrows of resources (`&R`, including `&self` receivers) need one additional
check: a borrow must not outlive — or be invalidated by a move of — the
resource it points into. A full borrow checker is unnecessary here because
three properties of resources collapse the hard parts:

1. No `&mut` on resources. Every affine resource method takes `&self`
   or consumes `self` by value; observable mutation happens host-side. With
   no `&mut`, there is no shared-XOR-mutable exclusivity analysis at all —
   the conceptually heaviest half of a borrow checker is simply absent.
2. Borrows do not escape their function. A `&R` may not be returned, stored
   in a struct field or container, or placed into a `stream` / `future`
   (CM forbids the last outright). Every borrow's extent is therefore a
   subset of one function body, so there are no lifetime parameters, no
   variance over lifetimes, and no interprocedural lifetime relationships.
   Function signatures stay annotation-free: `fn f(r: &Descriptor)`, never
   `fn f<'a>(...)`. This mirrors CM's own call-scoped `borrow`; Wado is
   stricter (function-scoped) but never less safe.
3. Tracking is binding-granular, not place-granular. A resource-bearing
   struct, array, or variant moves and drops as one unit. The checker never
   tracks "field 3 of this struct was moved" — the path-sensitive sub-place
   analysis that makes Rust's checker expensive is out of scope.

What remains is small. For each resource-typed local the checker holds one
of three states — `unborrowed`, `borrowed`, `moved` — and reports:

- use-after-move: using a binding in the `moved` state.
- move-or-drop-while-borrowed: moving or dropping a resource while a live
  `&` to it exists.
- borrow escape: returning or storing a `&R`.

States merge at `if` / `match` / loop join points exactly as in
definite-assignment analysis (a binding `moved` on one incoming path and
`unborrowed` on another becomes `maybe-moved`, and using it is an error).
Wado has no `goto`, so the analysis is a structural walk over the typed TIR
tree — `if` / `match` / `while` / `for` / early `return` — and needs no
explicit CFG.

The move-or-drop-while-borrowed check starts lexical: while a `let b = &r;`
binding is in scope, `r` may not be moved or dropped. Method-call receivers
(`r.foo()`) create a borrow that lives only for the call expression and never
conflict. A later, non-breaking refinement replaces lexical scoping with
last-use liveness (NLL-style); the value-copy client below makes that
refinement the analysis basis, so the two share one liveness computation (see
"Generalization to copyable value types").

The cost of "lightweight" is paid in expressiveness, deliberately:

- A function cannot return a `&R` or store one. Resource methods return owned
  handles or plain values instead — consistent with the existing stdlib and
  with WEP 2026-04-28's `downcast` returning `Option<T>` by value.
- An individual element cannot be moved out of a resource-bearing aggregate.
  Operate on `List<Descriptor>` by iteration: `for r in &arr` borrows each
  element, `for r in arr` consumes the array. Finer-grained element moves are
  an open question, not a v1 feature.

### `own` / `borrow` at the CM boundary

The affine model maps directly onto CM handle types, so CM-binding synthesis
needs no ownership heuristics:

| Wado position                      | CM handle                                    |
| ---------------------------------- | -------------------------------------------- |
| by-value `R` parameter / return    | `own<R>`                                     |
| `&R` parameter, `&self` receiver   | `borrow<R>`                                  |
| by-value `self` (consuming method) | `own<R>` — transfers / consumes the receiver |

Because the borrow checker has already proven that a by-value resource is
uniquely owned at the transfer site and that no `&` to it is live, the
`lift_own` preconditions (`num_lends == 0`, handle is `own`) hold statically.
CM's runtime trap becomes a guaranteed-unreachable backstop rather than a
real failure mode.

### Deterministic drop

A resource that is owned and not moved out is dropped when its binding goes
out of scope, on every control-flow path — block end, early `return`, and the
non-taken side of a branch. `move` suppresses the drop at the source.

A resource may also expose an explicit drop method. It is a _consuming_
method — `fn drop(self)` — the form used by `Stream`, `WaitableSet`, and the
other canonical resources in
[WEP 2026-03-01](./wep-2026-03-01-cm-resource-canonical-attrs.md) (amended
below from `fn drop(&self)`). Calling `r.drop()` consumes `r`, so the move
checker records the move and the automatic scope-exit drop does not also
fire — exactly one `resource.drop` is emitted. A `&self` drop method would
be unsound: the binding would stay usable after its handle is dead, and the
scope-exit drop would double-free.

The drop action depends on which side of the boundary the resource lives on:

- An imported resource handle held by the guest (e.g. `Descriptor`) is
  dropped by emitting `resource.drop` — the host frees the underlying object.
- A guest-defined resource runs its destructor — whether it is exported
  across a CM boundary or purely guest-internal (e.g. `AsyncCall<T>`).
  Destructor syntax and effect propagation are as specified in
  [WEP 2026-01-12](./wep-2026-01-12-resource-lifecycle.md); this WEP does not
  change them. It only makes the _ownership tracking_ that decides _when_ a
  drop fires authoritative rather than heuristic.

Compositional cleanup follows WEP 2026-01-12: a `struct`, `variant`, or array
that contains a resource is itself `unique` and gets a synthesized destructor
that drops its resource fields in declaration order; the synthesized
destructor's effect row is the union of the field destructors' effects.

Panic / trap does not unwind, so a drop is not guaranteed to run on panic.
This limitation is unchanged from WEP 2026-01-12 and is revisited only when
Wasm exception handling is adopted.

### Generalization to `unique`

The borrow checker is built for resources but its move-tracking half is
representation-independent. `unique` is therefore also a `struct` modifier:

```wado
unique struct Token { value: String }
```

A `unique struct` is move-only and uses the same affine tracking, but has no
synthesized destructor unless it contains resource fields. This lets the
feature land resource-first and generalize to user-defined move-only types
with no additional checker machinery — resources are simply the first client.

### Generalization to copyable value types

Affine resources and `unique struct`s use the move analysis to _forbid_ a
second use of a moved binding. A copyable value type uses the identical
analysis to _decide_ whether a materialization needs a physical deep copy or
can transfer storage in place. Copyable value types are the third client of the
one analysis.

Value semantics requires that every binding, field, element, argument, and
return logically own its value: `let b = a; b.push(4)` must not perturb `a`.
The physical copy that guarantees this is needed only when the source stays
live and observably distinct from the destination — exactly the condition the
ownership analysis already names. At each site where a value-typed value is
_consumed into an owner_ — a `let` / assignment binding, a struct field, a
list / tuple element, a by-value argument, or a return — the analysis
classifies the consumption:

| Consumption                                             | Copyable value type                         |
| ------------------------------------------------------- | ------------------------------------------- |
| last use of the source (the source is `moved`)          | move — transfer storage, emit no copy       |
| source still live afterward                             | copy — materialize an independent deep copy |
| source live, but neither it nor the destination mutates | share — alias, emit no copy (read-only)     |

Row one is where an affine resource moves and a `unique struct` the same; a
copyable type does likewise, so the copy is elided. Row two is where a resource
raises use-after-move; a copyable type instead materializes the copy that keeps
the two independent. Row three has no resource analogue — resources are never
shared — and is a value-type-only refinement.

This is the whole model, and it replaces "copy-everything-then-elide". Rather
than inserting a `$value_copy$T` wrapper at every materialization and later
proving each removable via a whole-program freshness / escape analysis, the
ownership analysis decides move / copy / share once, at the consumption site,
on structured TIR. A site the analysis cannot prove safe defaults to `copy` —
sound, at worst an extra copy, never a miscompile — the same safe default the
affine checker takes when it cannot prove a binding dead.

#### Last-use liveness is the basis, not a refinement

The affine checker can start lexical (a borrow holds until its `let` leaves
scope). The copyable client cannot: a move pays off precisely at the last
_use_, which routinely precedes scope exit — `items.push(elem)` is `elem`'s
last use even though `elem` is still in scope for the rest of the loop body. So
the ownership analysis is defined on last-use liveness (NLL-style): a binding is
`moved` at the program point of its final use on each path, computed by backward
liveness over the same structural TIR walk and merged at the same join points.
Promoting last-use liveness from the deferred refinement to the basis is
non-breaking for the affine clients — it only lengthens the span a binding is
available before its move — and is required by the value-copy client. It is
implemented once and shared.

#### What the value-copy client adds over the move core

The move core is shared verbatim. The copyable client layers two things the
affine clients do not need:

- Copy materialization on a live consumption — the affine error case. The
  existing deep-copy helpers (`$value_copy$T`) are reused, but emitted only at
  sites the analysis marks `copy`, not everywhere.
- The read-only-share refinement: a consumption whose source and destination
  are both never mutated again may alias. Mutability is a local property
  (`let` vs `let mut`, whether a `&mut` is taken), not a whole-program alias
  question, so it stays within the same structural walk; where it cannot be
  shown, the site falls back to `copy`.

One independent soundness bug is fixed alongside: a recursive value type's
`$value_copy$T` helper used to fall back to identity (`return v`) to avoid
unbounded synthesis, which silently shared storage. A materialized copy must be
a true deep copy, so the helper for a recursive type is emitted as a
mutually-recursive function that copies through the indirection rather than
returning its argument. This is implemented: the identity fallback covered not
just recursive types but every variant with a value-typed payload, every struct
transitively containing a variant, and `List<variant>` — copying any of those
aliased the payload (or the whole struct), and `VariantConstruct` did not copy
its payload at all. `needs_value_copy` now classifies a variant by its case
payloads (via the variant-case index registered on the `TypeTable`), the
synthesized variant helper is a `match` that re-constructs the matched case
with a copied payload, and variant construction copies a live payload like a
struct field. Recursive cycles terminate because a nested copy is a call to
the nested type's own helper — the helpers are mutually recursive.

#### Retirement of the freshness / escape machinery

As this WEP deletes `resource_cleanup.rs`'s heuristic ownership reconstruction,
the value-copy client deletes the two-phase copy machinery it supersedes:

- `lower::plan::value_copy` stops wrapping every materialization; it emits a
  copy only where the ownership analysis says `copy`.
- `optimize::escape` (the interprocedural freshness / `returns_fresh` fixpoint)
  and `optimize::value_copy_elide` (the wrapper-stripping pass) are removed.
  Their job — recovering copies that were never necessary — no longer exists,
  because the copies are not inserted in the first place.

The freshness analysis failed on exactly the shapes the ownership analysis
handles natively: a value threaded through `?`, a match-arm binding, or an
accumulator `push` is a chain of last-use moves, which liveness sees directly
and which no interprocedural `returns_fresh` property is needed to justify.

### Pipeline and implementation

- The move / borrow check is a diagnostic pass over the resolved,
  `AstId`-keyed `Semantics` layer that `annotate` produces — not over TIR.
  This is deliberate: the LSP path stops at `Semantics` and never builds TIR
  (`build_tir = false`, no reify), so a TIR-level pass could not surface
  use-after-move / move-while-borrowed errors in the editor. Running over
  `Semantics` places the analysis exactly where the existing item-level
  liveness and unused diagnostics already run — the shared LSP + batch path —
  and keys its output by `AstId`, so errors point at source spans and the
  facts are available to both the editor and batch compilation. The structural
  walk, the last-use liveness, and the three-state lattice are the whole pass.
  Its per-consumption classification (move / copy / share / error), keyed by
  `AstId`, is the single output every client reads.
- The value-copy client consumes that classification in the batch pipeline:
  reify (the sole TIR producer, running before monomorphization) stamps each
  consumption-site TIR node with its class read from the `AstId`-keyed result,
  so every monomorphized instance inherits one decision computed once on the
  generic body. Desugar-introduced temporaries (a `?` match-arm binding) carry
  no `AstId`; they are single-use by construction, hence trivially `move`,
  decided structurally at the stamp site.
- Drop elaboration remains a synthesis pass. It no longer reconstructs
  ownership: it consumes the checker's authoritative "owned and not moved at
  this scope exit" set and inserts the drop.
- Value-copy lowering (`lower::plan::value_copy`) reads the same classification:
  it emits a `$value_copy$T` at `copy` sites and nothing at `move` / `share`
  sites. There is no separate elision pass. `optimize::escape` and
  `optimize::value_copy_elide` are deleted.
- `synthesis/resource_cleanup.rs`: the heuristic ownership-reconstruction half
  (`is_resource_aggregate` and the borrow-vs-transfer guesses) is removed. The
  drop-elaboration mechanism — walking scopes, emitting `resource.drop` on
  fall-through paths, structural drop of resource-carrying aggregates via a
  synthesized `match` — is kept and repurposed to run off the checker's
  output. Rewriting it from scratch against the checker API is acceptable if
  cleaner; the requirement is only that the checker becomes the single source
  of truth for ownership.
- `Own<T>` / `Borrow<T>` in `cm_abi.rs` are not surfaced as user types; `&R`
  and by-value `R` are the surface forms. The builtin names may be retired
  once binding synthesis is type-driven.

### M5 value-copy client — as implemented

The shipped value-copy client is caller-side and single-phase, but its
move/copy decision is split across two analyses rather than one `AstId`-keyed
classification, because two consumers with different reach need it:

- Source last use — `elaborator::liveness` runs the backward last-use pass over
  the typed AST and projects the final-use sites to `(module, span)` in
  `moved_local_spans` (threaded `Package` → `FlatPackage` to the planner). This
  is the LSP-facing, `AstId`-keyed artifact the WEP describes.
- Synthesized last use — the AST pass cannot see reify- and
  synthesizer-emitted bodies (serde `deserialize`/`serialize`, `Default` /
  `Clone` derives, the `?` desugar), which is exactly where the hot copies are
  (a deserialized struct field). So `lower::plan::value_copy::last_use` runs a
  second last-use analysis directly on the monomorphized TIR of every function.
  It is post-monomorphization, so it keys on nothing — it recomputes per body —
  and needs no cross-phase span table.

The fold (`lower::translate`) moves a consumption when _either_ analysis marks
it: the span is in `moved_local_spans`, or the TIR local is in the per-function
move set. Union is sound because each is independently sound; the TIR pass is
the superset in practice but the span pass is retained for source fidelity.

The TIR pass (`compute_move_eligible`) is a backward liveness plus a freshness
fixpoint. A local is moved when:

- every read of it is a _final_ read (dead on every live path afterward) —
  precise across divergent `match` arms (a `?` `Err`-rewrap reads the scrutinee
  but only on the returning path, so the scrutinee stays a final read on the
  `Ok` path) and across loop back-edges (a value produced and consumed in one
  iteration is dead at the head);
- it _exclusively owns fresh storage_ — a least fixpoint over `is_owned_value`
  (shared with the return-convention analysis) proves its value traces to a
  fresh allocation, allowing multi-read intermediates (the `?` tag-test +
  payload-extract) to carry freshness up the chain;
- it has _no live alias_ at its binding — the local its value is a projection
  or whole-value move of (`alias_root`, `None` for a fresh allocation whose
  result aliases nothing) is dead after the binding. This is what keeps
  `if let Some(s) = opt { s.push(x) }` copying `s` while `opt` is read again,
  yet lets a deserialize temporary — whose scrutinee is a dead call result —
  move.

Freshness (does the value alias existing data) and consumability (is this the
last observation) are orthogonal: a fresh value read twice is not movable, and
the analysis keeps them as separate sets (`fresh` vs the final move set)
precisely so a twice-read fresh temporary propagates ownership without itself
being moved.

The return-convention analysis (`ownership.rs`) supplies the interprocedural
half: a call is a fresh allocation iff its callee returns owned. It is the
caller-side, single-phase replacement for `optimize::escape`'s `returns_fresh`
fixpoint.

The confinement half is now caller-side too. `lower::plan::value_copy::confine`
computes per-parameter confinement (a two-channel return/side escape fixpoint
over TIR, run before boxing so `&mut` / `&` are still distinguishable), and the
fold skips a by-value argument's copy when the callee parameter is confined and
the argument aliases no mutated sibling — the precise `no_mut_alias` check that
`optimize::value_copy_elide` did post-hoc, using a shared may-alias union-find.
With freshness and confinement both decided at insertion, `optimize::escape` and
`optimize::value_copy_elide` are deleted; `build_param_mut` moved to
`optimize::value_copy::mutation` (still used by `copy_prop`). `value_copy_demote`
is independent of both and survives.

- [x] Fold the confinement (`param_escapes`) recovery into the caller-side
      insertion so `optimize::escape` / `optimize::value_copy_elide` are deleted,
      as the WEP intends.

### Relationship to earlier WEPs

- [WEP 2026-01-12 (RAII)](./wep-2026-01-12-resource-lifecycle.md): its
  `unique` / move-only / destructor / compositional-cleanup model is adopted,
  scoped to affine resources, and given a concrete implementation via
  this WEP's checker. Its open question "should there be a borrow checker?"
  is answered: a resource-scoped one, with the three simplifications above.
  Its explicit-drop form — a free `drop(r)` — is superseded by the consuming
  `fn drop(self)` method (see the amendment below).
- [WEP 2026-03-01 (CM Resource Canonical Attributes)](./wep-2026-03-01-cm-resource-canonical-attrs.md):
  amended (below) — its resource `drop` methods take `&self`, which is
  unsound under the affine model; they become consuming `fn drop(self)`.
- [WEP 2026-04-28 (Inheritance)](./wep-2026-04-28-resource-inheritance.md):
  amended (in that WEP) so its value-semantics wording reads as scoped to
  host-object resources, not language-wide.
- [WEP 2026-01-12 (Value Semantics and Reference Stores)](./wep-2026-01-12-value-semantics-and-stores.md)
  and [WEP 2026-06-15 (Live ValueGraph)](./wep-2026-06-15-live-value-graph.md):
  the "copy-everything-then-elide" implementation of value semantics — blanket
  `$value_copy$T` insertion recovered by `optimize::escape` /
  `optimize::value_copy_elide` — is superseded by the value-copy client of this
  WEP's ownership analysis. The value-semantics _language rule_ is unchanged;
  only how the compiler realizes it changes.

## Amendments to earlier WEPs

### WEP 2026-04-28 (Resource Inheritance)

WEP 2026-04-28's value-semantics statements are correct for host-object
resources (the only kind `extends` admits) but are phrased as language-wide
claims. The following sentences are narrowed; the substance for that WEP's
scope is unchanged:

- "In Wado, resource handles are themselves immutable values ... They have
  value semantics, no lifetimes, no borrow checker." → scoped to host-object
  resources, with a cross-reference to this WEP for affine resources.
- Sidebar "resource handles are immutable" / "Across the language, every
  `resource` method takes `&self`." → scoped to host-object resources. Affine
  resources additionally have by-value consuming methods.

These edits are applied directly in WEP 2026-04-28.

### WEP 2026-03-01 (CM Resource Canonical Attributes)

WEP 2026-03-01 declares the resource `drop` method as `fn drop(&self)`. A
`&self` receiver is a non-consuming borrow, so under the affine model
`r.drop()` would leave `r` usable after its handle is dead and would
double-drop against the automatic scope-exit drop. The `drop` method is
amended to a consuming `fn drop(self)`; the `r.drop()` call-site syntax is
unchanged. This edit is applied directly in WEP 2026-03-01, with an amendment
note in that WEP.

## Consequences

### Positive

- One coherent ownership story: affine ownership for affine resources, GC
  value semantics for host-object resources, split by resource kind — a
  distinction both earlier WEPs already draw.
- Soundness is static: double-drop, use-after-transfer, and leaked borrows
  are compile errors, not runtime traps.
- No lifetimes, no annotations, no exclusivity analysis — the checker is on
  the order of definite-assignment analysis.
- The heuristic ownership reconstruction in `resource_cleanup.rs` — a known
  fragile spot — is deleted; drop insertion becomes a trivial consumer of the
  checker.
- `move` and `unique` get a real implementation, resource-first, generalizing
  to `unique struct` with no extra machinery.
- Value-copy elision becomes a property of one local, structural analysis
  instead of a whole-program freshness / escape fixpoint. The failure mode that
  fixpoint has — one non-fresh link (an error return, an `&mut`-capturing
  intermediate) poisoning a whole call chain, so hot-path copies survive — is
  gone: a last-use move needs no interprocedural property to justify.
- Two fragile subsystems collapse into the checker's output:
  `resource_cleanup.rs`'s ownership heuristic and `optimize::escape` +
  `optimize::value_copy_elide`.
- Missed optimizations are detectable without benchmarks: each move / copy /
  share decision is fixable in an e2e `wir_not_expect` fixture.

### Negative

- `move` and move errors are a new concept in an otherwise value-semantics
  language, applying to one type category (plus `unique struct`).
- The analysis must be last-use liveness from the start, not the simpler
  lexical scoping the affine checker could have shipped alone — a larger first
  increment, though a strictly more precise one, and shared by all three
  clients.
- Binding-granular tracking forbids moving an element out of a
  resource-bearing aggregate; such code must restructure around iteration.
- Borrows cannot escape a function, so a resource API cannot return a `&R`.
  In practice resource methods return owned handles or values, so this is a
  constraint on the rare case rather than the common one.

### Open questions

- Panic / unwind: should drops be guaranteed once Wasm exception handling is
  adopted?
- Should the lexical move-or-drop-while-borrowed rule be upgraded to last-use
  liveness, and when?
- Finer-grained (place-level) moves out of resource-bearing aggregates — if a
  real use case appears.
- `borrow<T>` in generated WIT applies only to resources; the `&Record` →
  `borrow<record>` mapping shown in
  [WEP 2026-01-29](./wep-2026-01-29-wit-wado-mapping.md) is a separate
  question about record parameters and is out of scope here.

## Implementation Roadmap

### M1: Affine resources + move checking

- [ ] `move` keyword in lexer / AST / parser; `unique` token consumed by the
      parser as a `struct` modifier and recognized as implicit on resources.
- [ ] Move-tracking pass over the resolved `Semantics` layer (`AstId`-keyed,
      shared LSP + batch path — not TIR): per-binding `unborrowed` / `borrowed`
      / `moved` lattice, backward last-use liveness, structural walk, join-point
      merge. The per-consumption classification is the shared output.
- [ ] use-after-move and move-while-borrowed diagnostics with source spans.
- [ ] Reject copying a resource binding; require `move` at value-consuming
      argument positions.

### M2: Borrow scoping

- [ ] Borrow-escape diagnostic: reject returning or storing a `&R`.
- [ ] Lexical move-or-drop-while-borrowed enforcement for `let b = &r;`.

### M3: Drop elaboration off the checker

- [ ] Remove heuristic ownership reconstruction from `resource_cleanup.rs`.
- [ ] Drive `resource.drop` / destructor insertion from the checker's
      owned-and-not-moved set.
- [ ] Compositional destructor synthesis for `unique struct` / variant /
      array carrying resources (per WEP 2026-01-12), in declaration order.
- [ ] Migrate the resource `drop` methods in `types.wado` from `fn drop(&self)`
      to consuming `fn drop(self)` (per the WEP 2026-03-01 amendment).

### M4: CM boundary + amendment

- [ ] Confirm CM-binding synthesis maps by-value `R` → `own`, `&R` → `borrow`
      with no ownership heuristics left.
- [x] Apply the wording amendments to WEP 2026-04-28 and WEP 2026-03-01.

### M5: Value-copy client (copy elision)

- [ ] Emit the per-consumption move / copy / share classification for copyable
      value types from the same checker output.
- [x] `lower::plan::value_copy` inserts `$value_copy$T` only at `copy` sites —
      freshness (`analyze`/`ownership`), last-use move (`last_use`), and
      confinement (`confine`) are all decided at insertion.
- [x] Read-only-share refinement: a read-only binding bound from a projection
      whose storage is provably never mutated while live shares the source and
      emits no copy — field-sensitive over disjoint fields (`self.rows[0]` shared
      while `self.tick` is mutated), pinned by `value_copy_elide_disjoint_field_mut`.
- [x] Fix recursive-type `$value_copy$T` synthesis to a true deep copy
      (mutually-recursive helper), replacing the identity `return v` fallback —
      covers variant payload deep copy, structs containing variants,
      `List<variant>`, and payload copy at `VariantConstruct` sites (pinned by
      the `value_copy_variant_*` e2e fixtures).
- [x] Delete `optimize::escape` and `optimize::value_copy_elide`.
- [ ] Pin representative move / copy / share decisions as e2e
      `wir_not_expect` / `wir_expect` fixtures (serde `?`-chain, accumulator
      `push`, literal-into-field, `let b = a; b.mut; …a`).

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
