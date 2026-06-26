# WEP: Wasm CM Component Import (`use`-based)

## Context

Core-wasm asset import (`use _ from "./x.wat" with { type: "wat" }`) is done
(WEP: WebAssembly Module Import Support, Phase 1). This WEP is the Component
Model analogue: importing functions from an external `.wasm` **component** via
`use { Catalog } from "./catalog.wasm" with { type: "wasm" }`, lowering/lifting
values at the CM boundary, and composing the dependency component into the
output so the result runs standalone.

This is the consumer side of [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md)
roadmap items "read embedded WIT from external `.wasm`" and "construct
world/interface/resource entries in the existing registries directly from
parsed WIT".

## Decisions

- The import target is a **component**, so the import kind is
  `ImportKind::Component`.
- **No Wado source text and no text WIT.** The component's binary type is
  decoded with `wit_component::decode` (which reconstructs the WIT interface from
  a Wado component's own type — see WIT Interoperability §"Phase 2 finding"), and
  the result drives compiler IR construction directly.
- **Reuse the WIT↔Wado type mapping**, do not reinvent it. The structural core
  (`CmShape`, the `option`/`list`/`tuple`/`result` assembly rule, and the
  primitive correspondence) in `wit_emit.rs` is shared; the consumer adds only
  the `wit_parser::Type → ast::Type` direction on top of that shared core.
- **Map the imported interface faithfully** to a Wado `interface` (effectful),
  called `Catalog::id_u32(x)` after `use { Catalog }`. The entire CM import
  pipeline (import plan, binding synthesis, codegen) is keyed off
  `CmInterfaceRegistry::interfaces()` and `used_wasi_functions`
  (`Interface::method`, populated from effect usage), so a function-bearing WIT
  interface maps onto a Wado `interface` exactly like WASI. This reuses the
  whole pipeline; only codegen's _satisfaction_ of the import differs.

## Design

### Pipeline position

The loader builds an `ast::Module` for the component programmatically (no source
text), inserts it into `loaded`, and the normal frontend
(bind/analyze/annotate) produces symbols, types, and `CmInterfaceRegistry`
entries — identical to how WASI bindings flow, except the module is synthesized
from decoded WIT rather than parsed from `lib/wasi/**`.

### Phases

1. **Fixture.** `wado compile --lib package-cm-catalog -o
   wado-compiler/tests/fixtures/sub/cm-catalog.wasm` — the value-type ABI corpus,
   used as the dependency component for the e2e round-trip.

2. **Shared type-mapping core (refactor first).** Make `CmShape` reusable and
   add a `wit_parser::Type → ast::Type` mapper that reuses the shared structural
   assembly and primitive correspondence.

3. **Loader: detect + decode + build AST.** In `handle_wasm_import`, detect a
   component by its binary header (`is_wasm_component`) and branch. Decode
   via `wit_component::decode`; for each exported interface build a
   `ast::Item::Interface` (with `#[cm(fq)]` + per-method `#[cm(fq#name)]` +
   `#[cm_params(...)]`) and its named types as top-level items with `#[cm(...)]`.
   `NamedType.source_interface` is set to the interface FQ directly. Insert the
   module into `loaded`; store the component bytes as a dependency asset keyed by
   the canonical `wasm:<path>` namespace.

4. **Registry provenance + import plan.** Record FQ → dependency-component
   namespace so `resolve_import_plan` classifies the interface as
   `ImportKind::Component` instead of `FunctionInterface`. Registration uses the
   same `register_module_decls` path the stdlib uses, hooked per-compilation via
   `Arc::make_mut(&mut tysys.cm_interface_registry)` (alongside the existing
   `--lib` `register_lib_local_decls` call).

5. **Binding synthesis.** Reused unchanged: the type-driven lower(args)/lift(result)
   adapters are the same as for WASI imports.

6. **Codegen composition.** The program imports each `ImportKind::Component`
   interface like a host interface, then `compose_dependency_components` composes
   the dependency in with `wasm-compose` (see "Codegen" below).

7. **E2E.** `tests/fixtures` round-trips `Catalog::id_*` against the fixture,
   asserting `lift(lower(x)) == x` across the value-type surface at O0/O2.

## Status

Working end-to-end: `use { Iface } from "./c.wasm" with { type: "wasm" }` →
`Iface::method(x)` resolves, composes, and round-trips at runtime. E2E fixture
`tests/fixtures/cm_component_import_catalog.wado` round-trips the full primitive
surface, `string`, and `enum` against the composed `cm-catalog.wasm` at O0/O2.

- [x] Phase 1 — fixture committed.
- [x] Phase 2 — shared type-mapping core (`CmShape`) + WIT→ast::Type (`wit_consume`).
- [x] Phase 3 — loader detect/decode/build-AST; the import resolves.
- [x] Phase 4 — registry provenance + `ImportKind::Component`; binding synthesis reused.
- [x] Phase 6 — codegen composes via `wasm-compose` (see below).
- [x] Phase 7 — e2e round-trip (primitives/string/enum).

### Codegen: fused composition via `wasm-compose`

The program component imports the dependency interface like a host
function-interface (`generate_cm_imports` treats `ImportKind::Component` exactly
like `FunctionInterface`). `compose_dependency_components` then statically
composes it: using `wasm-compose`'s in-memory `CompositionGraph`, each dependency
component is instantiated and its exported interface connected to the program's
matching import; both components' remaining imports (host WASI) are surfaced and
merged by name, and the program's exports are re-exported.

This replaced an earlier host-mediated approach (`canon lower` the dependency's
export into the program's imports) that validated but trapped `CannotEnterComponent`
at runtime: with concurrency support on (always, under WASI P3), the canonical
ABI forbids the host re-entering a top-level instance already on the stack, and
wasmtime elides that check only for **fused guest-to-guest adapters** — exactly
what `wasm-compose` produces. `wasm-compose` also handles the import
union/forwarding automatically (the dependency's own `wasi:cli/types` / `stderr`
panic-path imports become the composed component's imports).

### Value-type surface (params + lower/lift)

The instance-type emitter and the binding synthesis now cover most of the
value-type surface for component-import params and returns. Two fixes made this
work, both correcting drift between parallel code paths rather than adding new
ones:

- **Unified CM flattening.** Three near-identical flatteners
  (`cm_abi::cm_flat_types`, `component_model::flatten_cm_param_type`,
  `synthesis::flatten_param_type`) had drifted — conflicting tuple handling, a
  wrong `{i32,f32}` join, and a `wasi:`/`core:kiln/`-prefix gate that excluded a
  component's own package namespace, so records/variants/tuples collapsed to a
  single `i32`. They now delegate to one registry-aware
  `CmInterfaceRegistry::cm_flatten`. The outptr decision is likewise unified into
  `cm_return_needs_outptr`, so the binding and the core functype builder agree.
- **Component module-source provenance.** `register_component_decls` now records
  each interface FQ → its `ModuleSource::Wasm` (mirroring `register_lib_local_decls`).
  Without it, deriving a guest `ModuleSource` from a component FQ fell back to the
  empty `Core("")` default, so component records/variants never got a concrete
  guest GC type and failed WIR build. The `lib_interface_sources` map was renamed
  `cm_interface_module_sources` to reflect that it now serves `--lib` *and*
  component imports.

Working end-to-end (O0/O2): primitives, `string`, `enum`, `flags`, `List`,
`Option`, `Result<ok, err>` (arbitrary `err`), and records.

### Known gaps (follow-ups)

- **Variant and tuple import params.** Variant params still emit a broken
  discriminant/payload Match, and tuple params/returns still cast the GC tuple to
  `i32` — the value-flattening lower path needs the same registry-aware treatment
  the signature side now has.
- **World-level function exports** (case A, no named types) are rejected by
  `wit_consume` — only interface exports are handled. World-level free-function
  imports also need an import-plan path that isn't interface-keyed.

## Notes

- Producer-side `--lib` WIT embedding currently warns "duplicate item named
  `cm-catalog`" (world and default interface share the package name) and skips
  the `component-type` section. Consumption decodes the component's own type, so
  this does not block the import, but it is a separate producer bug to fix.
