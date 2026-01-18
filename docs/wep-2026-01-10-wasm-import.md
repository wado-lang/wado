# WEP: WebAssembly Module Import Support

## Context

Wado aims to be a "Wasm only" language, maintaining zero abstraction over WebAssembly. To achieve this goal and enable interoperability with the broader Wasm ecosystem, we need a mechanism to import and integrate existing WebAssembly modules directly into Wado programs.

This capability is essential for:

1. **Standard library implementation**: Integrating deterministic math functions (see WEP-2026-01-10-deterministic-libm)
2. **Ecosystem integration**: Using existing Wasm libraries (cryptography, parsers, etc.)
3. **Multi-language projects**: Composing modules written in different languages (Rust, C, AssemblyScript, etc.)

### Current State

Wado currently supports imports only from:

- `.wado` modules (local files, integrated at IR level during compilation)
- `core:*` namespace (core library, written in Wado)
- `wasi:*` namespace (WASI interfaces, mapped to WIT)
- `https:` URLs (remote modules)

There is no support for importing external compiled `.wasm` files (Component Model format).

## Decision

Introduce **WebAssembly Component Model** import support with a **two-tier strategy** to balance type safety and performance:

1. **Wado-to-Wado**: Core Wasm linking with zero overhead (current mechanism)
2. **External Components**: Component Model boundary with full type safety

### Two-Tier Strategy

The compiler determines linking strategy based on component origin, not file extension.

#### Tier 1: Wado-origin (Zero-overhead linking)

```wado
// Wado source file
use {helper} from "./lib.wado";

// Wado-compiled component (detected via metadata)
use {utils} from "./precompiled.wasm";  // If compiled by Wado compiler
```

**Implementation**:
- **`.wado` source**: Integrate at IR (TIR) level during compilation (no Wasm intermediate)
- **`.wasm` (Wado-origin)**: Detect via custom section marker `@custom "wado-compiler"`, link as core module with shared memory
- Enable cross-module optimizations (inlining, DCE, LTO)
- **Zero Component Model overhead**

**Use case**: Internal project modules, standard library, pre-compiled Wado modules

#### Tier 2: External Wasm Components (Type-safe interop)

```wado
// External component (from Rust, C, AssemblyScript, etc.)
use {compress} from "./zlib.wasm";  // No wado-compiler marker
```

**Implementation**:
- Component Model format (with embedded WIT)
- Type information extracted from component binary
- Canonical ABI for lowering/lifting
- **Component Model overhead** (necessary for type safety)

**Use case**: Third-party libraries, cross-language modules

### Syntax

```wado
// Wado source file (IR-level integration)
use {helper} from "./lib.wado";

// Wado-compiled component (auto-detected via metadata, core linking)
use {utils} from "./precompiled.wasm";
// Has @custom "wado-compiler" → Zero overhead, no annotation needed

// External Wasm component (Component Model boundary, annotation REQUIRED)
use {sin, cos} from "./libm.wasm" with { type: "wasm" };
// No "wado-compiler" marker → Type-safe interop

// JSON import (for comparison)
use config from "./config.json" with { type: "json" };
```

**Type annotation requirement**:
- **Wado-compiled `.wasm`**: No annotation needed (detected via `@custom "wado-compiler"`)
- **External `.wasm`**: `with { type: "wasm" }` **required**
- **`.wado` source**: No annotation needed (Wado source)

### Type Annotation Rules

| Import Source                 | `type` Attribute        | Format                  | Linking Strategy     |
| ----------------------------- | ----------------------- | ----------------------- | -------------------- |
| `.wado` files                 | Not applicable          | Wado source             | IR-level integration |
| `.wasm` (Wado-compiled)       | Not required (optional) | Component + metadata    | Core Wasm linking    |
| `.wasm` (External)            | **Required**            | `type: "wasm"`          | Component boundary   |
| `.json` files                 | **Required**            | `type: "json"`          | Compile-time embed   |
| `core:*`, `wasi:*` namespaces | Not applicable          | Special namespace       | IR-level integration |
| `https:` URLs                 | **Required**            | Must specify content    | Depends on type      |
| Future: `.wit` files          | **Required**            | `type: "wit"`           | Interface-only (TBD) |

**Detection logic**:
1. If `with { type: "wasm" }` is present → External component (Component Model boundary)
2. If no type annotation → Check for `@custom "wado-compiler"` marker:
   - Has marker → Wado-compiled (Core Wasm linking)
   - No marker → Error: External `.wasm` requires `with { type: "wasm" }`

### WIT Requirements

**Component Model format mandates embedded type information** in the binary:

- Component binaries include type sections (binary format)
- WIT text format can be extracted via `wasm-tools component wit`
- **External WIT files are NOT required** - types are in the component binary
- Optional: External `.wit` files for development convenience (future enhancement)

### Implementation Requirements

1. **Component Origin Detection**
   - If `with { type: "wasm" }` is present → External component (skip marker check)
   - If no type annotation → Check for `@custom "wado-compiler"` in component binary:
     - If present: Wado-origin → Use core linking strategy
     - If absent: Error - external `.wasm` requires `with { type: "wasm" }`
   - Custom section format: `(custom "wado-compiler" "version=X.Y.Z")`

2. **Component Model Binary Parsing**
   - Parse Component Model format using `wasmparser`
   - Extract embedded type information from component binary
   - No need to parse WIT text format (types are in binary)

