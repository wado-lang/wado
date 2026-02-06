# Wasm Plan Phase - Implementation Plan

Based on [WEP: Wasm Plan Phase](./wep-2026-02-03-wasm-plan-phase.md).

## Current State

`wasm_plan.rs` (459 lines) has two completed items:

- [x] `CmExportInfo` - CM boundary analysis for world exports (scratch locals, required imports)
- [x] `CmConverterRequirements` - CM converter analysis for WASI return types (used by DCE)

Remaining migration items from the WEP:

- [ ] Extract type ordering into `TypePlan`
- [ ] Extract component structure analysis into `ComponentPlan`
- [ ] Remove analysis code from codegen (codegen reads `WasmPlan` only)

## Analysis of What Moves

### TypePlan (from `build_main_module`, lines 710-1942)

The type registration in `build_main_module` has 6+ interleaved phases that mix analysis ("which types do we need, in what order?") with encoding ("register this struct type with the builder"). The analysis portions move to `wasm_plan`.

The current phases in `build_main_module`:

| Phase | Lines | Description | Analysis or Encoding? |
|-------|-------|-------------|----------------------|
| Primitive arrays | 966-976 | Scan type tables for primitive array types | Analysis + Encoding |
| Box types | 978-980 | Register box types from `used_box_primitives` | Encoding (analysis done in optimize) |
| Phase 1 | 982-1041 | Non-mono library structs+variants, topo-sorted | Analysis + Encoding |
| Tuple types | 1043-1051 | Scan type tables for tuple types | Analysis + Encoding |
| Phase 2 | 1053-1116 | Non-mono entry module structs+variants, with deferred handling | Analysis + Encoding |
| Struct aliases | 1118-1143 | Register struct/variant aliases | Encoding (data from symbol table) |
| Phase 2.5 | 1145-1157 | Arrays of non-mono structs | Analysis + Encoding |
| Closure types | 1159-1171 | Canonical closure types | Analysis + Encoding |
| Phase 3 | 1173-1228 | Mono library structs, topo-sorted, self-ref detection | Analysis + Encoding |
| Phase 3.5 | 1230-1243 | Pre-register arrays from mono struct fields | Analysis + Encoding |
| Phase 4 | 1245-1280+ | Mono entry module structs + deferred non-mono | Analysis + Encoding |

Analysis functions that move:

| Function | Lines | Description |
|----------|-------|-------------|
| `get_type_dependencies()` | 519-549 | Get struct/variant type dependencies |
| `get_self_referential_field_types()` | 555-600 | Detect self-referential struct cycles |
| `type_references_struct()` | 571-601 | Check if type references a struct (recursive) |
| `sort_types_topologically()` | 606-706 | Kahn's algorithm topological sort |

### ComponentPlan (from `generate_component`, lines 1943-2549)

The component generation mixes "what does the component need?" analysis with "emit the component binary" encoding.

| Section | Lines | Description | Analysis or Encoding? |
|---------|-------|-------------|----------------------|
| WASI imports | 1960-1964 | Call `generate_wasi_imports` | Both (registry querying + encoding) |
| Bundled module | 2020-2071 | Scan TIR imports for `namespace == "bundled"` | Analysis + Encoding |
| Future intrinsics | 2078-2156 | Scan TIR imports for future-* canonical names | Analysis + Encoding |
| Canonical intrinsics | 2162-2236 | Scan TIR imports for `namespace == "wasi"` | Analysis + Encoding |
| WASI lowering | 2238-2242 | Lower WASI functions | Encoding |
| HTTP lowering | 2247-2348 | Lower HTTP types | Encoding |
| World exports | 2404-2483 | Map world exports to core functions | Analysis + Encoding |
| Test exports | 2485-2532 | Export test functions | Encoding |

## Implementation Plan

### Step 1: Define `TypePlan` and `ComponentPlan` data structures

Add to `wasm_plan.rs`:

