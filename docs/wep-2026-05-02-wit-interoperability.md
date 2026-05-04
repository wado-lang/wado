# WEP: WIT Interoperability

## Context

Wado has accumulated WIT-related design and implementation in pieces, driven by
concrete needs (CLI world, HTTP service world, WASI P3 effects, library
publishing). The pieces are scattered across multiple WEPs, and the end-to-end
goal — being able to consume an arbitrary `.wasm` component (or `.wit` package)
that the compiler has never seen before — has never been written down in one
place.

This WEP collects the existing pieces, captures the current state of the
implementation, and states the long-term goal explicitly. It also makes a
shape-level decision (unifying `effect` and `interface`) that several earlier
WEPs left open or implicitly inconsistent.

### Existing WEPs in this area

| WEP                                                                                     | Scope                                                                                                                                             |
| --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)                            | Bidirectional type/structure mapping. `pub` vs `export`. Originally split `interface` and `effect`; superseded by the unification decision below. |
| [World Conformance and Export Syntax](./wep-2026-01-16-world-conformance-and-export.md) | `contract <World>;` declaration and `export(World::name)` mapping syntax.                                                                         |
| [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)              | Type-driven lift/lower binding synthesis at the TIR layer.                                                                                        |
| [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md)                  | The producer side: embed WIT into output `.wasm` via `component-type` custom section.                                                             |
| [WebAssembly Module Import Support](./wep-2026-01-10-wasm-import.md)                    | Phase 1 (core wasm asset import) is delivered. Phase 2 (CM-boundary external `.wasm`) is now subsumed by this WEP.                                |
| [Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)                                 | The CM target is fixed to WASI Preview 3.                                                                                                         |

## Goal

Make the Wado compiler fully WIT-driven so that an arbitrary CM component can
be consumed without compiler changes. Concretely:

1. The user writes `use { Foo } from "./external.wasm" with { type: "wasm" };`
   (or depends on a packaged component via `wado.toml`).
2. The compiler reads the `component-type` custom section embedded in the
   component (the section described by [WIT Bundling](./wep-2026-03-21-wit-bundling.md);
   the producer side is designed but not yet implemented).
3. The use resolver constructs Wado IR (worlds, interfaces, resources, types)
   directly from that embedded WIT.
4. The CM binding synthesis lifts/lowers values at the boundary based on that
   IR, without any per-component, per-world, or per-resource hand-coding in
   the compiler.

There is no separate registry of "known WASI modules". The set of supported
worlds equals the set of WITs the compiler has parsed during this compilation.

## Key Decision: Unify `effect` (declaration) into `interface`

WIT's organizing primitive is `interface`: a named group of free functions,
resources (with their methods), and types. Wado split this into two keywords —
`effect` for the import side (with effect tracking) and `interface` for the
export side (pure CM grouping). The split was justified in WEP: WIT and Wado
Mapping under the assumption that the import side carries Wado-level meaning
(effect tracking) while the export side does not.

In practice, `effect` blocks already contain free functions, resources, and
types. The keyword name no longer matches the contents, and the producer-side
`interface` keyword (a pure grouping) becomes a separate concept the user has
to learn.

This WEP collapses the two:

- `effect <Name> { ... }` declarations become `interface <Name> { ... }`.
- An interface contains free functions, resources, and types — exactly the
  WIT shape.
- The `with` clause continues to take interface names. `fn foo() with Stdout`
  reads as "this function uses the Stdout interface, which is effectful".
  Effect tracking semantics are unchanged.
- Worlds `import` and `export` interfaces by name. Each `import Foo` resolves
  to a known `pub interface Foo` declaration that carries `#[cm(...)]` and is
  therefore traceable back to its WIT FQ name.

The `effect` keyword is retained for one purpose only: **polymorphic effect
parameters**, e.g.

```wado
fn wrapper<effect E>(f: fn() with E) with E { f(); }
```

Here `E` is an effect variable, not an interface declaration. This is a
genuinely different concept (a type-level binder over effect rows) and keeping
the keyword for this case avoids overloading `interface` with a meaning it does
not have in WIT.

### What this resolves

