# WEP: TIR-Level CM Adapter Synthesis

## Context

The compiler's Component Model (CM) boundary logic — lifting GC values to linear memory, lowering linear memory to GC values, calling lowered WASI imports, wrapping exports for `canon lift` — is currently spread across multiple layers:

- `codegen.rs`: `generate_cm_effect_call` (~140 lines), `generate_cm_resource_method_call` (~250 lines), `emit_option_string_lowering` (~40 lines), `emit_field_size_payload_lowering` (~65 lines), plus scattered CM glue throughout function emission
- `component_model.rs`: `CmCallConvention` and its `from_return_type` logic (~350 lines), deriving ABI conventions from WASI return types via pattern matching on `Type` variants
- `internal.wado`: hand-written CM converter functions (`cm_lower_string`, `cm_list_string_to_array`, etc.) — each covers one specific type shape (~130 lines)
- `wasm_plan.rs`: `CmExportInfo` with pre-computed scratch locals and required imports for export glue

This architecture has two problems:

### Per-type hand-coding

Every new WASI return type shape requires adding a new `cm_*` function in `internal.wado`, a new match arm in `CmCallConvention::from_return_type`, and sometimes new codegen logic. For example, `list<tuple<string, string>>` required `cm_list_tuple_string_string_to_array` — a dedicated function for a single WASI call. This does not scale to arbitrary WIT types from third-party components.

### Codegen complexity

Codegen emits raw Wasm instructions for CM boundary crossing: `i32.load`, `i32.store`, `memory.grow`, `realloc` calls, outptr allocation, discriminant dispatch. This mixes two abstraction levels — Wado semantics and CM ABI mechanics — in the same function. The resulting code is hard to test, hard to debug (no intermediate representation between TIR and Wasm binary), and hard to optimize (the optimizer never sees CM glue code).

### Relationship to WIR

The WIR layer (WEP 2026-02-14) will replace codegen with a structured `tir_to_wir → wir_emit` pipeline. CM adapter synthesis can be done **before** WIR: it transforms `EffectCall` TIR nodes into ordinary `Call` nodes targeting synthesized adapter functions, reducing the surface area of CM-specific logic that `tir_to_wir` must handle.

## Decision

Introduce a **cm_adapter_gen** phase that synthesizes CM adapter functions as TIR, inserted between resolve and lower. Adapter functions express lifting, lowering, and boundary calls using Wado's own type system and builtins. They flow through the normal lower → optimize → codegen pipeline.

### Pipeline Position

```
parse → desugar → modules → symbols → tir (resolve)
                                        ↓
                                   cm_adapter_gen  ← NEW
                                        ↓
                                      lower → optimize → wasm_plan → codegen
```

The phase runs after type resolution (all types are concrete, WASI registry is built) and before lower (so adapter functions go through match desugaring, closure capture, monomorphization, and optimization).

### What cm_adapter_gen Does

For each WASI import function used by the program:

1. **Synthesize an adapter function** that:
   - Accepts Wado-typed parameters (String, Array, structs, etc.)
   - Lowers parameters to CM flat ABI (linear memory via builtins)
   - Calls the lowered WASI function (flat i32/i64 params)
   - Lifts the result from CM flat ABI back to Wado types
   - Returns the Wado-typed result

2. **Rewrite `EffectCall` nodes** to ordinary `Call` nodes targeting the adapter function.

For each export function:

3. **Synthesize an export adapter function** that:
   - Accepts flat CM parameters (i32/i64 values, pointers)
   - Lifts parameters to Wado types
   - Calls the user's export function
   - Lowers the result to flat CM ABI
   - Returns flat values or writes to outptr

### Import Adapter Example

Given a WASI function:

```wit
// wasi:http/types.fields.get: func(self: borrow<fields>, name: string) -> list<list<u8>>
```

Currently represented as:

```
TirExprKind::EffectCall {
    effect_name: "Fields",
    op_name: "get",
    args: [self_handle, name],
    cm_convention: Some(CmCallConvention { outptr_alloc: Some((8, 4)), ... }),
    cm_local_name: Some("wasi:http/types/[method]fields.get"),
}
```

