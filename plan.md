# Plan: Remove WASI-Specific Knowledge from codegen.rs

## Goal

__codegen.rs must not know about wasi:_ details_* - it should emit TIR as-is. This enables codegen to handle user-defined Component Model modules, not just WASI.

## Problem Statement

codegen.rs currently has:

1. **Hardcoded WASI effect/function names**: `"Environment"`, `"get_arguments"`, `"InsecureSeed"`, `"TerminalStdin"`, etc.
2. **Hardcoded CM ABI patterns**: outptr sizes, alignment values, canonical options
3. **Hardcoded type-to-converter mappings**: `cm_list_string_to_array`, `cm_list_tuple_string_string_to_array`
4. **Hardcoded interface definitions**: `ensure_terminal_*` functions build interface types manually

This prevents supporting user-defined CM modules that follow the same patterns.

## Design Principle

**TIR should be self-describing for codegen.** All CM ABI requirements must be derived from types and stored in TIR, not inferred at codegen time.

## Solution: CM Call Convention in TIR

### Phase 1: Define CM Call Convention Struct

Add to `component_model.rs`:

```rust
/// Component Model ABI call convention
/// Describes how to call a CM function and handle its return value
#[derive(Debug, Clone, Default)]
pub struct CmCallConvention {
    /// Whether lowering requires Memory canonical option
    pub needs_memory: bool,
    /// Whether lowering requires Realloc canonical option
    pub needs_realloc: bool,
    /// If Some, allocate outptr before call (size, align)
    pub outptr_alloc: Option<(u32, u32)>,
    /// Conversion function to call after the call (if any)
    /// Full path like "core/internal/cm_list_string_to_array"
    pub result_converter: Option<String>,
    /// For tuple returns: element types for struct creation
    pub tuple_return: Option<Vec<PrimitiveType>>,
    /// For option<own<resource>>: true if needs boxing
    pub option_resource_return: bool,
}

impl CmCallConvention {
    /// Derive convention from return type
    pub fn from_return_type(return_type: &Option<Type>) -> Self { ... }
}
```

### Phase 2: Extend TirExprKind::EffectCall

```rust
TirExprKind::EffectCall {
    effect_name: String,
    op_name: String,
    args: Vec<TirExpr>,
    // NEW: CM call convention (None for non-CM calls)
    cm_convention: Option<CmCallConvention>,
    // NEW: Full local alias name for CM call
    cm_local_name: Option<String>,
}
```

### Phase 3: Populate Convention During Lowering

In `lower.rs`, after resolving an effect call:

```rust
// Look up WASI function info from registry
if let Some(func_info) = wasi_registry.get_function(&format!("{}::{}", effect_name, op_name)) {
    // Derive convention from return type
    let convention = CmCallConvention::from_return_type(&func_info.return_type);
    let local_name = func_info.local_alias_name();
    // Store in TIR
    ...
}
```

### Phase 4: Refactor codegen.rs

Replace all hardcoded WASI patterns with generic convention handling:

```rust
TirExprKind::EffectCall { effect_name, op_name, args, cm_convention, cm_local_name } => {
    if let (Some(conv), Some(local_name)) = (cm_convention, cm_local_name) {
        // Generic CM call handling
        self.generate_cm_call(func, ctx, builder, &conv, local_name, args);
    } else {
        // Non-CM effect call (error or unsupported)
        ...
    }
}

fn generate_cm_call(&self, ..., conv: &CmCallConvention, ...) {
    // Allocate outptr if needed
    if let Some((size, align)) = conv.outptr_alloc {
        self.allocate_outptr(func, ctx, size, align);
    }

    // Call the CM function
    self.emit_call(func, builder, local_name);

    // Handle result conversion
    if let Some(converter) = &conv.result_converter {
        self.call_converter(func, builder, ctx, converter);
    }

    // Handle tuple struct creation
    if let Some(elements) = &conv.tuple_return {
        self.create_tuple_struct(func, elements);
    }

    // Handle option<resource> boxing
    if conv.option_resource_return {
        self.box_option_resource(func, ctx);
    }
}
```

### Phase 5: Make Interface Import Data-Driven

Replace `ensure_terminal_stdin_imported()` etc. with generic interface import:

```rust
fn ensure_interface_imported(
    &self,
    builder: &mut ComponentBuilder,
    ctx: &mut ComponentModelContext,
    interface_info: &WasiInterfaceInfo,
    project: &Project,
) {
    // Build interface type from WasiInterfaceInfo
    // This uses type information, not hardcoded interface definitions
}
```

### Phase 6: Remove Hardcoded Lower Functions

The lower functions `canon lower` for Random, Terminal, etc. should be generated from registry data:

```rust
for func_info in wasi_registry.all_functions_with_convention() {
    let conv = &func_info.call_convention;
    let options = if conv.needs_memory && conv.needs_realloc {
        vec![CanonicalOption::Memory(ctx.memory_idx()), CanonicalOption::Realloc(...)]
    } else {
        vec![]
    };
    builder.lower_func(..., options);
}
```

