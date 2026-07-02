# Trait Derivation Policy — Bound-Driven Synthesis

Status: Implemented. Every derivable compiler trait — `Eq` / `Ord` /
`Default` / `Inspect` / `InspectAlt` / `Display` / `DisplayAlt` /
`Serialize` / `Deserialize` — is `on_bound`: a body is synthesized only
where a reference actually needs it, discovered through the shared
`bound_driven_synth_requests` channel a bound check or explicit marker
records into. The format traits (`Inspect` / `InspectAlt` / `Display` /
`DisplayAlt`) are _total_ — every type is structurally formattable — so every
type parameter carries them as implicit bounds; this routes formatting of a
generic value through the same bound-check recording as `Eq` / `Ord`,
covering `{v:?}` over a type param without a written bound. References with no
type parameter in play (a `{p:?}` template or `p.inspect(f)` on a concrete
`p`, a `P::default()` call, an `assert` capture) record at their own
resolution site. A policy declaration for user-defined traits is open. See
Open Questions.

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

| Policy     | A `T: Trait` obligation is satisfied by …                    | Examples                                                                                             |
| ---------- | ------------------------------------------------------------ | ---------------------------------------------------------------------------------------------------- |
| `on_bound` | structural synthesis, on demand when a reference requires it | `Eq`, `Ord`, `Default`, `Inspect`, `InspectAlt`, `Display`, `DisplayAlt`, `Serialize`, `Deserialize` |
| `explicit` | only a written `impl Trait for T;` (or full manual impl)     | default for user traits                                                                              |

There is no longer an `automatic` policy: `Inspect` / `InspectAlt` /
`Display` / `DisplayAlt` / `Default` moved onto `on_bound` alongside `Eq` /
`Ord`. `Display` / `DisplayAlt` synthesize a fallback that delegates to
`Inspect` / `InspectAlt`.

The format traits differ from the rest in being _total_: every type is
structurally formattable (the pre-existing `automatic` policy already
generated an `Inspect` body for every type kind — struct, enum, variant,
flags, newtype, tuple, closure, opaque resource). This WEP makes that totality
a type-system fact: every type parameter carries `Inspect` / `InspectAlt` /
`Display` / `DisplayAlt` as implicit bounds, always satisfiable, never
rejecting. The payoff is discovery: formatting a generic value now flows
through an ordinary bound check that records the request, so `{v:?}` over a
type param needs no special path. `Eq` / `Ord` / `Default` / serde are _not_
total (a `fn`-typed field, a field without a default, blocks them), so they
carry no implicit bound and a `T: Trait` there is a real obligation the caller
must satisfy.

- A hand-written `impl Trait for T { … }` always wins.
- An `impl` declaration — hand-written or the empty marker `impl Trait for
  T;` — is a conformance check, never a synthesis trigger. It asserts (and,
  for the marker, validates immediately) that `T` can implement `Trait`; it
  does not by itself cause any code to be generated. The marker `impl Trait
  for T;` stays valid under every policy: under `explicit` it's the only way
  to make `T` eligible at all (the policy runs no structural scan on its
  own); under `on_bound` it's redundant but harmless, since the same
  eligibility already holds structurally. For the structurally-checkable
  traits (`Eq` / `Ord` / `Default` / serde) the marker is also a hard
  guarantee: a compile error at the marker's own span if any field/case is
  ineligible (`Default` additionally requires every field to carry a default
  expression), unlike a bound (simply unsatisfied elsewhere) or the
  structural rule (nothing to reject). A format-trait marker always validates
  — every nominal type is structurally formattable — so it serves as an
  intent/documentation annotation rather than a filter.
