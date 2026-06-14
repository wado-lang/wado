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
   the producer can compute the WIT — `wado wit`, Phase 1 — but embedding it
   into the binary is Phase 2, not yet done).
3. The use resolver constructs Wado IR (worlds, interfaces, resources, types)
   directly from that embedded WIT.
4. The CM binding synthesis lifts/lowers values at the boundary based on that
   IR, without any per-component, per-world, or per-resource hand-coding in
   the compiler.

There is no separate registry of "known WASI modules". The set of supported
worlds equals the set of WITs the compiler has parsed during this compilation.

## Key Decision: Unify `effect` block declarations into `interface`

WIT's organizing primitive is `interface`: a named group of free functions,
resources (with their methods), and types. Wado previously had `effect` as a
parallel block-declaration keyword (`effect Foo { ... }`). For WIT
interoperability the block form collapses into `interface`:

- `effect Foo { ... }` → `interface Foo { ... }`.

```wado
interface Stdout { ... }       // formerly effect Stdout { ... }
```

The `with` clause continues to take interface names. Effect tracking
semantics are unchanged.

The `effect` keyword is retained for one purpose: polymorphic effect
parameters (`<effect E>`). Here `E` is a type-level binder over effect
rows — a different concept from a CM interface, and one WIT does not have
an equivalent for. Keeping the keyword for this case makes the distinction
explicit and avoids overloading `interface` with a meaning it does not
have in WIT.

```wado
fn wrapper<effect E>(f: fn() with E) with E { f(); }
```

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
  (consumer side, still open). `wit-parser` and `wit-encoder` are wired into
  `wado-compiler` for the producer side; `wit-component` lands with Phase 2
  embedding and the consumer-side `component-type` reader.
- Producer-side WIT embedding (the `component-type` custom section described
  in [WIT Bundling](./wep-2026-03-21-wit-bundling.md)) is not yet wired into
  codegen (Phase 2). `wado wit` text emission (Phase 1) is done, so the
  contract is computable; embedding it into the binary is the remaining step
  before a Wado component is consumable via the embedded-WIT path.

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

The default scope is `full`. It needs no configuration and produces a
self-describing component, which is the first-class outcome for both
single-file scripts and manifest-backed projects (see "Embedding policy"
below). A project may change the default to `local` in `wado.toml`:

```toml
[wit]
scope = "local"   # override the built-in `full` default
```

`[wit].scope` only changes which scope is used; it never decides _whether_
WIT is embedded (see "Embedding policy" below for that). When WIT _is_
embedded, the scope resolution order is: explicit `--embed-wit=<scope>`,
then `[wit].scope`, then the built-in `full`.

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

Consumers extract via `wasm-tools component wit output.wasm`. Round-trip
verification (Wado emits → wasm-tools decodes → matches the text from
`wado wit`) is a required fixture for every world shape.

### Embedding policy

Two roles, clearly separated:

- `wado wit` writes the WIT as **text**, for humans to read and for
  tooling to diff. It is an inspection aid, not the deliverable.
- `wado compile` embeds the **binary** `component-type` section. This is
  what makes the output a first-class Component Model citizen — composable
  with `wac`, publishable with `wkg`, transpilable with `jco`, and
  consumable by another Wado compiler.

Because a component without embedded WIT is a second-class CM citizen,
and because Wado treats single-file scripts as first-class, **`wado
compile` embeds WIT by default** — with or without a `wado.toml`. The
manifest is a tuning knob for the _scope_ (`full` vs `local`), never the
switch that turns embedding on. `--no-wit` is the single, explicit
opt-out.

`-Os` is the exception: it is the production build for frontend delivery
(`jco`-transpiled to core Wasm + JS for the browser), where the WIT
metadata is dead weight that never reaches a CM host. So `-Os` defaults
to no embedding, exactly as if `--no-wit` were passed. An explicit
`--embed-wit=<scope>` still forces embedding under `-Os` for the rare
case that wants both the smallest symbols and a self-describing component.

Embedding is a property of producing a distributable artifact, so it
applies to `wado compile` only. `wado run`, `wado serve`, and `wado test`
compile to an ephemeral in-memory component and never embed WIT, keeping
the inner dev loop small and fast.

### CLI surfaces

`wado wit` — standalone WIT text:

```sh
wado wit file.wado                                 # stdout, default scope (full)
wado wit --scope local -o file.wit file.wado       # file output, user-only WIT
wado wit --world wasi:http/service file.wado       # pick the target world
```

Backed by `wit_emit::emit_wit_text`. Runs `wado_compiler::semantics()` —
the public async `Semantics`-only entry, which parses, loads, and analyzes,
then stops; no monomorphize, no lower, no codegen. Mirrors `wado dump` in
pipeline depth, but carries no `-O` level: WIT is a pre-codegen fact, so
the optimization level is irrelevant. Takes a single positional target —
a `.wado` file or a directory (see "Input resolution"); passing more than
one is an error. `--world` reuses the same flag and
default (`wasi:cli/command`, overridable by the `__DATA__` world) as
`wado compile`. `--scope` is optional and defaults per the resolution
order above. When the file has no world entry point (a library-shaped
`.wado` with only `pub interface` / bare `export` items), the emitter
produces an **empty world** — see "World-less libraries" under Open
Design Questions.

`wado compile` — embed WIT in the compiled component (default on):

```sh
wado compile file.wado                     # embeds, scope = full (default)
wado compile --embed-wit=local file.wado   # embeds, user-only WIT, refs upstream
wado compile --no-wit file.wado            # explicit opt-out
```

