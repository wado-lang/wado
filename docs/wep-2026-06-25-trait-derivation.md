# Trait Derivation Policy — Bound-Driven Synthesis

Status: Implemented for `Serialize` / `Deserialize` / `Eq` / `Ord` over
struct, variant, enum, and flags types, including anonymous structs (`Eq` /
`Ord` are struct/variant/enum only — flags erase to `u32` before dispatch and
need no synthesized impl of their own). Does not yet extend to
`GenericInstance` (user-defined generic struct/variant instantiations) for
`Serialize` / `Deserialize` specifically — `Eq` / `Ord` already handled
generics before this WEP and are unaffected — nor to a general per-trait
policy declaration for user-defined traits. See Consequences and Open
Questions.

## Context

Wado derives type-directed traits for user types, but with two inconsistent
**request policies** — the rule for _when_ a derived impl comes into existence
for a type `T`:

- Automatic, no request: `Inspect` / `InspectAlt` are synthesized for every type;
  `Display` / `DisplayAlt` fall back to them; `Eq` / `Ord` are derived when all
  fields qualify; `Default` is derived when all fields have defaults. The user
  writes nothing — and, for `Eq` / `Ord` / `Default`, the compiler synthesizes
  an impl for _every_ declared type unconditionally, whether or not the
  program ever uses it.
- Explicit request: `Serialize` / `Deserialize` exist for a type only if the user
  writes the empty marker `impl Serialize for T;`. A bare `T: Serialize` bound
  does not bring the impl into being.

This split is ad hoc on two axes. Serde forces a marker line per type — `impl
Serialize for Foo;` — purely to satisfy bounds the compiler could discharge
structurally, exactly as it already does for `Inspect`; `Inspect` / `Display`
are effectively satisfied on demand, and serde was the outlier that still
needed a manual marker. Separately, `Eq` / `Ord` synthesize for every declared
type regardless of use, unlike serde's per-request generation — for a large
program most of whose types are never compared, that is pure compile-time and
code-size waste with no compensating benefit (unlike `Inspect`, which exists
specifically so reflective tooling and `{x:?}` always work everywhere).

This is distinct from, and composes with,
[Library-Defined Derivation (`Reflect`)](./wep-2026-06-13-reflect-derivation.md),
which decides the _mechanism_ — every derivation becomes a generic library `impl`
over a compiler-synthesized `Reflect`. That WEP answers "how is the impl written";
this WEP answers "when is it instantiated for a given `T`". The two are
orthogonal: once serde is a generic impl over `Reflect`, the only remaining
question is whether a `T: Serialize` obligation may instantiate it automatically.

### Forcing functions

- Ergonomics: structured logging wants `field(k, anyValue)` and structured
  results to work without per-type marker boilerplate.
- Anonymous structs: the efficient field path in
  [`core:log`](./wep-2026-06-25-core-log.md) passes `{ user_id, ip }` as an
  anonymous struct. An anonymous struct has no name, so `impl Serialize for …`
  is unwritable. Bound-driven synthesis is the _only_ way such a value can
  satisfy `T: Serialize`. This policy is therefore a hard prerequisite for that
  path, not merely a convenience.
- Compile time / code size: `Eq` and `Ord` synthesize for every declared
  struct, enum, and variant today, regardless of whether the program ever
  compares one. A large program with many incomparable types pays that cost
  for nothing; moving to the same demand-driven model as serde removes it
  without changing a single existing `==` / `<` call site (see Bound-driven
  synthesis semantics).

## Decision

### A per-trait derivation policy

Introduce an explicit, named axis — the **derivation policy** — that a derivable
trait declares. It governs only _request semantics_; the derivation body is the
generic `Reflect`-based impl from the Reflect WEP.

| Policy      | A `T: Trait` obligation is satisfied for a derivable `T` by … | Examples (proposed)                                         |
| ----------- | ------------------------------------------------------------- | ----------------------------------------------------------- |
| `automatic` | structural synthesis, always; the impl is always present      | `Inspect`, `InspectAlt`, `Display`, `DisplayAlt`, `Default` |
| `on_bound`  | structural synthesis, on demand when the bound requires it    | `Serialize`, `Deserialize`, `Eq`, `Ord` (the change)        |
| `explicit`  | only a written `impl Trait for T;` (or full manual impl)      | (default for user traits)                                   |

