# WEP: WIT Interoperability

## Context

Wado has accumulated WIT-related design and implementation in pieces, driven by
concrete needs (CLI world, HTTP service world, WASI P3 effects, library
publishing). The pieces are scattered across multiple WEPs, and the end-to-end
goal — being able to consume an arbitrary `.wasm` component (or `.wit` package)
that the compiler has never seen before — has never been written down in one
place.

This WEP collects the existing pieces, captures the current state of the
implementation, and states the long-term goal explicitly.

### Existing WEPs in this area

| WEP                                                                                     | Scope                                                                                                              |
| --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)                            | Bidirectional type/structure mapping. `pub` vs `export`. `interface` vs `effect`.                                  |
| [World Conformance and Export Syntax](./wep-2026-01-16-world-conformance-and-export.md) | `contract <World>;` declaration and `export(World::name)` mapping syntax.                                          |
| [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)              | Type-driven lift/lower binding synthesis at the TIR layer.                                                         |
| [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md)                  | The producer side: embed WIT into output `.wasm` via `component-type` custom section.                              |
| [WebAssembly Module Import Support](./wep-2026-01-10-wasm-import.md)                    | Phase 1 (core wasm asset import) is delivered. Phase 2 (CM-boundary external `.wasm`) is now subsumed by this WEP. |
| [Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)                                 | The CM target is fixed to WASI Preview 3.                                                                          |

## Goal

Make the Wado compiler fully WIT-driven so that an arbitrary CM component can
be consumed without compiler changes. Concretely:

1. The user writes `use { Foo } from "./external.wasm" with { type: "wasm" };`
   (or depends on a packaged component via `wado.toml`).
2. The compiler reads the `component-type` custom section embedded in the
   component (the same section Wado already produces — see WIT Bundling).
3. The use resolver constructs Wado IR (worlds, interfaces, effects,
   resources, types) directly from that embedded WIT.
4. The CM binding synthesis lifts/lowers values at the boundary based on that
   IR, without any per-component, per-world, or per-resource hand-coding in
   the compiler.

There is no separate registry of "known WASI modules". The set of supported
worlds equals the set of WITs the compiler has parsed during this compilation.

## Non-Goals

- Supporting WASI Preview 1 or Preview 2. The CM target is P3.
- Parsing standalone `.wit` text files at runtime as a primary input. The
  primary input is the embedded `component-type` section. Standalone `.wit` is
  an authoring-time concern handled by `wado-from-idl`.
- Replacing `wado-from-idl`. It remains the build-time path for stdlib
  generation and for projects that want hand-curated `.wado` bindings.

## Current State

### What is already WIT-driven

| Aspect                                               | Mechanism                                                                           |
| ---------------------------------------------------- | ----------------------------------------------------------------------------------- |
| World definitions                                    | Parsed from `lib/wasi/**/worlds.wado`; `#[cm("...")]` carries the FQ name.          |
| Effect → CM import binding                           | Each effect method declares `#[cm_import("wasi:cli/stdout@...#write-via-stream")]`. |
| Resources, structs, enums, variants, flags, newtypes | Registered from stdlib `.wado` parsing; `#[cm(...)]` carries CM-side names.         |
| Entry function names (`run`, `handle`)               | Pulled from the world's declared `export` items, not hardcoded strings.             |
| HTTP detection                                       | Namespace prefix `wasi:http/` plus return-type shape (`Result<Response, _>`).       |
| CM lift/lower                                        | `synthesize_lift` / `synthesize_lower_to_flat` are recursive and type-driven.       |
| WIT in output                                        | Producer side embeds `component-type` (see WEP: WIT Bundling).                      |

### What is still hardcoded or specialized

- Default world fallback: `package.rs` uses `"wasi:cli/command"` when no
  `--world` flag is given. Acceptable as a default; not a structural problem.
- Synthetic `TEST_WORLD` constant in `world_registry.rs`. Used for the test
  harness; does not block external WIT support.
- HTTP handler specialization in `codegen/component.rs` (`has_http_handler_export`,
  `append_http_handler_export`, gated paths in `emit_world_exports`). This is
  the largest world-specific block left in the compiler and is the main item
  to design out as part of this work.
- `stdlib::ALL_WASI_MODULES` is an `include_str!`-driven static list. Adding a
  new WASI/CM library currently requires either putting the binding `.wado`
  in this list or feeding it through `CompilerHost`. Neither path reads
  embedded WIT directly.
