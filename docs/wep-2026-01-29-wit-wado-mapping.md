# WEP: WIT and Wado Mapping

## Context

Wado compiles to WebAssembly Component Model and needs a clear mapping between WIT (WebAssembly Interface Types) and Wado language constructs. This mapping enables:

- Runtime introspection of component types
- Tooling integration (composition, documentation generation)
- IDE support for component consumers
- Registry publishing with type information

## Decision

Define a clear bidirectional mapping between WIT constructs and Wado language features. This mapping guides:

1. **WIT generation**: Auto-generate WIT from Wado source for embedding in compiled components
2. **WIT consumption**: Import external WIT definitions into Wado (via `wado-from-idl`)
3. **Language design**: Ensure Wado constructs align with Component Model concepts

## Export Principle

Wado distinguishes between module-level visibility (`pub`) and Component Model boundary visibility (`export`):

| Keyword  | Scope             | Purpose                              |
| -------- | ----------------- | ------------------------------------ |
| `pub`    | Wado modules      | Share across Wado modules internally |
| `export` | CM world boundary | Expose to external components        |

This separation solves the common problem of "utility modules accidentally becoming public":

```wado
// utils.wado - internal utilities
pub fn helper() { ... }        // visible to other Wado modules
pub struct Internal { ... }    // visible to other Wado modules
// → NOT exposed at CM boundary

// api.wado - public API
export fn run() { ... }        // exposed at CM boundary
export struct Point { ... }    // exposed at CM boundary
```

Only items explicitly marked with `export` appear in the generated WIT and Component Model interface.

### Exportable Items

```wado
export fn process() -> Result<T, E>   // function
export struct Point { x: i32, y: i32 } // record
export variant Shape { ... }           // variant
export enum Color { ... }              // enum
export type ID = String;               // type alias
export interface MyApi { ... }         // interface (see below)
```

## Interface Block

### Design Rationale

In CM/WIT, an `interface` is a collection of related types and functions that forms a reusable unit of API surface. It is not a behavioral abstraction (like Java interfaces or Rust traits) — it is an **API boundary** that groups a coherent set of functionality for cross-component interop.

In most source languages, this concept is implicit:

| Language | What serves as "interface"  | Mechanism                                                               |
| -------- | --------------------------- | ----------------------------------------------------------------------- |
| C        | Header file (`.h`)          | Declares types and function signatures; consumers `#include` the header |
| Zig      | Module's `pub` declarations | File = module; public symbols form the API surface                      |
| Rust     | Crate's public API surface  | `pub` items in the crate root; no explicit "interface" keyword          |

In all three cases, the "interface" is simply **whatever you made public**. There is no separate syntax to declare it. CM differs because components need machine-readable contracts for language-agnostic interop.

Wado's `interface` block exists **solely for CM purposes**: it is a grouping syntax that declares which items form a named CM interface. It has no semantic meaning within Wado's type system — no namespace, no purity guarantee, no behavioral contract. Items are defined normally in Wado; the `interface` block references them by name.

### Relationship with `effect`

Both `interface` and `effect` map to WIT `interface`, but they serve different roles in Wado:

| Wado        | WIT         | Direction | Wado semantics                                                       |
| ----------- | ----------- | --------- | -------------------------------------------------------------------- |
| `effect`    | `interface` | import    | Defines capability requirements; functions require `with` annotation |
| `interface` | `interface` | export    | Groups items for CM export; no Wado-level semantics                  |

An `effect` has deep Wado meaning: it participates in effect tracking and constrains function signatures. An `interface` is purely organizational — it tells the CM layer how to package exports.

When generating WIT, both become `interface`:

```wado
// effect = import-side interface (Wado semantics: effect tracking)
interface Stdout {
    fn print(s: String);
}

// interface = export-side grouping (Wado semantics: none)
struct Point { x: i32, y: i32 }
fn distance(p1: &Point, p2: &Point) -> f64 { ... }

export interface Geometry {
    Point,
    distance,
}
```

