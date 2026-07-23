# WEP: Effect Reconstruction from CM Component Imports

Status: Implemented (v1, synchronous value-type surface)

Guest effects cross the Component Model boundary in both directions, under one
rule. A consumer's effect obligations are _reconstructed_ from a dependency's
real host-leaf imports rather than from the mere presence of a function-bearing
interface (the consuming direction). A Wado library, symmetrically, turns a
guest effect it leaves unhandled into a CM import that a consumer satisfies (the
producing direction) — either by holding the underlying capability, or by
supplying a **provider component** composed in as a fused sibling.

Async import/export (`stream<T>` / `future<T>`) is out of scope for v1.

## Context

Wado's model is `interface = effect = CM import/export`
([Design Philosophy](./design-philosophy.md)), and
[Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) maps an
imported component's interface onto an effectful Wado `interface`, like WASI.

That over-applies to pure components. `package-marl` (Markdown → HTML,
string→string, no I/O) exports an interface carrying no host capability, yet
calling `Marl::render(src)` as a compiled component required `with Marl`, and
that requirement propagated through the whole call tree — ceremony with no
capability behind it.

Two observations sharpen it:

- Effectfulness is _boundary-induced_, not semantic. The same `marl` depended on
  by source (its `pub` API) is a pure function needing no `with`; only the
  compiled-component path forced `with Marl`. The effect came from the packaging.
- The real definition is "effect = a capability the host must provide."
  "Function-bearing interface = effect" was a syntactic proxy that misfires for a
  fused guest-to-guest component, which is not a host capability.

### Rejected: generalize `#[benign]`

Silencing an imported interface's effect with `#[benign]` asserts a purity the
compiler cannot check — the `unsafe` pattern reborn, admitting unnoticed
non-determinism. The point is to _derive_ purity, not _trust_ it.

## Decision

One rule spans both directions:

> An interface is an effect iff, in the final composed component, it bottoms out
> as a host import. Satisfied by a fused sibling component instead, it is a
> transparent namespace whose effectfulness forwards to that sibling's own
> imports, recursively.

This is _composition-relative_: the same WIT interface is an effect when
host-satisfied and a namespace when sibling-satisfied. The distinction is
computed at link/compose time, not intrinsic to the declaration.

### Consuming — reconstruct a dependency's obligations

Calling into an imported component requires the union of that component's own
host-leaf imports, mapped to the underlying Wado effects
(`wasi:clocks/monotonic-clock` → `MonotonicClock`), not a `with Marl` token. A
truly pure component reconstructs to _no effect_ — derived, not asserted.

Because CM imports are explicit in the artifact, the reconstructed set is a sound
over-approximation by construction: it can over-include an unused capability but
never miss one. Hidden non-determinism is surfaced, not silenced — a component
that secretly reads the clock forces `with MonotonicClock` on its callers.

The substitution unit moves with the rule: a consumer mocks the underlying
capabilities a dependency uses (`MonotonicClock`), not "Marl". A pure component
has nothing to substitute.

### Producing — an unhandled guest effect becomes a CM import

A library may declare its own `interface` effect and leave it unhandled at the
boundary. Compiled as a component, that effect lowers to a CM **import** of a
synthesized interface — the mirror of reconstruction. marl can thus perform a
`Highlight` effect it does not implement, leaving the choice to consumers. An
effect handled inside the library stays internal and imports nothing.

### Satisfying — hold the capability, or compose a provider

A reconstructed guest effect (a non-WASI import) materializes in the consumer's
scope as an impl-able effect and is required like any other; using the
dependency without providing it is a missing-effect error. Two ways to provide
it:

- Hold the underlying capability and let it surface (for a leaf the host or an
  outer component ultimately satisfies).
- Name a **provider** on the import — the dependency-injection shape sanctioned
  by the Component Model (donut/sibling linking):

  ```wado
  use { Marl } from "wado-lang:marl" with { provider: "./highlight.wado" };
  ```

  The provider is a plain Wado file (`export fn highlight(...) { ... }`)
  compiled into a component exporting the dependency's imported interface, bound
  by operation name. Composition wires `provider.export → dependency.import` and
  discharges the effect, so the consumer calls `Marl` with no handler installed.

A provider is a _static_ link-time choice, not a per-call dynamic handler.
Attempting the latter — the consumer's in-process handler receiving calls from
inside the dependency — forms a component instantiation cycle the Component Model
forbids; see
[Research: Callbacks across the CM Boundary](./research-cm-boundary-callbacks.md)
for the surveyed alternatives (donut wrapping, a host effect pump, first-class
function values) and why static provider composition is the one that ships.

## Scope

- [x] Synchronous value-type surface (primitives, strings, records, containers).
- [x] Consuming: component-level union — every export requires the union of the
      component's imports. Sound and exact for well-factored pure packages
      (marl's union is empty). Over-approximation bites only when one component
      mixes pure and impure exports.
- [x] Producing: a guest effect unhandled at a library boundary lowers to a CM
      import; a provider satisfies it.
- [ ] Per-export reachability (attribute each import to the exports that reach
      it) — a refinement over the union, conservative on indirect calls.
- [ ] A single provider file spanning several of a dependency's imported
      interfaces (bind by operation name across all).
- [ ] Async import/export surface (`stream<T>` / `future<T>`).

Resources ride the same rule with no special path: a host-provided resource
(`wasi:*`, `Stream` / `Future`) bottoms out at the host and stays an effect; a
guest-implemented resource imported through fusion reconstructs like any other
interface. The unit is "host-leaf import vs fused-component export," not
"interface vs resource."

## Consequences

### Discoverability

The requirement is statically known at import time; tooling surfaces it — hover
prints the reconstructed signature (`... with MonotonicClock`), and effect-error
diagnostics name the full set with a provenance chain
([Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)):
"`render` requires `MonotonicClock` — marl.wasm imports
`wasi:clocks/monotonic-clock`."

### Costs (the price of honesty)

- Vocabulary decoupling: you `use { Marl }` but write `with MonotonicClock`. The
  effect name has no syntactic tie to the import; discoverability moves from
  "obvious from the API" to "surfaced by tooling."
- Implementation leakage: a dependency's internal capability use becomes part of
  the consumer's effect signatures. A marl patch that starts reading the clock
  breaks consumers' `with` annotations even though its value-API is unchanged.

Both are the flip side of "no hidden effects": surfacing `MonotonicClock` is
correct; `with Marl` was cheap because it was dishonest.

### Consistency wins

- Source-dependency and component-dependency agree: pure stays pure either way.
- The effect set the type-checker demands equals the host imports the composed
  binary actually has.

## Supersedes

Each supersession is scoped — host-satisfied (WASI) interfaces and host-provided
resources are unchanged.

- [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md) §"Pure
  interfaces": "an interface with functions is conservatively treated as
  effectful by the call site" — superseded for imported components; a fused
  component's interface is effectful only insofar as its own host-leaf imports
  are.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md): the
  unconditional "map the imported interface faithfully to an effectful Wado
  `interface`" — effectfulness is reconstructed from host-leaf imports.
- [Effect System Design](./wep-2026-01-27-effect-system-design.md) "Resource
  Types as Effects": "every resource op is a host call" — holds for a
  host-provided resource, not for a guest-implemented one imported through
  fusion, which reconstructs like any other interface.

## References

- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md)
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)
- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)
- [Effect Handler](./wep-2026-04-11-effect-handler.md)
- [Design Philosophy](./design-philosophy.md)
- [Research: Callbacks across the CM Boundary](./research-cm-boundary-callbacks.md)