cm_adapter_gen synthesizes a TIR function:

```wado
// Synthesized adapter: wasi:http/types/[method]fields.get
fn __cm_adapter__fields_get(self_handle: i32, name: String) -> Array<Array<u8>> {
    // 1. Lower params: String → (ptr, len) in linear memory
    let name_packed = internal::cm_lower_string(name);
    let name_ptr = name_packed as i32;
    let name_len = (name_packed >> 32) as i32;

    // 2. Allocate outptr for list<list<u8>> return
    let outptr = builtin::realloc(0, 0, 4, 8);

    // 3. Call lowered WASI function (flat ABI)
    cm_raw_call Fields::get(self_handle, name_ptr, name_len, outptr);

    // 4. Free lowered param memory (callee has consumed it)
    builtin::realloc(name_ptr, name_len, 1, 0);

    // 5. Lift result: read (base_ptr, count) from outptr
    let base_ptr = builtin::i32_load(outptr);
    let count = builtin::i32_load(outptr + 4);

    // 6. Build Array<Array<u8>> from linear memory
    let result: Array<Array<u8>> = Array::<Array<u8>>::with_capacity(count);
    for let mut i = 0; i < count; i += 1 {
        let elem_ptr = builtin::i32_load(base_ptr + i * 8);
        let elem_len = builtin::i32_load(base_ptr + i * 8 + 4);
        let bytes = internal::memory_to_gc_array(elem_ptr, elem_len);
        result.append(bytes);
    }

    // 7. Free result linear memory (outer list + outptr)
    builtin::realloc(base_ptr, count * 8, 4, 0);
    builtin::realloc(outptr, 8, 4, 0);

    return result;
}
```

And the `EffectCall` becomes:

```
TirExprKind::Call {
    func: FunctionRef::Resolved("__cm_adapter__fields_get"),
    args: [self_handle, name],
}
```

### Export Adapter Example

Given a user export:

```wado
export fn handle(request: Request) -> Result<Response, ErrorCode> { ... }
```

cm_adapter_gen synthesizes:

```wado
// Synthesized export adapter
fn __cm_export__handle(method_ptr: i32, method_len: i32,
                       path_ptr: i32, path_len: i32,
                       /* ... flat params ... */
                       outptr: i32) {
    // 1. Lift params from flat ABI
    let method = internal::memory_to_gc_string(method_ptr, method_len);
    let path = internal::memory_to_gc_string(path_ptr, path_len);
    let request = Request { method, path, /* ... */ };

    // 2. Call user function
    let result = handle(request);

    // 3. Lower Result<Response, ErrorCode> to outptr
    match result {
        Ok(response) => {
            builtin::i32_store(outptr, 0);  // discriminant = Ok
            // lower response fields to outptr + 4...
        },
        Err(code) => {
            builtin::i32_store(outptr, 1);  // discriminant = Err
            builtin::i32_store(outptr + 4, code as i32);
        },
    }
}
```

### Async Adapter Example

For async WASI functions (e.g., `Stdout::write_via_stream`):

```wado
fn __cm_adapter__write_via_stream(stream: i32, data: String) {
    let data_packed = internal::cm_lower_string(data);
    let data_ptr = data_packed as i32;
    let data_len = (data_packed >> 32) as i32;

    // Call lowered async function — returns subtask handle
    let subtask = cm_raw_call Stdout::write_via_stream(stream, data_ptr, data_len);

    // Wait for completion, then free lowered param memory
    internal::wait_for_subtask(subtask);
    builtin::realloc(data_ptr, data_len, 1, 0);
}
```

### Required Builtins

The adapter functions use builtins to perform low-level operations. Some already exist, others need to be added:

#### Existing (in builtin.wado / internal.wado)

