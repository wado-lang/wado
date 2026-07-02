# Trait Derivation Policy — Bound-Driven Synthesis

Status: Implemented. Every derivable compiler trait — `Eq` / `Ord` /
`Default` / `Inspect` / `InspectAlt` / `Display` / `DisplayAlt` — is
`on_bound`: a body is synthesized only where a call site actually references
the trait method. `Serialize` / `Deserialize` are `on_bound` too but keep
their own bound-recorded channel (see below). Discovery is a single
post-monomorphize lazy sweep (`synthesis::lazy_traits`): after every generic
type argument is concrete, it scans function bodies for referenced-but-unsynthesized
trait-method targets and materializes each concrete body to fixpoint. This
replaces the earlier per-call-site request recording, and closes the gap it
left — a reference no annotate-time eligibility check happened to see
(unbounded generic template formatting, `{v:?}` over a type param) is now
discovered structurally at the point every reference is concrete. A policy
declaration for user-defined traits is open. See Open Questions.

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
`Ord`. The format traits force the point the WEP always intended — a
reference, not a bound, triggers synthesis — because `{v:?}` over an unbounded
type param has no bound to record, so nothing short of usage-based discovery
is correct. `Display` / `DisplayAlt` synthesize a fallback that delegates to
`Inspect` / `InspectAlt`.

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

Whole-program and monomorphized, so there's no orphan rule to violate. For
the sweep-driven traits, a generic type's per-instantiation references are all
concrete by the time the sweep runs, so each concrete `(type, trait)` pair is
synthesized directly — no shared template. `Serialize` / `Deserialize` still
emit a generic template that monomorphize instantiates per concrete serializer
(the `Deserialize` `FieldSchema` keying keeps the `next_field` selector on the
base type — see [Serde](./wep-2026-02-28-serde.md)).

### Discovery mechanism

A structural obligation is only satisfiable if the reference that needs it can
be _found_. Recording eligibility as a side effect of each resolution path
that happens to check it (operator dispatch, `.method()` resolution, bound
checks) is fragile: every path must remember to record, and a path that
doesn't silently skips generation. The format traits make this unworkable
outright — `{v:?}` over an unbounded type param has no bound to record, and
the concrete `T^Inspect` reference only materializes when monomorphize
substitutes `T`, after any annotate-time recording has run.

The mechanism is a single post-monomorphize lazy sweep (`synthesis::lazy_traits`).
Once every generic type argument is concrete, it walks all function bodies for
`Call` / `MethodCall` targets whose `method_info` names an `on_bound` trait
method (`Eq::eq`, `Ord::cmp`, `Default::default`, `Inspect::inspect`,
`Display::fmt`, and the `Alt` siblings) that no function defines yet, and
synthesizes each concrete body — reusing the same per-type generators, now
driven by the reference instead of an eager scan. Synthesizing an `Inspect`
body emits `field.inspect(f)` calls, so the sweep runs to fixpoint: each new
body's references feed the next round. Because it runs where _every_ reference
is concrete and enumerable, no resolution path needs derivation-specific
awareness, and the historical "a path forgot to record" gap cannot recur — a
referenced-but-unsynthesized target is discovered by construction.

The pre-monomorphize synthesis pass keeps only the generic-template
generators the monomorphizer needs to instantiate (generic user structs /
variants and the blanket container impls); it no longer eagerly emits concrete
bodies.

`Serialize` / `Deserialize` are equally usage-based — no `T: Serialize` use,
no impl — but keep their bound-recorded pre-monomorphize channel
(`serde_synth`), for a structural reason: `serialize` / `deserialize` are
generic over the _serializer_ type `S` / `D`, not just the value type. Their
body must therefore exist as a template _before_ monomorphize, so the one mono
pass instantiates `S` / `D` per concrete serializer; the post-mono sweep runs
too late to drive that. Serde also has no unbounded-reference gap for the
sweep to close — a value is only ever serialized through a `T: Serialize`
bound (anonymous structs included), so bound recording captures every use.
The value-only-generic traits (`Eq` / `Ord` / `Default` / format) have no
serializer parameter, fully concretize at monomorphize, and _do_ have the
unbounded-reference path — so they, and only they, ride the sweep.