- Effect-vs-interface ambiguity: `interface` is the single concept; `effect` is
  a binder for polymorphic effect parameters only.
- Producer-side keyword duplication: there is one keyword for both sides.
  `pub interface Geometry { ... }` defines and groups; `export Geometry` from a
  world publishes it.
- World import information loss (see Current State below): `import Foo` in a
  world is now a reference to an `interface Foo` declaration, which carries
  `#[cm(...)]`. The interface FQ name is preserved by construction.

### Cross-package disambiguation (resolved by omission)

WIT distinguishes `wasi:filesystem/types` from `wasi:sockets/types` by
package. Wado handles this asymmetrically:

- Function-bearing WIT interfaces have a Wado-side `pub interface Foo` with
  `#[cm(...)]` carrying the FQ. Worlds reference them by bare name
  (`import Stdout;`); the FQ is recovered from the interface declaration,
  so there is no need for `from "..."` qualification.
- Type-only WIT interfaces (e.g. `wasi:filesystem/types`,
  `wasi:sockets/types`) have no Wado-side `pub interface` declaration; their
  resources and types reach call sites via `use { Descriptor } from
  "wasi:filesystem/types.wado"`. `wado-from-idl` omits these from world
  declarations entirely. The previous `import Types { ... }` blocks (which
  produced the cross-package collision) are gone.

Result: `import` / `export` in worlds is always bare and unambiguous, and
the proposed `from "<package>"` / `as` qualifications are unnecessary.

### Pure interfaces

WIT has no purity annotation. An interface that contains only types (no
functions) is simply not effectful — it never appears in a `with` clause, and
users `use` its types directly. No new syntax is needed. An interface with
functions is conservatively treated as effectful by the call site.

## Migration Plan

The migration runs on a single feature branch and lands as one merge:

- [x] Replace `effect <Name> { ... }` with `interface <Name> { ... }` across
      `lib/wasi/**`, `lib/core/**`, `wado-compiler` internals, examples, fixtures,
      and docs.
- [x] Update `wado-from-idl` so its generated stdlib emits `interface` blocks.
- [x] Keep the `effect` keyword in the parser for `<effect E>` polymorphic effect
      parameters only. Remove `effect` as a declaration form.
- [x] Add `cm_interface_fq` to `WorldImportInfo` and populate it from the
      referenced interface's `#[cm(...)]`.
- [x] World imports/exports are bare WIT-faithful interface refs (`import Foo;`
      / `export Foo;`); the brace-block form has been removed.
- [ ] Update WEP: WIT and Wado Mapping to mark the interface/effect split as
      superseded.

There is no compatibility shim and no deprecation period. Wado is pre-stable
and this is a source-level rename plus a small registry change; a single
landing keeps the codebase consistent.

### Why no `from "<package>"` / `as` qualifications

The earlier draft of this WEP proposed cross-package qualifications
(`import { Types } from "wasi:filesystem"`, `import { Types as SocketsTypes }
from "wasi:sockets"`) for resolving local-name collisions in worlds. After
auditing the stdlib and consumers, we removed that requirement:

- The only collision in the current stdlib was `Types` (filesystem vs
  sockets), and neither side declares `pub interface Types` in Wado: those
  WIT interfaces are type-only, and their resources/types reach call sites
  through ordinary `use { Descriptor } from "wasi:filesystem/types.wado"`.
  `wado-from-idl` therefore omits `import Types;` from the world altogether,
  collapsing the collision.
- World imports inside `wado-compiler` are consumed exclusively by
  `WorldInfo::imports_interface(name)` (e.g., the kiln gating). Aliases
  would have no consumer; `from` would only restate information already
  carried by the referenced `pub interface Foo`'s `#[cm(...)]`.

If a future WIT file forces a non-removable collision (two `pub interface`
declarations sharing a Wado-side name), reconsider; until then the bare
form is unambiguous and minimal.

## Non-Goals

- Supporting WASI Preview 1 or Preview 2. The CM target is P3.
- Parsing standalone `.wit` text files at runtime as a primary input. The
  primary input is the embedded `component-type` section. Standalone `.wit` is
  an authoring-time concern handled by `wado-from-idl`.
