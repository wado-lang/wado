# WEP: WebAssembly Module Import Support

## Context

Wado aims to be a "Wasm only" language, maintaining zero abstraction over WebAssembly. To achieve this goal and enable interoperability with the broader Wasm ecosystem, we need a mechanism to import and integrate existing WebAssembly modules directly into Wado programs.

This capability is essential for:

1. **Standard library implementation**: Integrating deterministic math functions (see WEP-2026-01-10-deterministic-libm)
2. **Ecosystem integration**: Using existing Wasm libraries (cryptography, parsers, etc.)
3. **Multi-language projects**: Composing modules written in different languages (Rust, C, AssemblyScript, etc.)

### Current State

Wado currently supports imports only from:

- `.wado` modules (local files, compiled as core Wasm and linked with shared memory)
- `core:*` namespace (core library, written in Wado)
- `wasi:*` namespace (WASI interfaces, mapped to WIT)
- `https:` URLs (remote modules)

There is no support for importing external compiled `.wasm` files (Component Model format).

## Decision

Introduce **WebAssembly Component Model** import support with a **two-tier strategy** to balance type safety and performance:

1. **Wado-to-Wado**: Core Wasm linking with zero overhead (current mechanism)
2. **External Components**: Component Model boundary with full type safety

### Two-Tier Strategy

#### Tier 1: Wado-to-Wado (Zero-overhead linking)

```wado
// Same project, Wado source files
use {helper} from "./lib.wado";
```

**Implementation**:
- Compile as core Wasm modules (not components)
- Link with shared memory (current `wado-bundled.wat` approach)
- Enable cross-module optimizations (inlining, DCE)
- **Zero Component Model overhead**

**Use case**: Internal project modules, standard library

#### Tier 2: External Wasm Components (Type-safe interop)

```wado
// External component (possibly from other language)
use {compress} from "./zlib.wasm";
```

**Implementation**:
- Must be Component Model format (with embedded WIT)
- Type information extracted from component binary
- Canonical ABI for lowering/lifting
- **Component Model overhead** (necessary for type safety)

**Use case**: Third-party libraries, cross-language modules

### Syntax

```wado
// External Wasm component (Component Model format)
use {sin, cos} from "./libm.wasm";

// No `type: "wasm"` needed - .wasm extension implies component
// WIT is extracted from component binary (embedded)

// JSON import (for comparison)
use config from "./config.json" with { type: "json" };
```

### Type Annotation Rules

| Import Source        | `type` Attribute | Format                  | Linking Strategy     |
| -------------------- | ---------------- | ----------------------- | -------------------- |
| `.wado` files        | Not applicable   | Wado source             | Core Wasm linking    |
| `.wasm` files        | Not applicable   | Component Model (+ WIT) | Component boundary   |
| `.json` files        | **Required**     | `type: "json"`          | Compile-time embed   |
| `core:*`, `wasi:*`   | Not applicable   | Special namespace       | Core Wasm linking    |
| `https:` URLs        | **Required**     | Must specify content    | Depends on type      |
| Future: `.wit` files | **Required**     | `type: "wit"`           | Interface-only (TBD) |

### WIT Requirements

**Component Model format mandates embedded type information** in the binary:

- Component binaries include type sections (binary format)
- WIT text format can be extracted via `wasm-tools component wit`
- **External WIT files are NOT required** - types are in the component binary
- Optional: External `.wit` files for development convenience (future enhancement)

### Implementation Requirements

1. **Component Model Binary Parsing**
   - Parse Component Model format using `wasmparser`
   - Extract embedded type information from component binary
   - No need to parse WIT text format (types are in binary)

2. **WIT Type Extraction and Mapping**
   - Extract type information from component binary type sections
   - Map WIT types to Wado types:
     - `bool`, `s32`, `u32`, `f32`, `f64` → Wado primitives
     - `string` → `String`
     - `list<T>` → `Array<T>`
     - `option<T>` → `Option<T>`
     - `record { ... }` → `struct { ... }`
   - Generate Wado type definitions for imported interfaces

3. **Type Checking**
   - Validate imported function signatures against usage
   - Ensure type compatibility at Component Model boundary
   - Reject imports with unsupported WIT types (early error)

4. **Dual Linking Strategy**

```
┌───────────────────────────────────────────────────────┐
│ Wado Compiler                                         │
├───────────────────────────────────────────────────────┤
│ 1. Import resolution                                  │
│    .wado → Parse as Wado source                       │
│    .wasm → Parse as Component Model binary            │
│                                                       │
│ 2. Type checking                                      │
│    .wado → Wado type system (internal)                │
│    .wasm → WIT type extraction + mapping              │
│                                                       │
│ 3. Code generation                                    │
│    .wado → Core Wasm module (shared memory)           │
│    .wasm → Component boundary (canonical ABI)         │
│                                                       │
│ 4. Linking                                            │
│    Wado-to-Wado → Core Wasm linking (zero overhead)   │
│    External .wasm → Component composition             │
│                                                       │
│ 5. Optimization (future)                              │
│    Core modules → wasm-opt inline, DCE                │
│    Components → Limited (ABI boundary)                │
└───────────────────────────────────────────────────────┘
```

### Tooling Dependencies

