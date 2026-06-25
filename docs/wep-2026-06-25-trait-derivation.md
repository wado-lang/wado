# Trait Derivation Policy — Bound-Driven Synthesis

Status: Draft

## Context

Wado derives type-directed traits for user types, but with two inconsistent
**request policies** — the rule for _when_ a derived impl comes into existence
for a type `T`:

- Automatic, no request: `Inspect` / `InspectAlt` are synthesized for every type;
  `Display` / `DisplayAlt` fall back to them; `Eq` / `Ord` are derived when all
  fields qualify; `Default` is derived when all fields have defaults. The user
  writes nothing.
- Explicit request: `Serialize` / `Deserialize` exist for a type only if the user
  writes the empty marker `impl Serialize for T;`. A bare `T: Serialize` bound
  does not bring the impl into being.

This split is ad hoc. The logger PoC (`example/logger_poc.wado`) had to write
four marker lines — `impl Serialize for Level; … for Metadata; … for Field; …
for Event;` — purely to satisfy bounds the compiler could discharge structurally,
exactly as it already does for `Inspect`. The observation that motivates this WEP:
`Inspect` / `Display` are effectively _satisfied on demand_ already; serde is the
outlier that still needs a manual marker.

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

## Decision

### A per-trait derivation policy

Introduce an explicit, named axis — the **derivation policy** — that a derivable
trait declares. It governs only _request semantics_; the derivation body is the
generic `Reflect`-based impl from the Reflect WEP.

| Policy      | A `T: Trait` obligation is satisfied for a derivable `T` by … | Examples (proposed)                             |
| ----------- | ------------------------------------------------------------- | ----------------------------------------------- |
| `automatic` | structural synthesis, always; the impl is always present      | `Inspect`, `InspectAlt`, `Eq`, `Ord`, `Default` |
| `on_bound`  | structural synthesis, on demand when the bound requires it    | `Serialize`, `Deserialize` (the change)         |
| `explicit`  | only a written `impl Trait for T;` (or full manual impl)      | (default for user traits)                       |

Across all policies:

- A hand-written `impl Trait for T { … }` always wins (the existing override
  rule), and customization markers (`#[serde(rename_all)]`, …) attach to it.
- The explicit marker `impl Trait for T;` remains valid for every policy. Under
  `on_bound` it is no longer _required_, but it is still useful to force an impl
  into existence where no bound would (a component `export` boundary, pinning
  coherence, or documentation intent).
- `automatic` and `on_bound` differ only in eagerness: `automatic` impls are
  always available (so reflective tooling and `{x:?}` always work); `on_bound`
  impls materialize only where a bound demands one, so a type never silently
  acquires the capability unless something asks for it.

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

This is whole-program and monomorphized; there is no orphan rule to violate
because there are no separately-compiled crates. Synthesis happens once per
`(trait, type)` actually required, so no dead impls are emitted (consistent with
[Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md)).

### Policy assignment

- `Inspect` / `InspectAlt` / `Eq` / `Ord` / `Default` stay `automatic` —
  unchanged behavior; this WEP only _names_ what they already do.
- `Serialize` / `Deserialize` move from `explicit` to `on_bound`. This is the
  substantive change: the marker `impl Serialize for T;` becomes optional, and
  any `T: Serialize` bound (including from an anonymous struct) is satisfiable.
- User-defined derivable traits default to `explicit`, and may opt into
  `automatic` / `on_bound` (the declaration syntax is an open question below).

### The trust-boundary opt-out

`Serialize` / `Deserialize` cross a data boundary (wire, storage), so making them
`on_bound` means a type becomes serializable the moment some code asks — and a
later field addition silently extends the wire shape. This is precisely why Rust
`serde` and Swift `Codable` are opt-in, and why `Inspect` (debug-only, low stakes)
being automatic is not a precedent that transfers for free.

Mitigations keep `on_bound` safe:

- A type-level opt-out (e.g. `#[no_derive(Serialize, Deserialize)]`) makes a
  `T: Serialize` bound fail with a clear "opted out" message instead of
  synthesizing — for types that must never cross the boundary.
- Field-level `#[hidden]` (already honored by `Inspect`) excludes a field from
  the synthesized serialization, the existing redaction lever.
- A manual impl always overrides, for full control.

Wado's whole-program model (no published crates, no downstream consumers who
could be surprised by a wire-shape change) materially lowers the risk relative to
Rust. The recommendation is `on_bound` with the opt-out; whether the opt-out
should instead be opt-_in_ for `Deserialize` (the untrusted-input direction,
which also carries `#[validate]` enforcement) is the main open decision.

## Consequences

### Benefits

- One uniform model replaces the current ad-hoc split; each derivable trait has a
  declared, legible policy.
- Removes per-type marker boilerplate for serde (the PoC's four `impl … for …;`
  lines vanish), matching the zero-ceremony experience of `Inspect`.
- Unblocks anonymous-struct serialization, a prerequisite for the efficient field
  path in [`core:log`](./wep-2026-06-25-core-log.md).
- No macros, no dynamic reflection; synthesis stays static and monomorphized,
  reusing the Reflect mechanism.

### Trade-offs

- `Serialize` / `Deserialize` crossing to `on_bound` weakens the explicit opt-in
  that today bounds the wire surface; the opt-out and `#[hidden]` are the
  countermeasures, and they are weaker than opt-in.
- Errors move from the (absent) impl site to the bound site; reason chains are
  what keep them legible.
- Coherence: a policy-driven blanket synthesis must not conflict with concrete
  impls (e.g. a primitive's own `impl Serialize`). This rides on the coherence
  rules still open in the Reflect / variadic WEPs (concrete-impl-wins,
  no-overlap) and does not add a new coherence regime of its own.

### Relationship and prerequisites

- Mechanism: [`Reflect` derivation](./wep-2026-06-13-reflect-derivation.md) §1–§3
  (Reflect synthesis + metadata) and §5 (serde migrated to a generic impl over
  `Reflect`). Bound-driven synthesis is "instantiate that generic impl at the
  bound site," so it is a thin policy layer on top and should land after the
  serde-over-Reflect migration.
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

## Open Questions

- Declaration syntax for a trait's policy (an attribute on the trait? a keyword?
  a property of being `Reflect`-derivable?).
- `Deserialize` direction: `on_bound` with opt-out, or opt-in, given it ingests
  untrusted data and carries `#[validate]` enforcement.
- Opt-out spelling (`#[no_derive(...)]` vs other) and whether it is per-trait.
- Coherence interaction with concrete impls, inherited from the Reflect /
  variadic coherence items.
  </content>