| Function                       | Purpose                                   |
| ------------------------------ | ----------------------------------------- |
| `builtin::realloc`             | Allocate linear memory                    |
| `builtin::memory_load32`       | Read i32 from linear memory               |
| `builtin::memory_store8`       | Write byte to linear memory               |
| `builtin::memory_load8_u`      | Read byte from linear memory              |
| `internal::cm_lower_string`    | Lower String to (ptr, len) packed as i64  |
| `internal::cm_lower_list_u8`   | Lower Array\<u8\> to (ptr, len)           |
| `internal::memory_to_gc_array` | Copy bytes from linear memory to GC array |
| `internal::gc_array_to_memory` | Copy bytes from GC array to linear memory |
| `internal::wait_for_subtask`   | Wait for async subtask completion         |

#### internal.wado scope

`internal.wado` provides CM helper functions only for types where lowering/lifting involves real work (allocation, memory copy). Scalar types (i32, i64, f32, f64, etc.) are lowered/lifted inline by the synthesizer using `builtin::i32_load` / `builtin::i32_store` directly — wrapping these in `internal::cm_lower_i32()` etc. would be trivial identity functions with no benefit.

| Type                                                    | Lowering                                            | Lifting                             | Provider                                                      |
| ------------------------------------------------------- | --------------------------------------------------- | ----------------------------------- | ------------------------------------------------------------- |
| i32, i64, f32, f64                                      | identity (flat param) / `builtin::*_store` (memory) | `builtin::*_load`                   | synthesizer inline                                            |
| bool                                                    | `value as i32`                                      | `i32_load8_u(addr) != 0`            | synthesizer inline                                            |
| char                                                    | `value as i32`                                      | `char::from_u32_unchecked`          | synthesizer inline                                            |
| String                                                  | alloc + copy → `(ptr, len)`                         | copy from linear memory → GC string | `internal::cm_lower_string`, `internal::memory_to_gc_string`  |
| Array\<u8\>                                             | alloc + copy → `(ptr, len)`                         | copy from linear memory → GC array  | `internal::cm_lower_array_u8`, `internal::memory_to_gc_array` |
| list\<T\>, option\<T\>, result\<T, E\>, record, variant | recursive                                           | recursive                           | synthesizer generates TIR (calls leaf helpers above)          |

Per-type converter functions (e.g., `cm_list_string_to_array`, `cm_option_string_to_option`) will be deleted once the synthesizer handles their types generically.

#### New builtins to add

| Function                | Wasm instruction | Purpose                                      |
| ----------------------- | ---------------- | -------------------------------------------- |
| `builtin::i32_load`     | `i32.load`       | Read i32 from linear memory at offset        |
| `builtin::i32_store`    | `i32.store`      | Write i32 to linear memory at offset         |
| `builtin::i64_load`     | `i64.load`       | Read i64 from linear memory at offset        |
| `builtin::i64_store`    | `i64.store`      | Write i64 to linear memory at offset         |
| `builtin::f32_load`     | `f32.load`       | Read f32 from linear memory at offset        |
| `builtin::f32_store`    | `f32.store`      | Write f32 to linear memory at offset         |
| `builtin::f64_load`     | `f64.load`       | Read f64 from linear memory at offset        |
| `builtin::f64_store`    | `f64.store`      | Write f64 to linear memory at offset         |
| `builtin::i32_load8_u`  | `i32.load8_u`    | Read byte from linear memory (zero-extended) |
| `builtin::i32_load16_u` | `i32.load16_u`   | Read u16 from linear memory                  |
| `builtin::i32_store8`   | `i32.store8`     | Write byte to linear memory                  |
| `builtin::i32_store16`  | `i32.store16`    | Write u16 to linear memory                   |

Note: `builtin::memory_load32` already exists but should be aliased to `builtin::i32_load` for consistency. The naming convention `builtin::i32_load` matches the Wasm instruction name.

#### Raw CM calls

Raw CM calls are represented as a dedicated TIR node `CmRawCall` rather than builtin functions. This avoids polluting the builtin namespace and produces more readable unparse output:

```
// TIR node
TirExprKind::CmRawCall {
    target: "Fields::get",                                    // effect::op reference
    local_name: "wasi:http/types/[method]fields.get",         // resolved import name
    args: [self_handle, name_ptr, name_len, outptr],          // flat ABI args
}

// Unparse output
cm_raw_call Fields::get(self_handle, name_ptr, name_len, outptr)
```

