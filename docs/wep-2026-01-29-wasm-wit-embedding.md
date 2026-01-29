# WEP: WebAssembly WIT Embedding

## Context

Wado compiles to WebAssembly Component Model. Currently, the compiler generates valid components but does not embed WIT metadata that describes the component's interface. Embedding WIT enables:

- Runtime introspection of component types
- Tooling integration (composition, documentation generation)
- IDE support for component consumers
- Registry publishing with type information

## Decision

Embed auto-generated WIT metadata into compiled Wasm components using the `wit-component` crate's `embed_component_metadata()` API.

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

### Interface

```wit
interface my-exports {
    record point { x: s32, y: s32 }
    add: func(a: s32, b: s32) -> s32;
}
```

Wado equivalent:

```wado
// Exported types and functions form an implicit interface
pub struct Point { x: i32, y: i32 }
export fn add(a: i32, b: i32) -> i32 { ... }
```

### World

```wit
world my-app {
    import wasi:cli/stdout@0.3.0;
    import wasi:cli/stderr@0.3.0;
    export run: func();
}
```

Wado equivalent:

```wado
// Imports derived from `use wasi:*` and effect usage
use {Stdout} from "wasi:cli";

// Exports derived from `export fn`
export fn run() with Stdout {
    println("Hello!");
}
```

## Auto-Generation Strategy

### World Generation

When no explicit world is declared, generate one from:

1. **Imports**: Collect from `WasiRegistry` (used WASI interfaces)
2. **Exports**: Collect from functions marked with `export`

```
┌─────────────────────┐    ┌─────────────────────┐
│    WasiRegistry     │    │  export fn list     │
│  (used interfaces)  │    │  (from TIR/codegen) │
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

### Type Export Rules

Only export types that are:
1. Marked with `pub` visibility
2. Used in `export fn` signatures (parameters or return types)
3. Transitively referenced by exported types

## Open Questions

### Interface Grouping

Should Wado support explicit interface declarations?

Option A: Implicit single interface (all exports in one interface)
```wit
world my-app {
    export my-exports;  // single interface with all exports
}
```

Option B: Explicit interface syntax
```wado
interface MyApi {
    fn add(a: i32, b: i32) -> i32;
    fn sub(a: i32, b: i32) -> i32;
}
export interface MyApi;
```

**Current decision**: Option A (implicit) for simplicity.

### Package Naming

Options:
- `wado:{module-name}` - fixed namespace
- `{user}:{module-name}` - user-configurable namespace
- From project manifest (future `wado.toml`)

**Current decision**: `wado:{module-name}` as default.

### Version Embedding

Options:
- No version (omit from package declaration)
- From CLI flag (`--version 1.0.0`)
- From project manifest (future)

**Current decision**: No version initially.

## Implementation Plan

### Phase 1: Basic Embedding

- [ ] Add `wit-component` dependency
- [ ] Generate WIT text from `WasiRegistry` + `export fn`
- [ ] Parse with `wit-parser` to get `Resolve`
- [ ] Call `embed_component_metadata()` in codegen

### Phase 2: Type Export

- [ ] Collect exported struct/variant/enum types
- [ ] Generate WIT record/variant/enum definitions
- [ ] Handle type references in function signatures

### Phase 3: CLI Integration

- [ ] Add `--emit-wit` flag to output WIT text
- [ ] Add `--no-wit-embed` flag to disable embedding
- [ ] Consider `wado wit` subcommand for WIT inspection

## Consequences

### Positive

- Components are self-describing
- Better tooling integration
- Enables component composition
- Registry-ready artifacts

### Negative

- Slightly larger binary size (custom section)
- Additional dependency (`wit-component`)
- Must keep WIT generation in sync with codegen

## References

- [Component Model WIT specification](https://component-model.bytecodealliance.org/design/wit.html)
- [wit-component crate](https://crates.io/crates/wit-component)
- [WEP: World Conformance](./wep-2026-01-16-world-conformance-and-export.md)
