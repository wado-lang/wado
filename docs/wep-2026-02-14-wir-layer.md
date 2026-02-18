# WEP: Wasm IR (WIR) Layer

## Context

`codegen.rs` is 14,000+ lines and mixes three concerns:

1. **Type layout decisions**: Mapping TIR types to Wasm GC types, registering struct/variant/array/tuple/closure types across 15+ phases with topological sorting and deferred registration
2. **Function-level analysis**: Pre-allocating locals, scalarization analysis, scratch local computation, copy context setup
3. **Instruction emission**: Translating TIR expressions to Wasm instructions

The existing `wasm_plan` phase (WEP 2026-02-03) moved Component Model analysis out of codegen, but the core problem remains: codegen is doing extensive TIR-to-Wasm translation and analysis that is tangled with low-level `wasm_encoder` API calls. There is no inspectable intermediate form between TIR and binary.

A complementary approach — [TIR-Level CM Adapter Synthesis](./wep-2026-02-15-cm-adapter-synthesis.md) — can reduce the CM-specific surface area of codegen independently.

## Decision

Introduce **WIR (Wasm IR)** — a tree-structured intermediate representation between TIR and Wasm binary. WIR is close to Wasm semantics but retains enough high-level information to be readable and debuggable.

### New Pipeline

```
lower → optimize → wasm_plan → tir_to_wir → wir_emit → wasm binary
                                    ↓
                                WirModule (inspectable via dump --wir)
```

`tir_to_wir` translates the optimized Project into a `WirModule`. `wir_emit` translates `WirModule` into Wasm binary bytes. `wasm_plan` remains unchanged and provides `ComponentPlan` metadata consumed by `tir_to_wir`.

### What WIR Is

WIR is a tree-structured IR that maps almost 1:1 to Wasm instructions, but with these ergonomic improvements over raw Wasm:

1. **Named locals**: Variables are referenced by name, not pre-allocated indices. Locals can be declared inline via `DeclareLocal` — the emit phase collects them and pre-allocates as Wasm locals.
2. **Named types**: Struct, variant, enum, and flags types retain their source-level names and field/case names. These are Wado-level type definitions — the emit phase expands them (e.g., a variant becomes N+1 Wasm struct types).
3. **Wado-level value types**: WIR uses `Bool`, `Char`, `I8`, `U8`, etc. instead of Wasm's `i32`-for-everything. The emit phase lowers to Wasm `ValType`/`StorageType`.
4. **Structured control flow**: Blocks, loops, if/else are tree nodes (not flat instruction sequences with labels).
5. **Explicit value copy**: Copy operations are explicit `ValueCopy` nodes rather than inline instruction sequences.
6. **TIR metadata preserved**: Module source, source spans, attributes, generic instantiation info, and newtype origin are carried through for debugging and unparse.
7. **Unparse support**: WIR can be rendered as pseudo-Wado for inspection via `wado dump --wir --unparse`.

### What WIR Is Not

- **Not a CFG**: WIR preserves Wasm's structured control flow (block/loop/if), not a control-flow graph.
- **Not a semantic optimization target**: Semantic optimizations (inlining, SROA, reference elimination) happen on TIR. WIR may host low-level Wasm optimizations (constant folding, LICM, peephole) in the future.
- **Not an abstraction over Wasm versions**: WIR targets specific Wasm features (GC, Component Model).

## Implementation

### Source Files

| File                      | Description                                                                                                         |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `wir.rs`                  | WIR data structures: `WirModule`, `WirTypeDef`, `WirType`, `WirInstr`, `WirTypeId`, `WirFuncId`, `WirName`, etc.    |
| `wir_unparse.rs`          | WIR → pseudo-Wado rendering for `wado dump --wir --unparse`                                                         |
| `tir_to_wir/mod.rs`       | Pipeline entry: `compile_with_wir(&Project) -> Vec<u8>` — orchestrates build → emit → validate → component wrapping |
| `tir_to_wir/context.rs`   | `WirContext` — builder that accumulates types, functions, and module-level entries during translation               |
| `tir_to_wir/types.rs`     | Type registration: translates TIR type definitions to `Vec<WirTypeDef>` with multi-phase topological sorting        |
| `tir_to_wir/functions.rs` | Function collection: gathers imports, entry/library functions, methods, data segments, exports                      |
| `tir_to_wir/translate.rs` | Function body translation: converts TIR expressions/statements to `WirInstr` trees                                  |
| `tir_to_wir/emit.rs`      | `WirEmitter`: converts `WirModule` to core Wasm bytes via `wasm_encoder`                                            |
| `tir_to_wir/component.rs` | Component Model wrapping: delegates to `Codegen::build_component_from_core_module()`                                |

