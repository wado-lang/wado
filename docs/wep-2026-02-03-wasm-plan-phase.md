# WEP: Wasm Plan Phase

## Context

`codegen.rs` (12,000+ lines) mixes two concerns:

1. **Analysis**: Scanning type tables, querying WASI registries, topological sorting, dependency resolution — deciding _what_ to generate
2. **Encoding**: Allocating Wasm indices, calling `wasm_encoder` APIs, emitting instructions — _how_ to generate it

The compiler's principle states: "codegen.rs emits the Project as is, which does not have the knowledge of the previous phases." But in practice, codegen performs significant analysis before it can emit anything.

## Decision

Incrementally move analysis out of codegen into `wasm_plan`. The `wasm_plan` phase analyzes the Project and attaches metadata and plans that codegen consumes.

```
lower → optimize → wasm_plan → codegen
                       ↓
                   Project {
                       component_plan: ComponentPlan,
                       // CmExportInfo attached to TirFunctions
                   }
```

### What Lives in wasm_plan

#### ComponentPlan

`ComponentPlan` captures the structural decisions for the Component Model component:

- Canonical intrinsics needed (stream-new, task-return, etc.)
- Whether future intrinsics are needed
- Bundled module functions to wire through (fts, libm)
- World exports to create at the component boundary
- Test exports to create

Codegen reads `ComponentPlan` instead of scanning TIR imports and querying the world registry directly.

#### CmExportInfo

Already computed by `wasm_plan` and attached to `TirFunction`. Tells codegen how to generate CM glue code for world exports (scratch locals, required imports, async flag).

#### CM Converter Analysis

`CmConverterRequirements` centralizes analysis of which CM converters are needed for WASI function return types. Used by both optimize (DCE) and codegen.

#### Type Ordering Analysis

Pure analysis functions for type registration ordering:

- `sort_types_topologically` — Kahn's algorithm topological sort of structs and variants
- `get_type_dependencies` — Extract type dependencies for a given TypeId
- `get_self_referential_field_types` — Detect self-referential struct cycles
- `type_references_struct` — Check if a type transitively references a struct

These are pure functions that don't depend on codegen state. Codegen delegates to them for ordering decisions.

### What Stays in Codegen

#### Type Registration

The type registration logic (15+ phases in `build_main_module`) stays in codegen because it is tightly coupled to codegen's index management state (`struct_types`, `array_types`, `tuple_types`, etc.). The "skip if not yet registered" checks depend on knowing what Wasm types have been allocated so far, which is encoding state.

Moving this to wasm_plan would require duplicating codegen's state tracking (shadow `HashSet<StructName>` etc.), creating a maintenance burden without improving separation of concerns.

Codegen calls the pure analysis functions from wasm_plan for ordering decisions (topo sort, self-ref detection) but manages the registration phases itself.

#### Component Encoding

Codegen reads `ComponentPlan` and mechanically encodes the component structure using `ComponentBuilder`. WASI import generation, function lowering, and type definitions remain in codegen as they are pure encoding.

#### Instruction Emission

`generate_expr()`, `generate_function()`, etc. remain in codegen.

### Design Principles

#### wasm_plan answers "what", codegen answers "how"

- wasm_plan: "The component needs these canonical intrinsics and world exports"
- codegen: Allocates indices, calls `ComponentBuilder` APIs, emits the binary

#### Pure analysis functions live in wasm_plan

- Functions that don't need codegen state (topo sort, dependency analysis) live in wasm_plan
- Functions that need codegen state (type registration, index allocation) stay in codegen

### Migration Path

- [x] Move scratch local analysis to wasm_plan (`CmExportInfo`)
- [x] Centralize CM converter analysis (`CmConverterRequirements`)
- [x] Extract component structure analysis into `ComponentPlan`
- [x] Move pure type ordering functions to wasm_plan (`sort_types_topologically`, `get_type_dependencies`, `get_self_referential_field_types`)

## Consequences

### Benefits

- Component structure decisions are centralized in `ComponentPlan` — codegen reads the plan
- Type ordering analysis is testable independently as pure functions
- New Wasm features (threads, SIMD, stack switching) fit naturally into planning

### Trade-offs

- Type registration phases stay in codegen due to tight coupling with index management
  - This is acceptable: the ordering logic is delegated to wasm_plan; only the registration mechanics stay
- `ComponentPlan` adds data structures
  - Acceptable: they replace implicit knowledge scattered across `generate_component` methods
- Index allocation stays in codegen (cannot be pre-planned without duplicating `wasm_encoder` state)
  - This is fine: indices are an encoding detail, not a planning concern