- Replacing `wado-from-idl`. It remains the build-time path for stdlib
  generation and for projects that want hand-curated `.wado` bindings.

## Current State

### What is already WIT-driven

| Aspect                                               | Mechanism                                                                                          |
| ---------------------------------------------------- | -------------------------------------------------------------------------------------------------- |
| World definitions                                    | Parsed from `lib/wasi/**/worlds.wado`; `#[cm("...")]` carries the FQ name.                         |
| Interface → CM import binding (currently `effect`)   | Each method declares `#[cm_import("wasi:cli/stdout@...#write-via-stream")]`.                       |
| Resources, structs, enums, variants, flags, newtypes | Registered from stdlib `.wado` parsing; `#[cm(...)]` carries CM-side names.                        |
| Entry function names (`run`, `handle`)               | Pulled from the world's declared `export` items, not hardcoded strings.                            |
| HTTP detection                                       | Namespace prefix `wasi:http/` plus return-type shape (`Result<Response, _>`).                      |
| CM lift/lower                                        | `synthesize_lift` / `synthesize_lower_to_flat` are recursive and type-driven.                      |
| `(interface, name)` scoping for type lookups         | `source_interface` field disambiguates same-named types across interfaces (e.g. two `ErrorCode`s). |

### What is still hardcoded or specialized

- Default world fallback: `package.rs` uses `"wasi:cli/command"` when no
  `--world` flag is given. Acceptable as a default; not a structural problem.
- Synthetic `TEST_WORLD` constant in `world_registry.rs`. Used for the test
  harness; does not block external WIT support.
- HTTP handler specialization in `codegen/component.rs` (`has_http_handler_export`,
  `append_http_handler_export`, gated paths in `emit_world_exports`). This is
  the largest world-specific block left in the compiler and is the main item
  to design out as part of this work. With `WorldExportInfo::from_interface_fq`
  now populated, a Phase 2 step can drive HTTP detection from the interface
  FQ directly and retire `append_http_handler_export`.
- `stdlib::ALL_WASI_MODULES` is an `include_str!`-driven static list. Adding a
  new WASI/CM library currently requires either putting the binding `.wado`
  in this list or feeding it through `CompilerHost`. Neither path reads
  embedded WIT directly.
- Cargo dependencies do not yet include `wit-parser` / `wit-component`. The
  compiler relies on `wado-from-idl`-generated `.wado` files as the source of
  truth.
- Producer-side WIT embedding (the `component-type` custom section described
  in [WIT Bundling](./wep-2026-03-21-wit-bundling.md)) is designed but not
  yet implemented in codegen. Without it, a Wado-compiled component cannot
  be consumed via the embedded-WIT path described in this WEP's Goal.

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
current Wado treatment flattens this — interfaces are globally visible, and
the user imports them with `use` regardless of which world is targeted.

Levels of faithfulness to consider:

- L1 (current): worlds declare entry points; interfaces are globally visible.
- L2: `contract <World>;` declarations verify that the module's interface
  usage is a subset of the world's imports (WEP: World Conformance, not yet
  implemented).
- L3: each world owns a scope of usable interfaces. Importing an interface
  not declared by the active world is a compile error.
- L4: full WIT structure — `include`, `with`, world inheritance.

External WIT consumption realistically requires at least L2 and probably L3:
without per-world scope, two unrelated worlds whose imports share a name
(e.g. two `Logger` interfaces from different packages) would still need to be
disambiguated only by the cross-package syntax above, which is workable but
fragile if the user forgets to qualify.

### `contract` declaration

WEP: World Conformance and Export Syntax defines the syntax. The parser does
not implement it. Designing L2/L3 above means deciding the runtime behavior
of `contract` before parser work starts.

### "All methods" import form (resolved)

`wado-from-idl` now emits bare `import Foo;` (matching WIT's
`import stdout;`), and the brace-block form has been removed from the
parser. Per-method tree-shaking is driven by `used_wasi_functions`
(populated from actual call sites), so nothing depends on the world
declaration for which methods are eligible.

## Roadmap