### Key Design Decisions

#### `WirTypeId` / `WirFuncId` with `Rc<str>`

WIR instructions reference types and functions via lightweight IDs instead of embedding names in every instruction. `WirTypeId { index: u32, fq: Rc<str> }` gives O(1) `Eq`/`Hash` via integer comparison, O(1) `Clone` via Rc refcount, and readable `Debug` output via the fq name. Definitions use `WirName { display, fq }` with both short and fully-qualified names.

#### Wado-Level Value Types

WIR uses `Bool`, `Char`, `I8`, `U8`, `I16`, `U16`, `Enum { type_id }`, etc. instead of Wasm's `i32`. The emit phase lowers to `ValType` (locals) or packed `StorageType` (struct fields) depending on context. This eliminates the `ValType`/`StorageType` split at the WIR level and makes unparse output readable.

#### `ValueCopy` as Compound Instruction

Value copy involves complex dispatching (struct copy, array loop, variant discriminant check). A single `ValueCopy { type_id, source_type, expr }` node preserves semantic intent and lets the emitter choose the lowering strategy.

#### Tree-Structured Instructions

WIR uses trees where operands are children (not stack values). `I32Add(StructGet { ... }, StructGet { ... })` is inspectable and debuggable. Flattening to stack-machine instructions is trivial (post-order traversal).

## Unparse Format

`wado dump --wir --unparse` outputs pseudo-Wado:

```
struct Point { mut x: i32, mut y: i32 }

fn "run"() with Stdout {
    let p: ref null Point;
    p = Point { x: 10, y: 20 };
    call core:cli/println(block -> ref null String {
        ...
    });
}

fn "core:cli/println"(message) with Stdout {  // from core:cli
    ...
}
```

Principles:

- Type definitions use Wado syntax (`struct`, `variant`, `enum`), not Wasm GC syntax
- Field access uses `self.x`, not `struct.get Point.x(self)`
- Struct construction uses `Type { field: value }` syntax
- Instructions use WAT-style mnemonics (`i32.add`, `f64.mul`)
- Wado-level types in signatures (`bool`, `char`, `u8`, not `i32`)
- Names are shortened: entry-point items show just the name, prelude types like `String` omit module path
- TIR variable names are preserved in WIR locals

## Migration Plan

### Strategy: Strangler Fig Pattern

The migration builds a complete WIR pipeline **alongside** the existing codegen, without modifying `codegen.rs`. Both pipelines consume the same `&Project` after `wasm_plan`. Once all tests pass, the old codegen is replaced and deleted.

```
                                  ┌→ codegen → wasm binary (existing, untouched)
lower → optimize → wasm_plan ─────┤
                                  └→ tir_to_wir → wir_emit → wasm binary (new, tested in parallel)
```

Benefits of this approach:

- `codegen.rs` is never modified — all existing tests always pass
- `codegen.rs` serves as the living reference throughout development
- Each feature added to `tir_to_wir` makes more WIR E2E tests pass
- `wado dump --wir --unparse` provides visibility at all times

### Phase 1: Scaffolding and Inspection (complete)

Created `wir.rs` (data structures), `wir_unparse.rs` (pseudo-Wado output), and `--wir` flag for the dump command.

### Phase 2: Parallel E2E Test Infrastructure (complete)

Created `compile_with_wir(&Project) -> Vec<u8>` in `tir_to_wir/mod.rs`. Created parallel test harnesses (`wir_e2e.rs`, `wir_progress.rs`) gated by `WADO_WIR_TEST=1` to track incremental progress. Both harnesses were removed after cutover; `tests/e2e.rs` is now the sole E2E test.