- Cargo dependencies do not yet include `wit-parser` / `wit-component`. The
  compiler relies on `wado-from-idl`-generated `.wado` files as the source of
  truth.

### Stale items already cleaned up

- The `__pending_trailers_tx` Wasm global is gone. Trailers handling no
  longer relies on a global; the previous reference in WEP: TIR-Level CM
  Binding Synthesis has been removed.
- `synthesize_result_export_adapter` has been renamed to
  `synthesize_result_export_binding`. Naming is now consistent with
  `synthesize_void_export_binding` and `synthesize_general_export_binding`.

## Open Design Questions

### World structure faithfulness

WIT worlds are containers: a world bundles a set of imports and exports, and
the same interface can appear in multiple worlds with different roles. The
current Wado treatment flattens this — `effect`s are global, and the user
imports them with `use` regardless of which world is targeted.

Levels of faithfulness to consider:

- L1 (current): worlds declare entry points; effects are globally visible.
- L2: `contract <World>;` declarations verify that the module's effect usage
  is a subset of the world's imports (WEP: World Conformance, not yet
  implemented).
- L3: each world owns a scope of usable effects and interfaces. Importing an
  effect not declared by the active world is a compile error.
- L4: full WIT structure — `include`, `with`, world inheritance.

External WIT consumption realistically requires at least L2 and probably L3:
without per-world scope, two unrelated worlds whose imports share a name
(e.g. two `Logger` interfaces) collapse into one effect.

### `contract` declaration

WEP: World Conformance and Export Syntax defines the syntax. The parser does
not implement it. Designing L2/L3 above means deciding the runtime behavior
of `contract` before parser work starts.

### Effect vs interface

WEP: WIT and Wado Mapping splits these along import/export lines. That is
fine for stdlib bindings but needs to be checked against arbitrary external
components, where the Wado consumer may want to consume an exported interface
(non-effectful) or import an interface that is effectful in the consumer's
world.

## Roadmap

This WEP is a roadmap document. Each item below either has its own WEP or
will get one when work starts.

- [x] Producer side: embed `component-type` in output (WEP: WIT Bundling).
- [x] Type-driven CM binding synthesis (WEP: TIR-Level CM Binding Synthesis).
- [ ] Decide world structure faithfulness level (L2 vs L3) and document.
- [ ] Implement `contract` declaration with the chosen scope rules (revise
      WEP: World Conformance accordingly).
- [ ] Decouple HTTP handler specialization from codegen: drive it from world
      metadata rather than hardcoded predicates.
- [ ] Add `wit-parser` / `wit-component` as compiler dependencies, behind a
      use-resolver entry point that reads embedded `component-type` from
      external `.wasm` imports.
- [ ] Construct world / interface / effect / resource entries in the existing
      registries directly from parsed WIT, on the same code path as
      stdlib-derived entries.
- [ ] Close the binding-synthesis gaps required for arbitrary worlds: struct,
      variant, and `Result` parameter lifting; sync export support;
      return-via-outptr when flat count exceeds `MAX_FLAT_RESULTS`.
- [ ] Retire ad-hoc HTTP detection (`has_http_handler_export`,
      `append_http_handler_export`) once world-driven dispatch is in place.

## Consequences

### Positive

- A single, documented end-to-end goal for WIT support replaces a scatter of
  point WEPs.
- Adding a new WASI or third-party CM library no longer requires patching the
  compiler or the stdlib list.
- The producer (WIT Bundling) and consumer (this WEP) sides become
  symmetrical: a Wado-compiled component can be consumed by another Wado
  compiler with no extra metadata path.

### Negative

- Bringing `wit-parser` / `wit-component` into the compiler increases the
  dependency surface and binary size of the compiler itself.
- L3 world scoping changes the meaning of `use`: an `use` of an effect not
  in the active world's imports becomes a compile error. This is a
  user-visible behavior change.

### Trade-offs

- Reading WIT directly from `.wasm` makes external components first-class but
  diverges from the current "the `.wado` file is the source of truth" model.
  The two paths will coexist: stdlib stays `.wado`-first via `wado-from-idl`;
  external imports become WIT-first.

## References

- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)
- [World Conformance and Export Syntax](./wep-2026-01-16-world-conformance-and-export.md)
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
- [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md)
- [WebAssembly Module Import Support](./wep-2026-01-10-wasm-import.md)
- [Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)
- [WebIDL Binding Generator (`wado-from-idl`)](./wep-2026-04-01-tide.md)