Codegen resolves the `local_name` to the imported function index. No builtin entity is needed per WASI function.

### Type-Driven Synthesis

The core of cm_adapter_gen is a **type-driven recursive synthesizer** that generates lift/lower TIR expressions for any Canonical ABI type:

```rust
// cm_adapter_gen.rs
impl CmAdapterGen {
    /// Synthesize TIR expressions to lift a CM value from linear memory
    fn synthesize_lift(&self, ty: &ResolvedType, addr: TirExpr) -> TirExpr {
        match ty {
            // Primitives: single load
            ResolvedType::I32 | ResolvedType::U32 =>
                builtin_call("i32_load", vec![addr]),
            ResolvedType::I64 | ResolvedType::U64 =>
                builtin_call("i64_load", vec![addr]),
            ResolvedType::F32 =>
                builtin_call("f32_load", vec![addr]),
            ResolvedType::F64 =>
                builtin_call("f64_load", vec![addr]),
            ResolvedType::Bool =>
                // i32.load8_u, then != 0
                ne(builtin_call("i32_load8_u", vec![addr]), i32_const(0)),

            // String: read (ptr, len), copy from linear memory
            ResolvedType::String => {
                let ptr = builtin_call("i32_load", vec![addr.clone()]);
                let len = builtin_call("i32_load", vec![add(addr, i32_const(4))]);
                internal_call("memory_to_gc_string", vec![ptr, len])
            }

            // Array<T>: read (ptr, count), lift each element
            ResolvedType::Array(elem_ty) => {
                let base = builtin_call("i32_load", vec![addr.clone()]);
                let count = builtin_call("i32_load", vec![add(addr, i32_const(4))]);
                let elem_size = self.cm_size(elem_ty);
                // Generate loop: for i in 0..count, lift element at base + i * elem_size
                self.synthesize_lift_list(elem_ty, base, count, elem_size)
            }

            // Option<T>: read discriminant, conditionally lift payload
            ResolvedType::Option(inner_ty) => {
                let disc = builtin_call("i32_load8_u", vec![addr.clone()]);
                let payload_offset = self.cm_align(inner_ty);  // padding after discriminant
                // if disc == 0 { None } else { Some(lift(inner, addr + offset)) }
                self.synthesize_lift_option(inner_ty, disc, addr, payload_offset)
            }

            // Result<T, E>: read discriminant, lift Ok or Err
            ResolvedType::Result(ok_ty, err_ty) => {
                let disc = builtin_call("i32_load", vec![addr.clone()]);
                let payload_offset = 4;  // after i32 discriminant
                // match disc { 0 => Ok(lift(ok, addr+4)), 1 => Err(lift(err, addr+4)) }
                self.synthesize_lift_result(ok_ty, err_ty, disc, addr, payload_offset)
            }

            // Record/struct: lift each field at its offset
            ResolvedType::Record(fields) => {
                self.synthesize_lift_record(fields, addr)
            }

            // Enum: load discriminant
            ResolvedType::Enum(_) =>
                builtin_call("i32_load", vec![addr]),

            // Variant: load discriminant, lift payload per case
            ResolvedType::Variant(cases) => {
                self.synthesize_lift_variant(cases, addr)
            }

            // Resource handle: just an i32
            ResolvedType::Resource =>
                builtin_call("i32_load", vec![addr]),
        }
    }

    /// Synthesize TIR expressions to lower a Wado value to linear memory
    fn synthesize_lower(&self, ty: &ResolvedType, value: TirExpr, addr: TirExpr) -> TirExpr {
        // Symmetric to synthesize_lift: store primitives, recurse for composites
        // ...
    }
}
```

### Canonical ABI Layout Computation

A pure `cm_abi.rs` module computes sizes, alignments, and field offsets for Canonical ABI types:

```rust
// cm_abi.rs

/// Canonical ABI size in bytes
pub fn cm_size(ty: &ResolvedType) -> u32 {
    match ty {
        ResolvedType::Bool | ResolvedType::U8 | ResolvedType::I8 => 1,
        ResolvedType::U16 | ResolvedType::I16 => 2,
        ResolvedType::I32 | ResolvedType::U32 | ResolvedType::F32 |
        ResolvedType::Char | ResolvedType::Enum(_) | ResolvedType::Resource => 4,
        ResolvedType::I64 | ResolvedType::U64 | ResolvedType::F64 => 8,
        ResolvedType::String | ResolvedType::Array(_) => 8,  // (ptr, len)
        ResolvedType::Option(inner) => {
            let payload = cm_size(inner);
            let align = cm_align(inner);
            align_to(1, align) + payload  // disc + padding + payload
        }
        ResolvedType::Result(ok, err) => {
            4 + max(cm_size(ok), cm_size(err))  // disc(i32) + max(ok, err)
        }
        ResolvedType::Record(fields) => {
            // Sum of fields with alignment padding
            layout_record(fields).size
        }
        ResolvedType::Variant(cases) => {
            4 + cases.iter().map(|c| cm_size(&c.payload)).max().unwrap_or(0)
        }
        // Tuple is a record
        ResolvedType::Tuple(elems) => {
            layout_tuple(elems).size
        }
    }
}

/// Canonical ABI alignment in bytes
pub fn cm_align(ty: &ResolvedType) -> u32 { ... }

/// Compute field offsets for a record/struct
pub fn layout_record(fields: &[CmField]) -> CmLayout { ... }
```

This replaces the ad-hoc size/align constants scattered throughout `CmCallConvention` (e.g., `outptr_alloc: Some((8, 4))` for string, `Some((12, 4))` for option\<string\>).

### What Gets Removed

After cm_adapter_gen is complete:

| Location           | Code                                    | Lines | Fate                                                      |
| ------------------ | --------------------------------------- | ----- | --------------------------------------------------------- |
| codegen.rs         | `generate_cm_effect_call`               | ~140  | Deleted — adapter handles CM calls                        |
| codegen.rs         | `generate_cm_resource_method_call`      | ~250  | Deleted — adapter handles resource calls                  |
| codegen.rs         | `emit_option_string_lowering`           | ~40   | Deleted — adapter generates inline                        |
| codegen.rs         | `emit_field_size_payload_lowering`      | ~65   | Deleted — adapter generates inline                        |
| codegen.rs         | `wado_type_to_cm_val_type`              | ~50   | Moved to cm_abi.rs                                        |
| component_model.rs | `CmCallConvention` + `from_return_type` | ~350  | Deleted — type-driven synthesis replaces pattern matching |
| internal.wado      | `cm_list_string_to_array`               | ~20   | Deleted — adapter generates inline                        |
| internal.wado      | `cm_option_string_to_option`            | ~15   | Deleted — adapter generates inline                        |
| internal.wado      | `cm_option_own_resource_to_option`      | ~10   | Deleted — adapter generates inline                        |
| internal.wado      | `cm_list_tuple_string_string_to_array`  | ~25   | Deleted — adapter generates inline                        |
| Total removed      |                                         | ~965  |                                                           |

New code:

| Module                 | Purpose                          | Lines (est.) |
| ---------------------- | -------------------------------- | ------------ |
| cm_adapter_gen.rs      | Type-driven adapter synthesis    | ~500         |
| cm_abi.rs              | Canonical ABI layout computation | ~200         |
| builtin.wado additions | Memory load/store builtins       | ~50          |
| Total added            |                                  | ~750         |

Net: **~215 lines reduction**, plus elimination of the per-type hand-coding pattern.

### What Stays

- `WasiRegistry` in component_model.rs — still needed to know which WASI functions exist and their signatures
- `ComponentPlan` in wasm_plan.rs — still needed for component wrapper construction
- `CmExportInfo` in wasm_plan.rs — absorbed into cm_adapter_gen (export adapter synthesis replaces scratch local pre-computation)
- Component wrapper encoding in codegen — still emits the outer Component Model structure (`ComponentBuilder` calls)

## Migration Plan

### Phase 1: Builtins and Infrastructure

