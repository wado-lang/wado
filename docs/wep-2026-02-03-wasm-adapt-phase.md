# WEP: Wasm Plan Phase

## Context

The Wado compiler currently handles WebAssembly Component Model (CM) adaptation in the codegen phase. This includes:

1. **CM glue code generation** - HTTP response creation, task-return calls, future operations
2. **CM helper functions** - `cm_list_string_to_array`, `cm_option_string_to_option`, etc.
3. **Scratch local allocation** - Temporary variables for CM operations
4. **Export signature analysis** - Determining what glue code is needed

This mixing of concerns makes codegen complex and harder to maintain. Additionally, CM helper functions are currently hardcoded in `lib/core/internal.wado`, which cannot scale to handle generic type combinations like `Array<Option<String>>` or `Result<Array<i32>, String>`.

## Decision

Introduce a new `wasm_plan` phase between optimize and codegen:

```
lower → optimize → wasm_plan → codegen
```

### Responsibilities

The `wasm_plan` phase handles all Wasm-specific preparation:

1. **CM boundary analysis**
   - Analyze export signatures to determine required glue code
   - Detect HTTP handler exports, Command exports, etc.
   - Identify required CM helper function signatures

2. **CM helper function generation (TIR)**
   - Generate helper functions as TIR based on actual type usage
   - Handle generic type combinations dynamically
   - Examples: `__cm_list_option_string_to_array`, `__cm_result_i32_string_to_result`

3. **Scratch local analysis**
   - Compute required scratch locals for CM operations
   - Add to `TirFunction.scratch_locals`

4. **CM export metadata**
   - Add `CmExportInfo` to functions that are world exports
   - Include signature kind, required imports, etc.

### Design Choices

#### Metadata over TIR for Glue Code

CM glue code (e.g., HTTP response creation in return statements) uses:

- CM canonical ABI calls (future-new, task-return)
- Linear memory operations (I32Load/Store at specific offsets)
- CM type flattening (result<response, error-code> → multiple i32/i64)

These are too low-level and Wasm-specific to represent in TIR cleanly. Instead, we use metadata to tell codegen what glue code to generate.

#### TIR for Helper Functions

CM helper functions (type converters) are generated as TIR because:

- They appear in golden fixtures for debugging
- They can be inspected with `--lower --unparse`
- Codegen simply converts TIR to Wasm without special cases

### Data Structures

```rust
/// Wasm value type for CM scratch locals (mirrors wasm_encoder::ValType)
pub enum CmValType {
    I32, I64, F32, F64,
    AnyRef, // Nullable anyref for storing GC objects
}

/// A scratch local variable needed for CM glue code
pub struct CmScratchLocal {
    pub name: String,
    pub val_type: CmValType,
}

/// CM export information attached to TirFunction
pub struct CmExportInfo {
    /// Whether this is an async export
    pub is_async: bool,
    /// Whether this export is an HTTP handler
    pub is_http_handler: bool,
    /// Scratch locals needed for CM glue code
    pub scratch_locals: Vec<CmScratchLocal>,
    /// CM functions that must be imported
    pub required_imports: Vec<String>,
}

/// Tracks which CM converters are needed (used by optimize_dce)
pub struct CmConverterRequirements {
    pub needs_list_string: bool,
    pub needs_list_u8: bool,
    pub needs_list_tuple_string: bool,
    pub needs_option_string: bool,
}
```

### Generated Helper Functions

The `wasm_plan` phase generates TIR functions for CM type conversion:

```
// Example: Converting CM list<option<string>> to Array<Option<String>>
fn __cm_list_option_string_to_array(ptr: i32, len: i32) -> Array<Option<String>> {
    let result = Array::<Option<String>>::with_capacity(len);
    for let mut i = 0; i < len; i += 1 {
        // Read option discriminant and string from linear memory
        // Construct Option<String> and append to result
    }
    return result;
}
```

Function naming convention: `__cm_{operation}_{type_signature}`

### Migration Path

1. ✓ Move scratch local analysis for CM operations from codegen to wasm_plan
2. ✓ Add CmExportInfo metadata to TirFunction
3. ✓ Centralize CM converter analysis in wasm_plan (shared by optimize_dce)
4. (Future) Generate CM helper functions dynamically instead of hardcoding
5. ✓ Simplify codegen to use metadata from wasm_plan

### Current Status

The wasm_plan phase is implemented with:

1. **CmExportInfo metadata** - Attached to TirFunctions that are world exports
2. **Scratch local computation** - Pre-computed in wasm_plan, used by codegen
3. **CM converter analysis** - Centralized functions for determining required converters

**CM Helper Functions**: Currently implemented in `lib/core/internal.wado` as Wado source code.
This approach works well because:

- Helpers go through normal compilation pipeline
- They appear in golden fixtures for debugging
- DCE correctly identifies which helpers are needed

Dynamic TIR generation is deferred until we need complex type combinations like
`Array<Option<String>>` that aren't covered by the current helpers.

## Consequences

### Benefits

1. **Separation of concerns** - Wasm/CM adaptation is isolated from codegen
2. **Scalability** - Generic type combinations handled dynamically
3. **Debuggability** - Generated helpers visible in TIR dumps and golden fixtures
4. **Simpler codegen** - Focuses on TIR → Wasm translation

### Trade-offs

1. **No optimization for generated helpers** - wasm_plan runs after optimize
   - Acceptable: helpers are small and unlikely to benefit from optimization
2. **Additional phase** - Slightly longer compilation
   - Acceptable: phase is lightweight analysis + targeted TIR generation

### Future Extensions

- Resource handle lifecycle management
- Stream/future type conversions
- Custom CM type adapters for user-defined types
