# Trait Derivation Policy — Bound-Driven Synthesis

Status: Implemented. `Eq` / `Ord` / `Default` / `Serialize` / `Deserialize`
are _demand_ `on_bound`: a body is synthesized only where a reference needs
it, discovered through the shared `bound_driven_synth_requests` channel a
bound check or explicit marker records into. They are not total — a `fn`-typed
field blocks `Eq` / `Ord` / serde, a field without a default blocks `Default`
— so a `T: Trait` obligation there is real, and gating avoids code-size waste
on types that never use the trait.

The format traits (`Inspect` / `InspectAlt` / `Display` / `DisplayAlt`) are
_total_ `on_bound`: every type is structurally formattable, so a `T: Inspect`
/ `T: Display` obligation always holds (for a type parameter or any concrete
type), and `impl Inspect for T;` is a conformance check that always validates.
Because they are total and universal debug output is a feature rather than
waste, their _generation_ stays eager (a body for every type kind, as under
the previous automatic policy) — gating it would demand a discovery mechanism
for `{v:?}` over an unbounded type param (whose concrete reference only
materializes at monomorphize) with no offsetting code-size benefit. Their
move to `on_bound` is therefore in the obligation and marker semantics, not
the generation schedule. A policy declaration for user-defined traits is
open. See Open Questions.

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

| Policy              | A `T: Trait` obligation is satisfied by …                    | Generation | Examples                                           |
| ------------------- | ------------------------------------------------------------ | ---------- | -------------------------------------------------- |
| `on_bound` (demand) | structural synthesis, on demand when a reference requires it | on demand  | `Eq`, `Ord`, `Default`, `Serialize`, `Deserialize` |
| `on_bound` (total)  | always — the trait is total over all types                   | eager      | `Inspect`, `InspectAlt`, `Display`, `DisplayAlt`   |
| `explicit`          | only a written `impl Trait for T;` (or full manual impl)     | on demand  | default for user traits                            |

There is no longer an `automatic` policy: its members split by totality.
`Default` joined the demand `on_bound` traits (`Eq` / `Ord` / serde) — it is
not total (a field without a default blocks it), so gating its generation
saves code on structs never defaulted. The format traits stayed eager but
gained `on_bound` obligation semantics (see below). `Display` / `DisplayAlt`
synthesize a fallback that delegates to `Inspect` / `InspectAlt`.

The format traits are _total_: every type is structurally formattable (the
former automatic policy already generated an `Inspect` body for every type
kind — struct, enum, variant, flags, newtype, tuple, closure, opaque
resource). Totality is now a type-system fact — a `T: Inspect` / `T: Display`
(and `Alt`) obligation always holds, for a type parameter or any concrete
type — so those bounds compile where the old policy rejected them, and an
`impl Inspect for T;` marker always validates. Generation stays eager: a total
trait wastes nothing meaningful by existing for every type (universal debug
output is the point), and gating it would need a discovery mechanism for
`{v:?}` over an unbounded type param — whose concrete `T^Inspect` reference
only materializes at monomorphize, after synthesis — for no code-size gain.
The other four traits are _not_ total (a `fn`-typed field, a field without a
default, blocks them), so a `T: Trait` there is a real obligation and gating
its generation removes genuine waste.

- A hand-written `impl Trait for T { … }` always wins.
- An `impl Trait for T;` marker is a conformance check that validates `T`
  structurally at its own span and records a bound-driven request — the same
  effect a `T: Trait` bound has, except a marker is a hard compile error if
  `T` is ineligible (a bound is merely unsatisfied elsewhere). For the
  structurally-checkable traits (`Eq` / `Ord` / `Default` / serde) that hard
  error fires when any field/case is ineligible (`Default` additionally
  requires every field to carry a default expression). A format-trait marker
  always validates — every nominal type is structurally formattable — so it
  serves as an intent/documentation annotation. Under `explicit` a marker is
  the only way to make `T` eligible at all; under `on_bound` it is redundant
  with the structural rule but still a useful declaration of intent.
