# WEP: Wasm Adapt Phase

## Context

The Wado compiler currently handles WebAssembly Component Model (CM) adaptation in the codegen phase. This includes:

1. **CM glue code generation** - HTTP response creation, task-return calls, future operations
2. **CM helper functions** - `cm_list_string_to_array`, `cm_option_string_to_option`, etc.
3. **Scratch local allocation** - Temporary variables for CM operations
4. **Export signature analysis** - Determining what glue code is needed

This mixing of concerns makes codegen complex and harder to maintain. Additionally, CM helper functions are currently hardcoded in `lib/core/internal.wado`, which cannot scale to handle generic type combinations like `Array<Option<String>>` or `Result<Array<i32>, String>`.

## Decision

Introduce a new `wasm_adapt` phase between optimize and codegen:

```
lower → optimize → wasm_adapt → codegen
```

### Responsibilities

The `wasm_adapt` phase handles all Wasm-specific preparation:

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
/// CM export information attached to TirFunction
pub struct CmExportInfo {
    /// Kind of CM export signature
    pub signature_kind: CmSignatureKind,
    /// Additional scratch locals needed for CM glue code
    pub scratch_locals: Vec<ScratchLocal>,
    /// CM functions that must be imported
    pub required_imports: Vec<String>,
}

pub enum CmSignatureKind {
    /// Command world: async fn run() -> Result<(), ()>
    Command,
    /// HTTP handler: async fn handle(Request) -> Result<Response, ErrorCode>
    HttpHandler,
    /// Other async export with custom signature
    AsyncExport { return_type: TypeId },
}
```

### Generated Helper Functions

The `wasm_adapt` phase generates TIR functions for CM type conversion:

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

1. Move scratch local analysis for CM operations from codegen to wasm_adapt
2. Add CmExportInfo metadata to TirFunction
3. Generate CM helper functions dynamically instead of hardcoding
4. Simplify codegen to use metadata and generated TIR

## Consequences

### Benefits

1. **Separation of concerns** - Wasm/CM adaptation is isolated from codegen
2. **Scalability** - Generic type combinations handled dynamically
3. **Debuggability** - Generated helpers visible in TIR dumps and golden fixtures
4. **Simpler codegen** - Focuses on TIR → Wasm translation

### Trade-offs

1. **No optimization for generated helpers** - wasm_adapt runs after optimize
   - Acceptable: helpers are small and unlikely to benefit from optimization
2. **Additional phase** - Slightly longer compilation
   - Acceptable: phase is lightweight analysis + targeted TIR generation

### Future Extensions

- Resource handle lifecycle management
- Stream/future type conversions
- Custom CM type adapters for user-defined types