```toml
[dependencies]
wasmparser = "0.121"        # Parse Wasm and Component Model binaries
wit-component = "0.18"      # Component Model type extraction
wasm-encoder = "0.38"       # Generate Wasm
wasm-tools (CLI)            # Component composition (`wasm-tools compose`)
```

**Note**: `wit-parser` is NOT needed - type information is extracted from component binaries, not parsed from WIT text files.

## Consequences

### Positive

✅ **Ecosystem interoperability**: Can use any Component Model library, regardless of source language
✅ **Component Model alignment**: Fully leverages embedded type information for type safety
✅ **Zero-overhead internal modules**: Wado-to-Wado linking maintains performance
✅ **Standard library flexibility**: Enables bundling optimized libraries (Rust libm, etc.)
✅ **Future-proof**: Supports emerging Wasm ecosystem standards
✅ **Type safety**: Component Model enforces type correctness at boundaries
✅ **Simplicity**: No manual type annotations needed (types are in component binary)

### Negative

⚠️ **Component Model overhead**: External imports have Canonical ABI lifting/lowering cost
⚠️ **Limited cross-component optimization**: Cannot inline across Component Model boundaries
⚠️ **Build time**: Parsing component binaries adds compilation overhead
⚠️ **Debugging**: Multi-language debugging can be challenging
⚠️ **Versioning**: Need to handle compatibility between Wasm components

### Trade-offs

#### Performance: Component Model Overhead

**Problem**: Canonical ABI (lowering/lifting) has runtime cost

**Mitigation**:
- **Two-tier strategy**: Wado-to-Wado uses core linking (zero overhead)
- **Strategic bundling**: Keep performance-critical code in Wado
- **Future optimization**: Component Model may add inline hints

**Benchmark expectations**:
- Wado-to-Wado function call: ~0 overhead (same as internal call)
- Component boundary call: ~10-50ns overhead (ABI translation)
- For most use cases (crypto, parsing, I/O), ABI overhead is negligible

#### Component Model Dependency

**Trade-off**: Ties Wado to Component Model evolution

**Rationale**:
- Component Model is the **only** standard for type-safe Wasm interop
- Core Wasm MVP lacks type information (`i32` ambiguity)
- Alternative (Core Wasm + manual bindings) is unsafe and unmaintainable

**Mitigation**:
- Component Model is stable (1.0 released)
- Wado can evolve independently within Component Model constraints

### Performance Considerations

#### wado-bundled Migration

Current `wado-bundled.wat` is **Core Wasm** (MVP format) for zero overhead:

```wat
;; Current: Core Wasm
(func $f64_to_buffer (param f64 i32) (result i32))
```

**Migration plan**:
1. **Keep core linking internally**: `wado-bundled` stays as core module
2. **Add WIT documentation**: Define types for external projects
3. **Optional component wrapper**: For external use only
4. **Wado compiler**: Continue using core linking (zero overhead)

**Rationale**: `wado-bundled` is internal to Wado stdlib - no need for Component Model overhead.

## Implementation Plan

### Phase 1: Component Model Parsing

- [ ] Parse `use {...} from "*.wasm"` syntax (no type annotation needed)
- [ ] Integrate `wasmparser` for Component Model binary parsing
- [ ] Extract type information from component binary sections
- [ ] Error handling for:
  - Missing/invalid `.wasm` files
  - Non-component binaries (must be Component Model format)
  - Unsupported WIT types

### Phase 2: WIT Type Mapping

- [ ] Map WIT primitives to Wado types (bool, integers, floats)
- [ ] Map WIT `string` to Wado `String`
- [ ] Map WIT `list<T>` to Wado `Array<T>`
- [ ] Map WIT `option<T>` to Wado `Option<T>`
- [ ] Map WIT `record` to Wado `struct`
- [ ] Generate Wado type definitions for imported interfaces
- [ ] Type check imported functions against usage

### Phase 3: Component Boundary Codegen

- [ ] Generate Canonical ABI adapters for imported functions
- [ ] Handle lowering: Wado types → WIT types
- [ ] Handle lifting: WIT types → Wado types
- [ ] Integrate with existing codegen pipeline

### Phase 4: Component Composition

- [ ] Use `wasm-tools compose` to combine components
- [ ] Handle import/export resolution
- [ ] Generate final Component Model output
- [ ] Ensure Wado-to-Wado core linking still works

### Phase 5: Optimization and Tooling

- [ ] Tree-shaking unused imports
- [ ] Cache parsed component metadata
- [ ] Cross-module optimization for Wado-to-Wado
- [ ] wasm-opt integration for core modules

### Future: wado-bundled Component Wrapper (Optional)

- [ ] Add WIT definitions for `wado-bundled` functions
- [ ] Create Component Model wrapper (for external projects)
- [ ] Keep core linking internally (Wado compiler)
- [ ] Document usage for non-Wado projects

## References

- [WebAssembly Component Model](https://github.com/WebAssembly/component-model)
- [WIT (Wasm Interface Types) Format](https://component-model.bytecodealliance.org/design/wit.html)
- [Canonical ABI](https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md)
- [wasm-tools Documentation](https://github.com/bytecodealliance/wasm-tools)
- Related WEP: [WEP-2026-01-10-deterministic-libm](./wep-2026-01-10-deterministic-libm.md)