- For the demand `on_bound` traits, the trigger for a body is usage: a bound
  check, marker, or operator/call reference records the request. See
  [Discovery Mechanism](#discovery-mechanism). The total format traits skip
  discovery — their bodies are always generated.
- `on_bound` and `explicit` differ on one axis: whether eligibility is
  discovered by an unprompted structural scan (`on_bound`) or only via an
  explicit marker (`explicit`).

### Bound-driven synthesis semantics

An `on_bound` obligation `T: Trait` is satisfied structurally: no manual impl
exists, and every field/case of `T` satisfies `Trait` recursively (`Default`
instead requires every field to carry a default expression). On failure, the
error reason-chains from the bound site to the offending field/case
([Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)).

A marker for any of the structurally-checkable traits (`Eq` / `Ord` /
`Default` / `Serialize` / `Deserialize`) validates `T` at its own span and is
a hard compile error if ineligible, then records the request exactly as a bare
bound does.

Whole-program and monomorphized, so there's no orphan rule to violate.
Generic types record nominally against the base declaration — the many
instantiations collapse onto one request, and synthesis emits a generic
template that monomorphize instantiates per concrete type. `Serialize` /
`Deserialize` templates are additionally generic over the _serializer_ type
`S` / `D` (the `Deserialize` `FieldSchema` keying keeps the `next_field`
selector on the base type — see [Serde](./wep-2026-02-28-serde.md)).

### Discovery mechanism

Discovery applies to the _demand_ `on_bound` traits (`Eq` / `Ord` / `Default`
/ serde) only; the total format traits generate unconditionally and need none.
A demand obligation is satisfiable only if the reference that needs it can be
_found_, and discovery funnels into one shared set,
`TypeTable::bound_driven_synth_requests`: the pre-monomorphize synthesis pass
reads it and emits a body (concrete or generic template) for each recorded
`(type, trait)` pair, gated so nothing is generated for a pair no reference
recorded.

Each demand reference records at its own resolution site: `Eq` / `Ord` at
operator dispatch and `==` / `<` method lowering (`type_implements_trait`
records while it recurses through fields, so a struct and every field it
reaches are recorded together); `Default` at a `T: Default` bound check or a
`P::default()` static-call resolution; serde at a `T: Serialize` /
`T: Deserialize` bound. None of these has an unbounded reference path — a
value is compared, defaulted, or serialized only through a bound or a concrete
call the resolver sees — so per-site recording is complete.

A total format trait has no such gate: `type_implements_trait` short-circuits
`true` for it (totality), and generation is eager, so `{v:?}` over an
unbounded type param — whose concrete `T^Inspect` reference only appears after
monomorphize substitutes `T` — always finds its body already emitted. This is
exactly the case that made gating the format traits unattractive, and eager
generation sidesteps it.

An explicit marker feeds the demand request set: it validates structurally at
its own span (hard error if ineligible) and records the request like a bound.
A format-trait marker validates but records nothing meaningful (generation is
already eager). This is scoped to compiler-synthesized bodies; a hand-written
`impl Trait for T { … }` is ordinary source, type-checked because it exists and
left to ordinary dead-code elimination.

### Policy assignment

- `Inspect` / `InspectAlt` / `Display` / `DisplayAlt` / `Default` move from
  `automatic` to `on_bound`: a body is generated only for a `(type, trait)`
  pair some reference actually needs.
- `Inspect` / `InspectAlt` / `Display` / `DisplayAlt` gain total `on_bound`
  obligation and marker semantics (a `T: <format>` bound always holds; a
  marker always validates) but keep eager generation.
- `Eq` / `Ord` / `Serialize` / `Deserialize` were already `on_bound`; no
  change.
- User-defined traits default to `explicit`; opting into `on_bound` is an open
  question (see below).

### The trust boundary

`Serialize` / `Deserialize` cross a wire/storage boundary, so `on_bound`
means a type becomes serializable the moment some code asks, and a later
field addition silently extends the wire shape — why Rust `serde` and Swift
`Codable` are opt-in. Wado accepts the trade-off: its whole-program model has
no downstream consumers to surprise. A manual impl or field-level `#[secret]`
remain the levers for tighter control; no dedicated opt-out is introduced.

`Eq` / `Ord` cross no data boundary — `on_bound` only changes _when_ their
impl is generated, never what any `==` / `<` call site returns. Their
motivation is pure compile-time / code size, with no opt-out to weigh.

## Consequences

### Benefits

- One uniform model replaces the ad-hoc split: every derivable trait is
  `on_bound`, discovered through one shared request set.
- Removes serde's per-type marker boilerplate and unblocks anonymous-struct
  serialization.
- Removes compile-time and code-size waste on unused impls — `Eq` / `Ord` for
  types never compared, and now `Default` for types never defaulted (`Default`
  is not total, so its waste is real).
- `T: Inspect` / `T: Display` (and `Alt`) bounds now hold for every type, which
  the old automatic policy rejected at bound-check for plain aggregates, and
  `impl Inspect for T;` markers are accepted.
- No macros, no dynamic reflection; synthesis stays static and monomorphized.

### Trade-offs

- `Serialize` / `Deserialize` crossing to `on_bound` weakens the opt-in that
  bounds the wire surface today; `#[secret]` and a manual impl are the only
  countermeasures.
- Errors move from the (absent) impl site to the bound site; reason chains
  keep them legible.
- A future `Reflect`-based rewrite of the synthesized body must not let a
  blanket `impl<T: Reflect> Trait for T` conflict with concrete impls — an
  open coherence question the current mechanism doesn't hit yet, since it
  instantiates the existing per-type synthesizer directly.
- No on_bound impl exists "for free" from a mere declaration without a bound,
  marker, or reference; an unmarked type intended for future use with zero
  current call sites gets no code until something references it. An explicit
  marker both guarantees a hard validation error if `T` is ineligible
  (`Eq` / `Ord` / `Default` / serde) and records a request, so a marked type
  does get its body.
- The format traits' totality is now a type-system commitment: "every type is
  formattable." This matches the pre-existing automatic policy (which generated
  `Inspect` for every type), but removes the freedom to introduce a genuinely
  non-formattable type later without revisiting the totality short-circuit.
