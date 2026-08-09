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

- WASI Preview 1 or Preview 2. The target is P3.
- Parsing standalone `.wit` text as a primary compiler input.
- Replacing `wado-from-idl`.

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