An explicit marker validates structurally at its own span (hard error if
ineligible) but records no reference and so generates nothing on its own — see
[Consequences](#consequences). This is scoped to compiler-synthesized bodies;
a hand-written `impl Trait for T { … }` is ordinary source, type-checked
because it exists and left to ordinary dead-code elimination.

### Policy assignment

- `Inspect` / `InspectAlt` / `Display` / `DisplayAlt` / `Default` move from
  `automatic` to `on_bound`: a body is generated only for a `(type, trait)`
  pair some reference actually needs.
- `Eq` / `Ord` were already `on_bound`; they now share the post-mono sweep
  instead of per-call-site recording.
- `Serialize` / `Deserialize` are `on_bound` via their bound-recorded channel.
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

- One uniform model replaces the ad-hoc split; a single sweep is the discovery
  point, so no resolution path can silently skip generation.
- Removes serde's per-type marker boilerplate and unblocks anonymous-struct
  serialization.
- Removes compile-time and code-size waste on unused impls — `Eq` / `Ord` for
  types never compared, and now `Inspect` / `Display` / `Default` for types
  never formatted or defaulted (previously synthesized for every type).
- `T: Inspect` / `T: Display` bounds now hold structurally for plain aggregate
  types, which the old automatic policy rejected at bound-check.
- No macros, no dynamic reflection; synthesis stays static and monomorphized.

### Trade-offs

- `Serialize` / `Deserialize` crossing to `on_bound` weakens the opt-in that
  bounds the wire surface today; `#[hidden]` and a manual impl are the only
  countermeasures.
- Errors move from the (absent) impl site to the bound site; reason chains
  keep them legible.
- A future `Reflect`-based rewrite of the synthesized body must not let a
  blanket `impl<T: Reflect> Trait for T` conflict with concrete impls — an
  open coherence question the sweep doesn't hit yet, since it instantiates the
  existing per-type synthesizer directly.
- No on_bound impl exists "for free" without a reference; an explicit marker
  guarantees a hard validation error at declaration if `T` is ineligible
  (`Eq` / `Ord` / `Default` / serde), but no longer guarantees a body is
  generated in advance. A type intended for future use with zero current call
  sites gets no code until something references it.
- The sweep is an extra post-monomorphize walk over all function bodies, run
  to fixpoint. Its cost is bounded by the set of distinct concrete
  `(type, trait)` references actually present — the same impls the old
  automatic policy generated eagerly and more — so it trades eager work for
  on-demand work rather than adding net synthesis.

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
  checking it (the interim mechanism).** The first `Eq` / `Ord` / serde
  implementation: works, but every resolution path must remember to opt in,
  and nothing enforces that a future path does. Fatal for the format traits —
  `{v:?}` over an unbounded type param has no path to record at, since the
  concrete reference only appears at monomorphize. Superseded by the sweep.
- **Extend per-call-site recording to the format traits** (record an `Inspect`
  request at template desugaring, `assert` capture, direct `.inspect()`, …).
  Handles the concrete cases but still misses the monomorphize-substituted
  ones and re-introduces the "every path must remember" fragility across even
  more sites. Rejected in favor of the single post-mono sweep.
- **Placeholder entries in `impl_index` at `TraitEnv::build()`** (an earlier
  draft of this WEP). Precompute eligibility from AST-level field types and
  insert body-less placeholders that resolution paths find through their
  existing lookup. Sound, but requires a correct pre-Annotate structural scan
  (generic fields, cross-module recursion, newtypes of newtypes) and a proven
  single lowering funnel. The post-mono sweep achieves the same "no path can
  skip generation" guarantee without either, by running where all references
  are already concrete. Rejected as more machinery for the same guarantee.

## Open Questions

- Declaration syntax for a user-defined trait's policy (opting a user trait
  into `on_bound`).
- Whether `Serialize` / `Deserialize` should eventually fold onto the sweep
  too. Blocked on their genericity over the serializer type `S` / `D`, which
  forces pre-monomorphize template emission; unifying would cost a second
  serializer-monomorphize pass for no correctness gain, so it stays deferred.
- Coherence interaction with concrete impls, relevant once a `Reflect`-based
  rewrite of the synthesized body lands.
