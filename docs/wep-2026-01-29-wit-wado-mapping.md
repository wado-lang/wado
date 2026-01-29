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
2. **WIT consumption**: Import external WIT definitions into Wado (via `wado-from-wit`)
3. **Language design**: Ensure Wado constructs align with Component Model concepts

## Export Principle

Wado distinguishes between module-level visibility (`pub`) and Component Model boundary visibility (`export`):

| Keyword | Scope | Purpose |
|---------|-------|---------|
| `pub` | Wado modules | Share across Wado modules internally |
| `export` | CM world boundary | Expose to external components |

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

## Interface and Effect

Wado introduces explicit `interface` blocks for grouping exports, complementing the existing `effect` for imports.

### Interface vs Effect

| Wado | WIT | Direction | Has Side Effects |
|------|-----|-----------|------------------|
| `interface` | `interface` | export (primary) | No (pure) |
| `effect` | `interface` | import (primary) | Yes |

Both `interface` and `effect` map to WIT `interface`. The distinction exists in Wado's type system:

- **`effect`**: Represents interfaces that modify global state or perform I/O. Functions from effects require `with` annotations for effect tracking.
- **`interface`**: Represents pure interfaces without side effects. Functions can be called without effect annotations.

### WIT Lacks Purity Annotations

WIT currently has no way to express whether an interface is pure or effectful. This is a known limitation:

- [Component Model Issue #321: Add `pure` annotation to WIT](https://github.com/WebAssembly/component-model/issues/321)

Wado's `effect` vs `interface` distinction is a Wado-side concept. When generating WIT, both become `interface`:

```wado
// Wado source
effect Stdout {
    fn print(s: String);
}

interface Calculator {
    fn add(a: i32, b: i32) -> i32;
}
```

```wit
// Generated WIT (no distinction)
interface stdout {
    print: func(s: string);
}

interface calculator {
    add: func(a: s32, b: s32) -> s32;
}
```

### Effect Tracking

```wado
use {print} from Stdout;       // from effect → requires `with`
use {add} from Calculator;     // from interface → no `with` needed

fn pure_function() -> i32 {
    return add(1, 2);          // OK: Calculator is pure
}

fn effectful_function() with Stdout {
    print("hello");            // OK: Stdout effect declared
}

fn error_function() {
    print("hello");            // ERROR: missing `with Stdout`
}
```

## Interface Syntax

### Explicit Interface (for grouping)

```wado
export interface MyApi {
    struct Point { x: i32, y: i32 }
    fn add(a: i32, b: i32) -> i32;
    fn distance(p1: Point, p2: Point) -> f64;
}
```

```wit
// Generated WIT
interface my-api {
    record point { x: s32, y: s32 }
    add: func(a: s32, b: s32) -> s32;
    distance: func(p1: point, p2: point) -> f64;
}

world my-app {
    export my-api;
}
```

### Implicit Interface (top-level exports)

For simple cases, top-level `export` declarations are collected into an implicit interface:

```wado
export struct Point { x: i32, y: i32 }
export fn origin() -> Point { ... }
```

```wit
// Generated WIT
interface exports {
    record point { x: s32, y: s32 }
    origin: func() -> point;
}

world my-app {
    export exports;
}
```

### Functions Without Types (direct world export)

When only functions are exported (no types), they can be exported directly in the world:

```wado
export fn run() { ... }
export fn add(a: i32, b: i32) -> i32 { ... }
```

```wit
// Generated WIT (no interface needed)
world my-app {
    export run: func();
    export add: func(a: s32, b: s32) -> s32;
}
```

## WIT to Wado Type Mapping

### Primitive Types

| WIT | Wado | Notes |
|-----|------|-------|
| `bool` | `bool` | Direct mapping |
| `s8`, `s16`, `s32`, `s64` | `i8`, `i16`, `i32`, `i64` | Signed integers |
| `u8`, `u16`, `u32`, `u64` | `u8`, `u16`, `u32`, `u64` | Unsigned integers |
| `f32`, `f64` | `f32`, `f64` | Floats |
| `char` | `char` | Unicode scalar value |
| `string` | `String` | UTF-8 string |

### Compound Types

| WIT | Wado | Notes |
|-----|------|-------|
| `list<T>` | `Array<T>` | Dynamic array |
| `option<T>` | `Option<T>` | Optional value |
| `result<T, E>` | `Result<T, E>` | Result type |
| `tuple<T, U, ...>` | `[T, U, ...]` | Tuple type |

### User-Defined Types

| WIT | Wado | Notes |
|-----|------|-------|
| `record` | `struct` | Named fields |
| `variant` | `variant` | Tagged union with payloads |
| `enum` | `enum` | Discriminated values without payloads |
| `flags` | `flags` | Bitfield (parsed but not yet implemented) |
| `resource` | `resource` | Handle type (not yet implemented) |
| `type alias` | `type` | Type synonym |

### Functions

| WIT | Wado | Notes |
|-----|------|-------|
| `func(a: t1) -> t2` | `fn f(a: t1) -> t2` | Function signature |
| `func() -> result<T, E>` | `fn f() -> Result<T, E>` | Fallible function |
| async function | `async fn` | Async in WASI P3 |

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

Options:
- `wado:{module-name}` - fixed namespace (default)
- `{user}:{module-name}` - user-configurable namespace
- From project manifest (future `wado.toml`)

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

### Phase 3: Interface Syntax

- [ ] Parse `export interface Name { ... }` blocks
- [ ] Generate named interfaces in WIT
- [ ] Support multiple interfaces per module

### Phase 4: CLI Integration

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

### Negative

- Slightly larger binary size (custom section)
- Additional dependency (`wit-component`)
- Must keep WIT generation in sync with codegen

## References

- [Component Model WIT specification](https://component-model.bytecodealliance.org/design/wit.html)
- [wit-component crate](https://crates.io/crates/wit-component)
- [Component Model Issue #321: Pure annotation](https://github.com/WebAssembly/component-model/issues/321)
- [WEP: World Conformance](./wep-2026-01-16-world-conformance-and-export.md)
- [WEP: Effect System Design](./wep-2026-01-27-effect-system-design.md)