```rust
/// Plan for all Wasm GC type definitions in the core module.
pub struct TypePlan {
    /// Type declarations in topological order, grouped by registration phase.
    pub phases: Vec<TypeRegistrationPhase>,
    /// Primitive box types needed (e.g., box_i32 for &i32).
    pub box_primitives: HashSet<PrimitiveType>,
}

/// A phase of type registration. Phases must be executed in order.
pub enum TypeRegistrationPhase {
    /// Primitive array types (from type table scan)
    PrimitiveArrays(Vec<PrimitiveArrayPlan>),
    /// Box types for primitive references
    BoxTypes(HashSet<PrimitiveType>),
    /// Non-monomorphized types from a module, in topological order
    NonMonoTypes {
        module_source: ModuleSource,
        types: Vec<TypeDeclPlan>,
    },
    /// Tuple types
    TupleTypes(Vec<TuplePlan>),
    /// Struct and variant aliases
    Aliases(Vec<AliasPlan>),
    /// Arrays of non-mono structs
    NonMonoStructArrays(Vec<ArrayElementPlan>),
    /// Canonical closure types
    ClosureTypes(Vec<ClosureSignaturePlan>),
    /// Monomorphized types from a module, in topological order
    MonoTypes {
        module_source: ModuleSource,
        types: Vec<TypeDeclPlan>,
    },
    /// Arrays from monomorphized struct fields
    MonoStructArrays(Vec<ArrayElementPlan>),
    /// Remaining arrays from all type tables
    RemainingArrays(Vec<ArrayElementPlan>),
}

pub enum TypeDeclPlan {
    Struct { name: StructName, source: ModuleSource, is_self_referential: bool, self_ref_field_types: Vec<TypeId> },
    Variant { name: String, source: ModuleSource },
    DeferredToPhase4 { name: StructName },
}
```

Key design decision: The `TypePlan` preserves the phased registration order because codegen's `CoreModuleBuilder` accumulates state (type indices) sequentially. We cannot flatten into a single ordered list because intermediate steps (like registering aliases between phases) are needed.

Alternative (simpler): Instead of encoding every phase, produce a single `Vec<TypeDeclPlan>` that already respects all ordering constraints. This would require the planning phase to be more sophisticated but would make codegen simpler.

Recommendation: Start with the simpler approach (single ordered list) and see if it works. The phase-based approach is the fallback if ordering constraints are too complex for a single pass.

```rust
/// Plan for the Component Model structure.
pub struct ComponentPlan {
    /// Canonical intrinsics needed (stream-new, task-return, etc.).
    pub canonical_intrinsics: Vec<String>,
    /// Whether future intrinsics are needed (future-new, future-write, etc.).
    pub needs_future_intrinsics: bool,
    /// Bundled module function names to wire through (fts, libm).
    pub bundled_functions: Vec<String>,
    /// Whether to include HTTP types and handler export.
    pub has_http_handler: bool,
    /// World exports to create at the component boundary.
    pub world_exports: Vec<WorldExportPlan>,
    /// Test functions to export.
    pub test_exports: Vec<TestExportPlan>,
}
```

### Step 2: Add `WasmPlan` to `Project`

```rust
// In project.rs
pub struct Project {
    // ... existing fields ...
    /// Wasm plan (populated by wasm_plan phase, consumed by codegen)
    pub wasm_plan: Option<WasmPlan>,
}

pub struct WasmPlan {
    pub types: TypePlan,
    pub component: ComponentPlan,
}
```

### Step 3: Extract `ComponentPlan` analysis

This is the safer starting point because `generate_component` is smaller (608 lines vs 1,233 lines) and its analysis sections are more clearly delineated.

Move from `generate_component` to `wasm_plan`:

1. Bundled import scanning: `entry_tir.imports.iter().filter(|i| i.namespace == "bundled")` -> `ComponentPlan.bundled_functions`
2. Future intrinsic detection: `entry_tir.imports.iter().any(|i| i.canonical_name matches future-*)` -> `ComponentPlan.needs_future_intrinsics`
3. Canonical intrinsic enumeration: `entry_tir.imports.iter().filter(|i| i.namespace == "wasi")` -> `ComponentPlan.canonical_intrinsics`
4. World export mapping: `project.world_registry.get(&project.target_world)` -> `ComponentPlan.world_exports`

