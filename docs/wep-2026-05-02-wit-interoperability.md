# WEP: WIT Interoperability

## Context

Wado's WIT support accumulated in pieces, each driven by a concrete need: the CLI
world, the HTTP service world, WASI P3 effects, library publishing. The pieces
live in separate WEPs, and the end-to-end goal they serve — producing and
consuming an arbitrary Component Model component with no compiler change — was
never stated in one place.

This WEP is that place. It states the goal, owns the shape-level decisions
several of the point WEPs left open, and specifies the producer side end to end.
The consumer side has since grown its own WEPs and is referenced rather than
restated here.

### Related WEPs

| WEP                                                                                     | Scope                                                                          |
| --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ |
| [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)                            | The bidirectional type and structure mapping, and `export` as the CM boundary. |
| [World Conformance and Export Syntax](./wep-2026-01-16-world-conformance-and-export.md) | The `contract <World>;` declaration and `export(World::name)` mapping syntax.  |
| [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)              | Type-driven lift/lower synthesis at the boundary.                              |
| [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md)                  | The `component-type` custom-section format.                                    |
| [WebAssembly Module Import Support](./wep-2026-01-10-wasm-import.md)                    | Core-wasm asset import; the CM analogue lives in its own WEP below.            |
| [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)                | The consumer side: importing an external `.wasm` component.                    |
| [Effect Reconstruction](./wep-2026-07-15-cm-import-effect-reconstruction.md)            | What effects an imported component's interface carries.                        |
| [Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)                                 | The CM target generation.                                                      |

## Goal

The compiler is WIT-driven in both directions, so that neither producing nor
consuming a component requires per-component, per-world, or per-resource code in
the compiler.

Producing: the frontend's view of a module — its declared interfaces, exported
items, active world, and type table — is emitted as WIT and embedded in the
output component, so a Wado artifact is a first-class CM citizen.

Consuming: a component's own type is decoded into compiler IR, and the binding
synthesis lifts and lowers against that IR. There is no registry of "known
modules": the set of supported worlds is the set of WITs parsed during this
compilation.

## Decision

### `interface` is the only block declaration

WIT's organizing primitive is `interface`: a named group of free functions,
resources with their methods, and types. Wado has one block keyword matching it.
There is no parallel `effect Foo { ... }` block form.

```wado
interface Stdout { ... }
```

`with` clauses take interface names and effect tracking is unchanged. The
`effect` keyword survives for exactly one purpose — polymorphic effect
parameters, where the binder ranges over effect rows rather than naming a CM
interface, a Wado concept WIT has no equivalent for:

```wado
fn wrapper<effect E>(f: fn() with E) with E { f(); }
```

<<<<<<< HEAD
The consequences are that one keyword covers both the import and the export side;
that `pub interface Geometry { ... }` defines and groups while `export Geometry`
in a world publishes, with no producer-side keyword duplication; and that a
world's `import Foo;` is a reference to an interface declaration carrying its CM
fully-qualified name, so the FQ is preserved by construction rather than
recovered by heuristics.

### Cross-package names are bare and unambiguous

WIT distinguishes `wasi:filesystem/types` from `wasi:sockets/types` by package.
Wado needs no qualification syntax for this, because the two kinds of interface
are handled asymmetrically:

- A function-bearing WIT interface has a Wado `pub interface` declaration
  carrying the FQ. Worlds reference it by bare name; the FQ comes from the
  declaration.
- A type-only WIT interface has no Wado interface declaration at all. Its
  resources and types reach call sites through ordinary `use` of the generated
  binding module, and it is omitted from world declarations entirely.

The only collision the stdlib ever presented was `Types`, and both sides are
type-only, so it collapses. Should two `pub interface` declarations ever be
forced to share a Wado-side name, this decision is worth revisiting; until then
`from "<package>"` / `as` qualifications would restate information the
declaration already carries.

### Purity and effects

WIT has no purity annotation, so purity is inferred rather than declared. An
interface holding only types is not effectful: it never appears in a `with`
clause and users `use` its types directly.

For an interface the host satisfies — WASI — an interface with functions is
conservatively effectful at the call site. For an interface an imported component
satisfies, effectfulness is reconstructed from that component's own host-leaf
imports, so a purely computational component maps to a namespace and needs no
`with`. See
[Effect Reconstruction](./wep-2026-07-15-cm-import-effect-reconstruction.md).

## Producer side

### WIT is a frontend fact

Everything WIT describes — declared interfaces, exported items, the active world,
the type table — is fully determined by name and type resolution.
Monomorphization, lowering, optimization, and codegen contribute nothing that
belongs in WIT. So WIT emission takes the frontend's semantic output plus the
target world and produces two artifacts: a WIT text document for `wado wit`, and
a `component-type` custom-section payload derived from that text for
`wado compile` to append.

This keeps codegen's principle intact — it emits the package as is, with no
knowledge of earlier phases. Embedding is a postprocess over finished component
bytes plus precomputed WIT text, not a codegen concern.

### Type mapping and naming

The Wado→WIT direction of the mapping table in
[WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md) applies. Closures,
polymorphic effect parameters in exported signatures, and generics with bounds
that WIT cannot represent are out of scope; encountering one in an exported
signature is a compile error naming the offending parameter at its source span.

Wado identifiers become WIT kebab-case (`MyApi` → `my-api`, `set_level` →
`set-level`). Two declarations that collide after kebabification are a compile
error surfacing both spans.

The structural constructors — `option`, `list`, `tuple`, `result`, `future`,
`stream` — are assembled by one shared rule over a single-level shape
classification. Both front-ends, resolved user types and CM-registry signatures,
only classify their input and render leaves, so they cannot drift in how a shape
becomes WIT. A resource value maps to `own<R>`, a reference to it to `borrow<R>`.

### Interface grouping and the default interface