```wit
// Generated WIT
interface stdout {
    print: func(s: string);
}

interface geometry {
    record point { x: s32, y: s32 }
    distance: func(p1: borrow<point>, p2: borrow<point>) -> f64;
}
```

### Default Interface (Implicit Grouping)

Bare `export` declarations (not inside an explicit `interface` block) are automatically collected into a **default interface**. The default interface is named after `[package].name` from `wado.toml`, or derived from the entry file name in single-file mode.

```wado
// [package].name = "geometry"
export struct Point { x: i32, y: i32 }
export fn distance(p1: &Point, p2: &Point) -> f64 { ... }
export fn origin() -> Point { ... }
```

```wit
// Generated WIT — default interface named "geometry"
interface geometry {
    record point { x: s32, y: s32 }
    distance: func(p1: borrow<point>, p2: borrow<point>) -> f64;
    origin: func() -> point;
}

world geometry {
    export geometry;
}
```

When only functions are exported (no types), they become direct world exports instead of forming an interface, since CM allows this and it is more idiomatic:

```wado
export fn run() { ... }
```

```wit
world my-app {
    export run: func();
}
```

This also applies to world-conformance entry points like `export fn run()` (wasi:cli/command) and `export fn handle(...)` (wasi:http/service), which are always direct world exports regardless of other exports.

### Explicit Interface (Fine-Grained Control)

When a component needs to export multiple interfaces, or when the default grouping is not sufficient, use explicit `interface` blocks. The block **lists names** of items defined elsewhere — it does not contain definitions:

```wado
// Define items normally
struct Point { x: i32, y: i32 }
struct Color { r: u8, g: u8, b: u8 }
fn distance(p1: &Point, p2: &Point) -> f64 { ... }
fn blend(a: &Color, b: &Color) -> Color { ... }
fn origin() -> Point { ... }

// Group into separate CM interfaces
export interface Geometry {
    Point,
    distance,
    origin,
}

export interface Colors {
    Color,
    blend,
}
```

```wit
interface geometry {
    record point { x: s32, y: s32 }
    distance: func(p1: borrow<point>, p2: borrow<point>) -> f64;
    origin: func() -> point;
}

interface colors {
    record color { r: u8, g: u8, b: u8 }
    blend: func(a: borrow<color>, b: borrow<color>) -> color;
}

world my-app {
    export geometry;
    export colors;
}
```

Items listed in an `export interface` are exported through that interface regardless of their original visibility. They do not need the `export` keyword individually.

### `#![no_default_interface]`

By default, bare `export` items form a default interface. The `#![no_default_interface]` attribute disables this, requiring all exports to be placed in explicit `interface` blocks:

```wado
#![no_default_interface]

// World-conformance entry points remain direct world exports
export fn run() with Stdout { ... }

// Other items must be explicitly grouped
struct Point { x: i32, y: i32 }
fn distance(p1: &Point, p2: &Point) -> f64 { ... }

export interface Geometry {
    Point,
    distance,
}
```

```wit
world my-app {
    import wasi:cli/stdout@0.3.0;
    export run: func();
    export geometry;
}

interface geometry {
    record point { x: s32, y: s32 }
    distance: func(p1: borrow<point>, p2: borrow<point>) -> f64;
}
```

With `#![no_default_interface]`, a bare `export struct Point` (not in any interface block) is a compile error. This ensures the developer is explicit about which CM interface each item belongs to.

### Exporting Effects

An `effect` with the `export` keyword becomes an exported CM interface. This is for components that **provide** a capability for other components to consume:

```wado
// Producer component: provides logging capability
export interface Logger {
    log,
    set_level,
}

fn log(message: String) {
    // implementation
}

fn set_level(level: i32) {
    // implementation
}
```

```wit
// Generated WIT
interface logger {
    log: func(message: string);
    set-level: func(level: s32);
}

world logging-service {
    export logger;
}
```

