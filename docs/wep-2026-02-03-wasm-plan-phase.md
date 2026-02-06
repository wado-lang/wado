# WEP: Wasm Plan Phase

## Context

The `codegen.rs` module (12,000+ lines) currently handles three distinct responsibilities:

1. **Type layout** (~2,000 lines): Deciding what Wasm GC types to define and in what order (topological sort, rec groups, 6+ registration phases)
2. **Component assembly** (~1,400 lines): Building the Component Model wrapper (WASI imports, canonical intrinsics, module wiring, HTTP types)
3. **Code generation** (~5,700 lines): Translating TIR expressions and statements to Wasm instructions

The compiler's principle states: "codegen.rs emits the Project as is, which does not have the knowledge of the previous phases." But in practice, codegen performs significant analysis work — scanning type tables, querying WASI registries, resolving dependencies — before it can emit anything.

## Decision

### Goal

Split responsibilities so that:

- `wasm_plan` analyzes the Project and produces a `WasmPlan` — everything codegen needs to know about Wasm-specific concerns
- `codegen` consumes the `WasmPlan` and mechanically translates to Wasm bytes — no analysis, just encoding

```
lower → optimize → wasm_plan → codegen
                       ↓
                   WasmPlan {
                       types: TypePlan,
                       component: ComponentPlan,
                       exports: Vec<CmExportInfo>,
                   }
```

### File Organization

```
wasm_plan.rs          WasmPlan production (TypePlan, ComponentPlan, CmExportInfo)
codegen.rs            Orchestration, type encoding (execute TypePlan → CoreModuleBuilder)
codegen_component.rs  Component encoding (execute ComponentPlan → ComponentBuilder)
codegen_expr.rs       generate_expr(), generate_stmt(), expression-level code generation
```

Analysis (scanning TypeTable, querying WasiRegistry, topological sort, dependency resolution) lives in `wasm_plan.rs`. Encoding (allocating indices, calling `wasm_encoder` APIs) lives in `codegen*.rs`.

### WasmPlan

The `wasm_plan` phase produces a `WasmPlan` that captures all analysis results. Codegen consumes it without re-analyzing the Project.

#### TypePlan

What Wasm GC types need to be defined in the core module.

```rust
/// Plan for all Wasm GC type definitions in the core module.
pub struct TypePlan {
    /// Type declarations in topological order (ready for sequential registration).
    pub type_order: Vec<TypeDeclPlan>,
    /// Primitive box types needed (e.g., box_i32 for &i32).
    pub box_primitives: HashSet<PrimitiveType>,
    /// Array element types needed.
    pub array_element_types: Vec<ArrayElementPlan>,
    /// Canonical closure signatures needed.
    pub closure_signatures: Vec<ClosureSignaturePlan>,
    /// Tuple signatures needed (element TypeId lists).
    pub tuple_signatures: Vec<Vec<TypeId>>,
}

/// A single type declaration to register, in dependency order.
pub enum TypeDeclPlan {
    /// Normal struct — register fields in order.
    Struct {
        name: StructName,
        source: ModuleSource,
        is_monomorphized: bool,
    },
    /// Self-referential struct — requires a rec group.
    RecStruct {
        name: StructName,
        source: ModuleSource,
        self_ref_field_types: Vec<TypeId>,
    },
    /// Variant — register base type + case subtypes.
    Variant {
        name: String,
        source: ModuleSource,
    },
    /// Monomorphized variant (Option<T>, Result<T,E>, custom).
    MonoVariant {
        mangled_name: String,
        base_variant: String,
        type_args: Vec<TypeId>,
    },
    /// Alias — point to an already-registered type under a different name.
    StructAlias {
        alias_name: StructName,
        target_name: StructName,
    },
    VariantAlias {
        alias_name: String,
        target_name: String,
    },
}
```

The key insight: `build_main_module()` currently has 6+ phases with interleaved planning and encoding. `TypePlan` pre-computes the ordering so codegen just iterates and registers.

