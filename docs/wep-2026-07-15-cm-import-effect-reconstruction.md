# WEP: Effect Reconstruction from CM Component Imports

Status: Implemented (v1)

The synchronous value-type surface is implemented: the loader collects each
imported component's host-leaf imports, the import plan mirrors the composed
binary, and the effect checker requires a direct `E::op()` call's reconstructed
effects. Enforcement is scoped to host-backed effects — an effect with a `#[cm]`
boundary (WASI or a CM component). A user-defined effect with no CM boundary is
resolved by the handler machinery, so its operations (including a handler
method's self-delegation) are not a direct-op requirement. The async import
surface (`stream`/`future`) remains out of scope (see below).

## Context

Wado's core model is `interface = effect = CM import/export` (see
[Design Philosophy](./design-philosophy.md) "Effects are WASI capabilities").
[WIT Interoperability](./wep-2026-05-02-wit-interoperability.md) unified block
declarations into `interface` and decided that a function-bearing interface is
_conservatively treated as effectful by the call site_.
[Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) maps an
imported `.wasm` component's exported interface onto an effectful Wado
`interface`, exactly like WASI.

This is simple and consistent, but it over-applies to pure components. A
pure package — `package-marl` (Markdown → HTML/Markdown, string→string, no
I/O) or `package-gale` — exports an interface that carries no host capability.
Imported as a `.wasm` component, calling `Marl::render(src)` nevertheless
requires `with Marl`, and that requirement propagates through the entire call
tree. The effect is real ceremony with no capability behind it.

Two observations sharpen the problem:

- Effectfulness is _boundary-induced_, not semantic. The same `marl` package
  depended on by source (its `pub` API, no CM boundary) is a pure function
  needing no `with`; only the compiled-component import path forces `with Marl`.
  This contradicts "effects are part of the type" — the effect comes from the
  packaging, not the code.
- "Effect = a capability the host must provide" is the real definition
  ([Design Philosophy](./design-philosophy.md)). "Function-bearing interface =
  effect" was a _syntactic proxy_ for it. A fused guest-to-guest component
  (statically composed via `wasm-compose`, see
  [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)) is not
  a host capability, so the proxy misfires for it.

## Rejected: generalize `#[benign]`

`#[benign(E)]` keeps the world import but elides `with E` propagation. Applying
it to a whole imported interface would silence the effect. Rejected: it is an
assertion of purity the compiler cannot check, the `unsafe` pattern reborn, and
it risks admitting unnoticed non-determinism. The whole point is to _derive_
purity, not to _trust_ it.

## Decision

Reconstruct a consumer's effect requirements from the imported component's
actual host-leaf imports, rather than treating its exported interface as an
opaque effect.

### The reconstructed rule

An interface is an effect if, in the final composed component, it bottoms out as
a host import. If it is satisfied by a fused sibling component, it is a
transparent namespace whose effectfulness forwards to that component's own
imports, recursively.

- The imported interface you call (`Marl`) becomes a namespace.
- The effects are the component's host-leaf imports, mapped to the underlying
  Wado effects (`wasi:clocks/monotonic-clock` → `MonotonicClock`), attached to
  the exported functions that reach them.
- A truly pure component (marl: only the ambient `wasi:cli/stderr` / `exit`
  panic path) reconstructs to _no effect_ — derived, not asserted.

This is the honest inverse of `#[benign]`: CM imports are explicit, so the
reconstructed set is a sound over-approximation by construction — it can
over-include an unused capability but can never miss one. Hidden non-determinism
is surfaced, not silenced: a component that secretly reads the clock forces
`with MonotonicClock` on its callers.

### Availability

The information is already in hand. `wit_component::decode` reconstructs the
component's world, including its imports; `wasm-compose` already surfaces a
dependency's remaining imports into the composed component; and
[WIT Interoperability](./wep-2026-05-02-wit-interoperability.md)'s
`resolve_import_plan` already asserts "type-level import set == compiled binary's
CM imports". Decode runs in the loader (Phase 3 of
[Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)), so a
component's declared imports are available before effect-check runs in the
frontend. The union over-approximation is a sound early estimate; DCE-based
pruning refines the consumer's own usage later without affecting soundness.

Standard imports map through the existing machinery: a `wasi:*` FQ resolves to
the existing Wado effect via `CmInterfaceRegistry`; a non-WASI interface is
synthesized from decoded WIT, reusing the export-side synthesis already in
[Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md).

The ambient panic path needs no subtraction here, because it no longer imports:
`log_stderr` / `log_stdout` rides an existing stdio import or lowers to
`unreachable`, gated on whether the target world provides a sink
(`NirPackage::provides_ambient_stdio_sink` — true for `wasi:cli/command`,
`wasi:http/service`, and the test world; false for `--lib` and kiln). So a
purely-computational component's decoded imports are already empty of the panic
path; what remains is exactly its real capabilities (plus type-only shared
interfaces such as `wasi:cli/types`, which carry no effect).

### Granularity