The consumer imports this as a regular `effect`:

```wado
// Consumer component
use { log } from "wasi:logging";  // or whatever the import path is

fn do_work() with Logger {
    log("processing...");
}
```

The `effect` keyword (rather than plain `interface`) signals to the consumer that these functions have side effects and require `with` annotations. This distinction is a Wado-level concept — WIT has no purity annotation (see [Component Model Issue #321](https://github.com/WebAssembly/component-model/issues/321)).

### Transitive Type Inclusion

Types referenced in exported function signatures are automatically included in the interface, even if not explicitly listed:

```wado
struct Point { x: i32, y: i32 }
fn origin() -> Point { return Point { x: 0, y: 0 }; }

export interface Geometry {
    origin,
    // Point is automatically included because origin() returns it
}
```

This avoids forcing developers to manually list every type dependency.

## WIT to Wado Type Mapping

### Primitive Types

| WIT                       | Wado                      | Notes                |
| ------------------------- | ------------------------- | -------------------- |
| `bool`                    | `bool`                    | Direct mapping       |
| `s8`, `s16`, `s32`, `s64` | `i8`, `i16`, `i32`, `i64` | Signed integers      |
| `u8`, `u16`, `u32`, `u64` | `u8`, `u16`, `u32`, `u64` | Unsigned integers    |
| `f32`, `f64`              | `f32`, `f64`              | Floats               |
| `char`                    | `char`                    | Unicode scalar value |
| `string`                  | `String`                  | UTF-8 string         |

### Compound Types

| WIT                | Wado           | Notes          |
| ------------------ | -------------- | -------------- |
| `list<T>`          | `Array<T>`     | Dynamic array  |
| `option<T>`        | `Option<T>`    | Optional value |
| `result<T, E>`     | `Result<T, E>` | Result type    |
| `tuple<T, U, ...>` | `[T, U, ...]`  | Tuple type     |

### User-Defined Types

| WIT          | Wado       | Notes                                     |
| ------------ | ---------- | ----------------------------------------- |
| `record`     | `struct`   | Named fields                              |
| `variant`    | `variant`  | Tagged union with payloads                |
| `enum`       | `enum`     | Discriminated values without payloads     |
| `flags`      | `flags`    | Bitfield (parsed but not yet implemented) |
| `resource`   | `resource` | Handle type (not yet implemented)         |
| `type alias` | `type`     | Type synonym                              |

### Functions

| WIT                      | Wado                     | Notes              |
| ------------------------ | ------------------------ | ------------------ |
| `func(a: t1) -> t2`      | `fn f(a: t1) -> t2`      | Function signature |
| `func() -> result<T, E>` | `fn f() -> Result<T, E>` | Fallible function  |
| async function           | `async fn`               | Async in WASI P3   |

## WIT Structure Mapping

### Package

```wit
package wado:my-app@1.0.0;
```

Derived from:

- Namespace: `wado` (fixed, or configurable)
- Name: entry module name or explicit declaration
- Version: optional, from project config

### World

```wit
world my-app {
    import wasi:cli/stdout@0.3.0;
    import wasi:cli/stderr@0.3.0;
    export my-api;
    export run: func();
}
```

Wado equivalent:

```wado
// Imports derived from effect usage
use {Stdout} from "wasi:cli";

// Explicit interface export
export interface MyApi { ... }

// Direct function export
export fn run() with Stdout {
    println("Hello!");
}
```

## Auto-Generation Strategy

### World Generation

When no explicit world is declared, generate one from:

1. **Imports**: Collect from `WasiRegistry` (used WASI interfaces via effects)
2. **Exports**: Collect from items marked with `export`

```
┌─────────────────────┐    ┌─────────────────────┐
│    WasiRegistry     │    │   export items      │
│  (used effects)     │    │  (fn, struct, etc.) │
└─────────┬───────────┘    └──────────┬──────────┘
          │                           │
          └───────────┬───────────────┘
                      ▼
              ┌───────────────┐
              │ WIT Generator │
              └───────┬───────┘
                      ▼
              ┌───────────────┐
              │   Resolve     │
              │   (wit-parser)│
              └───────┬───────┘
                      ▼
              ┌───────────────┐
              │ embed_metadata│
              └───────────────┘
```

### Export Collection Rules

1. **Explicit `export` required**: Only items with `export` keyword are included
2. **Transitive types**: Types referenced in exported signatures are automatically included
3. **Interface grouping**: Explicit `export interface` creates named interfaces; top-level exports form implicit interface

## Package Naming

Derived from `wado.toml` when present:

- WIT package: `{namespace}:{name}@{version}` (e.g., `myorg:geometry@1.0.0`)
- Without `namespace`: `local:{name}@{version}` (non-publishable packages)
- Without `wado.toml` (single-file mode): `local:{filename}` (e.g., `local:hello` for `hello.wado`)

## Implementation Plan

### Phase 1: Basic Embedding

- [ ] Add `wit-component` dependency
- [ ] Generate WIT text from `WasiRegistry` + `export fn`
- [ ] Parse with `wit-parser` to get `Resolve`
- [ ] Call `embed_component_metadata()` in codegen

### Phase 2: Type Export

- [ ] Support `export struct/variant/enum/type`
- [ ] Collect transitive type dependencies
- [ ] Generate WIT record/variant/enum definitions
- [ ] Default interface generation (named after `[package].name`)

### Phase 3: Interface Syntax

- [ ] Parse `export interface Name { item, ... }` blocks (name-listing syntax)
- [ ] Validate listed names resolve to defined items
- [ ] Generate named interfaces in WIT
- [ ] Support multiple interfaces per module
- [ ] `#![no_default_interface]` attribute

### Phase 4: Export Effect

- [ ] Parse `export effect Name { item, ... }` blocks
- [ ] Generate exported interfaces from effects
- [ ] Ensure consumer-side effect tracking works across package boundaries

### Phase 5: CLI Integration

- [ ] Add `--emit-wit` flag to output WIT text
- [ ] Add `--no-wit-embed` flag to disable embedding
- [ ] Consider `wado wit` subcommand for WIT inspection

## Consequences

### Positive

- Components are self-describing
- Better tooling integration
- Enables component composition
- Registry-ready artifacts
- Clear separation between internal (`pub`) and external (`export`) visibility
- `interface` as CM-only grouping keeps Wado's type system simple (no second namespace mechanism)
- Default interface from bare `export` covers the common case with zero syntax overhead
- Name-listing syntax makes the `interface` block a clear manifest of what's exported
- `export effect` reuses the existing effect concept for exported capabilities

### Negative

- Slightly larger binary size (custom section)
- Additional dependency (`wit-component`)
- Must keep WIT generation in sync with codegen

### Trade-offs

- `interface` has no Wado-level semantics (no namespace, no purity). This is simpler but means `interface` is purely a CM concept that Wado developers only encounter when thinking about component boundaries. This is acceptable because most languages do not have an explicit "interface" concept either — it is an emergent property of public declarations.
- Default interface naming depends on `[package].name`, creating a coupling between the manifest and generated WIT. This is intentional — the package name is the natural identity for a component's API surface.
- `#![no_default_interface]` is opt-in strictness. The default behavior (implicit grouping) is convenient for simple components; the attribute is for advanced use cases with multiple interfaces.

## References

- [Component Model WIT specification](https://component-model.bytecodealliance.org/design/wit.html)
- [wit-component crate](https://crates.io/crates/wit-component)
- [Component Model Issue #321: Pure annotation](https://github.com/WebAssembly/component-model/issues/321)
- [WEP: World Conformance](./wep-2026-01-16-world-conformance-and-export.md)
- [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [WEP: Package Manifest](./wep-2026-02-14-package-manifest.md)