`--embed-wit=<scope>` overrides the resolved scope for one invocation; it
always takes a value (`full` or `local`), and `--embed-wit` without a
value is a CLI error. `--no-wit` takes no value and is mutually exclusive
with `--embed-wit`.

### Input resolution

`wado wit` does not take a `.wado` file only. The positional input is a
_target_ that resolves through `manifest::resolve_input`:

| Input       | Resolution                                                                                   |
| ----------- | -------------------------------------------------------------------------------------------- |
| `foo.wado`  | A file is a single Wado source — analyze it directly.                                        |
| a directory | A directory is a Wado package — load `<dir>/wado.toml` and resolve the entry source from it. |
| (omitted)   | Discover the nearest `wado.toml` upward from the cwd.                                        |

`file = source`, `dir = package` is the whole rule — no third "manifest
file" form. A bare `wado.toml` path is not a target; point at its directory
instead. All three forms already work in `resolve_input`, and `wado
compile` routes through the same helper, so `wado wit` and `wado compile`
share one input model with no new plumbing.

When resolution goes through a manifest, the CLI loads it (via
`load_nearest_manifest`) to source two inputs that single-file mode lacks:
`WitEmitOptions::default_interface_name` from `[package].name`, and the
`--scope` default from `[wit].scope`. A bare `.wado` file falls back to the
file stem for the name and `full` for the scope.

World selection follows from the same resolution: `--world` wins when
given; otherwise the manifest's declared entry picks both the source and
the world, in precedence order `command` → `service` → `lib`. A `lib`-only
project resolves to the library case (empty world; see "World-less
libraries").

### Implementation phases

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

- [ ] Phase 2 — `wado compile` embedding (default on, scope `full`)
  - [ ] `wado-compiler/src/wit_bundle.rs`: text → `Resolve` →
        `wit_component::metadata::encode` → custom-section append.
  - [ ] `--embed-wit=<scope>` and `--no-wit` flags on `CompileOptions`,
        mutually exclusive; embedding defaults to `full` when neither is
        given, except under `-Os` which defaults to no embedding (an
        explicit `--embed-wit` still forces it). `wado run` / `serve` /
        `test` never embed.
  - [ ] Postprocess hook in `codegen/postprocess.rs` (or the immediate
        caller of `build_component`).
  - [ ] Round-trip fixture: for every Phase 1 fixture, compile (default
        `full`), then run `wasm-tools component wit` on the output and
        assert it matches `wado wit`.

- [ ] Phase 3 — Manifest scope override
  - [ ] Parse `[wit].scope` in `wado-manifest`.
  - [ ] `wado compile` uses `[wit].scope` as the scope when no CLI flag is
        given; the built-in default stays `full`; `--embed-wit` overrides
        the manifest; `--no-wit` opts out entirely.
  - [ ] Update WEP: WIT Bundling status from "designed" to "implemented"
        and reconcile its wording with the default-on, manifest-tunes-scope
        rule documented here.

## Open Design Questions

### World-less libraries

A `.wado` file may carry only `pub interface` declarations and bare
`export` items with no world entry point (`fn run` / `fn handle`). Such a
file is conceptually a _library_: a bag of interfaces meant to be consumed
by other components, not a runnable world. WIT can express this — a package
may contain interface definitions with no world, or with a world that only
re-exports interfaces — but Wado has not yet decided what a world-less
library _is_ at the language level (how it is declared, published, and
depended upon).

Until that is settled, `wado wit` and `wado compile` take the conservative
path: a file with no world entry point emits an **empty world** (the
resolved world name, no exports) alongside its interface definitions. This
keeps the output valid WIT and round-trippable without committing to a
library model. Promoting world-less libraries to a first-class concept —
and deciding whether the emitted world should disappear entirely rather
than be empty — is deferred to a future WEP.

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
- [ ] Producer side: emit WIT text and embed `component-type` in output
      (WEP: WIT Bundling for the format; this WEP §"Producer Side: WIT
      Generation and Embedding" for the detailed design). Phase 0
      (`Semantics` refactor) and Phase 1 (`wado wit` text) are done; Phase 2
      (`wado compile` embeds WIT by default, scope `full`, `--no-wit` to opt
      out) and Phase 3 (`[wit].scope` in `wado.toml`) remain.
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
- [ ] Close the binding-synthesis gaps required for arbitrary worlds: struct,
      variant, and `Result` parameter lifting; sync export support;
      return-via-outptr when flat count exceeds `MAX_FLAT_RESULTS`.
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

## Consequences

### Positive

- A single, documented end-to-end goal for WIT support replaces a scatter of
  point WEPs.
- One keyword (`interface`) for block declarations on both the import and
  export sides, matching WIT's vocabulary directly. The `effect` keyword
  keeps a narrow, well-defined role (polymorphic effect parameters
  `<effect E>`).
- World imports are traceable to WIT FQ names by construction, removing the
  fragile method-name-based disambiguation in `CmInterfaceRegistry`.
- Adding a new WASI or third-party CM library no longer requires patching the
  compiler or the stdlib list.
- The producer (WIT Bundling) and consumer (this WEP) sides become
  symmetrical: a Wado-compiled component can be consumed by another Wado
  compiler with no extra metadata path.

### Negative

- The block-form rename `effect Foo { ... }` → `interface Foo { ... }` is
  a one-shot, no-deprecation change against the stdlib, fixtures, and any
  user code. It has already landed.
- Bringing `wit-encoder` / `wit-component` into the compiler increases the
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