- The actual trigger for synthesis — the point where a body gets generated —
  is usage: some call site resolves a reference to the trait method. See
  [Discovery Mechanism](#discovery-mechanism).
- `on_bound` and `explicit` differ on one axis: whether eligibility is
  discovered by an unprompted structural scan (`on_bound`) or only via an
  explicit marker (`explicit`). Both generate a body only on a reference.

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
Generic types record nominally against the base declaration — the many
instantiations collapse onto one request, and synthesis emits a generic
template that monomorphize instantiates per concrete type. `Serialize` /
`Deserialize` templates are additionally generic over the _serializer_ type
`S` / `D` (the `Deserialize` `FieldSchema` keying keeps the `next_field`
selector on the base type — see [Serde](./wep-2026-02-28-serde.md)).

### Discovery mechanism

A structural obligation is only satisfiable if the reference that needs it can
be _found_. Discovery funnels into one shared set,
`TypeTable::bound_driven_synth_requests`: the pre-monomorphize synthesis pass
reads it and emits a body (concrete or generic template) for each recorded
`(type, trait)` pair, gated so nothing is generated for a pair no reference
recorded. What differs per trait is _how the reference is found_.

For the total format traits, the implicit `Inspect` / `InspectAlt` / `Display`
/ `DisplayAlt` bound on every type parameter does the work. Formatting a
generic value type-checks that value against the bound, and the bound check
(`type_implements_trait`) records the request while it recurses through fields
— so at the outermost concrete call, where a type argument first becomes
concrete, the type and every field it structurally reaches are recorded
together. This is the same recursion `Eq` / `Ord` bound checks already use.
Because the bound sits on _every_ type parameter, a concrete type entering
anywhere in a generic call chain is recorded at that boundary, and a generic
container built inside a body (`Box<P>` formatted downstream) rides its own
`impl<T: Inspect> Inspect for Box<T>` bound the same way.

References with no type parameter to carry the bound record at their own
resolution site: a `{p:?}` / `{p}` template interpolation and an `assert`
capture record from the concrete interpolation type (keyed by the format spec
— `Inspect` vs `Display` vs the `Alt` variants); a direct `p.inspect(f)` or
`P::default()` records at method / static-call resolution. `Eq` / `Ord`
records at operator dispatch, `Default` at a `T: Default` bound or a
`P::default()` call, serde at a `T: Serialize` bound — none of these have an
unbounded path, so no implicit bound is needed for them.

A missed direct site is not silent: with generation gated on requests, an
unrecorded reference leaves its target body unsynthesized, and monomorphize /
link then fails loud (`no generic template for …`) rather than miscompiling —
a compiler bug to fix, per the P0-on-suspected-bug rule, not a silent feature
gap.

An explicit marker validates structurally at its own span (hard error if
ineligible) but records no reference and so generates nothing on its own — see
[Consequences](#consequences). This is scoped to compiler-synthesized bodies;
a hand-written `impl Trait for T { … }` is ordinary source, type-checked
because it exists and left to ordinary dead-code elimination.

### Policy assignment

- `Inspect` / `InspectAlt` / `Display` / `DisplayAlt` / `Default` move from
  `automatic` to `on_bound`: a body is generated only for a `(type, trait)`
  pair some reference actually needs. The format traits additionally become
  implicit bounds on every type parameter (they are total).
- `Eq` / `Ord` / `Serialize` / `Deserialize` were already `on_bound`; no
  change.
- User-defined traits default to `explicit`; opting into `on_bound` is an open
  question (see below).

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

- One uniform model replaces the ad-hoc split: every derivable trait is
  `on_bound`, discovered through one shared request set.
- Removes serde's per-type marker boilerplate and unblocks anonymous-struct
  serialization.
- Removes compile-time and code-size waste on unused impls — `Eq` / `Ord` for
  types never compared, and now `Default` for types never defaulted (`Default`
  is not total, so its waste is real). Format traits stay near-total in
  practice (any type used generically is recorded), so their code-size change
  is minor; DCE reclaims the rest.
- `T: Inspect` / `T: Display` bounds now hold for every type — they are
  implicit on every type parameter — which the old automatic policy rejected
  at bound-check for plain aggregates.
- No macros, no dynamic reflection; synthesis stays static and monomorphized.

### Trade-offs

- `Serialize` / `Deserialize` crossing to `on_bound` weakens the opt-in that
  bounds the wire surface today; `#[hidden]` and a manual impl are the only
  countermeasures.
- Errors move from the (absent) impl site to the bound site; reason chains
  keep them legible.
- A future `Reflect`-based rewrite of the synthesized body must not let a
  blanket `impl<T: Reflect> Trait for T` conflict with concrete impls — an
  open coherence question the current mechanism doesn't hit yet, since it
  instantiates the existing per-type synthesizer directly.
- No on_bound impl exists "for free" without a reference; an explicit marker
  guarantees a hard validation error at declaration if `T` is ineligible
  (`Eq` / `Ord` / `Default` / serde), but no longer guarantees a body is
  generated in advance. A type intended for future use with zero current call
  sites gets no code until something references it.
- The format traits become implicit bounds on every type parameter, so the
  language now commits to "every type is formattable." This matches the
  pre-existing automatic policy (which generated `Inspect` for every type), but
  it removes the freedom to introduce a genuinely non-formattable type later
  without revisiting the bound.
- Discovery for the concrete format / `Default` references stays a
  finite set of recording sites (template desugar, `assert`, direct call,
  static `default()`); a missed site fails loud at monomorphize / link rather
  than miscompiling, but it is still per-site rather than a single funnel.

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
- **Keep the format traits `automatic` (status quo).** Simplest, but pays
  synthesis for every declared type, blocks `T: Inspect` / `T: Display` bounds
  from holding, and gives no conformance-marker story. Rejected.
- **A post-monomorphize lazy sweep** (an earlier draft of this WEP). Discover
  every referenced `(type, trait)` after all type arguments are concrete and
  synthesize each concrete body to fixpoint — the single funnel that no
  resolution path can bypass. Robust and precise, but reimplements the
  per-type synthesizers against concrete post-mono `FlatPackage` data and adds
  a whole pass. Rejected because the format traits are total: making them
  implicit bounds routes the generic case through the _existing_ `Eq` / `Ord`
  bound-check recording (with its field recursion) for a fraction of the code,
  and the residual concrete-reference sites are few and fail loud when missed.
- **Implicit bounds for `Eq` / `Ord` / `Default` too.** Would unify discovery
  fully, but those traits are not total (a `fn`-typed field blocks `Eq`; a
  field without a default blocks `Default`), so an implicit bound would reject
  ordinary generic code over ineligible types. Rejected — implicit bounds are
  sound only for the total traits.

## Open Questions

- Declaration syntax for a user-defined trait's policy (opting a user trait
  into `on_bound`).
- Whether the concrete format / `Default` recording sites can eventually
  collapse into one funnel (e.g. a single trait-method-reference lowering hook)
  instead of the current per-site set, removing even the fail-loud residual.
- Coherence interaction with concrete impls, relevant once a `Reflect`-based
  rewrite of the synthesized body lands.