3. **WIT Type Extraction and Mapping** (External components only)
   - Extract type information from component binary type sections
   - Map WIT types to Wado types:
     - `bool`, `s32`, `u32`, `f32`, `f64` → Wado primitives
     - `string` → `String`
     - `list<T>` → `Array<T>`
     - `option<T>` → `Option<T>`
     - `record { ... }` → `struct { ... }`
   - Generate Wado type definitions for imported interfaces

4. **Type Checking**
   - **Wado-origin**: Use internal Wado type system (same as `.wado` imports)
   - **External**: Validate via WIT type mapping
   - Ensure type compatibility at boundaries
   - Reject imports with unsupported WIT types (early error)

5. **Dual Linking Strategy**

```
┌─────────────────────────────────────────────────────────────┐
│ Wado Compiler                                               │
├─────────────────────────────────────────────────────────────┤
│ 1. Import resolution                                        │
│    .wado → Parse as Wado source (IR integration)            │
│    .wasm → Check type annotation:                           │
│            ├─ with { type: "wasm" }? → External component   │
│            └─ No annotation?                                │
│                ├─ Has "wado-compiler" marker? → Wado-origin │
│                └─ No marker? → Error (annotation required)  │
│                                                             │
│ 2. Type checking                                            │
│    .wado / Wado-origin .wasm → Wado type system (internal)  │
│    External .wasm → WIT type extraction + mapping           │
│                                                             │
│ 3. Code generation                                          │
│    .wado → IR-level integration (no Wasm intermediate)      │
│    Wado-origin .wasm → Core Wasm linking (shared memory)    │
│    External .wasm → Component boundary (canonical ABI)      │
│                                                             │
│ 4. Linking                                                  │
│    .wado / Wado-origin → Core Wasm linking (zero overhead)  │
│    External .wasm → Component composition (type-safe)       │
│                                                             │
│ 5. Optimization (future)                                    │
│    .wado / Wado-origin → wasm-opt inline, DCE, LTO          │
│    External → Limited (ABI boundary)                        │
└─────────────────────────────────────────────────────────────┘
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
✅ **Zero-overhead Wado modules**: Both `.wado` and Wado-compiled `.wasm` use core linking
✅ **Distributed compilation**: Pre-compiled Wado modules maintain full LTO capability
✅ **Standard library flexibility**: Enables bundling optimized libraries (Rust libm, etc.)
✅ **Automatic optimization**: Compiler auto-detects origin and chooses best strategy
✅ **Future-proof**: Supports emerging Wasm ecosystem standards
✅ **Type safety**: Component Model enforces type correctness at external boundaries
✅ **Simplicity**: No manual type annotations needed (auto-detected via metadata)

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
- Wado-origin function call: ~0 overhead (same as internal call, LTO enabled)
- Component boundary call: ~10-50ns overhead (ABI translation)
- For most use cases (crypto, parsing, I/O), ABI overhead is negligible

**Key advantage**: Pre-compiled Wado modules (`.wasm`) get same zero-overhead treatment as `.wado` source files, enabling distributed compilation without performance penalty.

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

### Phase 0: Wado-origin Detection

- [ ] Add custom section generator: `@custom "wado-compiler" "version=X.Y.Z"`
- [ ] Emit custom section in all Wado-compiled components
- [ ] Implement detection logic in component parser
- [ ] Test: Verify Wado-compiled `.wasm` uses core linking

### Phase 1: Component Model Parsing

- [ ] Parse `use {...} from "*.wasm"` syntax
- [ ] Parse `with { type: "wasm" }` annotation
- [ ] Integrate `wasmparser` for Component Model binary parsing
- [ ] Implement detection logic:
  - If `type: "wasm"` → External (skip marker check)
  - If no annotation → Check `wado-compiler` marker
- [ ] Extract type information from component binary sections (external only)
- [ ] Error handling for:
  - Missing/invalid `.wasm` files
  - `.wasm` without annotation and without `wado-compiler` marker
  - Non-component binaries (must be Component Model format)
  - Unsupported WIT types

### Phase 2: WIT Type Mapping (External components)

- [ ] Map WIT primitives to Wado types (bool, integers, floats)
- [ ] Map WIT `string` to Wado `String`
- [ ] Map WIT `list<T>` to Wado `Array<T>`
- [ ] Map WIT `option<T>` to Wado `Option<T>`
- [ ] Map WIT `record` to Wado `struct`
- [ ] Generate Wado type definitions for imported interfaces
- [ ] Type check imported functions against usage

### Phase 3: Component Boundary Codegen (External components)

- [ ] Generate Canonical ABI adapters for imported functions
- [ ] Handle lowering: Wado types → WIT types
- [ ] Handle lifting: WIT types → Wado types
- [ ] Integrate with existing codegen pipeline

### Phase 4: Dual Linking Implementation

- [ ] **Wado-origin path**: Extract core module and link directly (zero overhead)
- [ ] **External path**: Use `wasm-tools compose` to combine components
- [ ] Handle import/export resolution for both strategies
- [ ] Generate appropriate output (core module or component)
- [ ] Ensure Wado-origin core linking maintains LTO capability

### Phase 5: Optimization and Tooling

- [ ] Tree-shaking unused imports (both strategies)
- [ ] Cache parsed component metadata
- [ ] **Wado-origin LTO**: Cross-module inlining and DCE
- [ ] wasm-opt integration for core modules
- [ ] Benchmark: Verify zero overhead for Wado-origin imports

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
