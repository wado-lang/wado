# Research: Callbacks across the Component Model Boundary

How a Wado component can parameterize a dependency component's behavior with
its own code — the callback / dependency-injection pattern — given that the
Component Model has no first-class function values. Motivated by making a
published library (e.g. `wado-lang:marl`) extensible: the library performs a
guest-defined effect (`Highlight`), and the consumer supplies the
implementation from outside the library's OCI artifact.

Companion WEPs:

- [Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md)
  — the consuming direction (a component's imports become the consumer's
  effects). This note settles how such an import is *satisfied*.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) —
  the loader/codegen pipeline (decode, bindings, wasm-compose fusion).
- [Effect Handler](./wep-2026-04-11-effect-handler.md) — the in-component
  dynamic dispatch the boundary mechanisms below connect to.

## The problem

A guest effect declared in a library (`interface Highlight` used with
`with Highlight`, no handler installed) lowers to a CM **import** of a
synthesized interface when the library is compiled with `--lib`
(implemented; see Status below). The question is how the consumer provides
the implementation. The obvious wish — the consumer's `with h do { ... }`
handler receiving calls from inside the dependency — requires a call path
*dependency → consumer* while the consumer is itself mid-call into the
dependency.

Component instantiation is a DAG (imports are satisfied at instantiation, no
cycles), and component functions are not values, so a callback cannot be
passed at call time. The spec anticipated this exact need.

## What the spec says

### The use case is a design goal

[UseCases.md](../vendor/component-model/design/high-level/UseCases.md) #8,
verbatim:

> A component developer creates a fresh private instance of a dependency,
> supplying the component's own functions as imports to the dependency. The
> component does this to parameterize the dependency's behavior with the
> component's own logic or implementation choices (achieving the goals
> usually accomplished using callback registration or [dependency
> injection]).

### The sanctioned mechanism: donut wrapping

[Linking.md §"Higher-order Shared-Nothing Linking (aka donut wrapping)"](../vendor/component-model/design/mvp/Linking.md):
a parent component nests the child and satisfies the child's imports with
`canon lift`s of the parent's **own core functions**; the child's exports are
`canon lower`ed back into the parent's core code
(`M1 --lift--> C --lower--> M2`, with funcref-table plumbing so one core
module both calls the child and receives calls from it). The instance graph
stays acyclic — the callback edge is core-level, inside one component
instance.

Reentrance is explicitly legal: Component Invariant #2
([Explainer.md](../vendor/component-model/design/mvp/Explainer.md)) permits a
component to be reentered when it "call[s] a donut wrapped child component";
the Canonical ABI's `lift` guard traps only *recursive* reentry of the child.

### Function values are explicitly future work

[Concurrency.md](../vendor/component-model/design/mvp/Concurrency.md) future
extensions: "allow function closures to be passed as first-class values,
supporting the 'callback' pattern in many pre-existing APIs". Until then,
callbacks are encoded via linking (above) or via `stream`/`future` values.

## What engines implement

### wasmtime statically rejects donut adapters

wasmtime's fused-adapter compiler (FACT) compiles any adapter whose lift and
lower sides are the same instance **or in an ancestor relation** to an
unconditional trap — `crates/environ/src/fact/trampoline.rs:117-127`
(wasmtime 47):

```rust
// If the lift and lower instances are equal, or if one is an ancestor of
// the other, we trap unconditionally.  This ensures that recursive
// reentrance via an adapter is impossible.
```

This over-approximates the spec's "trap recursive reentry" runtime guard: a
donut parent→child call adapter is an ancestor-relation adapter, so **all**
donut calls trap, not just recursive ones. Verified empirically with a
hand-written donut component (`wado-compiler/tests/cm_donut_canary.rs`):
instantiation succeeds, the first parent→child call traps
`cannot enter component instance`.

Sibling adapters (the shape `wasm-compose` produces, and what the component
-import pipeline uses today) are unaffected.

### Host reentry is task-chain-scoped

