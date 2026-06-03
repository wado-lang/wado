# WEP: Migration to GC in Components

## Context

Wado uses Wasm GC internally for all heap-allocated values (structs, arrays, strings, closures, variants). At Component Model (CM) boundaries, these GC values are **serialized to linear memory** and deserialized back, because the current CM Canonical ABI only supports linear-memory-based data exchange. Every WASI call copies data twice: GC→linear memory on export, linear memory→GC on import.

A [pre-proposal (component-model#525)](https://github.com/WebAssembly/component-model/issues/525) extends the Canonical ABI with a `gc` mode that passes GC references directly across component boundaries — eliminating the linear memory round-trip. When this lands, Wado should migrate its CM bindings to GC mode for significant performance gains.

This WEP defines the migration strategy.

## Decision

Migrate CM boundary crossing from linear memory to GC-native in three phases:

1. **Align internal representations** — adjust Wado's GC types to be compatible with the CM GC canonical types
2. **Dual-mode binding synthesis** — add a `gc` path alongside the existing linear memory path in `cm_binding.rs`
3. **Switch default** — make GC mode the default once wasmtime support stabilizes

## Background: The CM GC Pre-Proposal

### New Canonical Options

The pre-proposal adds two canonical options:

- **`gc` (`0x09`)**: Switch from linear memory to GC mode
- **`core-type` (`0x08 <idx:u32>`)**: Declare the core function type used for GC lowering

When `gc` is present, `lift_func` and `lower_func` operate on GC references instead of linear memory pointers. `memory` and `realloc` are no longer needed.

### CM Type → Core GC Type Mapping

| CM Type                                | GC Lowering (value position)                                 | GC Lowering (storage position) |
| -------------------------------------- | ------------------------------------------------------------ | ------------------------------ |
| `bool`, `s8`, `u8`                     | `i32`                                                        | `i8`                           |
| `s16`, `u16`                           | `i32`                                                        | `i16`                          |
| `s32`, `u32`, `char`                   | `i32`                                                        | `i32`                          |
| `s64`, `u64`                           | `i64`                                                        | `i64`                          |
| `f32`, `f64`                           | native                                                       | native                         |
| `string`                               | `(ref null? (array (mut? i8)))`                              | —                              |
| `record` / `tuple`                     | `(ref null? (struct ...))`                                   | —                              |
| `list<T>`                              | `(ref null? (array (mut? T')))`                              | —                              |
| `variant` / `option` / `result`        | subtype hierarchy (base struct + subtypes in same rec group) | —                              |
| `own` / `borrow` / `future` / `stream` | `externref`                                                  | —                              |

Components choose nullability and mutability. The engine can pass values **zero-copy** when both sides use the same rec group and compatible mutability.

### Mutability and Copies

| Scenario                             | Copies at CM boundary |
| ------------------------------------ | --------------------- |
| Both immutable, same rec group       | 0                     |
| Both immutable, different rec groups | 1                     |
| One mutable, one immutable           | 1                     |
| Both mutable                         | 2                     |
| Current linear memory approach       | 2 (always)            |

### Upstream Status

| Item                | Status                                                                                                               |
| ------------------- | -------------------------------------------------------------------------------------------------------------------- |
| Wasm GC (core spec) | Shipped in Wasm 3.0 (Sept 2025)                                                                                      |
| Core GC in wasmtime | Feature-complete (v27.0+)                                                                                            |
| CM GC pre-proposal  | [component-model#525](https://github.com/WebAssembly/component-model/issues/525) (June 2025, open)                   |
| CM GC in wasmtime   | [wasmtime#10325](https://github.com/bytecodealliance/wasmtime/issues/10325) (prototyping, 🛸 flag "very incomplete") |

## Phase 1: Align Internal Representations

Before the GC canonical ABI exists, prepare Wado's internal GC types to be compatible.

### 1.1 String: Already Aligned

Wado's `String` is `(ref (array (mut i8)))` — UTF-8 bytes in a mutable i8 array. The CM GC proposal maps `string` to `(ref null? (array (mut? i8)))` for UTF-8. Structurally identical modulo nullability.

**Action:** None needed. Wado's string type naturally matches.

### 1.2 Records: Field Order and Storage Types

Wado structs compile to `(ref (struct (field ...) ...))`. The CM GC proposal maps `record` to the same. The key difference is **storage types**: the proposal uses packed types (`i8`, `i16`) for small integers in struct fields, while Wado currently uses `i32` for all integer fields.

**Action:** When emitting CM-boundary struct types, use packed storage types (`i8` for bool/u8/s8, `i16` for u16/s16). This only affects types that appear in WASI function signatures, not internal Wado types.

### 1.3 Arrays / Lists: Already Aligned

Wado's `List<T>` is `(ref (array (mut T')))`. The proposal maps `list<T>` to the same structure.

**Action:** None needed.

### 1.4 Variants: SubtypeHierarchy at CM Boundary

The proposal requires **subtype hierarchies** for all sum types (`variant`, `option`, `result`): an empty base struct with per-case subtypes, all in the same rec group. This differs from Wado's internal NullableRef optimization for 2-case variants.

Wado already uses SubtypeHierarchy for multi-case variants and `Result`. The gap is **NullableRef variants** (`Option<T>`, 2-case variants with one unit case) which use `(ref null $T)` internally.

**Action:** At CM boundaries, NullableRef variants must be converted to SubtypeHierarchy. Two options:

- **(a) Convert at the boundary:** The CM binding adapter wraps/unwraps NullableRef into SubtypeHierarchy. Keeps internal optimization, adds a small conversion cost.
- **(b) Use SubtypeHierarchy everywhere:** Simplifies CM boundary but regresses internal code (extra `ref.test`/`ref.cast` vs simple `ref.is_null`).

**Decision:** Option (a) — convert at the boundary. The NullableRef optimization is worth keeping for hot paths within a component. The conversion cost at CM boundaries is dominated by the function call overhead anyway.

### 1.5 Rec Group Strategy

Core Wasm deduplicates types structurally at rec group granularity. Types in different rec groups are distinct even if structurally identical, forcing copies.

For zero-copy WASI calls, Wado's CM-boundary types must live in a rec group that **the host recognizes**. In practice, the host (wasmtime) will define canonical rec groups for WASI types, and Wado must emit matching ones.

**Action:** Introduce a "CM type registry" in the compiler that tracks which GC types are used at CM boundaries and groups them into rec groups following the canonical layout. This is a `wir_build` concern — emit a dedicated rec group for CM-boundary types separate from internal types.

### 1.6 Mutability Decision

Wado uses mutable fields internally (struct fields are mutable via `&mut self`, arrays support `.push()`). The proposal warns that mutable fields force copies.

**Decision:** Use **mutable** fields in the CM GC lowering. Rationale:

- Wado's "at rest" representation is mutable. Using immutable CM types would force a copy on every import (immutable→mutable) and every export (mutable→immutable) — no better than linear memory.
- When both Wado components communicate, both-mutable means 2 copies, but this is a rare case. Most Wado CM calls are to WASI (host), where the host can handle mutable refs efficiently.
- If the host provides immutable refs, Wado takes one copy on import — same as the host providing a linear memory buffer.

## Phase 2: Dual-Mode Binding Synthesis

### 2.1 CM Binding Synthesis (`cm_binding.rs`)

Add a `CmMode` enum:

```rust
enum CmMode {
    LinearMemory,  // current behavior
    Gc,            // new: pass GC refs directly
}
```

The synthesizer already dispatches on type shape. In GC mode, most composite types become **identity** (pass through directly) instead of lower/lift through linear memory:

| Type       | Linear Memory Mode                         | GC Mode                                            |
| ---------- | ------------------------------------------ | -------------------------------------------------- |
| Scalars    | identity                                   | identity                                           |
| `String`   | `cm_lower_string` / `memory_to_gc_string`  | **identity** (same GC array type)                  |
| `List<u8>` | `cm_lower_array_u8` / `memory_to_gc_array` | **identity**                                       |
| Records    | recursive field-by-field copy via LM       | **identity or shallow copy** (if rec groups match) |
| Variants   | discriminant + payload via LM              | **ref.cast + unwrap** or identity                  |
| Resources  | handle table (i32)                         | `externref` (unchanged)                            |

For most types, GC mode synthesis is **dramatically simpler** than linear memory mode — many types need no adapter at all.

### 2.2 Component Builder (`codegen/component.rs`)

Currently, `lower_func` and `lift_func` emit:

```rust
[CanonicalOption::Memory(ctx.memory_idx()),
 CanonicalOption::Realloc(ctx.core_func_idx("realloc"))]
```

In GC mode, replace with:

```rust
[CanonicalOption::Gc,
 CanonicalOption::CoreType(core_type_idx)]
```

This requires `wasm_encoder` to support the new canonical options. Track `wasm-tools` for this.

### 2.3 Memory Module

In GC mode, the memory module (`mem`) is **still needed** for:

- `stream.read` / `stream.write` (byte streams use linear memory even in GC mode)
- `error-context` (uses memory for debug strings)
- Any fallback to linear memory mode

But `realloc` is no longer needed for regular WASI function calls. The memory module can be simplified.

### 2.4 Feature Flag

Add `--cm-gc` flag to `wado compile` / `wado run`:

```sh
wado compile --cm-gc -o out.wasm file.wado   # use GC canonical ABI
wado compile -o out.wasm file.wado            # default: linear memory (current)
```

Switch the default to GC mode once wasmtime enables the 🛸 flag by default.

## Phase 3: Switch Default and Clean Up

Once the CM GC spec is merged and wasmtime enables it by default:

1. Make `--cm-gc` the default
2. Keep `--cm-linear-memory` as a fallback for runtimes without GC-in-CM support
3. Remove internal helper functions that are only needed for linear memory CM binding (`cm_lower_string`, `memory_to_gc_string`, `cm_lower_array_u8`, `memory_to_gc_array` in `internal.wado`)
4. Simplify the memory module (no `realloc` needed for most components)

## Performance Impact

### Expected Gains

The biggest wins are for data-heavy WASI calls:

| Operation             | Current (LM)       | After (GC)    | Improvement |
| --------------------- | ------------------ | ------------- | ----------- |
| `String` export       | O(n) copy GC→LM    | O(1) ref pass | major       |
| `String` import       | O(n) copy LM→GC    | O(1) ref pass | major       |
| `List<u8>` round-trip | 2× O(n) copies     | 0 copies      | major       |
| `i32` / scalar        | identity           | identity      | none        |
| Record with 3 fields  | 3× store + 3× load | O(1) ref pass | moderate    |

For HTTP handlers processing request/response bodies, this eliminates the dominant cost of crossing the CM boundary.

### Unchanged

- Intra-component performance (no CM boundary involved)
- Resource handle passing (already efficient via i32/externref)
- Stream/future operations (still use linear memory for byte buffers)

## Consequences

### Positive

- Eliminates O(n) copies for strings, arrays, and records at CM boundaries
- Simplifies CM binding synthesis (most types become identity)
- Reduces linear memory pressure (no temporary buffers for CM calls)
- Natural fit for Wado's GC-based architecture

### Negative

- Dual-mode synthesis increases compiler complexity until linear memory mode is removed
- Rec group alignment requires careful coordination with the host
- Mutability choice (mutable fields) may prevent zero-copy in some multi-component scenarios
- Depends on upstream spec and wasmtime progress (not under Wado's control)

### Risks

- The pre-proposal may change significantly before merging. Phase 1 (alignment) is safe regardless; Phase 2 (dual-mode) should wait for spec stabilization.
- `wasm_encoder` may not support `CanonicalOption::Gc` / `CanonicalOption::CoreType` until wasmtime ships the feature.

## References

- [Pre-Proposal: Wasm GC Support in the Canonical ABI (component-model#525)](https://github.com/WebAssembly/component-model/issues/525)
- [Prototype Wasm GC and CM canonical ABI support (wasmtime#10325)](https://github.com/bytecodealliance/wasmtime/issues/10325)
- [Implement the WebAssembly GC Proposal (wasmtime#5032)](https://github.com/bytecodealliance/wasmtime/issues/5032)
- [Wasm GC Proposal Overview (archived)](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md)
- [Wasm 3.0 (GC shipped)](https://webassembly.org/news/2025-09-17-wasm-3.0/)
- [Wasmtime 27.0: Complete Wasm GC support](https://bytecodealliance.org/articles/wasmtime-27.0)
- [Bytecode Alliance RFC: Wasm GC in Wasmtime](https://github.com/bytecodealliance/rfcs/blob/main/accepted/wasm-gc.md)
- [Component Model Explainer](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Explainer.md)
- [Component Model Canonical ABI](https://github.com/WebAssembly/component-model/blob/main/design/mvp/CanonicalABI.md)
- [Component Model Linking](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Linking.md)
- [Shared-Everything Threads Proposal](https://github.com/WebAssembly/shared-everything-threads/blob/main/proposals/shared-everything-threads/Overview.md)