- Bare `export fn` / `export struct` / … form a default interface named after the
  package (or the entry file's stem, with no manifest). If only functions are
  exported, they become direct world exports instead.
- `export interface Foo { item1, item2, … }` groups named items into one WIT
  interface. The items are defined elsewhere; the block lists names.
- World-conformance entry points — `run` for the CLI world, `handle` for the HTTP
  service world — are always direct world exports.
- Types referenced transitively by an exported signature are pulled into the
  owning interface even when not listed.
- With no entry point at all, the world is emitted empty and the interfaces stand
  alone in the package.

### Libraries and the anonymous root world

A source file with only `export` items and no world entry point is a library,
declared by `[package].lib` and built with `wado compile --lib`. Its world is
synthesized from the entry module's export signatures rather than looked up in
the stdlib world table. Library exports lift synchronously, and the default
allocator is the free-list.

A component's type is anonymous — the binary encodes it as a bare list of imports
and exports with no world name, and WIT models a world as a text-level label over
that type. Decoding a library component yields `world root { … }`: the name is
discarded and re-synthesized. A library's world therefore carries no identity;
only its interface does, and the identity consumers import is the default
interface `<namespace>:<name>/<name>@<version>`.

So the library world is emitted as `root`, deliberately distinct from the default
interface. Worlds and interfaces share one per-package namespace, so naming both
after the package is a duplicate-item error that silently drops the embedded
section — which is why `[package].name` may not be `root`. The world's emit name
is textual only; the default-interface FQ is the codegen identity and the
component's real export. `wado wit --lib` renders the same shape, so text and
embedded section agree.

Export grouping follows one rule, applied by both the WIT emitter and codegen:

- No exported signature references a named user type → the exports are direct
  world exports, with structural types inlined.
- Some exported signature references a named user type → the exports plus the
  transitive closure of their named types are grouped into the default interface
  and exported as an instance. This is forced by WIT: a world export list admits
  functions and interfaces, never a bare type, so a named type reaches consumers
  as a reusable entity only through an exported interface.

Codegen mirrors this with no world-specific special-casing. The entry module's
exported and transitively referenced named types are registered under the
default-interface FQ, and both the export type plan and the type emitter resolve
named types purely through that registry, so library-local types need no separate
path. The value-type emitter is the same recursive engine that serves WASI
imports, generalized over a type sink so it emits either into an instance type or
as top-level component types for direct world exports.

### Faithful imports

The world's import set must equal what the compiled component actually imports;
otherwise the embedded section misrepresents the binary. Deriving the set
semantically from effect rows and the type closure is not faithful — it
over-includes type-alias-only interfaces and misses implicit imports such as the
stderr write on the assert and panic path, which no `with` clause mentions.

So the complete import and export interface set is built once as structured data
at the WIR layer, after dead-code elimination has settled which functions are
used, and codegen merely emits it. Each entry carries a kind that tells codegen
how to encode it without re-deriving any interface or world decision. `wado wit`
and the embedder read the same plan; the full-scope nested-package bodies stay a
separate type closure, which follows aliases so a `use`d alias resolves.

### Text and section are different deliverables

A Wado artifact is a full component, not a core module, so it already
self-describes: a decoder reconstructs the WIT from the component's own typed
imports and exports. A `component-type` custom section on an already-formed
component is carried opaquely and never read by that path.

The embedded section is therefore additive full-fidelity metadata, not the
mechanism that makes the binary inspectable. Its value over the intrinsic type is
that the intrinsic type is always tree-shaken to the used surface, whereas the
payload carries complete upstream interface bodies, exact package versions, and a
producers record — the convention that packaging and relink tooling consumes.
Round-tripping accordingly has two halves: decoding the component matches
`wado wit` semantically, and the embedded payload decodes standalone as a WIT
package.

This splits the two roles cleanly. `wado wit` writes text, for humans to read and
tools to diff; it is an inspection aid, not the deliverable. `wado compile`
embeds the binary section, which is what makes the output composable,
publishable, transpilable, and consumable by another Wado compiler.

It also settles scope. `wado wit --scope` chooses whether interfaces referenced
by the world are inlined in full or left as bare references for an external
registry to resolve; `full` is the default and self-describes, `local` is smaller
and focused on the package's own contract. Embedding has no such choice: a
component type is structural and self-contained, so the section is always the
full closure. That is a Wasm CM property, not a Wado policy, which is why there
is no scope knob on `wado compile` and no manifest key for one.
||||||| 2ed97d2fd
### What this resolves

- One keyword (`interface`) for both the import and export block forms,
  matching WIT's vocabulary directly. `effect` keeps a narrow, well-defined
  role (polymorphic effect parameters).
- `pub interface Geometry { ... }` defines and groups; `export Geometry`
  from a world publishes it. No producer-side keyword duplication.
- World import information loss is gone: `import Foo` in a world is a
  reference to an `interface Foo` declaration carrying `#[cm(...)]`, so
  the FQ name is preserved by construction.

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

Superseded for imported `.wasm` components by
[Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md).
A fused component's interface is
effectful only insofar as the component's own host-leaf imports are — a
purely-computational import needs no `with`. The conservative rule stands
unchanged for host-satisfied (WASI) interfaces.

## Migration Plan

The migration runs on a single feature branch and lands as one merge:

- [x] Replace `effect <Name> { ... }` with `interface <Name> { ... }` across
      `lib/wasi/**`, `lib/core/**`, `wado-compiler` internals, examples, fixtures,
      and docs.
- [x] Update `wado-from-idl` so its generated stdlib emits `interface` blocks.
- [x] Add `cm_interface_fq` to `WorldImportInfo` and populate it from the
      referenced interface's `#[cm(...)]`.
- [x] World imports/exports are bare WIT-faithful interface refs (`import Foo;`
      / `export Foo;`); the brace-block form has been removed.
- [x] Retain the `effect` keyword for polymorphic effect parameters
      (`<effect E>`). The block-declaration unification does not extend
      to type parameters: an effect variable binds an effect row, which
      is a Wado-specific concept WIT has no equivalent for. The current
      parser support stays.
- [x] Update WEP: WIT and Wado Mapping to mark the interface/effect split as
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
| Interface → CM import binding                        | Each method declares `#[cm_import("wasi:cli/stdout@...#write-via-stream")]`.                       |
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
- `stdlib::ALL_WASI_MODULES` is an `include_str!`-driven static list. Adding a
  new WASI/CM library currently requires either putting the binding `.wado`
  in this list or feeding it through `CompilerHost`. Neither path reads
  embedded WIT directly.
- `wado-compiler` still relies on `wado-from-idl`-generated `.wado` files as
  the source of truth; no path reads embedded WIT from external `.wasm` yet
  (consumer side, still open). `wit-parser`, `wit-encoder`, and `wit-component`
  are all in `wado-compiler`'s `[dependencies]` for the producer side; the
  consumer-side `component-type` reader still reuses these crates when it lands.
- Producer-side WIT embedding (the `component-type` custom section described in
  [WIT Bundling](./wep-2026-03-21-wit-bundling.md)) is **done** (Phase 2):
  `wado compile` embeds by default via `wit_bundle::embed_component_type`. Note
  the Phase 2 finding below — the Wado component is already self-describing, so
  the section is additive full-fidelity metadata rather than the inspection
  mechanism.

Resolved since the first draft: HTTP handler specialization in
`codegen/component.rs` is gone — CM imports/exports are now emitted from the
WIR import plan and registry-derived descriptors with no HTTP/world literals
(see §"Codegen Genericization", P1–P5).

### Stale items already cleaned up

- The `__pending_trailers_tx` Wasm global is gone. Trailers handling no
  longer relies on a global; the previous reference in WEP: TIR-Level CM
  Binding Synthesis has been removed.
- `synthesize_result_export_adapter` has been renamed to
  `synthesize_result_export_binding`. Naming is now consistent with
  `synthesize_void_export_binding` and `synthesize_general_export_binding`.

## Producer Side: WIT Generation and Embedding

This section is the detailed design for the producer-side roadmap item
"embed `component-type` in output". It implements the format spec from
[WIT Bundling](./wep-2026-03-21-wit-bundling.md) and the Wado↔WIT mapping
from [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md). Anything
left open in those two documents (default behavior, CLI shape, where the
generator hooks into the pipeline) is resolved here.

### Position in the pipeline

WIT generation is a function of the _frontend output_. Describing a
component's interface — declared interfaces, exported items, the active
world, the type table — is fully determined by name resolution and type
resolution. Monomorphization, lowering (TIR → NIR → WIR), optimization,
and codegen do not contribute information that belongs in WIT.

The producer side therefore takes [`Semantics`](../wado-compiler/src/semantics.rs)
plus the target world FQ and produces:

- A WIT text document (consumed by `wado wit`).
- A `component-type` custom section payload, derived from that WIT text
  via `wit-parser` + `wit-component::metadata::encode` (embedded by
  `wado compile`, which is default-on).

Codegen is unaffected. The `codegen.rs` principle ("emits `Package` as is,
without knowledge of earlier phases") still holds; the WIT bundle is
appended as a postprocess step that takes the completed component bytes
plus the precomputed WIT text.

### Semantics additions

`Semantics` is the contract output of the frontend, so every fact needed
to emit WIT lives on it. Phase 0 landed the registry accessors; Phase 1's
emitter read interfaces and exported items off those registries and the
loaded TIR directly, so no further `Semantics` accessor was required.

Phase 0 (landed):

| Accessor                             | Returns                                                                 |
| ------------------------------------ | ----------------------------------------------------------------------- |
| `Semantics::world_registry()`        | `Option<&'static WorldRegistry>` — the parsed `world` table.            |
| `Semantics::cm_interface_registry()` | `Option<&'static CmInterfaceRegistry>` — the parsed CM interface table. |

Both registries are already built by `Elaborator::annotate_modules` (which
calls `CmInterfaceRegistry::build_from_stdlib`, an `OnceLock`-cached
singleton) and live on `AnnotateState`. The accessors surface them
without re-running stdlib parsing, so LSP, `wado wit`, and batch
compilation share the same instance. `FlatPackage` / `Package` /
`NirPackage` continue to carry the same `&'static` references they
already held — Phase 0 added access without changing how the data is
threaded.

Phase 1 (landed, simpler than planned): the proposed `Semantics::interfaces()`
/ `exported_items()` indices were not needed. `emit_wit_text` reads the
exported items off the loaded TIR modules and the interface bodies off the
Phase 0 registries directly, so no new `Semantics` accessor was added. The
remaining inputs are `WitEmitOptions` fields threaded from the CLI:
`default_interface_name` (`[package].name` or entry-file stem) and
`world_imports` (the faithful WIR import plan — a project/codegen fact, not a
frontend one).

The wir-build / codegen path continues to consume `Package` as today. The
new WIT path is a sibling reader of `Semantics`; it does not touch
`Package`.

### Module layout

Two new modules under `wado-compiler/src/`:

- `wit_emit.rs` — `emit_wit_text(&Semantics, &WitEmitOptions) -> Result<String, WitEmitError>`.
- `wit_bundle.rs` — `embed_component_type(component_bytes: &[u8], &Semantics, &WitEmitOptions) -> Result<Vec<u8>, WitEmitError>`.

`wit_emit` depends on `wit-encoder` for WIT text production. `wit_bundle`
depends on `wit_emit` for text, parses it with `wit-parser` into a
`Resolve`, encodes the component-type via `wit-component::metadata::encode`,
and appends the custom section to the component the codegen already
produced.

`wit-encoder` and `wit-component` are pinned in `[workspace.dependencies]`
alongside the existing `wit-parser` (all at generation `0.246`, matching
wasmtime's tree). `wado-compiler` adds them to its own `Cargo.toml` in
Phase 1.

### Type mapping

Implements the table in
[WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md) §"WIT to Wado
Type Mapping". Direction here is Wado → WIT (the existing table reads in
both directions, but the producer only needs one).

Out of scope for the first cut: closures, polymorphic effect parameters in
exported signatures, generics with non-WIT-representable bounds.
Encountering one in an exported signature is a compile error with a
diagnostic that names the offending parameter and points at its source
span.

Name conversion: Wado identifiers (`distance`, `MyApi`, `set_level`) become
WIT kebab-case (`distance`, `my-api`, `set-level`). Conflicts after
kebabification (e.g. two declarations colliding) are a compile error at WIT
emit time with both source spans surfaced.

The structural constructors (`option` / `list` / `tuple` / `result` /
`future` / `stream`) are assembled by one shared rule (`wit_emit::assemble`
over a single-level `CmShape` classification). The two front-ends — resolved
types (user exports) and CM-registry AST signatures (`full`-scope interface
reconstruction) — only classify their input and render leaves, so the two
paths cannot drift in how a shape becomes WIT. `own<R>`/`borrow<R>` map from a
resource value / `&resource` respectively.

### Interface grouping

Drives off the `pub interface` and `export interface` shapes already
established in [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md):

- Bare `export fn` / `export struct` / ... form a **default interface**
  named after `default_interface_name`. If only functions are exported
  (no types), they become direct world exports instead.
- `export interface Foo { item1, item2, ... }` blocks group named items
  into one WIT interface. Items are defined elsewhere; the block lists
  names.
- World-conformance entry points (`fn run` for `wasi:cli/command`,
  `fn handle` for `wasi:http/service`) are always direct world exports.
- Types referenced transitively by exported signatures are pulled into
  the owning interface even if not explicitly listed.
- `#![no_default_interface]` disables the default-interface fallback;
  every non-entry-point export must live in an explicit `export interface`.
- When no entry point is present at all, the world is emitted empty and the
  interfaces stand alone in the package (see "World-less libraries").

### Imported interface resolution and scope

For every `import Foo;` in the target world, the emitter looks up the
referenced `pub interface Foo` via `Semantics::interfaces`, reads its
`#[cm("...")]` FQ name, and uses that FQ name in `world { import ...; }`.

What the WIT document contains for each referenced interface depends on
the requested emit scope:

| Scope   | Behavior                                                                                                      |
| ------- | ------------------------------------------------------------------------------------------------------------- |
| `full`  | Inline the full body of every interface referenced by the world (user-authored _and_ stdlib WASI/CM).         |
| `local` | Inline only user-authored interfaces. Stdlib references appear as `import wasi:cli/stdout@<v>;` with no body. |

`full` matches the `wit_component::metadata::encode` toolchain convention
and produces a self-describing component that `wasm-tools component wit`
can decode without a registry. `local` produces a smaller WIT focused on
the package's own contract; consumers need an external WIT registry to
resolve `wasi:*` references. Both are well-defined; the choice is
deployment-policy, not technical.

The default scope is `full`. The scope table above applies to **`wado wit`
text** (`wado wit --scope <full|local>`).

> Revised in the Phase 2 finding: scope does **not** apply to embedding. An
> embedded `component-type` must be self-contained, so it is always `full`; a
> `local` section is not encodable. The earlier plan for a `[wit].scope`
> manifest key and an `--embed-wit=<scope>` resolution order is therefore
> dropped — see §"Phase 2 finding" and the Phase 3 entry.

Stdlib interfaces under `lib/wasi/**` are emitted with the same machinery
as user interfaces; there is no special-case "stdlib" code path. Each
`pub interface` is a uniform building block.

### Faithful imports: a WIR-level component interface plan

The world's import set must equal what the compiled component actually
imports — otherwise the embedded `component-type` (Phase 2) misrepresents
the binary. Deriving the set semantically (effect rows + type closure) is
not faithful: it over-includes type-alias-only interfaces (`wasi:clocks/types`,
`duration = u64`) and misses implicit imports (`wasi:cli/stderr`, written by
the `assert` / panic path with no `with` clause).

Decision: build the complete import/export interface set once, as structured
data, at the WIR layer (after DCE, so `used_wasi_functions` is populated),
and have codegen merely emit it. This restores the `codegen.rs` principle
("emit `Package` as is, without knowledge of earlier phases"). `wado wit` and
the Phase 2 embedder read the same plan; `full`-scope nested-package bodies
stay a separate type-closure (which follows type aliases, so `use duration`
resolves).

Done (R1–R3, each gated by the full E2E suite):

- The plan is built in `wir_build::component_imports` (`resolve_import_plan`),
  stored in `WirPackage::{import_plan, imported_cm_interfaces}`. Every entry
  carries an `ImportKind` (`SharedTypes`, `FunctionInterface`,
  `ResourceUsingInterface`, `ResourceSource`, `ResourceGetter`, `HttpTypes`,
  `HttpClient`) so codegen knows the encoding without re-deriving any
  interface/world decision. `tests/wit_import_plan.rs` asserts the plan equals
  the compiled component's CM imports across the CLI corpus, the HTTP service,
  and a pure-compute program.
- The WIT emitter reads it via `WitEmitOptions::world_imports`; `wado wit`
  imports are faithful (implicit `wasi:cli/stderr` in, alias-only
  `wasi:clocks/types` out) and `full` scope still self-describes.

### Embedding target and format

Wado emits a complete component in a single pass, so there is no
intermediate "core module with bindings" step analogous to
`wit-bindgen + wasm-tools component new`. The `component-type` custom
section is therefore added to the _outer component_, not to a nested core
module.

The section payload matches `wit_component::metadata::encode()` exactly:

1. A serialized component binary typing the world.
2. A `wit-component-encoding` subsection declaring UTF-8 (Wado's native
   string encoding).
3. A `producers` subsection identifying the Wado compiler version.

Consumers run `wasm-tools component wit output.wasm`, but note (per the Phase 2
finding above) that for a Wado _component_ this reconstructs WIT from the
component's own type, **not** from the appended `component-type` section. The
embedded section is consumed by tools that read it explicitly (`wkg`,
`wasm-tools metadata`, relink flows) and decodes standalone as a WIT package.
Round-trip verification therefore splits: (a) `decode(component)` matches
`wado wit` semantically, and (b) the embedded payload decodes as a
`DecodedWasm::WitPackage`. Both are covered by `tests/wit_bundle.rs`.
=======
### What this resolves

- One keyword (`interface`) for both the import and export block forms,
  matching WIT's vocabulary directly. `effect` keeps a narrow, well-defined
  role (polymorphic effect parameters).
- `pub interface Geometry { ... }` defines and groups; `export Geometry`
  from a world publishes it. No producer-side keyword duplication.
- World import information loss is gone: `import Foo` in a world is a
  reference to an `interface Foo` declaration carrying `#[cm(...)]`, so
  the FQ name is preserved by construction.

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

Superseded for imported `.wasm` components by
[Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md).
A fused component's interface is
effectful only insofar as the component's own host-leaf imports are — a
purely-computational import needs no `with`. The conservative rule stands
unchanged for host-satisfied (WASI) interfaces.

## Migration Plan

The migration runs on a single feature branch and lands as one merge:

- [x] Replace `effect <Name> { ... }` with `interface <Name> { ... }` across
      `lib/wasi/**`, `lib/core/**`, `wado-compiler` internals, examples, fixtures,
      and docs.
- [x] Update `wado-from-idl` so its generated stdlib emits `interface` blocks.
- [x] Add `cm_interface_fq` to `WorldImportInfo` and populate it from the
      referenced interface's `#[cm(...)]`.
- [x] World imports/exports are bare WIT-faithful interface refs (`import Foo;`
      / `export Foo;`); the brace-block form has been removed.
- [x] Retain the `effect` keyword for polymorphic effect parameters
      (`<effect E>`). The block-declaration unification does not extend
      to type parameters: an effect variable binds an effect row, which
      is a Wado-specific concept WIT has no equivalent for. The current
      parser support stays.
- [x] Update WEP: WIT and Wado Mapping to mark the interface/effect split as
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
| Interface → CM import binding                        | Each method declares `#[cm_import("wasi:cli/stdout@...#write-via-stream")]`.                       |
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
- `stdlib::ALL_WASI_MODULES` is an `include_str!`-driven static list. Adding a
  new WASI/CM library currently requires either putting the binding `.wado`
  in this list or feeding it through `CompilerHost`. Neither path reads
  embedded WIT directly.
- `wado-compiler` still relies on `wado-from-idl`-generated `.wado` files as
  the source of truth; no path reads embedded WIT from external `.wasm` yet
  (consumer side, still open). `wit-parser`, `wit-encoder`, and `wit-component`
  are all in `wado-compiler`'s `[dependencies]` for the producer side; the
  consumer-side `component-type` reader still reuses these crates when it lands.
- Producer-side WIT embedding (the `component-type` custom section described in
  [WIT Bundling](./wep-2026-03-21-wit-bundling.md)) is **done** (Phase 2):
  `wado compile` embeds by default via `wit_bundle::embed_component_type`. Note
  the Phase 2 finding below — the Wado component is already self-describing, so
  the section is additive full-fidelity metadata rather than the inspection
  mechanism.

Resolved since the first draft: HTTP handler specialization in
`codegen/component.rs` is gone — CM imports/exports are now emitted from the
WIR import plan and registry-derived descriptors with no HTTP/world literals
(see §"Codegen Genericization", P1–P5).

### Stale items already cleaned up

- The `__pending_trailers_tx` Wasm global is gone. Trailers handling no
  longer relies on a global; the previous reference in WEP: TIR-Level CM
  Binding Synthesis has been removed.
- `synthesize_result_export_adapter` has been renamed to
  `synthesize_result_export_binding`. Naming is now consistent with
  `synthesize_void_export_binding` and `synthesize_general_export_binding`.

## Producer Side: WIT Generation and Embedding

This section is the detailed design for the producer-side roadmap item
"embed `component-type` in output". It implements the format spec from
[WIT Bundling](./wep-2026-03-21-wit-bundling.md) and the Wado↔WIT mapping
from [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md). Anything
left open in those two documents (default behavior, CLI shape, where the
generator hooks into the pipeline) is resolved here.

### Position in the pipeline

WIT generation is a function of the _frontend output_. Describing a
component's interface — declared interfaces, exported items, the active
world, the type table — is fully determined by name resolution and type
resolution. Monomorphization, lowering (TIR → NIR → WIR), optimization,
and codegen do not contribute information that belongs in WIT.

The producer side therefore takes [`Semantics`](../wado-compiler/src/semantics.rs)
plus the target world FQ and produces:

- A WIT text document (consumed by `wado wit`).
- A `component-type` custom section payload, derived from that WIT text
  via `wit-parser` + `wit-component::metadata::encode` (embedded by
  `wado compile`, which is default-on).

Codegen is unaffected. The `codegen.rs` principle ("emits `Package` as is,
without knowledge of earlier phases") still holds; the WIT bundle is
appended as a postprocess step that takes the completed component bytes
plus the precomputed WIT text.

### Semantics additions

`Semantics` is the contract output of the frontend, so every fact needed
to emit WIT lives on it. Phase 0 landed the registry accessors; Phase 1's
emitter read interfaces and exported items off those registries and the
loaded TIR directly, so no further `Semantics` accessor was required.

Phase 0 (landed):

| Accessor                             | Returns                                                                 |
| ------------------------------------ | ----------------------------------------------------------------------- |
| `Semantics::world_registry()`        | `Option<&'static WorldRegistry>` — the parsed `world` table.            |
| `Semantics::cm_interface_registry()` | `Option<&'static CmInterfaceRegistry>` — the parsed CM interface table. |

Both registries are already built by `Elaborator::annotate_modules` (which
calls `CmInterfaceRegistry::build_from_stdlib`, an `OnceLock`-cached
singleton) and live on `AnnotateState`. The accessors surface them
without re-running stdlib parsing, so LSP, `wado wit`, and batch
compilation share the same instance. `FlatPackage` / `Package` /
`NirPackage` continue to carry the same `&'static` references they
already held — Phase 0 added access without changing how the data is
threaded.

Phase 1 (landed, simpler than planned): the proposed `Semantics::interfaces()`
/ `exported_items()` indices were not needed. `emit_wit_text` reads the
exported items off the loaded TIR modules and the interface bodies off the
Phase 0 registries directly, so no new `Semantics` accessor was added. The
remaining inputs are `WitEmitOptions` fields threaded from the CLI:
`default_interface_name` (`[package].name` or entry-file stem) and
`world_imports` (the faithful WIR import plan — a project/codegen fact, not a
frontend one).

The wir-build / codegen path continues to consume `Package` as today. The
new WIT path is a sibling reader of `Semantics`; it does not touch
`Package`.

### Module layout

Two new modules under `wado-compiler/src/`:

- `wit_emit.rs` — `emit_wit_text(&Semantics, &WitEmitOptions) -> Result<String, WitEmitError>`.
- `wit_bundle.rs` — `embed_component_type(component_bytes: &[u8], &Semantics, &WitEmitOptions) -> Result<Vec<u8>, WitEmitError>`.

`wit_emit` depends on `wit-encoder` for WIT text production. `wit_bundle`
depends on `wit_emit` for text, parses it with `wit-parser` into a
`Resolve`, encodes the component-type via `wit-component::metadata::encode`,
and appends the custom section to the component the codegen already
produced.

`wit-encoder` and `wit-component` are pinned in `[workspace.dependencies]`
alongside the existing `wit-parser` (all at generation `0.246`, matching
wasmtime's tree). `wado-compiler` adds them to its own `Cargo.toml` in
Phase 1.

### Type mapping

Implements the table in
[WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md) §"WIT to Wado
Type Mapping". Direction here is Wado → WIT (the existing table reads in
both directions, but the producer only needs one).

Out of scope for the first cut: closures, polymorphic effect parameters in
exported signatures, generics with non-WIT-representable bounds.
Encountering one in an exported signature is a compile error with a
diagnostic that names the offending parameter and points at its source
span.

Name conversion: Wado identifiers (`distance`, `MyApi`, `set_level`) become
WIT kebab-case (`distance`, `my-api`, `set-level`). Conflicts after
kebabification (e.g. two declarations colliding) are a compile error at WIT
emit time with both source spans surfaced.

The structural constructors (`option` / `list` / `tuple` / `result` /
`future` / `stream`) are assembled by one shared rule (`wit_emit::assemble`
over a single-level `CmShape` classification). The two front-ends — resolved
types (user exports) and CM-registry AST signatures (`full`-scope interface
reconstruction) — only classify their input and render leaves, so the two
paths cannot drift in how a shape becomes WIT. `own<R>`/`borrow<R>` map from a
resource value / `&resource` respectively.

### Interface grouping

Drives off the `pub interface` and `export interface` shapes already
established in [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md):

- Bare `export fn` / `export struct` / ... form a **default interface**
  named after `default_interface_name`. If only functions are exported
  (no types), they become direct world exports instead.
- `export interface Foo { item1, item2, ... }` blocks group named items
  into one WIT interface. Items are defined elsewhere; the block lists
  names.
- World-conformance entry points (`fn run` for `wasi:cli/command`,
  `fn handle` for `wasi:http/service`) are always direct world exports.
- Types referenced transitively by exported signatures are pulled into
  the owning interface even if not explicitly listed.
- `#![no_default_interface]` disables the default-interface fallback;
  every non-entry-point export must live in an explicit `export interface`.
- When no entry point is present at all, the world is emitted empty and the
  interfaces stand alone in the package (see "World-less libraries").

### Imported interface resolution and scope

For every `import Foo;` in the target world, the emitter looks up the
referenced `pub interface Foo` via `Semantics::interfaces`, reads its
`#[cm("...")]` FQ name, and uses that FQ name in `world { import ...; }`.

What the WIT document contains for each referenced interface depends on
the requested emit scope:

| Scope   | Behavior                                                                                                      |
| ------- | ------------------------------------------------------------------------------------------------------------- |
| `full`  | Inline the full body of every interface referenced by the world (user-authored _and_ stdlib WASI/CM).         |
| `local` | Inline only user-authored interfaces. Stdlib references appear as `import wasi:cli/stdout@<v>;` with no body. |

`full` matches the `wit_component::metadata::encode` toolchain convention
and produces a self-describing component that `wasm-tools component wit`
can decode without a registry. `local` produces a smaller WIT focused on
the package's own contract; consumers need an external WIT registry to
resolve `wasi:*` references. Both are well-defined; the choice is
deployment-policy, not technical.

The default scope is `full`. The scope table above applies to **`wado wit`
text** (`wado wit --scope <full|local>`).

> Revised in the Phase 2 finding: scope does **not** apply to embedding. An
> embedded `component-type` must be self-contained, so it is always `full`; a
> `local` section is not encodable. The earlier plan for a `[wit].scope`
> manifest key and an `--embed-wit=<scope>` resolution order is therefore
> dropped — see §"Phase 2 finding" and the Phase 3 entry.

Stdlib interfaces under `lib/wasi/**` are emitted with the same machinery
as user interfaces; there is no special-case "stdlib" code path. Each
`pub interface` is a uniform building block.

### Faithful imports: a WIR-level component interface plan

The world's import set must equal what the compiled component actually
imports — otherwise the embedded `component-type` (Phase 2) misrepresents
the binary. Deriving the set semantically (effect rows + type closure) is
not faithful: it over-includes type-alias-only interfaces (`wasi:clocks/types`,
`duration = u64`) and misses implicit imports (`wasi:cli/stderr`, written by
the `assert` / panic path with no `with` clause).

Decision: build the complete import/export interface set once, as structured
data, at the WIR layer (after DCE, so `used_wasi_functions` is populated),
and have codegen merely emit it. This restores the `codegen.rs` principle
("emit `Package` as is, without knowledge of earlier phases"). `wado wit` and
the Phase 2 embedder read the same plan; `full`-scope nested-package bodies
stay a separate type-closure (which follows type aliases, so `use duration`
resolves).

Done (R1–R3, each gated by the full E2E suite):

- The plan is built in `wir_build::component_imports` (`resolve_import_plan`),
  stored in `WirPackage::{import_plan, imported_cm_interfaces}`. Every entry
  carries an `ImportKind` (`SharedTypes`, `FunctionInterface`,
  `ResourceUsingInterface`, `ResourceSource`, `ResourceGetter`, `HttpTypes`,
  `HttpClient`) so codegen knows the encoding without re-deriving any
  interface/world decision. `tests/integration/wit_import_plan.rs` asserts the plan equals
  the compiled component's CM imports across the CLI corpus, the HTTP service,
  and a pure-compute program.
- The WIT emitter reads it via `WitEmitOptions::world_imports`; `wado wit`
  imports are faithful (implicit `wasi:cli/stderr` in, alias-only
  `wasi:clocks/types` out) and `full` scope still self-describes.

### Embedding target and format

Wado emits a complete component in a single pass, so there is no
intermediate "core module with bindings" step analogous to
`wit-bindgen + wasm-tools component new`. The `component-type` custom
section is therefore added to the _outer component_, not to a nested core
module.

The section payload matches `wit_component::metadata::encode()` exactly:

1. A serialized component binary typing the world.
2. A `wit-component-encoding` subsection declaring UTF-8 (Wado's native
   string encoding).
3. A `producers` subsection identifying the Wado compiler version.

Consumers run `wasm-tools component wit output.wasm`, but note (per the Phase 2
finding above) that for a Wado _component_ this reconstructs WIT from the
component's own type, **not** from the appended `component-type` section. The
embedded section is consumed by tools that read it explicitly (`wkg`,
`wasm-tools metadata`, relink flows) and decodes standalone as a WIT package.
Round-trip verification therefore splits: (a) `decode(component)` matches
`wado wit` semantically, and (b) the embedded payload decodes as a
`DecodedWasm::WitPackage`. Both are covered by `tests/integration/wit_bundle.rs`.
>>>>>>> origin/main

### Embedding policy

Because single-file scripts are first-class, `wado compile` embeds by default,
with or without a manifest. `--no-embed-wit` is the single explicit opt-out.

`-Os` is the exception. It is the production build for frontend delivery, where
the artifact is transpiled to core Wasm plus JavaScript and the metadata never
reaches a CM host, so `-Os` defaults to no embedding. An explicit `--embed-wit`
forces it back on for the rare build that wants both stripped symbols and a
self-describing component.

Embedding is a property of producing a distributable artifact, so it applies to
`wado compile` alone. `wado run`, `wado serve`, and `wado test` compile to an
ephemeral in-memory component and never embed, keeping the inner loop fast.

### CLI surface

```sh
wado wit file.wado                             # WIT text on stdout, full scope
wado wit --scope local -o file.wit file.wado   # user-authored interfaces only
wado wit --world wasi:http/service file.wado   # pick the target world
wado wit --lib .                               # the library world

wado compile file.wado                         # embeds (default)
wado compile --no-embed-wit file.wado          # opt out
wado compile --embed-wit file.wado             # force on, e.g. under -Os
```

`wado wit` runs the frontend and stops — no monomorphization, no lowering, no
codegen — so it carries no `-O` level: WIT is a pre-codegen fact. `--lib` and
`--world` are mutually exclusive.

The positional target follows one rule shared with `wado compile`: a file is a
source, a directory is a package, and omitting it discovers the nearest manifest
upward from the working directory. There is no third "manifest file" form; point
at the directory instead. When resolution goes through a manifest, the manifest
supplies the default interface name from `[package].name`, and the declared entry
picks both source and world in the order `command` → `service` → `lib`. A bare
file falls back to its stem for the name and the CLI world.

## Consumer side

Reading an external component's own type and building compiler IR from it is
specified by
[Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md), with
effect reconstruction in its own WEP. The full synchronous value-type surface is
covered; resources, async value types, and world-level type exports are the
remaining gaps, listed there.

Standalone `.wit` text is not a consumer input. The input is the component. WIT
text is an authoring-time concern handled by `wado-from-idl`, which stays the
build-time path for stdlib generation and for projects that want hand-curated
bindings.

## Remaining specializations

These are the places the compiler still names something specific. None blocks
external WIT support; they are listed so the inventory stays honest.

- The default target world when neither `--world` nor `--lib` is given is the CLI
  world. A default, not a structural problem.
- The test world is a synthetic entry in the world table, used by the test
  harness.
- The set of stdlib binding modules is a static compiled-in list. Adding a CM
  library means either joining that list or feeding the binding through the host
  I/O boundary; neither path reads embedded WIT.

## Open questions

- [ ] World structure faithfulness. Today interfaces are globally visible and a
      world only declares entry points (L1). L2 would have a `contract <World>;`
      declaration verify that a module's interface usage is a subset of the
      world's imports; L3 would give each world a scope of usable interfaces,
      making a `use` outside it an error; L4 would model WIT's `include` / `with`
      / world inheritance. Consuming external WIT realistically wants L2 and
      probably L3, since two unrelated worlds can carry same-named interfaces.
- [ ] The `contract` declaration. Its syntax is specified by
      [World Conformance](./wep-2026-01-16-world-conformance-and-export.md) but
      the parser does not implement it, and its runtime meaning depends on
      choosing L2 or L3 above.
- [ ] An opt-out of the default-interface fallback, so that every non-entry-point
      export must live in an explicit `export interface`.

## Non-goals

<<<<<<< HEAD
- WASI Preview 1 or Preview 2. The target is P3.
- Parsing standalone `.wit` text as a primary compiler input.
- Replacing `wado-from-idl`.
||||||| 2ed97d2fd
Each phase ends with green E2E tests for the listed fixtures.

- [x] Phase 0 — Dependencies and `Semantics` groundwork. Added the
      wasm-tools WIT crates to `[workspace.dependencies]`; exposed
      `Semantics::{world_registry, cm_interface_registry}()` over the
      `OnceLock`-cached `&'static` registries built by
      `Elaborator::annotate_modules`; renamed `WasiRegistry` →
      `CmInterfaceRegistry` and the CM-general `wasi_*` / `Wasi*` helpers to
      `cm_*` / `Cm*` (genuinely WASI-scoped methods keep the `wasi_` prefix).
      Non-breaking surface addition.

- [x] Phase 1 — `wado wit` text emission. `wado wit` (`wado-cli/src/wit.rs`,
      `Cmd::Wit`) takes a single file-or-directory target via `resolve_input`,
      runs `wado_compiler::semantics()` with no `-O`, and bails silently when
      `Semantics::is_complete()` is false. `wado-compiler/src/wit_emit.rs`
      (`emit_wit_text`) does type mapping, kebabification, interface grouping,
      transitive-type closure, and both `full` / `local` scopes; no-entry-point
      files emit an empty world. `tests/wit.rs` asserts the rendered text per
      shape (empty world, functions-only direct exports, record default
      interface, CLI `run`, full-scope resources/interfaces, string/list/option)
      and re-parses each with `wit-parser`.
  - Deviation from the original plan: `Semantics::interfaces()` /
    `exported_items()` were not added. The emitter reads the
    `CmInterfaceRegistry` / `WorldRegistry` directly and takes the faithful
    world import set from the WIR import plan, so `WitEmitOptions` carries
    `world_imports: Vec<String>` (alongside `scope`, `world_fq`,
    `default_interface_name`) instead. Only `wit-parser` and `wit-encoder` are
    pulled into `wado-compiler`; `wit-component` lands with Phase 2.

- [x] Phase 2 — `wado compile` embedding (default on, scope `full`)
  - [x] `wado-compiler/src/wit_bundle.rs`: text → `Resolve` →
        `wit_component::metadata::encode` → custom-section append
        (`embed_component_type` / `encode_component_type`). `wit-parser` and
        `wit-component` moved into `wado-compiler`'s `[dependencies]`.
  - [x] `--embed-wit` and `--no-embed-wit` flags on `CompileOptions`,
        mutually exclusive and value-less; embedding is on by default except
        under `-Os` which defaults off (an explicit `--embed-wit` still forces
        it). The embedded section is always the self-contained full closure, so
        there is no scope knob on `wado compile` (see the finding below).
        `wado run` / `serve` / `test` never embed (they do not route through
        `compile::run`).
  - [x] Hook: postprocess in the `wado compile` CLI path
        (`compile::maybe_embed_wit`), not in `compile_with_options`. This
        keeps the shared compile entry — used by `run`/`serve`/`test` —
        embedding-free, matching "applies to `wado compile` only". The
        faithful world import set comes from the same `resolve_world_imports`
        the `wado wit` path uses; `Semantics` is re-derived with one extra
        frontend pass (`wado_compiler::semantics`), since
        `compile_with_options` consumes its own `Semantics` into `Package`.
  - [x] Tests: `tests/wit_bundle.rs` asserts, per world shape, that the
        un-embedded component already self-describes (`decode` →
        `DecodedWasm::Component`), that embedding is byte-additive and leaves
        the component decodable, and that the encoded payload decodes as a
        standalone `DecodedWasm::WitPackage`. `compile::embed_policy_tests`
        pins the default-on/`-Os`-off/`--no-embed-wit`/`--embed-wit`
        resolution.

#### Phase 2 finding: the component already self-describes

A Wado artifact is a _full Component Model component_, not a core module, so it
is already self-describing: `wit_parser::decode` (the `wasm-tools component wit`
backend) reconstructs the WIT from the component's own typed CM imports/exports
via its `decode_component` path — verified empirically on the CLI corpus. `decode`
only consults a `component-type` payload when the _whole file_ is a WIT-package
blob (`is_wit_package()` keys off top-level component-type exports); a
`component-type` custom section on an already-formed component is opaquely
carried, **never read** by `component wit`.

The embedded section is therefore _additive full-fidelity metadata_, not the
mechanism that makes the binary inspectable. Its value over the intrinsic type:
the component's own type is always tree-shaken to the used surface, whereas the
embedded payload carries the complete upstream interface bodies, exact package
versions, and a `producers` record (the `metadata::encode` convention `wkg` /
`wasm-tools metadata` / relink flows consume). The earlier framing in
[WIT Bundling](./wep-2026-03-21-wit-bundling.md) ("a standalone `.wasm` cannot
describe its own interface") holds for core modules, not for Wado's component
output; the round-trip therefore splits in two: (a) `decode(component)` matches
`wado wit` semantically, and (b) the embedded payload decodes as a WIT package.

Consequence for scope: an embedded section must be self-contained, because
`metadata::encode` types the world against a fully-resolved `Resolve`. So
embedding always emits the **full** interface closure; a `local`
(registry-referencing) document does not re-parse standalone. This is a Wasm CM
binary property (component types are structural and self-contained), not a Wado
choice. `local` / `full` therefore stays a `wado wit` _text_ concept (`wado wit
--scope`) only. Consequently `wado compile` carries **no scope knob**:
`--embed-wit` / `--no-embed-wit` take no value, and the originally-planned
`[wit].scope` manifest override (Phase 3) is dropped — it would have nothing to
tune for the binary.

Phase 3 — Manifest scope override: dropped, not implemented. Embedding is
always the self-contained full closure (see the finding above), so there is no
embed-time scope to put in `wado.toml`. `wado wit --scope` remains the only
place `local` / `full` is meaningful (text output). WIT Bundling's status is
reconciled to "implemented, default-on, no scope knob".

## Open Design Questions

### World-less libraries (`wado compile --lib`)

A `.wado` file with only `export` items and no world entry point
(`fn run` / `fn handle`) is a _library_. It is declared by `[package].lib` in
`wado.toml` and built with `wado compile --lib` (`[package].namespace` is
required for `--lib`, and `--lib` is mutually exclusive with `--world`). The
library world is synthesized from the entry module's export signatures and
carried on `Package.lib_world_info` — it cannot live in the `&'static` stdlib
`WorldRegistry`, so it is special-cased like the `test` world. Library exports
use a synchronous lift (the core function returns the value directly), and the
default allocator is `freelist`.

#### World naming: the anonymous root

A component's type is anonymous: the Component Model binary encodes it as
`componenttype ::= 0x41 vec(componentdecl)` (`Binary.md`) — a bare list of
imports/exports with no world name — and WIT models a world as "an equivalent
of a `component` type" whose name is only a text-level label (`WIT.md`).
`wasm-tools component wit` confirms this: decoding a Wado library component
yields `world root { … export <namespace>:<name>/<name> }`. The world name is
discarded on decode and re-synthesized as `root`.

So a library's world carries **no identity**; only its interface does. The
identity consumers import is the **default interface**
`<namespace>:<name>/<name>@<version>`. The library world is the anonymous root
and is emitted as `root` (matching the decode convention), kept distinct from
the default interface so the two never share one `namespace:package/<name>`
slot — WIT worlds and interfaces occupy a single per-package namespace, so
naming both after the package is a `duplicate item` error that silently drops
the `component-type` embed. `[package].name` may therefore not be `root`. This
splits two previously-conflated values: the world's emit name (`root`, textual
only) and the default-interface FQ (`ns:name/name`, the codegen identity and
the component's real export). The anonymous component binary is unaffected.

`wado wit --lib` renders this same shape (`world root` exporting the default
interface), so the human-readable text and the embedded `component-type` agree.

Export grouping follows the same A/B rule the WIT producer applies
(`wit_emit`, mirroring WEP: WIT and Wado Mapping → "Default Interface"):

- A — no exported signature references a named user type: the `export fn`s are
  **direct world exports** (`export id: func(...)`), structural types
  (`list`/`option`/`tuple`/`result`/`string`) inlined.
- B — any exported signature references a named user type
  (`struct`/`variant`/`enum`/`flags`/type alias): the exports plus the
  transitive closure of their named types are grouped into the **default
  interface** named after `[package].name`, exported as an instance. This is
  required for reuse — a world export list admits only functions and
  interfaces, never a bare type, so a named type reaches consumers as a
  reusable, `use`-able entity only through an exported interface.

The codegen mirrors this without world-specific special-casing:

1. The keystone is registry-driven type resolution. The entry module's
   exported (and transitively referenced) named types are registered into
   `CmInterfaceRegistry` under the default-interface FQ. Both the export type
   plan (`resolve_cm_export_type`) and the CM type emitter resolve named types
   purely through this registry, so lib-local types need no special path once
   registered.
2. The CM value-type emitter is the single recursive engine that already
   builds the full value surface (`list`/`option`/`tuple`/`result`/`record`/
   `variant`/`enum`/`flags`/newtype/nested) for WASI imports. It is generalized
   over a type sink so it emits either into an imported/exported `InstanceType`
   or as top-level component defined types for direct world exports — one
   engine, no parallel export-side reimplementation.
3. `package-cm-catalog` is the corpus enumerating the value-type ABI surface;
   `tests/cm_catalog.rs` round-trips a crafted value through every `id_*`
   export (`lift(lower(x)) == x`) at `-O0`/`-O2`, and the same fixture runs
   under the test world via `e2e.rs`.

`wado wit` (without `--lib`) still wraps a no-entry-point file's interface in
the resolved default world (an otherwise empty `world`), which keeps its output
valid, round-trippable WIT.

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
- [x] Unify `effect` block declarations into `interface` (landed; see
      Migration Plan above). The `effect` keyword survives in polymorphic
      effect parameters (`<effect E>`) — see Migration Plan for the
      rationale.
- [x] World imports/exports are bare WIT-faithful interface refs
      (`import Foo;` / `export Foo;`); brace-form removed. `WorldImportInfo`
      and `WorldExportInfo` carry `cm_interface_fq` resolved from the
      referenced `pub interface Foo`'s `#[cm(...)]`.
- [x] Producer side: emit WIT text and embed `component-type` in output
      (WEP: WIT Bundling for the format; this WEP §"Producer Side: WIT
      Generation and Embedding" for the detailed design). Phase 0
      (`Semantics` refactor), Phase 1 (`wado wit` text), and Phase 2
      (`wado compile` embeds WIT by default, full closure, `--no-embed-wit` to
      opt out) are done. Phase 3 (`[wit].scope` in `wado.toml`) was dropped —
      embedding is always self-contained, so there is no embed-time scope to tune.
- [ ] Decide world structure faithfulness level (L2 vs L3) and document.
- [ ] Implement `contract` declaration with the chosen scope rules (revise
      WEP: World Conformance accordingly).
- [x] Decouple HTTP handler specialization from codegen: driven from
      `WorldExportInfo::from_interface_fq` (P1/P2) rather than the return-type
      sniffer (`returns_http_response`) and the post-hoc
      `append_http_handler_export`. See §"Codegen Genericization".
- [ ] Add `wit-component` as a `wado-compiler` dependency (consumer side).
      `wit-parser` is already in `[workspace.dependencies]` for
      `wado-from-idl`; the consumer-side use-resolver reads embedded
      `component-type` from external `.wasm` imports via the same crate.
- [ ] Construct world / interface / resource entries in the existing
      registries directly from parsed WIT, on the same code path as
      stdlib-derived entries.
- [ ] `wado compile --lib`: export the entry module's `export fn`s under a
      package-named library world, with A/B grouping (direct world exports vs
      default interface) matching the WIT producer. Plumbing and primitive sync
      exports landed (M1–M2); containers, `result`/`string` returns, lib-local
      named-type registration + emission, and default-interface grouping remain.
      `package-cm-catalog` + `tests/cm_catalog.rs` track and drive the surface.
- [ ] Close the binding-synthesis gaps for arbitrary exports: container and
      user-named-type lift/lower, `result`/`string` returns, and
      return-via-outptr when flat count exceeds `MAX_FLAT_RESULTS`. Sync export
      support and primitive lift/lower landed with `--lib`.
- [x] Retire ad-hoc HTTP detection from the import/codegen path: the
      `has_http_handler_export` package field and `append_http_handler_export`
      are gone (P2/P5). `WorldInfo::has_http_handler_export` survives only as the
      allocator-default heuristic (HTTP service → free-list).
- [x] Emit `wasi:cli/run@<v>` and `wasi:http/handler@<v>` as proper CM
      instance exports in `emit_world_exports` (P2: `append_interface_instance_exports`).

## Codegen Genericization: Removing HTTP/world Special-Casing

Done (P1–P5). `codegen/component.rs` now encodes CM imports/exports from the
WIR import plan and registry-derived descriptors with no `package == "http"`,
`namespace_prefix == "wasi:http/"`, `returns_http_response`,
`{pkg}-handler-result`, or `get_package_version("http")` literals. HTTP and kiln
(`emit_kiln_world_types`) fall out of one generic mechanism. Key pieces:

- Handler detection is structural (`WorldExportInfo::is_handler_instance_export`:
  `from_interface_fq.is_some()` + non-unit `Result`). `append_http_handler_export`
  is replaced by `append_interface_instance_exports`, which wraps every
  `from_interface_fq = Some(fq)` export in an `fq`-named instance (CLI `run` is now
  `wasi:cli/run@<v>`); runtime drivers bind it via the generated `Command` bindings.
- A structural CM type interner in `ComponentModelContext` (`CmTypeKey →
  type-index`, recursive `intern_cm_type`) resolves the `result<own<response>,
  error-code>` transport composite that no signature names; the export lift and
  the `Client` import resolve to one index, dropping the `{pkg}-handler-result`
  name. World exports resolve type names within their own package first
  (`resolve_cm_source_with_prefix`), fixing a kiln/HTTP `Response` collision.
- `wasi:http/{types,client}` are now ordinary `ResourceDefiningInterface` /
  `ResourceUsingInterface` entries classified structurally by
  `resolve_import_plan`; the HTTP constructors are declared in
  `lib/wasi/http/types.wado` with `#[cm(...)]` and lowered generically. The dead
  `has_http_handler_export` field is removed (`WorldInfo::has_http_handler_export`
  survives only as the allocator-default heuristic); `tests/wit_import_plan.rs`
  keeps the plan faithful to the emitted bytes.

Scope was the stdlib-derived registry path only; external `.wasm` consumption is
a separate item.
=======
Each phase ends with green E2E tests for the listed fixtures.

- [x] Phase 0 — Dependencies and `Semantics` groundwork. Added the
      wasm-tools WIT crates to `[workspace.dependencies]`; exposed
      `Semantics::{world_registry, cm_interface_registry}()` over the
      `OnceLock`-cached `&'static` registries built by
      `Elaborator::annotate_modules`; renamed `WasiRegistry` →
      `CmInterfaceRegistry` and the CM-general `wasi_*` / `Wasi*` helpers to
      `cm_*` / `Cm*` (genuinely WASI-scoped methods keep the `wasi_` prefix).
      Non-breaking surface addition.

- [x] Phase 1 — `wado wit` text emission. `wado wit` (`wado-cli/src/wit.rs`,
      `Cmd::Wit`) takes a single file-or-directory target via `resolve_input`,
      runs `wado_compiler::semantics()` with no `-O`, and bails silently when
      `Semantics::is_complete()` is false. `wado-compiler/src/wit_emit.rs`
      (`emit_wit_text`) does type mapping, kebabification, interface grouping,
      transitive-type closure, and both `full` / `local` scopes; no-entry-point
      files emit an empty world. `tests/integration/wit.rs` asserts the rendered text per
      shape (empty world, functions-only direct exports, record default
      interface, CLI `run`, full-scope resources/interfaces, string/list/option)
      and re-parses each with `wit-parser`.
  - Deviation from the original plan: `Semantics::interfaces()` /
    `exported_items()` were not added. The emitter reads the
    `CmInterfaceRegistry` / `WorldRegistry` directly and takes the faithful
    world import set from the WIR import plan, so `WitEmitOptions` carries
    `world_imports: Vec<String>` (alongside `scope`, `world_fq`,
    `default_interface_name`) instead. Only `wit-parser` and `wit-encoder` are
    pulled into `wado-compiler`; `wit-component` lands with Phase 2.

- [x] Phase 2 — `wado compile` embedding (default on, scope `full`)
  - [x] `wado-compiler/src/wit_bundle.rs`: text → `Resolve` →
        `wit_component::metadata::encode` → custom-section append
        (`embed_component_type` / `encode_component_type`). `wit-parser` and
        `wit-component` moved into `wado-compiler`'s `[dependencies]`.
  - [x] `--embed-wit` and `--no-embed-wit` flags on `CompileOptions`,
        mutually exclusive and value-less; embedding is on by default except
        under `-Os` which defaults off (an explicit `--embed-wit` still forces
        it). The embedded section is always the self-contained full closure, so
        there is no scope knob on `wado compile` (see the finding below).
        `wado run` / `serve` / `test` never embed (they do not route through
        `compile::run`).
  - [x] Hook: postprocess in the `wado compile` CLI path
        (`compile::maybe_embed_wit`), not in `compile_with_options`. This
        keeps the shared compile entry — used by `run`/`serve`/`test` —
        embedding-free, matching "applies to `wado compile` only". The
        faithful world import set comes from the same `resolve_world_imports`
        the `wado wit` path uses; `Semantics` is re-derived with one extra
        frontend pass (`wado_compiler::semantics`), since
        `compile_with_options` consumes its own `Semantics` into `Package`.
  - [x] Tests: `tests/integration/wit_bundle.rs` asserts, per world shape, that the
        un-embedded component already self-describes (`decode` →
        `DecodedWasm::Component`), that embedding is byte-additive and leaves
        the component decodable, and that the encoded payload decodes as a
        standalone `DecodedWasm::WitPackage`. `compile::embed_policy_tests`
        pins the default-on/`-Os`-off/`--no-embed-wit`/`--embed-wit`
        resolution.

#### Phase 2 finding: the component already self-describes

A Wado artifact is a _full Component Model component_, not a core module, so it
is already self-describing: `wit_parser::decode` (the `wasm-tools component wit`
backend) reconstructs the WIT from the component's own typed CM imports/exports
via its `decode_component` path — verified empirically on the CLI corpus. `decode`
only consults a `component-type` payload when the _whole file_ is a WIT-package
blob (`is_wit_package()` keys off top-level component-type exports); a
`component-type` custom section on an already-formed component is opaquely
carried, **never read** by `component wit`.

The embedded section is therefore _additive full-fidelity metadata_, not the
mechanism that makes the binary inspectable. Its value over the intrinsic type:
the component's own type is always tree-shaken to the used surface, whereas the
embedded payload carries the complete upstream interface bodies, exact package
versions, and a `producers` record (the `metadata::encode` convention `wkg` /
`wasm-tools metadata` / relink flows consume). The earlier framing in
[WIT Bundling](./wep-2026-03-21-wit-bundling.md) ("a standalone `.wasm` cannot
describe its own interface") holds for core modules, not for Wado's component
output; the round-trip therefore splits in two: (a) `decode(component)` matches
`wado wit` semantically, and (b) the embedded payload decodes as a WIT package.

Consequence for scope: an embedded section must be self-contained, because
`metadata::encode` types the world against a fully-resolved `Resolve`. So
embedding always emits the **full** interface closure; a `local`
(registry-referencing) document does not re-parse standalone. This is a Wasm CM
binary property (component types are structural and self-contained), not a Wado
choice. `local` / `full` therefore stays a `wado wit` _text_ concept (`wado wit
--scope`) only. Consequently `wado compile` carries **no scope knob**:
`--embed-wit` / `--no-embed-wit` take no value, and the originally-planned
`[wit].scope` manifest override (Phase 3) is dropped — it would have nothing to
tune for the binary.

Phase 3 — Manifest scope override: dropped, not implemented. Embedding is
always the self-contained full closure (see the finding above), so there is no
embed-time scope to put in `wado.toml`. `wado wit --scope` remains the only
place `local` / `full` is meaningful (text output). WIT Bundling's status is
reconciled to "implemented, default-on, no scope knob".

## Open Design Questions

### World-less libraries (`wado compile --lib`)

A `.wado` file with only `export` items and no world entry point
(`fn run` / `fn handle`) is a _library_. It is declared by `[package].lib` in
`wado.toml` and built with `wado compile --lib` (`[package].namespace` is
required for `--lib`, and `--lib` is mutually exclusive with `--world`). The
library world is synthesized from the entry module's export signatures and
carried on `Package.lib_world_info` — it cannot live in the `&'static` stdlib
`WorldRegistry`, so it is special-cased like the `test` world. Library exports
use a synchronous lift (the core function returns the value directly), and the
default allocator is `freelist`.

#### World naming: the anonymous root

A component's type is anonymous: the Component Model binary encodes it as
`componenttype ::= 0x41 vec(componentdecl)` (`Binary.md`) — a bare list of
imports/exports with no world name — and WIT models a world as "an equivalent
of a `component` type" whose name is only a text-level label (`WIT.md`).
`wasm-tools component wit` confirms this: decoding a Wado library component
yields `world root { … export <namespace>:<name>/<name> }`. The world name is
discarded on decode and re-synthesized as `root`.

So a library's world carries **no identity**; only its interface does. The
identity consumers import is the **default interface**
`<namespace>:<name>/<name>@<version>`. The library world is the anonymous root
and is emitted as `root` (matching the decode convention), kept distinct from
the default interface so the two never share one `namespace:package/<name>`
slot — WIT worlds and interfaces occupy a single per-package namespace, so
naming both after the package is a `duplicate item` error that silently drops
the `component-type` embed. `[package].name` may therefore not be `root`. This
splits two previously-conflated values: the world's emit name (`root`, textual
only) and the default-interface FQ (`ns:name/name`, the codegen identity and
the component's real export). The anonymous component binary is unaffected.

`wado wit --lib` renders this same shape (`world root` exporting the default
interface), so the human-readable text and the embedded `component-type` agree.

Export grouping follows the same A/B rule the WIT producer applies
(`wit_emit`, mirroring WEP: WIT and Wado Mapping → "Default Interface"):

- A — no exported signature references a named user type: the `export fn`s are
  **direct world exports** (`export id: func(...)`), structural types
  (`list`/`option`/`tuple`/`result`/`string`) inlined.
- B — any exported signature references a named user type
  (`struct`/`variant`/`enum`/`flags`/type alias): the exports plus the
  transitive closure of their named types are grouped into the **default
  interface** named after `[package].name`, exported as an instance. This is
  required for reuse — a world export list admits only functions and
  interfaces, never a bare type, so a named type reaches consumers as a
  reusable, `use`-able entity only through an exported interface.

The codegen mirrors this without world-specific special-casing:

1. The keystone is registry-driven type resolution. The entry module's
   exported (and transitively referenced) named types are registered into
   `CmInterfaceRegistry` under the default-interface FQ. Both the export type
   plan (`resolve_cm_export_type`) and the CM type emitter resolve named types
   purely through this registry, so lib-local types need no special path once
   registered.
2. The CM value-type emitter is the single recursive engine that already
   builds the full value surface (`list`/`option`/`tuple`/`result`/`record`/
   `variant`/`enum`/`flags`/newtype/nested) for WASI imports. It is generalized
   over a type sink so it emits either into an imported/exported `InstanceType`
   or as top-level component defined types for direct world exports — one
   engine, no parallel export-side reimplementation.
3. `package-cm-catalog` is the corpus enumerating the value-type ABI surface;
   `tests/integration/cm_catalog.rs` round-trips a crafted value through every `id_*`
   export (`lift(lower(x)) == x`) at `-O0`/`-O2`, and the same fixture runs
   under the test world via `e2e.rs`.

`wado wit` (without `--lib`) still wraps a no-entry-point file's interface in
the resolved default world (an otherwise empty `world`), which keeps its output
valid, round-trippable WIT.

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
- [x] Unify `effect` block declarations into `interface` (landed; see
      Migration Plan above). The `effect` keyword survives in polymorphic
      effect parameters (`<effect E>`) — see Migration Plan for the
      rationale.
- [x] World imports/exports are bare WIT-faithful interface refs
      (`import Foo;` / `export Foo;`); brace-form removed. `WorldImportInfo`
      and `WorldExportInfo` carry `cm_interface_fq` resolved from the
      referenced `pub interface Foo`'s `#[cm(...)]`.
- [x] Producer side: emit WIT text and embed `component-type` in output
      (WEP: WIT Bundling for the format; this WEP §"Producer Side: WIT
      Generation and Embedding" for the detailed design). Phase 0
      (`Semantics` refactor), Phase 1 (`wado wit` text), and Phase 2
      (`wado compile` embeds WIT by default, full closure, `--no-embed-wit` to
      opt out) are done. Phase 3 (`[wit].scope` in `wado.toml`) was dropped —
      embedding is always self-contained, so there is no embed-time scope to tune.
- [ ] Decide world structure faithfulness level (L2 vs L3) and document.
- [ ] Implement `contract` declaration with the chosen scope rules (revise
      WEP: World Conformance accordingly).
- [x] Decouple HTTP handler specialization from codegen: driven from
      `WorldExportInfo::from_interface_fq` (P1/P2) rather than the return-type
      sniffer (`returns_http_response`) and the post-hoc
      `append_http_handler_export`. See §"Codegen Genericization".
- [ ] Add `wit-component` as a `wado-compiler` dependency (consumer side).
      `wit-parser` is already in `[workspace.dependencies]` for
      `wado-from-idl`; the consumer-side use-resolver reads embedded
      `component-type` from external `.wasm` imports via the same crate.
- [ ] Construct world / interface / resource entries in the existing
      registries directly from parsed WIT, on the same code path as
      stdlib-derived entries.
- [ ] `wado compile --lib`: export the entry module's `export fn`s under a
      package-named library world, with A/B grouping (direct world exports vs
      default interface) matching the WIT producer. Plumbing and primitive sync
      exports landed (M1–M2); containers, `result`/`string` returns, lib-local
      named-type registration + emission, and default-interface grouping remain.
      `package-cm-catalog` + `tests/integration/cm_catalog.rs` track and drive the surface.
- [ ] Close the binding-synthesis gaps for arbitrary exports: container and
      user-named-type lift/lower, `result`/`string` returns, and
      return-via-outptr when flat count exceeds `MAX_FLAT_RESULTS`. Sync export
      support and primitive lift/lower landed with `--lib`.
- [x] Retire ad-hoc HTTP detection from the import/codegen path: the
      `has_http_handler_export` package field and `append_http_handler_export`
      are gone (P2/P5). `WorldInfo::has_http_handler_export` survives only as the
      allocator-default heuristic (HTTP service → free-list).
- [x] Emit `wasi:cli/run@<v>` and `wasi:http/handler@<v>` as proper CM
      instance exports in `emit_world_exports` (P2: `append_interface_instance_exports`).

## Codegen Genericization: Removing HTTP/world Special-Casing

Done (P1–P5). `codegen/component.rs` now encodes CM imports/exports from the
WIR import plan and registry-derived descriptors with no `package == "http"`,
`namespace_prefix == "wasi:http/"`, `returns_http_response`,
`{pkg}-handler-result`, or `get_package_version("http")` literals. HTTP and kiln
(`emit_kiln_world_types`) fall out of one generic mechanism. Key pieces:

- Handler detection is structural (`WorldExportInfo::is_handler_instance_export`:
  `from_interface_fq.is_some()` + non-unit `Result`). `append_http_handler_export`
  is replaced by `append_interface_instance_exports`, which wraps every
  `from_interface_fq = Some(fq)` export in an `fq`-named instance (CLI `run` is now
  `wasi:cli/run@<v>`); runtime drivers bind it via the generated `Command` bindings.
- A structural CM type interner in `ComponentModelContext` (`CmTypeKey →
  type-index`, recursive `intern_cm_type`) resolves the `result<own<response>,
  error-code>` transport composite that no signature names; the export lift and
  the `Client` import resolve to one index, dropping the `{pkg}-handler-result`
  name. World exports resolve type names within their own package first
  (`resolve_cm_source_with_prefix`), fixing a kiln/HTTP `Response` collision.
- `wasi:http/{types,client}` are now ordinary `ResourceDefiningInterface` /
  `ResourceUsingInterface` entries classified structurally by
  `resolve_import_plan`; the HTTP constructors are declared in
  `lib/wasi/http/types.wado` with `#[cm(...)]` and lowered generically. The dead
  `has_http_handler_export` field is removed (`WorldInfo::has_http_handler_export`
  survives only as the allocator-default heuristic); `tests/integration/wit_import_plan.rs`
  keeps the plan faithful to the emitted bytes.

Scope was the stdlib-derived registry path only; external `.wasm` consumption is
a separate item.
>>>>>>> origin/main

## Consequences

- One documented end-to-end goal replaces a scatter of point WEPs, and the
  producer and consumer sides are symmetrical: a Wado-compiled component is
  consumable by another Wado compiler with no side-channel metadata.
- One keyword for block declarations matches WIT's vocabulary, and `effect` keeps
  a narrow, well-defined role.
- World imports are traceable to WIT FQ names by construction, so nothing depends
  on disambiguating by method name.
- Adding a WASI or third-party CM library needs no compiler patch.
- Effectfulness is decided per call site rather than per declaration, which
  follows WIT's lack of a purity annotation and removes a Wado-specific concept
  users would otherwise have to learn.
- The WIT tooling crates live in the compiler, which costs dependency surface and
  binary size.
- If L3 world scoping lands, the meaning of `use` changes: importing an interface
  outside the active world's imports becomes an error. That is a user-visible
  behaviour change and needs its own migration.
- The stdlib stays `.wado`-first through `wado-from-idl` while external imports
  are WIT-first. The two paths coexist deliberately.

## References

- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)
- [World Conformance and Export Syntax](./wep-2026-01-16-world-conformance-and-export.md)
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
- [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md)
- [WebAssembly Module Import Support](./wep-2026-01-10-wasm-import.md)
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)
- [Effect Reconstruction from CM Component Imports](./wep-2026-07-15-cm-import-effect-reconstruction.md)
- [Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)
- [WebIDL Binding Generator (`wado-from-idl`)](./wep-2026-04-01-tide.md)