- [x] v1 — component-level union. Every export of the component requires the
      union of the component's imports. Needs only the decoded component _type_
      (no call-graph analysis), is sound, and is exact for well-factored pure
      packages (marl's union is empty). Over-approximation bites only when one
      component mixes pure and impure exports — arguably a packaging smell.
      Implemented: the loader records the union
      (`CmInterfaceRegistry::host_leaf_imports_for`); the import plan and the
      effect checker both consume it, and the loader auto-loads the WASI packages
      behind the union so their effects are in scope.
- [ ] v2 — per-export reachability. A core-wasm call-graph analysis inside the
      imported component attributes each import to the exports that can reach it.
      More precise, but conservative on `call_indirect` / funcref / GC vtable /
      async callbacks, and it is a whole-program analysis over someone else's
      compiled artifact. Optional refinement on top of v1.

### Resources are in scope — the same mechanism as interfaces

Resource operations are effects
([Effect System Design](./wep-2026-01-27-effect-system-design.md) "Resource Types
as Effects"): every constructor / method / static is a host call. Reconstruction
_subsumes_ this rule rather than exempting resources from it. A resource lives in
an interface, so the host-leaf criterion applies to the owning interface with no
resource-specific path:

- A host-provided resource (`wasi:*`, CM `Stream` / `Future` — all of today's
  resources) bottoms out at the host, so its operations stay effects. A pure
  component (marl) exposes no resource across its boundary, so it is unaffected.
- A guest-defined resource is a spec-level feature — Wado can define a `resource`
  and implement its methods; the current gap is implementation lag, not design
  (type-level declaration works: the affine "guest one" in
  [Ownership Analysis](./wep-2026-05-21-resource-ownership.md), fixture
  `user_resource_smoke.wado`; consuming an external component's resource is the
  open item in [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)). A
  guest-implemented resource exported by a component and imported through fusion
  is guest-to-guest, so its owning interface reconstructs to the exporter's own
  host-leaf imports — identical to a pure function interface.

So the reconstruction unit is not "interface vs resource" but "host-leaf import
vs fused-component export". Resources ride the same rule; the effect a caller
sees is whatever host capabilities the resource's implementation transitively
needs, empty for a purely-computational one.

### Out of scope

Async import surface (`stream<T>` / `future<T>` in an imported component's
signatures) is Phase 8 of
[Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) and not
wired yet. The synchronous value-type surface — where pure packages live — is
the v1 target.

## Consequences

### The conceptual shift

`interface = effect` splits: an interface is either a host-leaf capability (an
effect) or a fused-component namespace (not an effect). This is not intrinsic to
the interface declaration — it is _composition-relative_. The same WIT interface
is an effect when host-satisfied and a namespace when satisfied by a fused
component. The distinction is not visible from the interface declaration alone;
it is computed by the Wado compiler at link/compose time from the import plan,
not decided by the runtime host.

The substitution unit moves with it: instead of mocking "Marl", a consumer mocks
the underlying capabilities Marl uses (e.g. `MonotonicClock`). For a pure
component there is nothing to substitute — correctly.

### Discoverability

The requirement is statically known at import time, so this is not blind
trial-and-error, provided tooling surfaces it:

- `wado query hover --symbol "./marl.wasm"#render` prints the reconstructed
  signature (`... with MonotonicClock`); LSP hover shows the same before compile.
- Effect-error diagnostics name the complete required set at once, with a
  provenance reason chain
  ([Diagnostic Reason Chains](./wep-2026-06-02-diagnostic-reason-chains.md)):
  "`render` requires `MonotonicClock` — marl.wasm imports
  `wasi:clocks/monotonic-clock@0.3.0`".
- The component-level union is directly readable from the artifact
  (`wasm-tools component wit` / `wado wit ./marl.wasm`). Only v2's per-export
  attribution is compiler-private.

### Costs (the price of honesty)

- Vocabulary decoupling. You `use { Marl }` but write `with MonotonicClock`. The
  effect name has no syntactic tie to the imported name, so discoverability moves
  from "obvious from the API" to "surfaced by tooling / diagnostics". Under
  `with Marl` the effect name _was_ the import name — trivially self-evident, but
  only because it was an opaque, always-present, capability-free token.
- Implementation leakage. A component's internal use of a capability becomes part
  of the consumer's effect signatures. A marl patch that starts reading the clock
  breaks consumers' `with` annotations even though marl's public value-API is
  unchanged. Under `with Marl` the effect surface was stable and encapsulated.

Both costs are the flip side of Wado's "no hidden effects" principle: surfacing
`MonotonicClock` is _correct_; `with Marl` was cheap precisely because it was
dishonest.

### Consistency wins

- Source-dependency and component-dependency agree: pure stays pure either way.
  Only today's CM-component-import path disagrees, and this removes that
  divergence.
- The effect set the type-checker demands equals the host imports the composed
  binary actually has, dovetailing with `resolve_import_plan`'s faithfulness
  guarantee.

## Supersedes

This is a design decision; implementation is pending (Status: Draft), so the
supersessions below apply to the design, not yet the shipped behavior. Each is
scoped — host-satisfied (WASI) interfaces and host-provided resources are
unchanged.

- [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md) §"Pure
  interfaces": "an interface with functions is conservatively treated as
  effectful by the call site" — superseded for imported `.wasm` components; a
  fused component's interface is effectful only insofar as its own host-leaf
  imports are.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)
  Decision: "map the imported interface faithfully to a Wado `interface`
  (effectful)" — the unconditional `(effectful)` is superseded; effectfulness is
  reconstructed from the component's host-leaf imports.
- [Effect System Design](./wep-2026-01-27-effect-system-design.md) "Resource
  Types as Effects": "every operation on a resource is a host call … therefore
  resource types are effects" — subsumed and generalized, not dropped. A
  host-provided resource still bottoms out at the host (stays an effect); the
  "every resource op is a host call" premise no longer holds for a
  guest-implemented resource imported through fusion, which reconstructs like any
  other interface.

## References

- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md)
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)
- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)
- [Effect Handler](./wep-2026-04-11-effect-handler.md)
- [Design Philosophy](./design-philosophy.md)