Across all policies:

- A hand-written `impl Trait for T { … }` always wins (the existing override
  rule), and customization markers (`#[serde(rename_all)]`, …) attach to it.
- The explicit marker `impl Trait for T;` remains valid for every policy. Under
  `on_bound` it is no longer _required_, but it is still useful to force an impl
  into existence where no bound would (a component `export` boundary, pinning
  coherence, or documentation intent) — and, for `Eq` / `Ord` specifically, it
  is a hard **guarantee**: the marker is a compile error if any field/case is
  not itself eligible, unlike a bound (which simply is not satisfied) or the
  bare structural rule (which has nothing to reject — it only ever answers a
  question someone asked).
- `automatic` and `on_bound` differ only in eagerness: `automatic` impls are
  always available (so reflective tooling and `{x:?}` always work); `on_bound`
  impls materialize only where a bound demands one, so a type never silently
  acquires the capability unless something asks for it. For `Eq` / `Ord` this
  is invisible at the bound-check level — `T: Eq` was already satisfied the
  moment fields qualified, still is, and every `==` / `<` call site is
  unchanged — the eagerness difference is purely in whether
  `synthesis::traits` emits a body for a type nothing ever asked about.

### Bound-driven synthesis semantics

For a trait with `on_bound` policy, an obligation `T: Trait` is discharged as if
a blanket `impl<T> Trait for T where T: Reflect { … }` existed (the generic body
from the Reflect WEP), subject to:

1. No applicable manual impl exists (else that wins).
2. `T` is structurally derivable for `Trait` — every field/case type itself
   satisfies `Trait` (recursively, via the same rule).
3. On failure, the error reason-chains from the bound site to the offending
   field/case, via
   [Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md), e.g.
   "`Event: Serialize` requires `Metadata: Serialize` requires `Badge: Serialize`
   — `Badge` is not serializable and has no `impl`."

Shipped mechanism (deviates from the `Reflect`-blanket sketch above, since
`Reflect` itself remains unbuilt — see Relationship and prerequisites): the
elaborator's structural trait-bound check — which already existed for `Eq` /
`Ord` pre-WEP, and gained a parallel branch for `Serialize` / `Deserialize` —
now also _records_ the `(type_name, module, trait_name)` triple on the type
table (shared by every module, since elaboration runs one pass per module and
finishes before synthesis can act on the fact) whenever it finds a structural
match. `synthesis::serde_synth` reads the `Serialize` / `Deserialize` entries
and synthesizes each body with the same walk the explicit `impl Trait for T;`
marker already used; `synthesis::traits` reads the `Eq` / `Ord` entries the
same way, replacing its previous unconditional "every declared type" sweep.
Both read a _snapshot_ of the shared set rather than draining it — the two
passes run at different points in the pipeline, and a destructive drain by
whichever runs first would silently discard the other's entries before it
gets a turn. Points 1-3 above hold either way — only "how the body is
written" differs from the sketch, and a future `Reflect`-based rewrite of the
body slots into the same request-recording plumbing unchanged.

The precedence rule (point 1) is enforced differently for the two families,
matching how each already avoided clobbering a hand-written impl before this
WEP: `Serialize` / `Deserialize` check "no impl already exists" _before_
recording, since nothing else would catch a duplicate; `Eq` / `Ord` don't need
that check at the recording site — `synthesis::traits`'s own `has_impl` /
`record_impl` dedup (pre-existing, unchanged) already skips regenerating over
one, so recording redundantly alongside a hand-written impl is harmless. The
explicit-marker path is the one place this distinction is user-visible:
`impl Eq for T;` validates `T` structurally _before_ recording — ignoring
whether the marker itself would otherwise count as "an impl exists" — and is a
hard compile error, with a reason chain, if `T` does not qualify. This is
deliberately stronger than serde's explicit marker today, which does not
pre-validate this way (a pre-existing gap, not introduced by this change; see
Open Questions).

