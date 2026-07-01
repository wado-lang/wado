# Trait Derivation Policy — Bound-Driven Synthesis

Status: Implemented for `Serialize` / `Deserialize` / `Eq` / `Ord` over struct,
variant, enum, and flags types, including anonymous structs (`Eq` / `Ord` are
struct/variant/enum only — flags erase to `u32` and need no impl of their
own). Not yet extended to `GenericInstance` for `Serialize` / `Deserialize`,
or to a policy declaration for user-defined traits. See Open Questions.

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
- The explicit marker `impl Trait for T;` stays valid under every policy —
  optional under `on_bound`, but still useful to force an impl into
  existence with no bound present. For `Eq` / `Ord` it's also a hard
  guarantee: a compile error if any field/case is ineligible, unlike a bound
  (simply unsatisfied) or the structural rule (nothing to reject).
- `automatic` and `on_bound` differ only in eagerness. For `Eq` / `Ord` this
  is invisible at the bound-check level — `T: Eq` was already satisfied the
  moment fields qualified, and every `==` / `<` call site is unchanged. The
  only difference is whether a body gets emitted for a type nothing asked
  about.

### Bound-driven synthesis semantics

An `on_bound` obligation `T: Trait` is satisfied structurally: no manual impl
exists, and every field/case of `T` satisfies `Trait` recursively. On
failure, the error reason-chains from the bound site to the offending
field/case ([Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)).
See [Synthesis](./compiler.md#synthesis) for the recording/generation
mechanism.

The explicit marker differs by family: `impl Eq for T;` / `impl Ord for T;`
validates `T` structurally before recording and is a hard compile error if
ineligible. Serde's marker does not pre-validate (a gap; see Open
Questions).

Whole-program and monomorphized, so there's no orphan rule to violate.
`GenericInstance` is out of scope for `Serialize` / `Deserialize` —
elaboration only sees the generic template, so a request keyed by a concrete
instantiation wouldn't resolve to a body (the same gap
[Serde](./wep-2026-02-28-serde.md) tracks for generic-struct `Deserialize`).
`Eq` / `Ord` are unaffected: their generic synthesis predates this WEP and
already records against the base declaration.

### Policy assignment

- `Inspect` / `InspectAlt` / `Default` stay `automatic` — no behavior change.
- `Serialize` / `Deserialize` move from `explicit` to `on_bound`.
- `Eq` / `Ord` move from `automatic` to `on_bound`: an impl is generated only
  for a `(type, trait)` pair some call site, bound, or marker actually
  demands.
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
- `Eq` / `Ord` no longer exist "for free" without a direct use site; a type
  intended for future comparison needs the explicit marker to guarantee the
  impl in advance.

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

## Open Questions

- Declaration syntax for a user-defined trait's policy.
- `GenericInstance` is not yet `on_bound`-eligible for `Serialize` /
  `Deserialize` (`Eq` / `Ord` are unaffected).
- `Serialize` / `Deserialize`'s explicit marker does not pre-validate
  structurally the way `Eq` / `Ord`'s does — whether to close this gap is
  open.
- Coherence interaction with concrete impls, relevant once a `Reflect`-based
  rewrite of the synthesized body lands.