## Type-to-Convention Mapping

| Return Type                              | outptr_alloc  | result_converter                       | tuple_return | option_resource |
| ---------------------------------------- | ------------- | -------------------------------------- | ------------ | --------------- |
| (none)                                   | None          | None                                   | None         | false           |
| `i32`, `i64`, `u32`, `u64`, `f32`, `f64` | None          | None                                   | None         | false           |
| `list<string>`                           | Some((8, 4))  | `cm_list_string_to_array`              | None         | false           |
| `list<tuple<string, string>>`            | Some((8, 4))  | `cm_list_tuple_string_string_to_array` | None         | false           |
| `option<string>`                         | Some((12, 4)) | `cm_option_string_to_option`           | None         | false           |
| `tuple<u64, u64>`                        | Some((16, 8)) | None                                   | [U64, U64]   | false           |
| `option<own<resource>>`                  | Some((4, 4))  | None                                   | None         | true            |

## Files to Modify

1. **component_model.rs**: Add `CmCallConvention`, derivation logic
2. **tir.rs**: Extend `TirExprKind::EffectCall` with convention fields
3. **lower.rs**: Populate convention during lowering
4. **codegen.rs**: Replace hardcoded patterns with convention-based codegen

## Implementation Order

- [x] Define `CmCallConvention` struct with `from_return_type()`
- [x] Add tests for type-to-convention derivation
- [x] Extend TIR with convention fields (`cm_convention`, `cm_local_name` in EffectCall)
- [ ] Update lower phase to populate conventions (not needed - used WasiRegistry lookup at codegen time)
- [x] Refactor codegen to use conventions via `generate_cm_effect_call` helper
- [x] Remove hardcoded effect call patterns from `TirExprKind::Call` branch
- [x] Remove hardcoded effect call patterns from `TirExprKind::EffectCall` branch
- [x] Refactor interface import functions (ensure_*_imported) - resource-based interfaces done
- [x] Refactor `canon lower` generation to be data-driven
- [x] Refactor scratch local helpers to be convention-driven
- [x] Clean up unused code

## Current Status

**Phase 1 Complete**: Expression codegen no longer has hardcoded WASI patterns

The following are now convention-driven via `generate_cm_effect_call`:

- Stdout::write_via_stream (async, subtask handling)
- Stderr::write_via_stream (async, subtask handling)
- Environment::get_arguments (list<string> return)
- Environment::get_environment (list<tuple<string,string>> return)
- Environment::get_initial_cwd (option<string> return)
- InsecureSeed::get_insecure_seed (tuple<u64,u64> return)
- Terminal*::get_terminal_* (option<own<resource>> return)

**Phase 2 Complete**: `canon lower` generation is now data-driven

- `lower_wasi_functions()` iterates over WasiRegistry
- Canonical options derived from `CmCallConvention`:
  - `is_async` → `CanonicalOption::Async`
  - `needs_memory` → `CanonicalOption::Memory`
  - `needs_realloc` → `CanonicalOption::Realloc`
- `CmCallConvention.with_params()` handles Stream<T> parameters
- `CmCallConvention.with_async()` ensures async functions have Memory+Realloc
- Async functions with void return skipped (not fully supported: wait_until, wait_for)

**Phase 3 Complete**: Resource-based interface imports are now data-driven

- `WasiRegistry` tracks resource types from `pub resource` declarations
- `WasiInterfaceInfo.resource_type` contains `(wado_name, cm_name)` for interfaces with resources
- `import_interfaces_with_resources()` iterates over registry and imports resource-based interfaces
- `import_interface_with_resource()` is a generic function that handles any interface with a resource type
- Removed hardcoded `ensure_terminal_stdin/stdout/stderr_imported` functions

**Remaining fallback functions** (used when DCE or registry skip an interface):

- `ensure_stdout_stderr_imported` - Stream writer interfaces
- `ensure_environment_imported` - Environment interface
- `ensure_exit_imported` - Exit interface

These fallbacks exist for cases where the main registry loop skips an interface but it's still needed (e.g., panic handler needs stdout)

## Success Criteria

- [x] No WASI effect/function name strings in expression codegen (`generate_expr`)
- [x] CM ABI patterns derived from type information
- [x] All 1020 E2E tests pass
- [x] `canon lower` generation is data-driven (uses CmCallConvention)
- [x] Scratch local pre-allocation is convention-driven (uses registry lookup)
- [x] Resource-based interface imports are data-driven (uses `resource_type` from registry)

## Testing

Focus tests:

- `Environment::get_arguments` (list<string>)
- `Environment::get_environment` (list<tuple<string,string>>)
- `Environment::get_initial_cwd` (option<string>)
- `InsecureSeed::get_insecure_seed` (tuple<u64,u64>)
- `Terminal*::get_terminal_*` (option<own<resource>>)
- `Random::get_random_bytes` (list<u8>)
