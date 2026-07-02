# Trait Derivation Policy — Bound-Driven Synthesis

Status: Implemented for `Serialize` / `Deserialize` / `Eq` / `Ord` over struct,
variant, enum, and flags types, including anonymous structs (`Eq` / `Ord` are
struct/variant/enum only — flags erase to `u32` and need no impl of their
own). The shipped discovery mechanism records a request at each
call-resolution site that happens to check eligibility (`operators.rs`,
`trait_query.rs`); [Discovery Mechanism](#discovery-mechanism) below
specifies a placeholder-based redesign that closes the gap this leaves (a
resolution path that doesn't call the check silently skips generation — see
`operators.rs`'s `Variant` comment) but is not yet implemented. Not yet
extended to `GenericInstance` for `Serialize` / `Deserialize`, or to a policy
declaration for user-defined traits. See Open Questions.

## Context

Wado derives type-directed traits under two inconsistent policies — the rule
for _when_ a derived impl exists for a type `T`:

- Automatic: `Inspect` / `InspectAlt` / `Eq` / `Ord` / `Default` synthesize
  unconditionally for every eligible type, whether or not the program uses
  them.
- Explicit: `Serialize` / `Deserialize` exist only if the user writes the
  empty marker `impl Serialize for T;`. A bare `T: Serialize` bound does not
  trigger synthesis.

The split is ad hoc: serde's marker is boilerplate the compiler could
discharge structurally (as it already does for `Inspect`), and it makes
anonymous-struct serialization impossible (no name to write a marker
against). `Eq` / `Ord`, meanwhile, synthesize for every declared type
regardless of use — compile-time and code-size waste for types the program
never compares.

Orthogonal to
[Library-Defined Derivation (`Reflect`)](./wep-2026-06-13-reflect-derivation.md),
which decides _how_ an impl is written. This WEP decides _when_ one is
instantiated for a given `T`.

### Forcing functions

- Anonymous structs have no name, so `impl Serialize for …` is unwritable —
  bound-driven synthesis is the only way to satisfy `T: Serialize`. A hard
  prerequisite for the efficient field path in
  [`core:log`](./wep-2026-06-25-core-log.md).
- `Eq` / `Ord` synthesizing for every declared type costs compile time and
  code size with no compensating benefit (unlike `Inspect`, which exists so
  `{x:?}` always works everywhere).

## Decision

### A per-trait derivation policy

| Policy      | A `T: Trait` obligation is satisfied by …                | Examples                                                    |
| ----------- | -------------------------------------------------------- | ----------------------------------------------------------- |
| `automatic` | structural synthesis, always; the impl always exists     | `Inspect`, `InspectAlt`, `Display`, `DisplayAlt`, `Default` |
| `on_bound`  | structural synthesis, on demand when a bound requires it | `Serialize`, `Deserialize`, `Eq`, `Ord` (the change)        |
| `explicit`  | only a written `impl Trait for T;` (or full manual impl) | default for user traits                                     |

- A hand-written `impl Trait for T { … }` always wins.
- An `impl` declaration — hand-written or the empty marker `impl Trait for
  T;` — is a conformance check, never a synthesis trigger. It asserts (and,
  for the marker, validates immediately) that `T` can implement `Trait`; it
  does not by itself cause any code to be generated. The marker `impl Trait
  for T;` stays valid under every policy: under `explicit` it's the only way
  to make `T` eligible at all (the policy runs no structural scan on its
  own); under `automatic` / `on_bound` it's redundant but harmless, since
  the same eligibility already holds structurally. For `Eq` / `Ord` (and,
  after closing the historical gap, `Serialize` / `Deserialize`) the marker
  is also a hard guarantee: a compile error at the marker's own span if any
  field/case is ineligible, unlike a bound (simply unsatisfied elsewhere) or
  the structural rule (nothing to reject).
- The actual trigger for synthesis — the point where a body gets generated —
  is usage: some call site resolves a reference to the trait method. See
  [Discovery Mechanism](#discovery-mechanism).
- `automatic`, `on_bound`, and `explicit` differ along two independent axes:
  whether eligibility is discovered by an unprompted structural scan
  (`automatic` / `on_bound`) or only via an explicit marker (`explicit`),
  and whether a body is generated unconditionally (`automatic`) or only on
  a reference (`on_bound` / `explicit`). For `Eq` / `Ord` the eligibility
  axis is invisible at the bound-check level — `T: Eq` was already satisfied
  the moment fields qualified, whether or not any code emits — so the only
  observable difference `automatic` → `on_bound` makes is whether a body
  gets generated for a type nothing referenced.

### Bound-driven synthesis semantics

An `on_bound` obligation `T: Trait` is satisfied structurally: no manual impl
exists, and every field/case of `T` satisfies `Trait` recursively. On
failure, the error reason-chains from the bound site to the offending
field/case ([Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)).

Both `Eq` / `Ord`'s marker and (closing the historical gap) `Serialize` /
`Deserialize`'s marker validate `T` structurally at their own span and are a
hard compile error if ineligible — see [Discovery Mechanism](#discovery-mechanism)
for how that validation feeds the same eligibility state a bare bound
consults.

Whole-program and monomorphized, so there's no orphan rule to violate.
`GenericInstance` is out of scope for `Serialize` / `Deserialize` —
elaboration only sees the generic template, so a request keyed by a concrete
instantiation wouldn't resolve to a body (the same gap
[Serde](./wep-2026-02-28-serde.md) tracks for generic-struct `Deserialize`).
`Eq` / `Ord` are unaffected: their generic synthesis predates this WEP and
already records against the base declaration.

### Discovery mechanism

A structural obligation is only ever satisfiable if it can be _found_ by
whichever code path resolves a method call — an operator, a `.method()`
call, or a generic bound check at a call site. Recording eligibility as a
side effect of each such path checking it (the shipped mechanism today) is
fragile: every resolution path must remember to run the check, and a path
that doesn't silently skips generation (documented in `operators.rs`'s
`Variant` comparison comment — comparison dispatch that fell through to
`try_lower_comparison` at monomorphize time, after synthesis had already
run, would never see its method generated). Nothing in the type system
enforces that every current and future resolution path remembers to opt in.

The fix is to stop treating eligibility-checking and reference-discovery as
one event. They become two:

1. Conformance check, at `TraitEnv::build()` — before any module's Annotate
   pass runs, `TraitEnv` is extended to precompute, from AST-level struct /
   variant / enum field types (no full type inference needed, mirroring
   `TraitEnv::build`'s existing struct-field-dependency pass), which
   `(type, on_bound trait)` pairs are structurally eligible. Each eligible
   pair gets a body-less placeholder entry inserted into the same
   `impl_index` / `all_impl_index` tables a hand-written `impl Trait for T {
   … }` occupies. An `explicit`-policy trait runs no such scan; its only
   placeholders come from an explicit marker, validated immediately at its
   own span exactly as `record_explicit_derive_request` does today, with the
   same result (a placeholder registration) instead of a synthesis request.
   This is a genuine cost paid for every declared type under `automatic` /
   `on_bound`, but it is the check only — the WEP's compile-time / code-size
   concern is about body generation, not this structural scan.
2. Reference discovery, during Annotate and after — every method-resolution
   path (operator dispatch, direct `.method()` calls, generic bound checks)
   already queries `TraitEnv`'s impl tables to find its target; it now finds
   the placeholder there directly; no call site needs auto-derive-aware
   fallback logic of its own. The one place that still needs to know about
   placeholders is where a resolved match is lowered into a TIR
   `Call` / `FunctionRef` — if the target is a placeholder rather than a
   real impl, that single site records the reference. This mirrors
   monomorphize's existing generic-instantiation dispatch loop: a template
   is registered once and materialized only when something references it;
   a referenced-but-unmaterialized entry is a bug, not a silently-skipped
   feature (see [Link → Monomorphize → Erase](./compiler.md#link--monomorphize--erase)).

Synthesis (`synthesis::traits`, `synthesis::serde_synth`) then drains
exactly the set of referenced placeholders, the same shape as today's
`bound_driven_synth_requests` snapshot-read, just fed by one funnel instead
of many. An explicit marker with zero references still validates (hard
error if ineligible) but generates nothing — see
[Consequences](#consequences) for the trade-off this changes from today's
shipped behavior.

This redesign only changes _how_ eligibility and reference discovery are
recorded, not the policy semantics in [A per-trait derivation policy](#a-per-trait-derivation-policy) above. It is scoped to
compiler-synthesized bodies; a hand-written `impl Trait for T { … }` (with a
body) is ordinary source, type-checked once because it exists, and left to
ordinary dead-code elimination — it was never part of the eligibility /
reference-discovery problem this section solves.

### Policy assignment

- `Inspect` / `InspectAlt` / `Default` stay `automatic` — no behavior change.
- `Serialize` / `Deserialize` move from `explicit` to `on_bound`.
- `Eq` / `Ord` move from `automatic` to `on_bound`: an impl is generated only
  for a `(type, trait)` pair some call site or bound actually references. A
  marker makes the pair eligible (or, under `explicit`, eligible at all);
  it never references it by itself.
- User-defined traits default to `explicit`; opting into `automatic` /
  `on_bound` is an open question (see below).

### The trust boundary

`Serialize` / `Deserialize` cross a wire/storage boundary, so `on_bound`
means a type becomes serializable the moment some code asks, and a later
field addition silently extends the wire shape — why Rust `serde` and Swift
`Codable` are opt-in. Wado accepts the trade-off: its whole-program model has
no downstream consumers to surprise. A manual impl or field-level `#[hidden]`
remain the levers for tighter control; no dedicated opt-out is introduced.

`Eq` / `Ord` cross no data boundary — `on_bound` only changes _when_ their
impl is generated, never what any `==` / `<` call site returns. Their
motivation is pure compile-time / code size, with no opt-out to weigh.

## Consequences

### Benefits

- One uniform model replaces the ad-hoc split.
- Removes serde's per-type marker boilerplate and unblocks anonymous-struct
  serialization.
- Removes `Eq` / `Ord` compile-time and code-size waste on types the program
  never compares.
- No macros, no dynamic reflection; synthesis stays static and monomorphized.

### Trade-offs

- `Serialize` / `Deserialize` crossing to `on_bound` weakens the opt-in that
  bounds the wire surface today; `#[hidden]` and a manual impl are the only
  countermeasures.
- Errors move from the (absent) impl site to the bound site; reason chains
  keep them legible.
- A future `Reflect`-based rewrite of the synthesized body must not let a
  blanket `impl<T: Reflect> Trait for T` conflict with concrete impls — an
  open coherence question the shipped mechanism doesn't hit yet, since it
  instantiates the existing per-type synthesizer directly.
- `Eq` / `Ord` no longer exist "for free" without a reference; the explicit
  marker guarantees a hard validation error at declaration if `T` is
  ineligible, but — unlike the shipped mechanism — no longer guarantees a
  body is generated in advance. A type intended for future comparison with
  zero current call sites gets no code until something references it.
- Moving the per-type-trait eligibility scan to `TraitEnv::build()` means
  paying it for every declared type under `automatic` / `on_bound`, before
  Annotate has run and full type resolution is available. The scan has to
  work from AST-level field types (mirroring the existing struct-field
  topological sort), which is less information than
  `type_implements_trait_inner` uses today — see Open Questions.

### Relationship and prerequisites

Ships directly against the existing bespoke synthesizers
(`synthesis::serde_synth`, `synthesis::traits`), not against
[`Reflect`](./wep-2026-06-13-reflect-derivation.md), which remains unbuilt.
The original plan was to land this after migrating serde onto a
`Reflect`-based impl, but the two turned out independent — this WEP only
changes _when_ a request is created, not _how_ the body is written. A future
`Reflect`-based rewrite can land later against the same plumbing.

## Alternatives Considered

- **Keep `Serialize` explicit (status quo).** Lowest risk, but inconsistent
  with the automatic traits and makes anonymous-struct serialization
  impossible. Rejected.
- **Make every derivable trait `automatic`.** Maximum ergonomics, but erases
  the trust-boundary distinction — a private type would silently become
  wire-serializable with no opt-out. Rejected.
- **A `#[derive(Serialize)]` attribute.** Wado has no derive macros, and
  `impl Serialize for T;` already serves as the explicit form. Rejected.
- **Keep `Eq` / `Ord` automatic (status quo).** Simplest, but pays synthesis
  cost for every declared type regardless of use, with no offsetting
  benefit. Rejected.
- **Record eligibility as a side effect of each call-resolution path
  checking it (the shipped mechanism).** What's implemented today: works,
  but every resolution path (operator dispatch, method-call resolution,
  generic bound checks) must remember to opt in, and nothing enforces that a
  future path does. Superseded by [Discovery Mechanism](#discovery-mechanism)
  once implemented.
- **Keep per-call-site recording but centralize it behind one funnel
  function every resolution path is required to call.** Removes some
  duplication but keeps the same failure mode as the status quo — a new
  resolution path can still forget to call the funnel. Rejected in favor of
  placeholders, which resolution paths find through the lookup they already
  perform, needing no trait-derivation-specific awareness at all.

## Open Questions

- Declaration syntax for a user-defined trait's policy.
- `GenericInstance` is not yet `on_bound`-eligible for `Serialize` /
  `Deserialize` (`Eq` / `Ord` are unaffected). The placeholder mechanism may
  offer a path — a placeholder keyed by a concrete instantiation, discovered
  post-monomorphize instead of pre-monomorphize — but this isn't worked out.
- Whether `TraitEnv::build()`'s AST-level field-type scan (before Annotate,
  no full type resolution) can decide structural eligibility correctly for
  every case `type_implements_trait_inner` handles today — generic struct
  fields, cross-module recursive types, newtypes of newtypes. If some cases
  can't be decided that early, they need a documented fallback (e.g. treat
  as ineligible until Annotate revisits it, or keep those cases on the
  interim per-call-site mechanism).
- Whether "lower a resolved match into a TIR `Call` / `FunctionRef`" is
  truly the single funnel every reference path goes through, including
  paths synthesis itself introduces (template-string desugaring calling
  `Display` / `Inspect`, effect dispatch, CM boundary adapters) — needs an
  audit before the interim per-call-site checks can be deleted.
- Coherence interaction with concrete impls, relevant once a `Reflect`-based
  rewrite of the synthesized body lands.
