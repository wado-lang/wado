# WEP: TIR-Level CM Binding Synthesis

## Context

The compiler needs to cross the Component Model (CM) boundary at two points: **imports** (calling WASI functions) and **exports** (exposing Wado functions to the host). This boundary crossing requires lowering Wado GC values to CM flat ABI (linear memory scalars) and lifting CM values back to Wado types.

Previously, this logic was scattered across codegen (raw Wasm instruction emission), per-type converter functions in `internal.wado`, and pattern-matching convention structs. Every new WIT type shape required hand-coded support in multiple places.

CM binding synthesis replaces all of that with a single **type-driven recursive synthesizer** that generates binding functions as TIR. Adapters flow through the normal compiler pipeline (monomorphize → lower → optimize → codegen) like any other function.

## Architecture

### Pipeline Position

```
load → analyze → resolve (TIR) → effect_check
                                       ↓
                                   synthesis (traits + inspect + cm_binding)
                                       ↓
                    monomorphize → lower → optimize → wir_build → wir_optimize → codegen
```

The CM binding synthesis runs as part of the `synthesis` phase (alongside trait synthesis and
inspect synthesis), after effect checking and before monomorphize. Adapter functions go through
monomorphization, optimization, WIR translation, and codegen like user code.

Note: `wasm_plan` (component planning) is now integrated into the `wir_build` phase as
`wir_build::component_plan::build_component_plan()`, not a standalone pipeline phase.

### Import Adapters

For each WASI function used by the program, `cm_binding_gen` synthesizes an binding that:

1. Accepts Wado-typed parameters (String, Array, structs, etc.)
2. Lowers parameters to CM flat ABI (linear memory via builtins)
3. Calls the lowered WASI function via `CmRawCall`
4. Lifts the result from CM flat ABI back to Wado types
5. Returns the Wado-typed result

All call sites are rewritten from the original WASI call to the binding call.

### Export Adapters

For each world export, `cm_binding_gen` synthesizes an binding that wraps the user's function. The binding is what the component actually exports — it handles CM ABI translation and calls the user function internally.

### Type-Driven Synthesis

The core synthesizer (`synthesize_lift`, `synthesize_lower_to_flat`) is recursive and type-driven:

| Type                                                    | Lowering                      | Lifting                             | Provider                                                      |
| ------------------------------------------------------- | ----------------------------- | ----------------------------------- | ------------------------------------------------------------- |
| i32, i64, f32, f64                                      | identity / `builtin::*_store` | `builtin::*_load`                   | synthesizer inline                                            |
| bool                                                    | `value as i32`                | `i32_load8_u(addr) != 0`            | synthesizer inline                                            |
| char                                                    | `value as i32`                | `char::from_u32_unchecked`          | synthesizer inline                                            |
| String                                                  | alloc + copy → `(ptr, len)`   | copy from linear memory → GC string | `internal::cm_lower_string`, `internal::memory_to_gc_string`  |
| Array\<u8\>                                             | alloc + copy → `(ptr, len)`   | copy from linear memory → GC array  | `internal::cm_lower_array_u8`, `internal::memory_to_gc_array` |
| list\<T\>, option\<T\>, result\<T, E\>, record, variant | recursive                     | recursive                           | synthesizer generates TIR                                     |

No per-type converter functions exist. The synthesizer handles all composite types by recursing into their structure.

### Canonical ABI Layout

`cm_abi.rs` computes sizes, alignments, field offsets, and flat type sequences for any Canonical ABI type. This replaces the ad-hoc size/align constants that were previously scattered across convention structs.

### CmRawCall

Raw CM calls are a dedicated TIR node:

```
TirExprKind::CmRawCall {
    local_name: "wasi:http/types/[method]fields.get",
    args: [self_handle, name_ptr, name_len, outptr],
}
```

Codegen resolves `local_name` to the imported function index. This node is also used for `task-return` and other CM intrinsics in export adapters. For stream/future/waitable-set CM operations, see [WEP: Redesign Wasm CM Builtins as Resource Canonical Attributes](wep-2026-03-01-cm-resource-canonical-attrs.md).

## Current State

### Import Adapters — Complete

All WASI interfaces (cli, clocks, random, http, sockets) use binding synthesis. Both instance method calls (MethodCall, e.g., `fields.append(name, value)`) and resource static method calls (StaticCall, e.g., `Response::new(headers, null, trailers)`) are rewritten to binding calls. The old codegen CM paths (`generate_cm_effect_call`, `generate_cm_resource_method_call`) and per-type converters (`cm_list_string_to_array`, `cm_option_string_to_option`, etc.) have been deleted.

### Export Adapters — Async Only

Export adapters currently handle two cases:

#### Void exports (`() -> ()`)

Used by Command world's `run()` and test functions. The binding calls the user function, then calls `task-return(0)`.

#### Result-returning exports (`(...) -> Result<T, E>`)

Used by Service world's `handle()`. The binding:

