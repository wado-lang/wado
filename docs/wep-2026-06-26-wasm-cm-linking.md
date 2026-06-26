# WEP: Wasm CM Linking (`use`-based component import)

## Context

Core-wasm asset import (`use _ from "./x.wat" with { type: "wat" }`) is done
(WEP: WebAssembly Module Import Support, Phase 1). This WEP is the Component
Model analogue: importing functions from an external `.wasm` **component** via
`use { Catalog } from "./catalog.wasm" with { type: "wasm" }`, lowering/lifting
values at the CM boundary, and **linking** the dependency component into the
output so the result runs standalone.

This is the consumer side of [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md)
roadmap items "read embedded WIT from external `.wasm`" and "construct
world/interface/resource entries in the existing registries directly from
parsed WIT".

## Decisions

- The import target is a **component**, so the import kind is
  `ImportKind::Component`.
- **No Wado source text and no text WIT.** The component's binary type is
  decoded with `wit_component::decode` (which reconstructs WIT from a Wado
  component's own type — see WIT Interoperability §"Phase 2 finding"), and the
  result drives compiler IR construction directly.
- **Reuse the WIT↔Wado type mapping**, do not reinvent it. The structural core
  (`CmShape`, the `option`/`list`/`tuple`/`result` assembly rule, and the
  primitive correspondence) in `wit_emit.rs` is shared; the consumer adds only
  the `wit_parser::Type → ast::Type` direction on top of that shared core.
- **Model the imported interface as a Wado `interface`** (effectful), called
  `Catalog::id_u32(x)` after `use { Catalog }`. The entire CM import pipeline
  (import plan, binding synthesis, codegen) is keyed off
  `CmInterfaceRegistry::interfaces()` and `used_wasi_functions`
  (`Interface::method`, populated from effect usage), so a function-bearing WIT
  interface maps onto a Wado `interface` exactly like WASI. This reuses the
  whole pipeline; only codegen's *satisfaction* of the import differs.

## Design

### Pipeline position

The loader builds an `ast::Module` for the component programmatically (no source
text), inserts it into `loaded`, and the normal frontend
(bind/analyze/annotate) produces symbols, types, and `CmInterfaceRegistry`
entries — identical to how WASI bindings flow, except the module is synthesized
from decoded WIT rather than parsed from `lib/wasi/**`.

### Phases

1. **Fixture.** `wado compile --lib package-cm-catalog -o
   wado-compiler/tests/fixtures/cm-catalog.wasm` — the value-type ABI corpus,
   used as the link target for the e2e round-trip.

2. **Shared type-mapping core (refactor first).** Make `CmShape` reusable and
   add a `wit_parser::Type → ast::Type` mapper that reuses the shared structural
   assembly and primitive correspondence.

3. **Loader: detect + decode + build AST.** In `handle_wasm_import`, detect a
   component (wasmparser component header). `WasmAssetKind::Component`. Decode
   via `wit_component::decode`; for each exported interface build a
   `ast::Item::Interface` (with `#[cm(fq)]` + per-method `#[cm(fq#name)]` +
   `#[cm_params(...)]`) and its named types as top-level items with `#[cm(...)]`.
   `NamedType.source_interface` is set to the interface FQ directly. Insert the
   module into `loaded`; store the component bytes as a linkable asset keyed by
   the canonical `wasm:<path>` namespace.

4. **Registry provenance + import plan.** Record FQ → linked-component namespace
   so `resolve_import_plan` classifies the interface as `ImportKind::Component`
   instead of `FunctionInterface`. Registration uses the same
   `register_module_decls` path the stdlib uses, hooked per-compilation via
   `Arc::make_mut(&mut tysys.cm_interface_registry)` (alongside the existing
   `--lib` `register_lib_local_decls` call).

5. **Binding synthesis.** Reused unchanged: the type-driven lower(args)/lift(result)
   adapters are the same as for WASI imports.

6. **Codegen composition.** For each `ImportKind::Component`: `component_raw`
   embeds the dependency, `instantiate` runs it, `alias_export` pulls the
   exported interface instance and each used func, `lower_func` canon-lowers each
   into a core func (the host component's memory/realloc), and a core instance of
   those lowered funcs is supplied to the main core module's instantiation under
   the import namespace — the same slot WASI lowered funcs occupy. Reuses
   `lower_wasi_functions`; only the *source* of the component func differs
   (nested-instance export vs host import).

7. **E2E.** `tests/fixtures` round-trips `Catalog::id_*` against the fixture,
   asserting `lift(lower(x)) == x` across the value-type surface at O0/O2.

## Status

- [x] Phase 1 — fixture committed.
- [ ] Phase 2 — shared type-mapping core + WIT→ast::Type.
- [ ] Phase 3 — loader detect/decode/build-AST.
- [ ] Phase 4 — registry provenance + `ImportKind::Component`.
- [ ] Phase 6 — codegen composition.
- [ ] Phase 7 — e2e round-trip.

## Notes

- Producer-side `--lib` WIT embedding currently warns "duplicate item named
  `cm-catalog`" (world and default interface share the package name) and skips
  the `component-type` section. Consumption decodes the component's own type, so
  this does not block linking, but it is a separate producer bug to fix.
