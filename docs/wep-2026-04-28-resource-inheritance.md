# Resource Inheritance and Downcast (`resource extends`)

## Context

Wado's `resource` declares an opaque handle to a host-managed object. Today every `resource` is flat: it has methods, but no relation to any other resource type. This matches the Component Model (CM), whose canonical ABI defines resources as flat — there is no inheritance, no subtyping between resource types.

Several use cases push back on this restriction.

### Use cases

1. **WebIDL bindings (the immediate motivator).** Browser APIs are defined as deep single-inheritance hierarchies (`EventTarget → Node → Element → HTMLElement → HTMLInputElement`). A single JS object simultaneously satisfies every type in its prototype chain. Without inheritance, the binding generator has to either (a) duplicate every parent method on every child, (b) force the user to cast at every level (the wasm-bindgen `dyn_into` pattern), or (c) lose type information entirely.

2. **User-level handle abstractions.** A pure-Wado program may want a `resource Connection` parent with `resource TlsConnection extends Connection` and `resource PlainConnection extends Connection` children. Today this is expressible only via a `trait`, but traits cannot themselves be passed across CM boundaries.

3. **Future WIT extensions.** WIT does not have inheritance today. If WIT later grows it (or if Wado wants to expose layered host APIs to other languages), a Wado-side concept maps directly.

The browser case dominates the design pressure, but the feature is not browser-specific.

### Why this is hard

CM has no inheritance. Whatever Wado decides at the language level has to be lowered to a flat-resource model at the CM boundary, or use a non-CM-canonical representation (Wasm GC's `externref`). The choice of representation cascades into every other design decision — upcast cost, downcast mechanism, method dispatch, ABI shape, host contract — so this WEP starts there.

A separate concern: `resource` today maps to `i32` in CM-LM mode and to `externref` in CM-GC mode (per [GC in Components](./wep-2026-03-28-gc-in-components.md)). Inheritance forces us to be explicit about which one is the model, because `i32` handle tables and `externref` answer "is this handle also a parent?" very differently.

## Decision

(Sections below are filled in progressively.)

## Consequences

### Implementation Roadmap

- [ ] (to be filled)

## See Also

- [GC in Components](./wep-2026-03-28-gc-in-components.md) — resource representation in CM-GC vs CM-LM
- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md) — how flat CM resources map to Wado today
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md) — where the CM lowering happens
- [WebIDL Binding Generator (Tide)](./wep-2026-04-01-tide.md) — the primary consumer of this feature