- [ ] Add `builtin::i32_load`, `builtin::i32_store`, `builtin::i64_load`, `builtin::i64_store`, `builtin::f32_load`, `builtin::f32_store`, `builtin::f64_load`, `builtin::f64_store` and sub-word variants to `builtin.wado` and codegen's builtin dispatch.
- [ ] Create `cm_abi.rs` with Canonical ABI size/align/layout computation.
- [ ] Add unit tests for `cm_abi.rs` against known Canonical ABI layouts.

### Phase 2: Import Adapters (Incremental)

Migrate one WASI interface at a time, validating via existing E2E tests.

- [ ] Create `cm_adapter_gen.rs` scaffolding: the phase entry point, TIR function synthesis helpers.
- [ ] Implement `synthesize_lift` and `synthesize_lower` for primitives (i32, i64, f32, f64, bool, char).
- [ ] Implement `synthesize_lift` and `synthesize_lower` for String.
- [ ] Migrate `wasi:cli/Stdout` and `wasi:cli/Stderr` (simplest: void return, String param). Validate with E2E tests.
- [ ] Implement `synthesize_lift` for `list<T>`, `option<T>`, `result<T, E>`.
- [ ] Migrate `wasi:cli/Environment` (returns `list<tuple<string, string>>`). This replaces `cm_list_tuple_string_string_to_array`.
- [ ] Migrate `wasi:http/types` resource methods. This replaces `generate_cm_resource_method_call`.
- [ ] Migrate remaining WASI interfaces.
- [ ] Delete `CmCallConvention` and per-type converters in `internal.wado`.

### Phase 3: Export Adapters

- [ ] Implement export adapter synthesis for `wasi:cli/run` (simplest export).
- [ ] Implement export adapter synthesis for `wasi:http/incoming-handler` (complex: async, Result return).
- [ ] Delete `CmExportInfo` scratch local logic — adapters declare their own locals.
- [ ] Delete export-related CM glue from codegen.

### Phase 4: Cleanup

- [ ] Remove `TirExprKind::EffectCall` — all effect calls are now ordinary `Call`s to adapters.
- [ ] Remove `cm_convention` and `cm_local_name` fields from TIR.
- [ ] Inline or remove `CmCallConvention`.
- [ ] Verify all E2E tests pass, including HTTP serve tests.

## Consequences

### Benefits

- **Extensibility**: Any Canonical ABI type is supported by the recursive synthesizer — no per-type hand-coding.
- **Optimization**: Adapter functions go through lower → optimize, so the optimizer can inline small adapters, eliminate dead branches in match arms, and propagate constants.
- **Debuggability**: `wado dump --tir --unparse` and `wado dump --lower --unparse` show the full CM glue as Wado code.
- **Independence from WIR**: This can be implemented and shipped before WIR migration begins. It reduces the CM surface area that `tir_to_wir` must handle (Step 3e in WIR WEP).
- **Simpler codegen**: Codegen no longer needs to know about CM lifting/lowering. It just compiles adapter functions like any other function.
- **Simpler `tir_to_wir`**: When WIR migration begins, CM adapters are already ordinary TIR functions — no special CM translation logic needed in `tir_to_wir`.

### Trade-offs

- **TIR growth**: The synthesized adapter functions add to the TIR function count. Mitigated by dead code elimination (unused adapters are removed) and by adapter functions being small.
- **Builtin surface area**: More builtins (`builtin::i32_load`, etc.) increase the builtin dispatch table. These are trivial 1:1 mappings to Wasm instructions.
- **Two-phase migration**: During migration, some WASI calls use adapters while others still use the old `generate_cm_effect_call` path. Each interface is migrated independently, so the codebase is always in a working state.

### Risks

- **Canonical ABI correctness**: The layout computation must match the Component Model specification exactly. Mitigated by unit tests against known layouts and by validating against wasmtime's runtime behavior via E2E tests.
- **Performance regression in adapter code**: Synthesized TIR may produce suboptimal Wasm compared to hand-written codegen. Mitigated by the optimizer (which sees adapter functions) and by golden fixture comparison.