After this step, `generate_component` reads `ComponentPlan` instead of scanning TIR imports directly.

Validation: all existing tests pass (`make test`).

### Step 4: Extract type ordering analysis into `TypePlan`

This is the larger and riskier change. The approach:

1. Move `get_type_dependencies`, `get_self_referential_field_types`, `type_references_struct`, `sort_types_topologically` from `codegen.rs` to `wasm_plan.rs` (as standalone functions, not methods on the codegen struct).

2. Build `TypePlan` in the `wasm_plan()` function by running the same analysis logic that currently lives in `build_main_module`. This requires:
   - Iterating over all TIR modules in the same order as codegen
   - Running topological sort on each module's types
   - Detecting self-referential structs
   - Identifying deferred structs (non-mono depending on mono)
   - Collecting primitive arrays, tuples, closures, etc.

3. Refactor `build_main_module` to iterate over `TypePlan` instead of doing its own analysis.

The risk here is that type registration interleaves analysis with state that depends on *previously registered types*. For example, Phase 2 defers structs that depend on mono structs to Phase 4. The deferral decision depends on which struct names are monomorphized, which is analysis. But the registration order also depends on what's been registered so far (for aliases). We need to verify that planning can be fully separated from encoding.

Mitigation: Start by extracting the pure analysis functions (topo sort, dependency analysis, self-ref detection) without changing the control flow. Then incrementally move the "what to register" decisions into the plan.

### Step 5: Make codegen consume `WasmPlan` only for structural decisions

After Steps 3-4, codegen should:
- Read `WasmPlan.types` to know what types to register and in what order
- Read `WasmPlan.component` to know what CM structure to emit
- Read `TirModule` for function bodies (instruction emission)
- NOT query `TypeTable`, `WasiRegistry`, or `WorldRegistry` for structural decisions

### Step 6: Clean up

- Remove dead analysis code from `codegen.rs`
- Add unit tests for `TypePlan` construction
- Add `--dump --wasm-plan` option to inspect the plan
- Update `docs/compiler.md` with the new phase description

## Execution Order and Risk Assessment

| Step | Effort | Risk | Reason |
|------|--------|------|--------|
| Step 1 | Small | Low | Data structures only, no behavior change |
| Step 2 | Small | Low | Adding a field to `Project` |
| Step 3 | Medium | Low | `ComponentPlan` analysis is clearly separable |
| Step 4 | Large | Medium | Type ordering has complex inter-phase dependencies |
| Step 5 | Medium | Low | Mechanical refactoring once plan is in place |
| Step 6 | Small | Low | Cleanup and documentation |

Recommended execution: Steps 1-2 together, then Step 3, then Step 4 (which is the bulk of the work), then Steps 5-6.

## Testing Strategy

- After each step, run `make test` to verify no regressions
- After Step 3: Verify component generation produces identical bytes for a sample program
- After Step 4: Verify `build_main_module` produces identical type registrations
- Use `wado dump --lower --unparse` to compare before/after TIR output
- Use `wado compile --wat-to-stdout` to compare before/after WAT output

## Open Questions

1. Should `TypePlan` use a single ordered list or preserve the phase structure?
   - Single list is simpler for codegen but harder to construct
   - Phase structure mirrors current code but couples plan to encoding order
   - Recommendation: Start with phases, refactor to single list if it becomes cleaner

2. How much state does type registration need from previously registered types?
   - `register_struct_type` uses `self.struct_types` (previously registered structs) to look up field types
   - This means codegen needs some state even with a plan
   - The plan tells codegen *what* to register in *what order*, but codegen still does index allocation

3. Should the analysis functions (`sort_types_topologically`, etc.) be free functions in `wasm_plan.rs` or methods on a builder?
   - They don't need `self` access to codegen state
   - Free functions are simpler and more testable
   - Recommendation: Free functions
