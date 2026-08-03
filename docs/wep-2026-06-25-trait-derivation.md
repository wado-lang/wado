# WEP: Trait Derivation Policy — Bound-Driven Synthesis

Status: Implemented. `Eq` / `Ord` / `Default` / `Serialize` / `Deserialize`
are _demand_ `on_bound`: a body is synthesized only where a reference needs
it, discovered through the shared `bound_driven_synth_requests` channel a
bound check or explicit marker records into. They are not total — a `fn`-typed
field blocks `Eq` / `Ord` / serde, a field without a default blocks `Default`
— so a `T: Trait` obligation there is real, and gating avoids code-size waste
on types that never use the trait.

The _debug_ format traits (`Inspect` / `InspectAlt`) are _total_ `on_bound`:
every type is structurally debug-formattable, so a `T: Inspect` obligation
always holds (for a type parameter or any concrete type), and `impl Inspect
for T;` is a conformance check that always validates. Because they are total
and universal debug output is a feature rather than waste, their _generation_
stays eager (a body for every type kind) — gating it would demand a discovery
mechanism for `${v:?}` over an unbounded type param (whose concrete reference
only materializes at monomorphize) with no offsetting code-size benefit.

The _display_ traits (`Display` / `DisplayAlt`) are **not** total (revised
2026-07-15). `Display` is never auto-derived for a struct, variant, or generic
container — a type has a human-facing string representation only if someone
wrote `impl Display`, so `${x}` on such a type is a compile error (use `${x:?}`
for debug output), and `T: Display` is a real obligation. The two exceptions
are types with an _unambiguous canonical string form_, which auto-derive it:
a plain `enum` displays its **bare case name** (`Red`, distinct from `Inspect`'s
type-qualified `Color::Red`), and a newtype **inherits its base type's
`Display`** transparently (a `Meters = f64` renders `3.14`, no `as Name` tag).
`DisplayAlt` (`${x:#}`) tracks `Display`: it auto-derives a fallback delegating
to `Display` only where a `Display` impl exists. A policy declaration for
user-defined traits is open. See Open Questions.

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
[Library-Defined Derivation (`ReflectStruct`)](./wep-2026-06-13-reflect-derivation.md),
which decides _how_ an impl is written. This WEP decides _when_ one is
instantiated for a given `T`.

### Forcing functions

- Anonymous structs have no name, so `impl Serialize for …` is unwritable —
  bound-driven synthesis is the only way to satisfy `T: Serialize`. A hard
  prerequisite for the efficient field path in
  [`core:log`](./wep-2026-06-25-core-log.md).
- `Eq` / `Ord` synthesizing for every declared type costs compile time and
  code size with no compensating benefit (unlike `Inspect`, which exists so
  `${x:?}` always works everywhere).

## Decision

### A per-trait derivation policy

| Policy              | A `T: Trait` obligation is satisfied by …                                   | Generation                                        | Examples                                           |
| ------------------- | --------------------------------------------------------------------------- | ------------------------------------------------- | -------------------------------------------------- |
| `on_bound` (demand) | structural synthesis, on demand when a reference requires it                | on demand                                         | `Eq`, `Ord`, `Default`, `Serialize`, `Deserialize` |
| `on_bound` (total)  | always — the trait is total over all types                                  | eager                                             | `Inspect`, `InspectAlt`                            |
| `display`           | a real `impl Display`, a plain `enum` (bare case name), or a `Display` base | enum: eager body; newtype: inherited at call site | `Display`, `DisplayAlt`                            |
| `explicit`          | only a written `impl Trait for T;` (or full manual impl)                    | on demand                                         | default for user traits                            |

There is no longer an `automatic` policy: its members split by totality.
`Default` joined the demand `on_bound` traits (`Eq` / `Ord` / serde) — it is
not total (a field without a default blocks it), so gating its generation
saves code on structs never defaulted. The debug format traits stayed eager
but gained `on_bound` obligation semantics (see below). `DisplayAlt`
synthesizes a fallback that delegates to `Display`.

The debug format traits `Inspect` / `InspectAlt` are _total_: every type is
structurally debug-formattable (an `Inspect` body is generated for every type
kind — struct, enum, variant, flags, newtype, tuple, closure, opaque
resource). Totality is a type-system fact — a `T: Inspect` obligation always
holds, for a type parameter or any concrete type — so those bounds compile
where the old policy rejected them, and an `impl Inspect for T;` marker always
validates. Generation stays eager: a total trait wastes nothing meaningful by
existing for every type (universal debug output is the point), and gating it
would need a discovery mechanism for `${v:?}` over an unbounded type param —
whose concrete `T^Inspect` reference only materializes at monomorphize, after
synthesis — for no code-size gain.

`Display` / `DisplayAlt` are the `display` policy — deliberately **not** total
(revised 2026-07-15). A human-facing string representation is not something the
compiler can invent structurally the way it can a debug dump: for a struct
there is no canonical field layout, delimiter, or ordering to pick, so `Display`
is left to the author. `${x}` on a type with no `Display` is a compile error
(use `${x:?}`), and `T: Display` is a real obligation. Two type kinds are the
exception because their string form _is_ canonical and unambiguous, so the
compiler derives it eagerly:

- a plain `enum` displays its **bare case name** (`Red`) — distinct from
  `Inspect`'s type-qualified `Color::Red`, so `Display` is not a redundant
  debug echo;
- a newtype **inherits its base type's `Display`** transparently (`Meters =
  f64` renders `3.14`, no `as Name` tag). This is call-site inheritance, not a
  synthesized body: the format call peels the newtype to its base
  (`peel_transparent_newtype`), covering every transparent format trait. Only
  `Inspect` / `InspectAlt` are overridden per newtype (for the `as Name` tag).

`DisplayAlt` derives a fallback delegating to `Display` only where a `Display`
impl exists (manual, enum-synthesized, or newtype-inherited), so the alternate
display of a `Display`-less type is likewise a compile error. The demand
`on_bound` traits are _not_ total (a `fn`-typed field, a field without a
default, blocks them), so a `T: Trait` there is a real obligation and gating
its generation removes genuine waste.

- A hand-written `impl Trait for T { … }` always wins.
- An `impl Trait for T;` marker is a conformance check that validates `T`
  structurally at its own span and records a bound-driven request — the same
  effect a `T: Trait` bound has, except a marker is a hard compile error if
  `T` is ineligible (a bound is merely unsatisfied elsewhere). For the
  structurally-checkable traits (`Eq` / `Ord` / `Default` / serde) that hard
  error fires when any field/case is ineligible (`Default` additionally
  requires every field to carry a default expression). An `Inspect` /
  `InspectAlt` / `DisplayAlt` marker always validates — every nominal type is
  debug-formattable, and `DisplayAlt` falls back to whatever `Display` the type
  has — so it serves as an intent/documentation annotation. A `Display` marker
  (`impl Display for T;`) is **rejected**: `Display` is not derivable for an
  arbitrary type, so a bare marker cannot conjure a body (write a real
  `impl Display { fn fmt … }`, or rely on the enum / newtype auto-derivation).
- For the demand `on_bound` traits, the trigger for a body is usage: a bound
  check, marker, or operator/call reference records the request. See
  [Discovery Mechanism](#discovery-mechanism). The total debug traits and the
  `display`-policy auto-derivations skip discovery — their bodies are always
  generated (for every type, and for every enum / `Display`-based newtype,
  respectively).
- `on_bound` and `explicit` differ on one axis: whether eligibility is
  discovered by an unprompted structural scan (`on_bound`) or only via an
  explicit marker (`explicit`).

### Bound-driven synthesis semantics

An `on_bound` obligation `T: Trait` is satisfied structurally: no manual impl
exists, and every field/case of `T` satisfies `Trait` recursively (`Default`
instead requires every field to carry a default expression). On failure, the
error reason-chains from the bound site to the offending field/case
([Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)).

A plain `enum` and a `flags` type carry no members to recurse into, so both
satisfy every structural obligation outright: `Eq` / `Ord` compare the
discriminant and the bitmask respectively. Their bodies are generated the same
way, which is what keeps a struct or variant carrying one from losing its own
derive.

Open: a hand-written `impl Eq` / `impl Ord` does not yet win everywhere for the
three kinds whose values erase to a scalar before the comparison is emitted —
`enum`, `flags`, and a newtype. The comparison operators lower to the scalar
compare directly rather than dispatching, and a `T: Eq` bound sees the erased
base for `flags` and newtypes. A manual impl must win over the derived
comparison at every site, a newtype most of all: it exists to be a distinct
type. `tests/fixtures/eq_ord_manual_impl_todo.wado` pins the sites that already
dispatch and marks the rest `#[TODO]`.

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
/ serde) only; the total debug traits and the `display` auto-derivations
generate unconditionally and need none.
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

A total debug trait has no such gate: `type_implements_trait` short-circuits
`true` for it (totality), and generation is eager, so `${v:?}` over an
unbounded type param — whose concrete `T^Inspect` reference only appears after
monomorphize substitutes `T` — always finds its body already emitted. This is
exactly the case that made gating the debug traits unattractive, and eager
generation sidesteps it. `Display` does not face this: it is not total, so
`${v}` over an unbounded `T` is a compile error unless `T: Display` is written,
and a bounded generic only instantiates with `Display` types — no post-mono
discovery gap.

An explicit marker feeds the demand request set: it validates structurally at
its own span (hard error if ineligible) and records the request like a bound.
An `Inspect` / `InspectAlt` / `DisplayAlt` marker validates but records nothing
meaningful (generation is already eager). This is scoped to compiler-synthesized
bodies; a hand-written `impl Trait for T { … }` is ordinary source, type-checked
because it exists and left to ordinary dead-code elimination.

### Policy assignment

- `Inspect` / `InspectAlt` are total `on_bound`: a `T: Inspect` bound always
  holds, an `impl Inspect for T;` marker always validates, generation is eager.
