# WEP: A Package's Default Interface Is Its Module

## Context

A Wado library reaches its consumer one of two ways. A source dependency
(`path` or `git`) is compiled into the consumer. A registry dependency is a
prebuilt Component Model component, composed in. The two cannot offer the same
API: a component carries no generic, closure or trait, so a `pub`-only item
reaches a consumer only from source.

They differed even where the Component Model has no limit. A library exporting
`fn id_string(s: String) -> String` was called two ways:

```wado
use { id_string } from "lib:catalog";           // source dependency
id_string("x");

use { CmCatalog } from "wado-lang:cm-catalog";  // registry dependency
CmCatalog::id_string("x");
```

Two naming decisions caused it, neither forced by the Component Model:

- The consumer turned every exported interface into a Wado `interface`, a
  namespace of its own.
- The producer put a library's exports into its default interface only when a
  signature named a user type. Otherwise they became world-level functions,
  which a consumer imports by bare name. Adding one type to one signature
  moved every export.

## Decision

### The default interface is the module

A component's default interface is the one named after its own package:
`acme:geo/geo`, with the interface name equal to the package name's last
segment. A consumer offers each of its functions as a free function of the
module, so `use { id_string } from "wado-lang:cm-catalog"` imports what a
source dependency's `export fn id_string` would.

The rule reads only the artifact, so it holds for a registry dependency and for
a component file imported by path alike. A component built elsewhere that
follows the same naming is read the same way. Any other interface is reached
through its name alone, and so is a default interface the component imports
rather than exports: the consumer provides that one.

A composed component may export more than one default interface. A function
name two of them share is left out of the module, so neither shadows the other,
and each is called through its interface. Importing one by bare name is an
error that names the interfaces declaring it. A function the component's world
exports directly keeps its name over an interface function of the same name.

### The interface stays importable

`use { CmCatalog }` and `CmCatalog::id_string(x)` still work. The interface is
the Component Model's own name for these functions, and it is how a consumer in
another language calls a Wado library. Both spellings reach one function.

### A library always exports its default interface

A library's exports always go into its default interface, never to world-level
functions. Their shape no longer depends on whether some signature names a type.

## Consequences

- The `export` items of a library import and call the same way from source and
  from a component.
- A library whose exports name no type changes shape for a non-Wado consumer:
  it now finds the functions under the interface rather than at the world.
- A world-level function import carries only primitives and strings
  ([Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md#known-gaps)).
  A Wado library no longer produces one, so that limit no longer reaches a
  Wado-to-Wado dependency.

## Known gaps

- A Wado call of an `export async fn` from source returns the value. Through a
  component it returns an `AsyncCall<T>`.
- A `pub`-only item named through a component is reported as not found. The
  report does not say that the item exists and reaches only a source consumer.
- A record's fields are all public on the consumer's side, whatever the
  library declared. A type's methods and trait impls do not cross.

## References

- [Wasm CM Component Import (`use`-based)](./wep-2026-06-26-wasm-cm-component-import.md)
- [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md)
- [Provider Metadata — Source-Bundled Package Artifacts](./wep-2026-07-26-provider-metadata.md)
