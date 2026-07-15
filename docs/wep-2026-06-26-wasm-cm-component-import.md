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
  - Treating the imported interface as effectful (requiring `with Iface`) is
    revisited by
    [Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md):
    for a pure component the effect should be derived from the component's own
    host-leaf imports (empty → no `with`), not from its exported interface.

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
`tests/fixtures/cm_component_import_catalog.wado` round-trips the full
**synchronous** value-type surface (primitives, `string`, `enum`, `flags`,
newtype, `List`, `Option`, `Result`, records, variants, tuples, and nested
combinations) against the composed `cm-catalog.wasm` at O0/O2, asserting
whole-value `==` (auto-derived `Eq`) on every aggregate.

The **async** value types — `stream<T>` and `future<T>` — are not yet handled;
that is Phase 8 below.

- [x] Phase 1 — fixture committed.
- [x] Phase 2 — shared type-mapping core (`CmShape`) + WIT→ast::Type (`wit_consume`).
- [x] Phase 3 — loader detect/decode/build-AST; the import resolves.
- [x] Phase 4 — registry provenance + `ImportKind::Component`; binding synthesis reused.
- [x] Phase 6 — codegen composes via `wasm-compose` (see below).
- [x] Phase 7 — e2e round-trip (synchronous value-type surface).
- [ ] Phase 8 — async value types (`stream<T>` / `future<T>`); see below.
- [ ] Phase 9 — world-level function exports (case A, no named types); see below.

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

The instance-type emitter and the binding synthesis cover the full value-type
surface for component-import params and returns. The work was almost entirely
removing drift between parallel code paths — for an imported component, a
type's `source_interface` namespace is arbitrary (`wado-lang:cm-catalog/...`), which
broke every WASI-only shortcut:

- **Component module-source provenance.** `register_component_decls` records each
  interface FQ → its `ModuleSource::Wasm` (mirroring `register_lib_local_decls`);
  the `lib_interface_sources` map is renamed `cm_interface_module_sources`.
  Without it, deriving a guest `ModuleSource` from a component FQ fell to the
  empty `Core("")` default, so component records/variants never got a concrete
  guest GC type and failed WIR build (`StructLiteral expected Ref WirType`).
- **Unified CM flattening.** Three near-identical flatteners
  (`cm_abi::cm_flat_types`, `component_model::flatten_cm_param_type`,
  `synthesis::flatten_param_type`) had drifted — conflicting tuple handling, a
  wrong `{i32,f32}` join, and a `wasi:`/`core:kiln/`-prefix gate that excluded a
  component's namespace, so records/variants/tuples collapsed to a single `i32`.
  They now delegate to one registry-aware `CmInterfaceRegistry::cm_flatten`. The
  outptr decision is unified into `cm_return_needs_outptr` so the binding and the
  core functype builder agree.
- **Registry-aware resolution everywhere.** `cm_type_to_type_id`, the lift's
  variant/enum reconstruction, and `cm_variant_size_align` all resolved CM named
  types via `find_named_type_by_cm_package` / `resolve_wasi_source_for` — a
  prefix/WASI-only scan that misses `Wasm`-sourced types. They now fall back to
  the FQ → `ModuleSource` provenance (`find_named_type_by_source`) /
  `resolve_cm_source_for`, so component records, variants, enums, and their CM
  layout sizes resolve to the concrete elaborator-registered type.
- **Missing lower paths filled.** `is_param_type_supported_with_types` accepts
  tuples; `synthesize_flatten_value_to_flat_args` flattens tuples and nested
  `List`; the `List<T>` element lower routes through the registry-aware
  `synthesize_lower_wasi_type_to_memory` (records-in-lists); option payload slots
  zero-init by slot type; and the result-case join uses `coerce_flat_lower`
  (bit-reinterpret) so an `f32`/`i32`-joined slot is not numerically truncated.

Working end-to-end (O0/O2), verified by `cm_component_import_catalog`:
primitives, `string`, `char`, `enum`, `flags`, newtype, `List`, `Option`,
`Result<ok, err>` (arbitrary `err`, including mismatched core classes), records,
variants (incl. payload-bearing and tuple payloads), tuples, and nested
combinations (`list<record>`, `option<record>`, `tuple<record, _>`,
`option<list>`, `result<list<record>, _>`, `list<tuple<record, _>>`, …).

- [x] Phase 5/6/7 — full synchronous value-type surface for params and returns.
- [ ] Phase 8 — async value types (`stream<T>` / `future<T>`). An imported
      component function whose signature carries a `stream`/`future` is neither
      tested nor wired through the import binding yet. This is the async-import
      surface (subtask / `AsyncCall`, WEP-2026-04-22).
- [ ] Phase 9 — world-level function exports (case A, no named types). Currently
      rejected by `wit_consume`, which handles only interface exports; world-level
      free-function imports also need an import-plan path that isn't interface-keyed.

## Notes

- Producer-side `--lib` WIT embedding currently warns "duplicate item named
  `cm-catalog`" (world and default interface share the package name) and skips
  the `component-type` section. Consumption decodes the component's own type, so
  this does not block the import, but it is a separate producer bug to fix.
