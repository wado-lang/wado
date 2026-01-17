# ARD: WebAssembly Module Import Support

**Date**: 2026-01-10
**Status**: Accepted

## Context

Wado aims to be a "Wasm only" language, maintaining zero abstraction over WebAssembly. To achieve this goal and enable interoperability with the broader Wasm ecosystem, we need a mechanism to import and integrate existing WebAssembly modules directly into Wado programs.

This capability is essential for:

1. **Standard library implementation**: Integrating deterministic math functions (see ARD-2026-01-10-deterministic-libm)
2. **Ecosystem integration**: Using existing Wasm libraries (cryptography, parsers, etc.)
3. **Multi-language projects**: Composing modules written in different languages (Rust, C, AssemblyScript, etc.)

### Current State

Wado currently supports imports only from:

- `.wado` modules (local files)
- `core:*` namespace (core library, written in Wado)
- `wasi:*` namespace (WASI interfaces, mapped to WIT)
- `https:` URLs (remote modules)

There is no support for importing compiled `.wasm` files.

## Decision

Introduce `.wasm` file import support with **mandatory type annotation** to ensure type safety and clarity.

### Syntax

```wado
// Explicit type annotation required for non-.wado imports
use {sin, cos} from "./libm.wasm" with { type: "wasm" };

// Optional: specify WIT file for type information
use {foo} from "./external.wasm" with {
    type: "wasm",
    wit: "./external.wit",  // Override/supplement embedded WIT
};

// JSON import (for comparison)
use config from "./config.json" with { type: "json" };
```

### Type Annotation Rules

| Import Source        | `type` Attribute | Notes                          |
| -------------------- | ---------------- | ------------------------------ |
| `.wado` files        | Optional         | Type inferred from Wado source |
| `.wasm` files        | **Required**     | `type: "wasm"`                 |
| `.json` files        | **Required**     | `type: "json"`                 |
| `core:*`, `wasi:*`   | Not applicable   | Special namespace handling     |
| `https:` URLs        | **Required**     | Must specify content type      |
| Future: `.wit` files | **Required**     | `type: "wit"` (interface-only) |

### Implementation Requirements

1. **Wasm Component Model Support**
   - Parse Component Model format (`.wasm` with embedded WIT)
   - Parse Core Wasm with external WIT (`.wit` file)
   - Extract type information from embedded WIT metadata

2. **Type Checking**
   - Use WIT definitions for type checking imported functions
   - Validate import/export signature matching
   - Generate Wado type definitions from WIT interfaces

3. **Linking and Composition**
   - Integrate imported Wasm modules into final component
   - Resolve cross-module references
   - Handle component composition (similar to `wasm-tools compose`)

4. **Compiler Pipeline**

```
┌─────────────────────────────────────────────┐
│ Wado Compiler                               │
├─────────────────────────────────────────────┤
│ 1. Parse import declarations                │
│    - .wado → Wado parser                    │
│    - .wasm → wasmparser + wit-parser        │
│    - .json → JSON parser                    │
│                                             │
│ 2. Type checking                            │
│    - .wado → Wado type system               │
│    - .wasm → WIT-based type checking        │
│                                             │
│ 3. Code generation                          │
│    - Generate Wasm for Wado modules         │
│    - Link imported Wasm modules             │
│                                             │
│ 4. Component composition                    │
│    - Combine all modules into final .wasm   │
└─────────────────────────────────────────────┘
```

### Tooling Dependencies

```toml
[dependencies]
wasmparser = "0.121"        # Parse Wasm binary
wit-parser = "0.15"         # Parse WIT definitions
wit-component = "0.18"      # Component Model support
wasm-encoder = "0.38"       # Generate Wasm
wasm-compose = "0.5"        # Compose components
```

## Consequences

### Positive

✅ **Ecosystem interoperability**: Can use any Wasm library, regardless of source language
✅ **Component Model alignment**: Fully leverages WIT for type safety
✅ **Standard library flexibility**: Enables bundling optimized libraries (Rust libm, etc.)
✅ **Future-proof**: Supports emerging Wasm ecosystem standards
✅ **Type safety**: Mandatory `type` annotation prevents ambiguity
✅ **Consistency**: Same `use ... with { type: "..." }` pattern for all non-Wado imports

### Negative

⚠️ **Complexity**: Requires implementing Component Model composition
⚠️ **Build time**: Parsing and linking Wasm modules adds compilation overhead
⚠️ **Debugging**: Multi-language debugging can be challenging
⚠️ **Versioning**: Need to handle compatibility between Wasm modules
⚠️ **Mandatory annotation**: More verbose than extension-based inference

### Trade-offs

- **Type annotation requirement**: Prioritizes explicitness over brevity
  - **Rationale**: Wado's design philosophy emphasizes clarity and explicit dependencies
  - **Mitigation**: IDE/editor tooling can auto-suggest `with { type: "wasm" }`

- **Component Model dependency**: Ties Wado to Component Model evolution
  - **Rationale**: Component Model is the standard for Wasm interoperability
  - **Mitigation**: Abstract WIT parsing behind internal API for future flexibility

## Implementation Plan

### Phase 1: Foundation (Minimal Viable Feature)

- [ ] Parse `use {...} from "*.wasm" with { type: "wasm" }` syntax
- [ ] Integrate `wasmparser` for basic Wasm validation
- [ ] Extract exports from Wasm module
- [ ] Error handling for missing/invalid `.wasm` files

### Phase 2: WIT Integration

- [ ] Parse embedded WIT from Component Model format
- [ ] Support external WIT with `with { wit: "..." }` attribute
- [ ] Generate Wado type definitions from WIT interfaces
- [ ] Type check imported functions against usage

### Phase 3: Linking

- [ ] Compose Wado-generated Wasm with imported modules
- [ ] Handle import/export resolution
- [ ] Generate final Component Model output

### Phase 4: Optimization

- [ ] Tree-shaking unused imports
- [ ] Optimize module composition
- [ ] Cache parsed WIT definitions

## References

- [WebAssembly Component Model](https://github.com/WebAssembly/component-model)
- [WIT (Wasm Interface Types) Format](https://component-model.bytecodealliance.org/design/wit.html)
- [wasm-tools Documentation](https://github.com/bytecodealliance/wasm-tools)
- Related ARD: [ARD-2026-01-10-deterministic-libm](./ard-2026-01-10-deterministic-libm.md)
