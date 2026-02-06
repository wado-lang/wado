# WEP: Wasm Plan Phase

## Context

`codegen.rs` (12,000+ lines) mixes two concerns:

1. **Analysis**: Scanning type tables, querying WASI registries, topological sorting, dependency resolution — deciding _what_ to generate
2. **Encoding**: Allocating Wasm indices, calling `wasm_encoder` APIs, emitting instructions — _how_ to generate it

The compiler's principle states: "codegen.rs emits the Project as is, which does not have the knowledge of the previous phases." But in practice, codegen performs significant analysis before it can emit anything.

## Decision

Move analysis out of codegen into `wasm_plan`. The `wasm_plan` phase analyzes the Project and produces a `WasmPlan` — everything codegen needs to know about Wasm-specific concerns. Codegen consumes the `WasmPlan` and mechanically translates to Wasm bytes.

```
lower → optimize → wasm_plan → codegen
                       ↓
                   WasmPlan {
                       types: TypePlan,
                       component: ComponentPlan,
                       exports: Vec<CmExportInfo>,
                   }
```

### What Moves to wasm_plan

#### TypePlan

Currently in codegen's `build_main_module()`: 6+ phases of type registration with interleaved scanning and encoding. The scanning part moves to wasm_plan.

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

Analysis that moves:

- Topological sort (`sort_types_topologically`)
- Dependency analysis (`get_type_dependencies`)
- Self-referential detection (`get_self_referential_field_types`)
- Type table scanning (each phase's "which types are needed" logic)
- Monomorphized variant collection
- Array/tuple/closure signature collection

#### ComponentPlan

Currently in codegen's `generate_component()`: analysis of WASI registry, world exports, and canonical intrinsics is interleaved with component encoding. The analysis part moves to wasm_plan.

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

Analysis that moves:

- WASI import filtering (DCE + `has_effect()` checks)
- Canonical intrinsic discovery (scanning TIR imports)
- Bundled module requirements
- World export mapping

#### CmExportInfo (existing)

Already computed by `wasm_plan` and attached to `TirFunction`. No changes needed.

### What Stays in Codegen

- Type encoding: iterating `TypePlan` and calling `CoreModuleBuilder` (`define_gc_struct_type`, `define_rec_group`, etc.)
- Component encoding: iterating `ComponentPlan` and calling `ComponentBuilder`
- Instruction emission: `generate_expr()`, `generate_function()`, etc.
- Index management: `CoreModuleBuilder` and `ComponentModelContext` state

### Design Principles

#### wasm_plan answers "what", codegen answers "how"

- wasm_plan: "We need struct `Point` with fields `[i32, i32]`, before `Line` which depends on it"
- codegen: Allocates type index 5, emits `define_gc_struct_type("Point", [I32, I32])`, records in `struct_types`

#### wasm_plan reads Project, codegen reads WasmPlan + TIR

- wasm_plan queries: `TypeTable`, `WasiRegistry`, `WorldRegistry`, `Project.has_effect()`, `Project.used_box_primitives`
- codegen queries: `WasmPlan` (for structure decisions) + `TirModule` (for function bodies)

### Migration Path

- [x] Move scratch local analysis to wasm_plan (`CmExportInfo`)
- [x] Centralize CM converter analysis (`CmConverterRequirements`)
- [ ] Extract type ordering into `TypePlan`
- [ ] Extract component structure analysis into `ComponentPlan`
- [ ] Remove analysis code from codegen (codegen reads WasmPlan only)

## Consequences

### Benefits

- codegen becomes a pure encoder — easier to understand and modify
- wasm_plan captures all Wasm-specific decisions — testable independently
- New Wasm features (threads, SIMD, stack switching) fit naturally into planning

### Trade-offs

- Two-pass approach (plan then encode) for type registration, vs current single-pass
  - Acceptable: planning pass is lightweight analysis; encoding is the expensive part
- `WasmPlan` structs add data structures
  - Acceptable: they replace implicit knowledge scattered across codegen methods
- Index allocation stays in codegen (cannot be pre-planned without duplicating `wasm_encoder` state)
  - This is fine: indices are an encoding detail, not a planning concern
