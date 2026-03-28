# WEP: GC in Components — Research and Impact Analysis

## Context

Wado compiles to Wasm Components using Wasm GC internally for all heap-allocated values (structs, arrays, strings, closures, variants). At the Component Model (CM) boundary, the compiler currently **converts all GC values to/from linear memory** via the Canonical ABI's flat representation. This is the only approach supported by the current CM spec.

A [pre-proposal (WebAssembly/component-model#525)](https://github.com/WebAssembly/component-model/issues/525) by fitzgen (June 2025) proposes extending the Canonical ABI to allow **GC-native value passing** across component boundaries. This WEP documents the current state of the spec, wasmtime implementation status, and the implications for Wado.

## Current State: CM Boundary = Copy Through Linear Memory

### How It Works Today

The current Canonical ABI prescribes a **linear-memory-based** flat ABI for all component-level value exchange:

1. **Export (lift):** Wado GC values → copy to linear memory → CM flat ABI scalars
2. **Import (lower):** CM flat ABI scalars → copy from linear memory → Wado GC values

For example, passing a `String` across the CM boundary:
- **Lower (call WASI):** GC string → `cm_lower_string` copies bytes to linear memory → `(ptr, len)` pair
- **Lift (from WASI):** `(ptr, len)` pair → `memory_to_gc_string` copies bytes from linear memory → GC string

This means **every CM boundary crossing requires a full copy** through linear memory, even when both sides use Wasm GC internally.

### Wado's Current Architecture

The compiler's CM binding synthesis (`synthesis/cm_binding.rs`) generates TIR adapter functions that handle all type conversions. The layout computation (`cm_abi.rs`) follows the Canonical ABI specification for sizes, alignments, and field offsets in linear memory.

Key files:
- `wado-compiler/src/synthesis/cm_binding.rs` — adapter synthesis
- `wado-compiler/src/cm_abi.rs` — Canonical ABI layout
- `wado-compiler/src/codegen/component.rs` — component builder
- `wado-compiler/src/component_model.rs` — CM types and registry

## The Pre-Proposal: GC-Native Canonical ABI (component-model#525)

### Overview

The proposal introduces two new canonical options that enable GC-native value passing:

| Option | Encoding | Purpose |
|--------|----------|---------|
| `gc` | `0x09` | Switch canonical ABI from linear memory to GC mode |
| `core-type` | `0x08 <idx:u32>` | Specify which core function type to use for GC lowering |

Both must be present together. When `gc` is set, the canonical ABI bypasses linear memory entirely and passes GC references directly.

### Type Lowering in GC Mode

The proposal defines mappings between component types and core Wasm GC types, distinguishing **value types** (direct args/returns) from **storage types** (nested in structs/arrays):

| Component Type | GC Value Type | GC Storage Type |
|----------------|---------------|-----------------|
| `bool`, `s8`, `u8` | `i32` | `i8` |
| `s16`, `u16` | `i32` | `i16` |
| `s32`, `u32` | `i32` | `i32` |
| `s64`, `u64` | `i64` | `i64` |
| `f32`, `f64` | native | native |
| `char` | `i32` | `i32` |
| `string` (UTF-8) | `(ref null? (array (mut? i8)))` | — |
| `string` (UTF-16) | `(ref null? (array (mut? i16)))` | — |
| `record` / `tuple` | `(ref null? (struct ...))` | — |
| `list<T>` | `(ref null? (array (mut? T')))` | — |
| `variant` | `(ref null? (struct))` + subtypes | — |
| `option<T>` | `(ref null? (struct))` + subtypes | — |
| `result<T, E>` | `(ref null? (struct))` + subtypes | — |
| `own` / `borrow` | `externref` | — |
| `future` / `stream` | `externref` | — |

### Key Design Decisions

#### 1. Components Choose Their Own Core Types

The proposal avoids prescribing a single canonical GC type for each component type. Instead, each component specifies its preferred core types via `core-type`. This minimizes copies:

- If two components use the **same rec group** for a record type → **zero-copy** passing
- If they use **different rec groups** (even structurally identical) → engine must copy
- If one uses **mutable** fields and the other **immutable** → at least one copy

#### 2. Mutability Trade-offs

| Scenario | Copies |
|----------|--------|
| Both immutable, same rec group | 0 |
| Both immutable, different rec groups | 1 |
| One mutable, one immutable | 1 |
| Both mutable | 2 (worse than linear memory's 1) |

Making fields immutable enables zero-copy passing but forces a copy if the component's "at rest" representation needs mutability (e.g., for array `.append()`).

#### 3. Variant / Option / Result Representation

The proposal uses **subtype hierarchies** (not nullable refs) for sum types:
- Each case becomes a subtype of a shared base struct
- All case types must be in the same `rec` group
- This avoids ambiguity with nested optionals (`option<option<T>>`)

This differs from Wado's internal representation which uses NullableRef optimization for 2-case variants with one unit case. At the CM boundary, the proposal requires SubtypeHierarchy for all sum types.

#### 4. Resources Remain as externref

`own<T>`, `borrow<T>`, `future<T>`, `stream<T>`, and `error-context` all lower to `externref`. No change from the current approach.

### What's NOT in the Proposal

- **Width/depth subtyping** for records and tuples (planned for future extension)
- **Per-case `core-type`** for variants (rejected due to verbosity)
- **Shared-everything linking** integration (separate concern)

## Wasmtime Implementation Status

### Core Wasm GC: Complete (v27.0+)

Since wasmtime v27.0 (November 2024), core Wasm GC is fully implemented:
- All GC instructions (struct.new, array.new, ref.cast, ref.test, etc.)
- Two collector implementations: standard GC collector and null collector (bump-allocate, never collect)
- Enabled via `Config::wasm_gc` or `-W gc` CLI flag

### GC in Components: Prototype Stage

- **Tracking issue:** [bytecodealliance/wasmtime#10325](https://github.com/bytecodealliance/wasmtime/issues/10325) (opened March 2025 by fitzgen)
- **Status:** Open, no merged PRs visible. Still in prototyping phase.
- **Feature flag:** `WasmFeatures` has a flag for "Support for Wasm GC in the component model proposal" (corresponds to 🛸 emoji), defaults to `false`
- fitzgen is actively implementing in both wasm-tools and wasmtime

### Timeline Estimate

Given that:
- The pre-proposal was posted June 2025
- Prototyping is ongoing in wasmtime
- No spec merge has occurred yet

A reasonable estimate: **experimental support in wasmtime H2 2026, stable in 2027**.

## Shared-Everything Linking (Separate Concern)

[Shared-everything linking](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Linking.md) allows multiple core modules within a component to share `memory` and `table` instances. This is orthogonal to GC-in-components:

- **Shared-everything linking**: modules within one component share linear memory (for C/Rust-style linking)
- **GC in components**: GC types cross component boundaries (for GC-language interop)

The [shared-everything-threads proposal](https://github.com/WebAssembly/shared-everything-threads) (separate from shared-everything linking) adds `shared` annotations for cross-thread sharing. This is also orthogonal.

## Impact on Wado

### What Changes When GC-in-CM Lands

1. **CM binding synthesis** (`cm_binding.rs`) will need a parallel "GC mode" path that emits GC ref passing instead of linear memory copies.

2. **Component builder** (`codegen/component.rs`) will need to emit the `gc` and `core-type` canonical options.

3. **Type mapping**: Wado's internal GC types are already structurally similar to the proposal's lowerings:
   - Wado `String` → `(ref (array (mut i8)))` ← proposal's UTF-8 string
   - Wado structs → `(ref (struct ...))` ← proposal's record
   - Wado `Array<T>` → `(ref (array (mut T')))` ← proposal's `list<T>`

4. **Variant representation at CM boundary**: The proposal requires SubtypeHierarchy for all sum types. Wado already uses SubtypeHierarchy internally (except for NullableRef-optimized 2-case variants). The CM adapter would need to convert between representations for NullableRef variants.

5. **Mutability alignment**: Wado uses mutable struct fields and mutable arrays internally. The proposal suggests this leads to copies. Wado may want to explore immutable "transfer" representations for CM boundary crossing.

### Potential Zero-Copy Path

If Wado aligns its internal GC representations with the CM GC canonical types:

| Type | Current (linear memory) | GC-in-CM (aligned) |
|------|------------------------|---------------------|
| `String` import | copy LM→GC | **zero-copy** (same array type) |
| `String` export | copy GC→LM | **zero-copy** (same array type) |
| Record import | copy LM→GC | **zero-copy** (if same rec group) |
| Record export | copy GC→LM | **zero-copy** (if same rec group) |
| `Array<u8>` import | copy LM→GC | **zero-copy** |
| `Array<u8>` export | copy GC→LM | **zero-copy** |

This would eliminate the biggest performance cost of WASI calls for data-heavy operations.

### Rec Group Strategy

The proposal's rec group deduplication rule is critical. For zero-copy:
- All types used at CM boundaries should be in a carefully structured rec group
- Types that cross component boundaries together should share the same rec group
- The compiler must ensure its rec group layout matches what the host/other components expect

### Action Items (When Ready)

1. **Track the proposal**: Monitor component-model#525 and wasmtime#10325 for spec stabilization
2. **Dual-mode binding synthesis**: Keep linear memory path, add GC path behind feature flag
3. **Rec group planning**: Design the compiler's rec group emission to align with CM GC canonical types
4. **Mutability analysis**: Consider immutable "snapshot" types for CM boundary crossing
5. **Benchmark**: Compare linear memory vs GC boundary crossing once wasmtime prototype is available

### No Immediate Action Required

The proposal is pre-stage and wasmtime support is not yet available. Wado's current linear-memory CM binding approach is correct and performant for the current spec. The architecture (type-driven TIR synthesis) is well-positioned to add a GC mode when the time comes.

## References

- [Pre-Proposal: Wasm GC Support in the Canonical ABI (component-model#525)](https://github.com/WebAssembly/component-model/issues/525)
- [Prototype Wasm GC and CM canonical ABI support (wasmtime#10325)](https://github.com/bytecodealliance/wasmtime/issues/10325)
- [Component Model Linking](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Linking.md)
- [Shared-Everything Dynamic Linking Example](https://github.com/WebAssembly/component-model/blob/main/design/mvp/examples/SharedEverythingDynamicLinking.md)
- [Wasmtime 27.0: Complete Wasm GC support](https://bytecodealliance.org/articles/wasmtime-27.0)
- [Implement the WebAssembly GC Proposal (wasmtime#5032)](https://github.com/bytecodealliance/wasmtime/issues/5032)
- [Wasm GC Proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md)
- [Component Model Explainer](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Explainer.md)
- [Component Model Canonical ABI](https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md)