This WEP is a roadmap document. Each item below either has its own WEP or
will get one when work starts.

- [x] Type-driven CM binding synthesis (WEP: TIR-Level CM Binding Synthesis).
- [x] Unify `effect` declarations into `interface` (single-branch migration;
      see Migration Plan above). Retain `effect` keyword for `<effect E>`
      polymorphic effect parameters.
- [x] World imports/exports are bare WIT-faithful interface refs
      (`import Foo;` / `export Foo;`); brace-form removed. `WorldImportInfo`
      and `WorldExportInfo` carry `cm_interface_fq` resolved from the
      referenced `pub interface Foo`'s `#[cm(...)]`.
- [ ] Producer side: embed `component-type` in output (WEP: WIT Bundling).
      Designed but not implemented — required before round-trip Wado-to-Wado
      consumption via the embedded-WIT path can work.
- [ ] Decide world structure faithfulness level (L2 vs L3) and document.
- [ ] Implement `contract` declaration with the chosen scope rules (revise
      WEP: World Conformance accordingly).
- [ ] Decouple HTTP handler specialization from codegen: drive it from
      `WorldExportInfo::from_interface_fq` rather than the return-type sniffer
      (`returns_http_response`) and the post-hoc `append_http_handler_export`.
- [ ] Add `wit-parser` / `wit-component` as compiler dependencies, behind a
      use-resolver entry point that reads embedded `component-type` from
      external `.wasm` imports.
- [ ] Construct world / interface / resource entries in the existing
      registries directly from parsed WIT, on the same code path as
      stdlib-derived entries.
- [ ] Close the binding-synthesis gaps required for arbitrary worlds: struct,
      variant, and `Result` parameter lifting; sync export support;
      return-via-outptr when flat count exceeds `MAX_FLAT_RESULTS`.
- [ ] Retire ad-hoc HTTP detection (`has_http_handler_export`,
      `append_http_handler_export`) once world-driven dispatch is in place.
- [ ] Emit `wasi:cli/run@<v>` and `wasi:http/handler@<v>` as proper CM
      instance exports in `emit_world_exports` (currently top-level
      function exports + post-hoc wrap).

## Consequences

### Positive

- A single, documented end-to-end goal for WIT support replaces a scatter of
  point WEPs.
- One keyword (`interface`) for both the import and export sides, matching
  WIT's vocabulary directly. The `effect` keyword keeps a narrow,
  well-defined role (polymorphic effect parameters).
- World imports are traceable to WIT FQ names by construction, removing the
  fragile method-name-based disambiguation in `WasiRegistry`.
- Adding a new WASI or third-party CM library no longer requires patching the
  compiler or the stdlib list.
- The producer (WIT Bundling) and consumer (this WEP) sides become
  symmetrical: a Wado-compiled component can be consumed by another Wado
  compiler with no extra metadata path.

### Negative

- A one-shot rename of `effect` to `interface` touches the entire stdlib and
  any user code that declared effects. There is no migration period; this is
  a single-branch change.
- Bringing `wit-parser` / `wit-component` into the compiler increases the
  dependency surface and binary size of the compiler itself.
- L3 world scoping changes the meaning of `use`: a `use` of an interface not
  in the active world's imports becomes a compile error. This is a
  user-visible behavior change.

### Trade-offs

- Reading WIT directly from `.wasm` makes external components first-class but
  diverges from the current "the `.wado` file is the source of truth" model.
  The two paths will coexist: stdlib stays `.wado`-first via `wado-from-idl`;
  external imports become WIT-first.
- Reusing `interface` for both effectful and pure groupings means the
  presence of effect tracking is decided per-call (does this function call an
  effectful interface member?) rather than per-declaration. This is closer to
  WIT's lack of purity annotation and removes a Wado-specific concept users
  had to learn.

## References

- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)
- [World Conformance and Export Syntax](./wep-2026-01-16-world-conformance-and-export.md)
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
- [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md)
- [WebAssembly Module Import Support](./wep-2026-01-10-wasm-import.md)
- [Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)
- [WebIDL Binding Generator (`wado-from-idl`)](./wep-2026-04-01-tide.md)
