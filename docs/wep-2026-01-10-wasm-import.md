# WEP: WebAssembly Module Import Support

## Context

Wado aims to be a "Wasm only" language, maintaining zero abstraction over WebAssembly. To achieve this goal and enable interoperability with the broader Wasm ecosystem, we need a mechanism to import and integrate existing WebAssembly modules directly into Wado programs.

This capability is essential for:

1. **Standard library implementation**: Integrating deterministic math functions (see WEP-2026-01-10-deterministic-libm)
2. **Ecosystem integration**: Using existing Wasm libraries (cryptography, parsers, etc.)
3. **Multi-language projects**: Composing modules written in different languages (Rust, C, AssemblyScript, etc.)

### Current State

Wado supports imports from:

- `.wado` modules (local files, integrated at IR level during compilation)
- `core:*` namespace (core library, written in Wado)
- `wasi:*` namespace (WASI interfaces, mapped to WIT)
- `https:` URLs (remote modules)

Phase 1 of this proposal adds **core-wasm asset imports** via `use _ from "<path>" with { type: "wat" | "wasm" };`. Component Model imports are deferred to a later phase.

## Phase 1 (delivered)

Phase 1 covers core wasm imports — the minimum needed to migrate `lib/core/libm.wat` from a special-cased "bundled" path to a regular asset import.

### Syntax

Phase 1 supports both wildcard and named imports:

```wado
// Named imports — the loader synthesises Wado bindings from the wasm
// module's export signatures, so each name resolves to a normal Wado
// function with the right type.
use { libm_sin, libm_cos } from "./libm.wat" with { type: "wat" };
let s = libm_sin(1.5);

// Wildcard imports — same loading machinery, no symbols are bound. Useful
// when the asset is referenced indirectly through `pub use` re-exports.
use _ from "./helpers.wasm" with { type: "wasm" };
```

`with { type: "wat" }` and `with { type: "wasm" }` are the only forms recognised as wasm-asset imports. Without the `with` clause, `.wat` / `.wasm` paths fall through to the regular import resolution (which rejects non-`.wado` schemas via the existing Kiln-missing-with diagnostic).

### Semantics

1. The loader fetches the asset bytes (stdlib lookup for `core:*.wat`, `host.load_source` for user paths), runs `wat::parse_bytes` if `kind == Wat`, and validates the result.
2. The bytes are cached in `LoadResult::wasm_assets` keyed by the canonical namespace string `wasm:<canonical-path>` (e.g. `wasm:core:libm.wat`). Each `WasmAsset` also carries the function-export signatures extracted via `wasmparser`.
3. The loader synthesises a Wado source string from those signatures — one `pub fn name(...) -> ret;` declaration per export, each tagged `#[canonical("wasm:<path>", "<export>")]` — and runs it through the regular parse/bind/desugar pipeline. The resulting AST module is registered under `ModuleSource::Wasm { path, kind }`, so named imports (`use { libm_sin } from "./libm.wat" ...`) resolve through the same path as imports of any other Wado module.
4. `BuiltinRegistry::register_wasm_module` folds the synthesised declarations into the registry alongside `core:builtin`'s entries, and `FunctionRef::builtin_name` + DCE's `is_builtin_func` recognise `ModuleSource::Wasm` so calls into a wat asset's exports lower through the same `TirImport` path as `core:builtin` declarations.
5. Codegen looks up each asset by namespace (post-DCE), transforms the module to import its memory from `env.memory`, prunes to the union of exports actually referenced, and embeds it in the resulting component.

### Phase 1 limitations (enforced)

- **Imports.** A wasm asset may import only `env.memory`. Any other import (`env.foo`, multiple memories, non-memory imports) is a compile-time error.
- **Start sections.** Wasm assets may not contain a `start` section. (Side-effecting init at instantiation time is not supported in Phase 1.)
- **Single memory.** At most one memory definition.
- **Export shape.** Each function export must use only the core wasm subset `{i32, i64, f32, f64, v128}` for parameters and at most one result. Reference-typed parameters and multi-return are rejected up-front with a pointed diagnostic.
- **Re-exported imports.** A wasm export that aliases an imported function is rejected; only module-defined functions can be exposed to Wado.
- **Origin detection (`@custom "wado-compiler"` marker).** Not used in Phase 1; all assets go through the core-linking path.
- **WIT type extraction.** Not used in Phase 1; types come from `#[canonical(...)]` declarations on Wado-side functions, not from the wasm module itself.

### Migration of `lib/core/libm.wat`

The bundled libm path was the motivating use case. Phase 1 retires the previous special "bundled" namespace and the `core:builtin` libm declarations:

- `lib/builtins/wado-bundled-libm.wat` → `lib/core/libm.wat`
- `wado-compiler/src/bundled.rs` → folded into `stdlib.rs` (`get_stdlib_wasm_asset` returns the bytes by canonical path)
- `core:prelude/primitive.wado` name-imports the libm exports directly from `../libm.wat`, renaming each onto its Wado-side identifier (`libm_sin as f64_sin`, …, `libm_log as f64_ln`, etc.) at the import site, and calls them as ordinary functions in place of the previous `builtin::f64_sin(x)` style. There is no intermediate stdlib module — `primitive.wado` is the only consumer, so a `core:libm` re-exporter would be pure indirection.
- The libm function declarations have been removed from `lib/core/builtin.wado`.
- `embed_bundled_modules` in `codegen/component.rs` is generalised into `embed_imported_wasm_modules`, driven by post-DCE imports grouped by namespace.

