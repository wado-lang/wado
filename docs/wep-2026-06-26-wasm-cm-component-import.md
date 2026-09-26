# WEP: Wasm CM Component Import (`use`-based)

## Context

Core-wasm asset import (`use _ from "./x.wat" with { type: "wat" }`) is specified
by [WebAssembly Module Import](./wep-2026-01-10-wasm-import.md). This WEP is the
Component Model analogue: importing functions from an external `.wasm`
**component**, lowering and lifting values at the CM boundary, and composing the
dependency into the output so the result runs standalone.

It is the consumer side of
[WIT Interoperability](./wep-2026-05-02-wit-interoperability.md), whose producer
side emits and embeds the WIT this reads back.

## Decision

### Import form

```wado
use { Catalog } from "./catalog.wasm" with { type: "wasm" };
```

The clause is the same one a core-wasm asset uses; the loader tells a component
from a core module by its binary header and takes the component path.

### The component's own type is the interface definition

No Wado declaration file and no side-car `.wit`. The component's binary type is
decoded and drives compiler IR construction directly, so there is nothing to
keep in sync with the artifact — a component built by Wado is consumed through
the type it embedded about itself.

### What the exports become

- An exported WIT interface becomes a Wado `interface`, called as
  `Catalog::id_u32(x)` after `use { Catalog }`. Its named types become Wado
  items of the corresponding shape: a WIT record becomes a struct, a variant a
  variant, an enum an enum, flags a flags, and a type alias a newtype.
- A function the world exports directly, outside any interface, becomes a free
  function imported by bare name.

The correspondence is the
[WIT↔Wado mapping](./wep-2026-01-29-wit-wado-mapping.md) read in the consuming
direction. Its structural core — the `option` / `list` / `tuple` / `result`
assembly rule and the primitive correspondence — is shared with the producer, so
a Wado library consumed as a component presents the types it declared.

Because an imported interface is an ordinary Wado `interface`, the entire
existing CM pipeline applies to it: import planning, the type-driven
lower/lift adapters of
[CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md), and codegen.
Only the way the import is _satisfied_ differs, and that is composition below.

### Effects

An imported interface is not effectful by construction. Its effects are
reconstructed from the component's own host-leaf imports, so a component that
imports nothing maps to a namespace rather than an effect — see
[Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md).

### Value-type surface

Parameters and results cover the full synchronous value-type surface:
primitives, `string`, `char`, `enum`, `flags`, newtype, `List`, `Option`,
`Result<ok, err>` for arbitrary `err`, records, variants including payload-
bearing and tuple-payload cases, tuples, and arbitrary nesting of these
(`list<record>`, `option<record>`, `result<list<record>, _>`,
`list<tuple<record, _>>`, …).

Named types resolve through the interface's own module provenance rather than
by namespace prefix, so a dependency's package namespace is arbitrary — nothing
in the pipeline assumes `wasi:`.

### Async value types and `async func`

`stream<T>` and `future<T>` cross an import the same way they cross an export:
the value is the readable end, and the writable end stays with whoever created
the pair. A consumer therefore creates the pair itself, hands the readable end
to the dependency, and reads what the dependency hands back — two components
sharing a stream, with no host between them.

A dependency's `async func` becomes an `async fn` returning
[`AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md), as a host async import
does, so the caller writes into the stream while the dependency's subtask reads
it. Without that, the two ends deadlock: the copy is a rendezvous, so a
synchronous callee cannot read what its caller has not yet written.

An interface method and a world-level function are the same function at
different levels of the world, so one signature rule serves both: the same
value-type engine on the component type, the same core import signature, and
`canon lower async` whenever the function is async.

### Composition

The program component imports the dependency's interface exactly as it imports a
host interface. The dependency is then statically composed into the output: it
is instantiated, its exported interface is connected to the program's matching
import, both sides' remaining host imports are surfaced and merged by name, and
the program's own exports are re-exported. The result is one self-contained
component.

Composition is static rather than host-mediated by necessity, not preference.
Lowering the dependency's export into the program's imports through the host
validates but traps at run time: with concurrency support always on under WASI
P3, the canonical ABI forbids the host re-entering a top-level instance already
on the stack, and engines elide that check only for fused guest-to-guest
adapters — which is precisely what static composition produces. It also unions
the two components' host imports without hand-written forwarding.

## Roadmap

None. Every open item is a known gap below.

## Known gaps

### A component exporting a resource is rejected

A component exporting a `resource`, with its methods, static constructors and
`borrow<T>` parameters, is rejected when its type is decoded. So is a resource
handle inside a `stream` or `future` payload. Wado has resources; what is
missing is the consuming direction of the mapping.

What it admits is a whole class of components Wado cannot use: any dependency
built around resources. Nothing in a component's type tells an interned referent
from one minted per call, and a `dtor`-less exported handle would be a copyable
[token](./wep-2026-05-21-resource-ownership.md). The compile-time-bounded half
of a bundled ICU surface ([`core:icu`](./wep-2026-08-09-core-icu.md)) needs that
token.

### `error-context` is rejected

An `error-context` in a component's signature has no import mapping, not even to
the prelude's `ErrorContext`. A component whose signature carries one is
rejected where it is imported
(`cm_component_import_error_context_rejected.wado`).

### A world-level type export is rejected

A component exporting a type directly from its world, rather than from an
interface, is rejected.

### A world-level function carries only primitives and strings

A function a world exports directly takes and returns primitives and `string`.
A record, list or variant in that signature is not supported. The interface
path has no such limit.