#### ComponentPlan

What the Component Model wrapper looks like.

```rust
/// Plan for the Component Model structure.
pub struct ComponentPlan {
    /// WASI interfaces to import, with their supported functions.
    pub wasi_imports: Vec<WasiImportPlan>,
    /// Canonical intrinsics needed (stream-new, task-return, etc.).
    pub canonical_intrinsics: Vec<CanonicalIntrinsicPlan>,
    /// Bundled module functions to wire through (fts, libm).
    pub bundled_functions: Vec<String>,
    /// Whether to include HTTP types and handler export.
    pub http_handler: Option<HttpHandlerPlan>,
    /// World exports to create at the component boundary.
    pub world_exports: Vec<WorldExportPlan>,
}

pub struct WasiImportPlan {
    pub interface_path: String,
    pub functions: Vec<String>,
}

pub struct WorldExportPlan {
    pub export_name: String,
    pub core_function_name: String,
    pub is_async: bool,
}
```

#### CmExportInfo (existing)

Already computed by `wasm_plan` and attached to `TirFunction`. No changes needed.

### What Stays in Codegen

Codegen retains the mechanical encoding work:

- **Type encoding**: Converting `TypeDeclPlan` items to `CoreModuleBuilder` calls (`define_gc_struct_type`, `define_gc_struct_subtype`, `define_rec_group`)
- **Component encoding**: Building `ComponentBuilder` with types, imports, instances, exports using index allocation
- **Instruction emission**: `generate_expr()`, `generate_function()`, `generate_match_expr()` — the bulk of codegen
- **Index management**: `CoreModuleBuilder` and `ComponentModelContext` state — inherently coupled to encoding order

### Design Principles

#### wasm_plan answers "what", codegen answers "how"

- wasm_plan: "We need struct `Point` with fields `[i32, i32]`, before `Line` which depends on it"
- codegen: Allocates type index 5, emits `define_gc_struct_type("Point", [I32, I32])`, records in `struct_types`

#### wasm_plan reads Project, codegen reads WasmPlan + TIR

- wasm_plan queries: `TypeTable`, `WasiRegistry`, `WorldRegistry`, `Project.has_effect()`, `Project.used_box_primitives`
- codegen queries: `WasmPlan` (for structure decisions) + `TirModule` (for function bodies)

#### Incremental migration

Not everything needs to move at once. Each piece can be migrated independently:

1. Type ordering analysis → `TypePlan`
2. WASI import analysis → `ComponentPlan.wasi_imports`
3. Canonical intrinsic analysis → `ComponentPlan.canonical_intrinsics`
4. World export analysis → `ComponentPlan.world_exports`

### Migration Path

- [x] Move scratch local analysis to wasm_plan (`CmExportInfo`)
- [x] Centralize CM converter analysis (`CmConverterRequirements`)
- [ ] Split `codegen.rs` into `codegen_*.rs` files (file organization only, no logic changes)
- [ ] Extract type ordering into `TypePlan` (topological sort, dependency analysis, rec group detection)
- [ ] Extract WASI import analysis into `ComponentPlan`
- [ ] Extract world export analysis into `ComponentPlan`
- [ ] Remove analysis code from codegen (codegen reads WasmPlan only)

## Consequences

### Benefits

- codegen becomes a pure encoder — easier to understand and modify
- wasm_plan captures all Wasm-specific decisions — testable independently
- File organization reflects responsibility boundaries
- New Wasm features (threads, SIMD, stack switching) fit naturally into planning

### Trade-offs

- Two-pass approach (plan then encode) for type registration, vs current single-pass
  - Acceptable: planning pass is lightweight analysis; encoding is the expensive part
- `WasmPlan` structs add data structures
  - Acceptable: they replace implicit knowledge scattered across codegen methods
- Index allocation stays in codegen (cannot be pre-planned without duplicating `wasm_encoder` state)
  - This is fine: indices are an encoding detail, not a planning concern