### Phase 3: Core Translation (complete)

All translation steps implemented: type registration (structs, variants, enums, flags, arrays, tuples, closures, function types, rec groups), module skeleton (imports, globals, data, elements, exports, name section), function body translation (constants, locals, arithmetic, comparisons, casts, control flow, calls, GC ops, value copy, match, closures, globals), and Component Model wrapper (WASI imports, bundled modules, world exports). CM adapter functions are handled as ordinary TIR functions via [TIR-Level CM Adapter Synthesis](./wep-2026-02-15-cm-adapter-synthesis.md).

### Phase 4: Cutover (complete)

- [x] Verified behavioral equivalence: all 3380 E2E tests pass via the WIR pipeline
- [x] Replaced `Codegen::generate_wasm` with `compile_with_wir`
- [x] Deleted `codegen.rs`
- [x] Promoted `tests/e2e.rs` as the sole E2E test (removed `wir_e2e.rs` and `wir_progress.rs`)

Remaining cleanup:

- [ ] Remove debug file writes (`/tmp/wir_debug_core.wat`) from `tir_to_wir/mod.rs`
- [ ] Merge `wasm_plan` into `tir_to_wir` — currently still called as a separate pipeline phase from `lib.rs`. Pipeline becomes: `optimize → tir_to_wir → wir_emit`
- [ ] Delete `copy_context.rs` — only used by `optimize_rewrite.rs` for `needed_copy_types` expansion

### Phase 5 (Future): Optimizer Migration

After WIR is stable, split optimizations into two levels:

```
lower → tir_optimize → tir_to_wir → wir_optimize → wir_emit
```

- `tir_optimize`: Semantic optimizations (inlining, DCE, SROA, ref-elim, copy-prop) on TIR with full TypeTable access
- `wir_optimize`: Wasm-level optimizations (constant folding, LICM, peephole) on WIR where types are embedded in instruction names
- Move ValueCopy analysis from `optimize_rewrite.rs` into `tir_to_wir`
- Move `CopyContext` (scratch local pre-allocation) into `wir_emit`

## Development Workflow

### Checking Progress

`tests/wir_progress.rs` runs all E2E fixtures through the WIR pipeline and reports aggregate pass/fail counts. It is gated by `WADO_WIR_TEST=1` and always succeeds (informational only).

```sh
WADO_WIR_TEST=1 cargo test -p wado-compiler --test wir_progress -- --nocapture 2>&1 | grep -E '═|Passed|Failed|TODO'
```

This outputs a summary like:

```
═══════════════════════════════════════════════════════
  WIR Pipeline Progress (O2)
═══════════════════════════════════════════════════════
  Passed:   481 / 671  (71.7%)
  Failed:   190
═══════════════════════════════════════════════════════
```

### Running Individual Fixtures

```sh
WADO_WIR_TEST=1 cargo test -p wado-compiler --test wir_e2e -- hello_world
```

### Debugging with WIR Unparse

```sh
cargo run --bin wado -- dump --wir --unparse file.wado
```

This shows the full `WirModule` as pseudo-Wado, allowing inspection of the planned Wasm output before binary emission. Use this to diagnose type registration issues, incorrect instruction translation, or missing functions — rather than relying solely on E2E test pass/fail.

## Consequences

### Benefits

- **Debuggability**: `wado dump --wir --unparse` shows exactly what Wasm will be generated, with readable names
- **Testability**: WIR generation and WIR emission can be tested independently
- **Maintainability**: codegen.rs (14k lines) splits into focused modules
- **Extensibility**: New Wasm features (SIMD, stack switching) are added as WIR nodes, not interleaved with encoding logic

### Trade-offs

- **Additional IR**: One more representation to maintain. Mitigated by WIR being close to Wasm (not a novel abstraction).
- **Memory**: WIR trees allocate more than flat instruction streams. Acceptable since Wado programs are not extremely large.
- **Two code paths during migration**: The strangler fig approach intentionally maintains two pipelines until cutover. Safe because the existing pipeline is never modified. Risk of stalling is mitigated by progress tracking.
