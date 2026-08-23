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

## Not yet supported

- [ ] Resources and handles. A component exporting a `resource` — with its
      methods, static constructors, and `borrow<T>` parameters — is rejected
      when its type is decoded. Wado has resources; what is missing is the
      consuming direction of the mapping. It splits in two, and the first half
      is much the smaller: a `dtor`-less exported handle decodes to a copyable
      newtype (a [non-owning token](./wep-2026-05-21-resource-ownership.md)) with no ownership analysis
      to consume, where a `dtor`-bearing one needs the full affine mapping. The
      compile-time-bounded half of a bundled ICU surface
      ([`core:icu`](./wep-2026-08-09-core-icu.md)) rests on the first alone.
- [ ] Async value types (`stream<T>` / `future<T>`) in an imported signature.
      This is the async import surface of
      [Generic `AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md).
- [ ] World-level type exports. A component exporting a type directly from its
      world, rather than from an interface, is rejected.
- [ ] Component-defined named types in a world-level function signature. That
      path carries the primitive and string surface; records, lists, and
      variants in a world-level function remain future work. The interface path
      has no such restriction.

## Consequences

- A prebuilt component becomes usable from Wado with nothing to author or
  maintain alongside it — no binding module, no WIT copy. The artifact is the
  contract.
- The compiled output stays standalone: dependencies are composed in, not left
  as imports for a host to satisfy.
- Language boundaries disappear at the consumption site. A dependency written in
  any language that produces a component is called with ordinary Wado syntax and
  ordinary Wado types.
- A dependency's surface must stay within the supported value types. A component
  built around resources is unusable until that gap closes, which bounds which
  third-party components can be adopted today.