`Store::may_enter` (wasmtime `component/concurrent.rs`) forbids the host
entering a top-level instance that is **on the current task's call chain**
("the behavior defined in the spec"). Consequences:

- A host import handler synchronously calling back into its caller's
  component traps — regardless of sync/async lifting.
- A **detached task** (scheduled from the host event loop, not on the guest
  task's chain) may enter the instance concurrently, provided the target
  export is async-lifted so the instance admits concurrent tasks.

## Options

### Provider composition — link-time dependency injection (adopted)

Satisfy the dependency's guest-effect import with a **sibling provider
component** at composition time: the consumer designates an implementation
(its own Wado module compiled as a mini component, or a third-party
component), and codegen connects `provider.export → dependency.import` in the
existing `wasm-compose` graph. Sibling adapters are engine-clean and already
exercised by the component-import pipeline.

This is UseCases #8 exactly, and it completes the
[reconstruction WEP](./wep-2026-07-15-cm-import-effect-reconstruction.md)'s
own rule — "if [an interface] is satisfied by a fused sibling component, it
is a transparent namespace": a declared provider makes the reconstructed
effect disappear from the consumer's obligations; with no provider, the
effect remains required (and, today, must be reachable from the host or a
handler — see Host effect pump).

Trade-off: binding is static. No `with h do` dynamic extent across the
boundary; the provider cannot close over the consuming program's state (it is
its own component). For self-contained parameterizations — a syntax
highlighter, a codec, a policy function — this is a fit, not a limitation.

### Host effect pump — dynamic handlers via the host

Leave the dependency's guest-effect import unsatisfied in the composed
artifact; the wado runtime harness (wasmtime embedding, jco shim) provides it
with a host function that schedules a call to the consumer's async-lifted
effect-dispatch shim export as a **detached task**, then resolves the
dependency's pending import call. The consumer's installed handler
(`with h do`, dispatch global) receives the call mid-flight — true dynamic
extent across the boundary.

Legal under the task-chain rule above (detached task + async-lifted export),
but requires a wado-aware host: the artifact is not self-satisfying, and a
generic host would need to replicate the pump. Candidate follow-up once
provider composition ships; the two share the same library-side contract
(the synthesized import), so they compose rather than compete.

### Effect channels — stream-encoded callbacks

Encode the effect protocol in values: the library's export takes/returns
`stream<request>` / `stream<response>` handles; the consumer pumps requests
through its local handler concurrently with the call. Pure guest-to-guest,
portable to any P3 host, real dynamic extent.

Cost: the library's published WIT shape becomes a channel protocol rather
than a clean interface import; both sides must be async; the compiler would
need to lower effect ops to channel operations (or the library hand-writes
the protocol). Considered a fallback shape, not the default contract.

### Donut wrapping — blocked on engines

The spec-sanctioned mechanism (above). Requires wasmtime's FACT to implement
the spec's recursive-reentry-only guard instead of the unconditional
ancestor trap. Worth pursuing upstream; `cm_donut_canary.rs` documents the
current behavior and will flag when the engine changes.

### First-class function values — future spec

The eventual loosening. When function closures become passable values, a
guest effect could be satisfied per-call with an actual closure. Tracked in
Concurrency.md's future extensions; nothing to build against today.

## Decision

Adopt **provider composition** now; keep the library-side contract (guest
effect → synthesized CM import) unchanged so **host effect pump** can be
layered on later for dynamic handlers, and so a future donut/first-class
world needs no contract change.

## Status

- Producer (library side): implemented — an unhandled guest effect in a
  `--lib` build lowers to a synthesized CM import
  (`register_lib_guest_effect_imports`; test `guest_effect_import.rs`).
- Consumer reconstruction: implemented — a dependency's non-WASI import
  materializes as an impl-able effect; unhandled use is a missing-effect
  error (`wit_consume::build_bindings`; fixture
  `guest_effect_missing_handler.wado`).
- Provider composition: in progress.
- Host effect pump, donut upstream: not started.