- `Display` / `DisplayAlt` are the `display` policy: not total. `Display` is
  auto-derived only for a plain `enum` (bare case name) or a newtype over a
  `Display` base (transparent inheritance); every other type needs a manual
  `impl Display`. `${x}` / `T: Display` on a non-`Display` type is a compile
  error. `DisplayAlt` derives a `Display`-delegating fallback wherever `Display`
  exists. A `Display` marker is rejected.
- `Default` moved from `automatic` to demand `on_bound`: a body is generated
  only for a `(type, Default)` pair some reference actually needs.
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
- `T: Inspect` (and `InspectAlt`) bounds now hold for every type, which the old
  automatic policy rejected at bound-check for plain aggregates, and
  `impl Inspect for T;` markers are accepted.
- `Display` regains meaning: `T: Display` now certifies a real string
  representation, so `String::push_display` and any `Display`-bounded API reject
  types with only a debug form instead of silently emitting one. `${x}` on a
  non-`Display` type fails at compile time with a clear "use `${x:?}`" path.
- No macros, no dynamic reflection; synthesis stays static and monomorphized.

### Trade-offs

- `Serialize` / `Deserialize` crossing to `on_bound` weakens the opt-in that
  bounds the wire surface today; `#[secret]` and a manual impl are the only
  countermeasures.
- Errors move from the (absent) impl site to the bound site; reason chains
  keep them legible.
- A future `ReflectStruct`-based rewrite of the synthesized body must not let a
  blanket `impl<T: Reflect> Trait for T` conflict with concrete impls — an
  open coherence question the current mechanism doesn't hit yet, since it
  instantiates the existing per-type synthesizer directly.
- No on_bound impl exists "for free" from a mere declaration without a bound,
  marker, or reference; an unmarked type intended for future use with zero
  current call sites gets no code until something references it. An explicit
  marker both guarantees a hard validation error if `T` is ineligible
  (`Eq` / `Ord` / `Default` / serde) and records a request, so a marked type
  does get its body.
- The debug traits' totality is a type-system commitment: "every type is
  debug-formattable." This matches the pre-existing automatic policy (which
  generated `Inspect` for every type), but removes the freedom to introduce a
  genuinely non-debug-formattable type later without revisiting the totality
  short-circuit.
- `Display`'s non-totality (revised 2026-07-15) is a source-compatibility
  break: every `${x}` and `String::push_display` that relied on the old
  Display-delegates-to-Inspect fallback for a struct / variant / container now
  fails to compile and must switch to `${x:?}` or add a manual `impl Display`.
  Accepted as the point of the change — a silent debug fallback is what made
  `T: Display` meaningless.
- Debug generation stays eager, so this WEP does not reduce debug code size —
  a deliberate trade (see Alternatives): gating a total trait buys little and
  costs a discovery mechanism. `Default` gating uses a finite set of recording
  sites (bound check, `P::default()` resolution, marker); a missed site fails
  loud at monomorphize / link rather than miscompiling.

### Relationship and prerequisites

Ships directly against the existing bespoke synthesizers
(`synthesis::serde_synth`, `synthesis::traits`), not against
[`ReflectStruct`](./wep-2026-06-13-reflect-derivation.md), which remains unbuilt.
The original plan was to land this after migrating serde onto a
`ReflectStruct`-based impl, but the two turned out independent — this WEP only
changes _when_ a request is created, not _how_ the body is written. A future
`ReflectStruct`-based rewrite can land later against the same plumbing.

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
- **Gate debug generation on demand, like `Eq` / `Ord`.** Would save the code
  of `Inspect` bodies for types never debug-formatted. But the debug traits are
  total, and `${v:?}` over an unbounded type param has no bound to record and no
  concrete reference until monomorphize — closing that gap needs either a
  post-monomorphize discovery sweep (reimplementing the per-type synthesizers
  against concrete `FlatPackage` data, a whole pass) or implicit `Inspect`
  bounds on every type parameter (which over-records for every type used
  generically, erasing most of the saving). Neither buys enough against a total
  trait whose universal availability is a feature. Rejected in favor of eager
  generation plus total obligation semantics.
- **Keep `Display` total (auto-derived, delegating to `Inspect`).** The status
  quo before 2026-07-15. Ergonomic — `${x}` always worked — but `T: Display`
  was then satisfied by every type, so it certified nothing: `push_display` and
  every `Display`-bounded API silently accepted debug-only types, and the
  Display body was a redundant echo of `Inspect`. Rejected: making `Display`
  mean "has a real string representation" is the whole point. `enum` and newtype
  keep an auto-derivation only because their canonical form is unambiguous.
- **Implicit bounds for `Eq` / `Ord` / `Default`.** Would let those ride the
  same bound-check recording, but they are not total (a `fn`-typed field blocks
  `Eq`; a field without a default blocks `Default`), so an implicit bound would
  reject ordinary generic code over ineligible types. Rejected.

## Open Questions

- Declaration syntax for a user-defined trait's policy (opting a user trait
  into `on_bound`).
- Whether the debug traits should eventually gate generation after all (via a
  post-monomorphize discovery pass), should code size on debug output ever
  matter enough to justify the machinery.
- Coherence interaction with concrete impls, relevant once a `ReflectStruct`-based
  rewrite of the synthesized body lands.