This is whole-program and monomorphized; there is no orphan rule to violate
because there are no separately-compiled crates. Synthesis happens once per
`(trait, type)` actually required, so no dead impls are emitted (consistent with
[Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md)).

`GenericInstance` (a user-defined generic struct/variant instantiation, e.g.
`Wrapper<Foo>`) is out of scope **for `Serialize` / `Deserialize`**:
elaboration only sees the not-yet-monomorphized generic template, so a
request keyed by a concrete instantiation would not resolve to a body — the
same gap [Serde](./wep-2026-02-28-serde.md) already tracks for generic-struct
`Deserialize`. `Eq` / `Ord` are unaffected — their generic synthesis predates this WEP and
already records against the base declaration, not the concrete instantiation.
Built-in generics (`List<T>`, `Option<T>`, …) are unaffected either way — they
carry their own hand-written impls.

### Policy assignment

- `Inspect` / `InspectAlt` / `Default` stay `automatic` — unchanged behavior;
  this WEP only _names_ what they already do.
- `Serialize` / `Deserialize` move from `explicit` to `on_bound`: the marker
  `impl Serialize for T;` becomes optional, and any `T: Serialize` bound
  (including from an anonymous struct) is satisfiable.
- `Eq` / `Ord` move from `automatic` to `on_bound`: an impl is generated only
  for a `(type, trait)` pair some `==` / `<` call site (or bound, or explicit
  marker) actually demands, not for every declared type. No call site changes
  behavior — `T: Eq` was satisfied the moment fields qualified, still is — the
  difference is purely whether `synthesis::traits` emits a body nothing asked
  for.
- User-defined derivable traits default to `explicit`, and may opt into
  `automatic` / `on_bound` (the declaration syntax is an open question below).

### The trust boundary

`Serialize` / `Deserialize` cross a data boundary (wire, storage), so making them
`on_bound` means a type becomes serializable the moment some code asks — and a
later field addition silently extends the wire shape. This is precisely why Rust
`serde` and Swift `Codable` are opt-in, and why `Inspect` (debug-only, low stakes)
being automatic is not a precedent that transfers for free.

Wado accepts this trade-off rather than adding an opt-out marker: its
whole-program model (no published crates, no downstream consumers who could be
surprised by a wire-shape change) materially lowers the risk relative to Rust.
The levers that already exist are the ones available for a type that needs
tighter control — a manual impl always overrides, and field-level `#[hidden]`
(already honored by `Inspect`) excludes a field from the synthesized
serialization. No dedicated opt-out (e.g. a `#[no_derive(...)]` marker) is
introduced.

This section is specific to `Serialize` / `Deserialize`. `Eq` / `Ord` cross no
data boundary, so moving them to `on_bound` raises no analogous question: it
changes only _when_ their impl is generated, never what any `==` / `<` call
site returns. Their motivation is pure compile-time / code size (see Forcing
functions), with no opt-out to weigh.

## Consequences

### Benefits

- One uniform model replaces the current ad-hoc split; each derivable trait has a
  declared, legible policy.
- Removes serde's per-type marker boilerplate, matching the zero-ceremony
  experience of `Inspect`.
- Unblocks anonymous-struct serialization, a prerequisite for the efficient field
  path in [`core:log`](./wep-2026-06-25-core-log.md).
- Removes `Eq` / `Ord` compile-time and code-size waste on types the program
  never compares — synthesis now happens only for a `(type, trait)` pair a
  real call site, bound, or marker actually demands, instead of for every
  declared struct, enum, and variant.
- No macros, no dynamic reflection; synthesis stays static and monomorphized.

### Trade-offs

- `Serialize` / `Deserialize` crossing to `on_bound` weakens the explicit opt-in
  that today bounds the wire surface; `#[hidden]` and a manual impl override are
  the only countermeasures, and they are weaker than opt-in.
- Errors move from the (absent) impl site to the bound site; reason chains are
  what keep them legible.