1. Calls the user function (passing through parameters)
2. Tests the Result discriminant
3. Ok: lowers T to flat CM values via `synthesize_lower_to_flat`, calls `task-return`
4. Err: lowers E (variant, struct, or primitive) to flat CM values, calls `task-return`

The lowering is fully generic — `synthesize_lower_to_flat` recurses into any type structure (primitives, strings, options, structs, variants) without hard-coded type names.

### What's Generic vs What's Specific

| Component                          | Generic?         | Notes                                                                    |
| ---------------------------------- | ---------------- | ------------------------------------------------------------------------ |
| `synthesize_lower_to_flat`         | Yes              | Recursive, type-driven. Only hard-codes `"String"` for `cm_lower_string` |
| `synthesize_variant_lower_to_flat` | Yes              | Iterates variant cases from declaration                                  |
| `flatten_*` flat type computation  | Yes              | Type-structure-based                                                     |
| `cm_abi.rs` layout computation     | Yes              | Pure Canonical ABI spec                                                  |
| Result Ok/Err dispatch             | Intentional      | Result is a language primitive with fixed case ordering                  |
| `task-return` in all adapters      | Async assumption | WASI P3 is async-only; needs change for sync                             |

## Remaining Work: Generic CM Exports

The current export binding synthesis works for the two hosted worlds (Command, Service). To support **arbitrary world exports** — including publishing Wado libraries as `.wasm` components — the following work is needed.

### Parameter Lifting

Current adapters pass user function parameters through unchanged. This works because:

- Command world `run()` takes no parameters
- Service world `handle(request: Request)` receives a resource handle (i32), which needs no lifting

For generic exports with composite parameters, the binding must **lift flat CM params to Wado types**:

```wado
// User writes:
export fn greet(name: String) -> String { ... }

// Adapter must synthesize:
fn __cm_export__greet(name_ptr: i32, name_len: i32) {
    let name = internal::memory_to_gc_string(name_ptr, name_len);
    let result = greet(name);
    // lower result...
}
```

This requires:

- [x] Compute flat parameter types from the world export's WIT signature
- [x] Generate `synthesize_lift_from_flat_params` for each parameter in the export binding
- [x] Map flat params to binding function parameters

Implemented via `synthesize_lift_from_flat_params` which lifts flat CM params (i32/i64/f32/f64) to Wado-typed values. Handles primitives, bool, char, String, resources, Array, Option, and tuples. The export binding now detects when param lifting is needed (via `export_needs_param_lifting`) and generates flat-typed binding params with lifting code.

### Non-Result Return Types

Current adapters handle `()` and `Result<T, E>`. For generic exports, any return type should work:

```wado
export fn add(a: i32, b: i32) -> i32 { ... }
export fn get_name() -> String { ... }
export fn get_pair() -> [String, i32] { ... }
```

This requires:

- [x] `synthesize_lower_to_flat` for non-Result returns (the function exists, just not wired for direct returns)
- [ ] Handle return-via-flat-params vs return-via-outptr (CM spec: if flat count > `MAX_FLAT_RESULTS`, use an outptr)

Implemented via `synthesize_general_export_adapter` which handles non-Result return types. The binding calls the user function, lowers the return value to flat CM values, and calls `task-return(0, ...flat_values)` — wrapping in Ok since CM exports wrap returns in `result<T, error-context>`.

### Sync Export Support

All current adapters assume WASI P3 async semantics (`task-return`). For publishing `.wasm` components consumed by non-async hosts, sync exports are needed:

```
// Async (current): binding calls task-return with flat values
cm_raw_call task-return(disc, val1, val2, ...);

// Sync: binding returns flat values directly
return (val1, val2, ...);
```

This requires:

- [ ] Detect whether the target world is async (P3) or sync (P2/standalone)
- [ ] Sync adapters return flat values or write to outptr instead of calling `task-return`
- [ ] Sync adapters use `canon lift` / `canon lower` rather than `task-return`

### Export Validation

- [x] Validate that user's export function parameter count matches the world declaration
- [ ] Validate parameter types match (beyond count)
- [ ] Validate return type compatibility
- [ ] Produce clear error messages for type mismatches

Parameter count validation is implemented: if the user's export function has a different number of parameters than the world declaration, a clear compile error is produced.

### Summary

| Task                        | Difficulty | Status  | Notes                                     |
| --------------------------- | ---------- | ------- | ----------------------------------------- |
| Parameter lifting           | Medium     | Done    | `synthesize_lift_from_flat_params`        |
| Non-Result return types     | Low        | Done    | `synthesize_general_export_adapter`       |
| Sync export support         | Medium     | Pending | World metadata for async/sync distinction |
| Export signature validation | Low        | Partial | Parameter count validated; types not yet  |

The type-driven synthesizer (`synthesize_lift`, `synthesize_lower_to_flat`, flat type computation) is already generic. The remaining work is sync export support and full type validation.

### Known Limitations and Edge Cases

#### Parameter Lifting Gaps

`synthesize_lift_from_flat_params` handles primitives, bool, char, String, resources, Array (with linear memory round-trip), Option, and tuples. The following types are **not yet implemented**:

- **Struct parameters (non-String)**: Treated as i32 passthrough. Should lift each field from consecutive flat params.
- **Result parameters**: Falls through to unit default. Unlikely in practice (Result is typically a return type, not a parameter).
- **Variant parameters**: Treated as i32 passthrough. Should lift discriminant + case-specific payloads.

These gaps are safe for current worlds (Command, Service) but would need to be addressed for custom worlds with complex parameter types.

#### Flat Return Type Mismatch in General Adapter

`synthesize_general_export_adapter` computes flat return types from the **user function's return type**, not from the world's `result<T, error-context>` wrapper. This means:

- `task-return(0, ...T_flat)` provides `1 + |T_flat|` args
- The CM expects `1 + max(|T_flat|, |E_flat|)` args (union of Ok and Err payloads)
- If `error-context` has more flat slots than the Ok payload, the binding may provide too few args

In practice this is safe because:

- Current worlds use `Result<(), ()>` (no error-context) or `Result<T, E>` (handled by `synthesize_result_export_adapter`)
- `error-context` is typically i32 (1 slot), and most return types have >= 1 slot

To fix: compute flat return types from the world's full `result<T, error-context>` type, and zero-fill any extra slots.

#### Array Lifting Uses Temporary Linear Memory

For `Array<T>` where T is not u8, `synthesize_lift_from_flat_params` writes flat params (ptr, len) to a temporary 8-byte linear memory block, then calls `synthesize_lift` which reads from that block. This:

- Requires `builtin::realloc` to be linked (always true for programs with linear memory)
- Allocates and immediately frees 8 bytes (wasteful but correct)
- Could be optimized with a direct `synthesize_lift_list_from_flat` that takes ptr/len as locals

#### Call-Site Flattening for Multi-Flat Parameters

Import adapters use two strategies depending on the parameter type:

- **Adapter-internal lowering** (String, Array\<u8\>): The binding accepts a single Wado-level parameter and lowers it internally to multiple flat CM args (ptr + len). This works because String and Array have well-defined Wado TypeIds that codegen can convert to Wasm types.
- **Call-site flattening** (Option\<T\>, other multi-flat types): The binding accepts pre-flattened i32 params, and the call-site rewrite transforms Wado-level args into flat values before passing them.

The call-site flattening approach was chosen for Option\<T\> because binding-internal lowering faces a fundamental type mismatch: Wado's `null` literal generates `ref.null` (a GC nullable reference) at the Wasm level, but the binding would need to accept it as a parameter and extract an i32 discriminant + payload. Converting between GC references and i32 scalars requires non-trivial unwrapping logic (pattern matching, unboxing) that the TIR synthesizer cannot easily generate for all Option\<T\> instantiations.

By flattening at the call site:

- `null` → `[i32(0), i32(0), ...]` (discriminant=0, zero payload)
- `OptionSome(value)` → `[i32(1), value, ...]` (discriminant=1, inner value)

The binding body becomes a simple pass-through for these parameters. This avoids the GC-to-scalar type mismatch entirely.

Current limitation: only literal `null` and `OptionSome` expressions are supported at call sites. Arbitrary `Option<T>` variables would require runtime null-check logic at the rewrite site, which is not yet implemented.

#### No Type-Level Validation

Parameter count is validated, but parameter types and return type compatibility are not checked. For example, the compiler won't error if the user declares `export fn run(x: String)` but the world expects `run(x: i32)`. The binding would generate incorrect lifting code (treating i32 as String).

## Consequences

### Benefits

- **Extensibility**: Any Canonical ABI type is supported by the recursive synthesizer — no per-type hand-coding.
- **Optimization**: Adapter functions go through lower → optimize → wir_optimize, so the optimizer can inline small adapters, eliminate dead branches, and propagate constants.
- **Debuggability**: `wado dump --tir-resolved` and `wado dump --tir-lowered` show the full CM glue as Wado code.
- **Simpler codegen**: Codegen no longer needs to know about CM lifting/lowering. It compiles binding functions like any other function.
- **WIR-compatible**: CM bindings are ordinary TIR functions that translate to WIR without special handling in `wir_build`.

### Risks

- **Canonical ABI correctness**: Layout computation must match the CM spec exactly. Mitigated by unit tests (37 in `cm_abi.rs`) and E2E tests against wasmtime.
- **Performance**: Synthesized TIR may produce suboptimal Wasm compared to hand-written codegen. Mitigated by the optimizer and golden fixture comparison.

## Related WEPs

- [WEP: Redesign Wasm CM Builtins as Resource Canonical Attributes](wep-2026-03-01-cm-resource-canonical-attrs.md) — Moves stream/future/waitable-set canonical operations from `builtin.wado` to `#[canonical]` attributes on resource methods, complementing the import/export binding synthesis here.
- [WEP: WASI HTTP Integration](wep-2026-02-21-wasi-http.md) — HTTP handler patterns built on the export binding synthesis and CM async primitives.