- Format generation stays eager, so this WEP does not reduce format code size —
  a deliberate trade (see Alternatives): gating a total trait buys little and
  costs a discovery mechanism. `Default` gating uses a finite set of recording
  sites (bound check, `P::default()` resolution, marker); a missed site fails
  loud at monomorphize / link rather than miscompiling.

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
- **Gate format generation on demand, like `Eq` / `Ord`.** Would save the
  code of `Inspect` / `Display` bodies for types never formatted. But the
  format traits are total, and `{v:?}` over an unbounded type param has no
  bound to record and no concrete reference until monomorphize — closing that
  gap needs either a post-monomorphize discovery sweep (reimplementing the
  per-type synthesizers against concrete `FlatPackage` data, a whole pass) or
  implicit format bounds on every type parameter (which over-records for every
  type used generically, erasing most of the saving). Neither buys enough
  against a total trait whose universal availability is a feature. Rejected in
  favor of eager generation plus total obligation semantics.
- **Implicit bounds for `Eq` / `Ord` / `Default`.** Would let those ride the
  same bound-check recording, but they are not total (a `fn`-typed field blocks
  `Eq`; a field without a default blocks `Default`), so an implicit bound would
  reject ordinary generic code over ineligible types. Rejected.

## Open Questions

- Declaration syntax for a user-defined trait's policy (opting a user trait
  into `on_bound`).
- Whether the format traits should eventually gate generation after all (via a
  post-monomorphize discovery pass), should code size on debug output ever
  matter enough to justify the machinery.
- Coherence interaction with concrete impls, relevant once a `Reflect`-based
  rewrite of the synthesized body lands.