There is no behaviour change at the user level — `f64::sin(x)` still routes through the same libm export. The wasm-import path is now the only mechanism the codegen uses for both stdlib and user wasm assets.

## Phase 2 and beyond

The remainder of this document is the design space for follow-on phases. Phase 2 will add Component Model boundary handling for external `.wasm` files, including WIT-derived signatures and canonical-ABI lifting / lowering.

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
use {utils} from "./precompiled.wasm" with { type: "wasm" };
// Has @custom "wado-compiler" marker → Zero overhead
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
use {compress} from "./zlib.wasm" with { type: "wasm" };
// No wado-compiler marker → Component Model boundary
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

// Wado-compiled component (core linking via marker detection)
use {utils} from "./precompiled.wasm" with { type: "wasm" };
// Has @custom "wado-compiler" → Zero overhead

// External Wasm component (Component Model boundary)
use {sin, cos} from "./libm.wasm" with { type: "wasm" };
// No "wado-compiler" marker → Type-safe interop

// JSON import
use config from "./config.json" with { type: "json" };
```

**Type annotation requirement (for security)**:

- **`.wado` source**: No annotation needed (Wado source)
- **All `.wasm` files**: `with { type: "wasm" }` **required**
- **`.json` files**: `with { type: "json" }` **required**
- **Rationale**: Explicit type declaration prevents accidental execution of untrusted code

### Type Annotation Rules

| Import Source                 | `type` Attribute | Format            | Linking Strategy     |
| ----------------------------- | ---------------- | ----------------- | -------------------- |
| `.wado` files                 | Not applicable   | Wado source       | IR-level integration |
| `.wasm` files (any origin)    | **Required**     | `type: "wasm"`    | See detection logic  |
| `.json` files                 | **Required**     | `type: "json"`    | Compile-time embed   |
| `core:*`, `wasi:*` namespaces | Not applicable   | Special namespace | IR-level integration |
| `https:` URLs                 | **Required**     | Must specify type | Depends on type      |

**Detection logic for `.wasm` imports** (after `type: "wasm"` validation):

1. Check for `@custom "wado-compiler"` marker in component binary:
   - Has marker → Wado-origin (Core Wasm linking, zero overhead)
   - No marker → External (Component Model boundary, type-safe)

### WIT Requirements

**Component Model format mandates embedded type information** in the binary:

- Component binaries include type sections (binary format)
- WIT text format can be extracted via `wasm-tools component wit`
- **External WIT files are NOT required** - types are in the component binary
- Optional: External `.wit` files for development convenience (future enhancement)

### Implementation Requirements

1. **Import Validation and Origin Detection**
   - Validate `.wasm` imports have `with { type: "wasm" }` (security requirement)
   - Parse component binary and check for `@custom "wado-compiler"` marker:
     - If present: Wado-origin → Use core linking strategy
     - If absent: External → Use Component Model boundary
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
┌──────────────────────────────────────────────────────────────┐
│ Wado Compiler                                                │
├──────────────────────────────────────────────────────────────┤
│ 1. Import resolution                                         │
│    .wado → Parse as Wado source (IR integration)             │
│    .wasm → Validate `with { type: "wasm" }` (required)       │
│            Check "wado-compiler" marker:                     │
│            ├─ Has marker? → Wado-origin                      │
│            └─ No marker? → External component                │
│                                                              │
│ 2. Type checking                                             │
│    .wado / Wado-origin .wasm → Wado type system (internal)   │
│    External .wasm → WIT type extraction + mapping            │
│                                                              │
│ 3. Code generation                                           │
│    .wado → IR-level integration (no Wasm intermediate)       │
│    Wado-origin .wasm → Core Wasm linking (shared memory)     │
│    External .wasm → Component boundary (canonical ABI)       │
│                                                              │
│ 4. Linking                                                   │
│    .wado / Wado-origin → Core Wasm linking (zero overhead)   │
│    External .wasm → Component composition (type-safe)        │
│                                                              │
│ 5. Optimization (future)                                     │
│    .wado / Wado-origin → wasm-opt inline, DCE, LTO           │
│    External → Limited (ABI boundary)                         │
└──────────────────────────────────────────────────────────────┘
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
✅ **Security**: Explicit type annotations prevent accidental execution of untrusted code

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

1. **Wado-origin Detection**: Add `@custom "wado-compiler"` marker to all Wado-compiled components
2. **Component Model Parsing**: Parse `.wasm` imports with `type: "wasm"` validation and marker detection
3. **WIT Type Mapping**: Map WIT types to Wado types for external components
4. **Dual Linking**: Implement core linking for Wado-origin, Component Model boundary for external
5. **Optimization**: Enable LTO for Wado-origin modules

## References

- [WebAssembly Component Model](https://github.com/WebAssembly/component-model)
- [WIT (Wasm Interface Types) Format](https://component-model.bytecodealliance.org/design/wit.html)
- [Canonical ABI](https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md)
- [wasm-tools Documentation](https://github.com/bytecodealliance/wasm-tools)
- Related WEP: [WEP-2026-01-10-deterministic-libm](./wep-2026-01-10-deterministic-libm.md)