- Coherence: a _future_ `Reflect`-based rewrite of the synthesized body (see
  below) must not let a blanket `impl<T: Reflect> Trait for T` conflict with
  concrete impls (e.g. a primitive's own `impl Serialize`) — that rides on the
  coherence rules still open in the Reflect / variadic WEPs. The mechanism
  actually shipped does not introduce this question yet: it instantiates the
  existing per-type synthesizer directly, the same one the explicit marker
  already used, so "no applicable manual impl exists (else that wins)" is the
  only precedence rule in play today.
- `Eq` / `Ord` no longer exist "for free" on a type with no direct comparison
  use site. A type intended for future comparison, or reached only through a
  not-yet-written generic bound, needs the explicit marker to guarantee the
  impl in advance — previously every structurally-eligible type carried one
  unconditionally, whether the program used it or not.

### Relationship and prerequisites

- Mechanism: shipped directly against the existing bespoke synthesizers —
  `synthesis::serde_synth` for `Serialize` / `Deserialize` (the same one
  `impl Serialize for T;` already used) and `synthesis::traits` for `Eq` /
  `Ord` (the same one the pre-WEP unconditional sweep already used) — **not**
  against [`Reflect` derivation](./wep-2026-06-13-reflect-derivation.md),
  which remains unbuilt. The original plan was to land this after migrating
  serde onto a generic `Reflect`-based impl (§5 of that WEP), but the two
  turned out to be independent: this WEP only changes _when_ a request for
  the existing synthesizer is created (a bound match, not just a written
  marker), not _how_ the body is written. A future `Reflect`-based rewrite of
  the body is still free to land later against the same request-recording
  plumbing.
- Diagnostics: [reason chains](./wep-2026-06-02-diagnostic-reason-chains.md) for
  the field-level failure path.

## Alternatives Considered

### Keep `Serialize` explicit (status quo)

Lowest risk, strongest wire-surface discipline, but inconsistent with the
automatic traits, boilerplate-heavy, and — decisively — makes anonymous-struct
serialization impossible (no name to mark). Rejected as the long-term model.

### Make every derivable trait `automatic`

Maximum ergonomics, but erases the trust-boundary distinction entirely; a private
type silently becomes wire-serializable with no opt-out point. Rejected; the
policy axis exists precisely to treat `Inspect` and `Serialize` differently.

### A `#[derive(Serialize)]` attribute

The Rust/Go-macro shape. Rejected: Wado has no derive macros, and the Reflect
mechanism already makes derivation a library impl — a triggering attribute would
duplicate what an `on_bound` policy expresses without one, and `impl Serialize
for T;` already serves as the explicit-request form when one is wanted.

### Keep `Eq` / `Ord` automatic (status quo)

Simplest, and free of any request bookkeeping, but pays synthesis cost
(compile time, code size) for every declared type regardless of use — the
exact waste the Forcing functions section calls out. Rejected: `on_bound`
removes the waste with no change to any existing `==` / `<` call site, so
there is no offsetting benefit to keeping `automatic`.

## Open Questions

- Declaration syntax for a trait's policy (an attribute on the trait? a keyword?
  a property of being `Reflect`-derivable?) — still open for user-defined
  traits, which default to `explicit` and have no way to opt into
  `automatic` / `on_bound` yet.
- `GenericInstance` (a user-defined generic struct/variant instantiation, e.g.
  `Wrapper<Foo>`) is not yet `on_bound`-eligible for `Serialize` /
  `Deserialize` specifically — see Bound-driven synthesis semantics. (`Eq` /
  `Ord` are unaffected; their generic synthesis predates this WEP.)
- `Serialize` / `Deserialize`'s explicit marker does not pre-validate
  structurally the way `Eq` / `Ord`'s now does (see Bound-driven synthesis
  semantics): `impl Serialize for T;` for a `T` with an ineligible field is
  not a compile error at the marker site, unlike `impl Eq for T;`. Whether to
  close this gap — making every explicit marker an equally hard guarantee —
  is left open.
- Coherence interaction with concrete impls, inherited from the Reflect /
  variadic coherence items — relevant once a `Reflect`-based rewrite of the
  synthesized body lands (see Trade-offs); the shipped mechanism does not run
  into it.
