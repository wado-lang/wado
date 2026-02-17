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

| File | Description |
| ---- | ----------- |
| `wir.rs` | WIR data structures: `WirModule`, `WirTypeDef`, `WirType`, `WirInstr`, `WirTypeId`, `WirFuncId`, `WirName`, etc. |
| `wir_unparse.rs` | WIR → pseudo-Wado rendering for `wado dump --wir --unparse` |
| `tir_to_wir/mod.rs` | Pipeline entry: `compile_with_wir(&Project) -> Vec<u8>` — orchestrates build → emit → validate → component wrapping |
| `tir_to_wir/context.rs` | `WirContext` — builder that accumulates types, functions, and module-level entries during translation |
| `tir_to_wir/types.rs` | Type registration: translates TIR type definitions to `Vec<WirTypeDef>` with multi-phase topological sorting |
| `tir_to_wir/functions.rs` | Function collection: gathers imports, entry/library functions, methods, data segments, exports |
| `tir_to_wir/translate.rs` | Function body translation: converts TIR expressions/statements to `WirInstr` trees |
| `tir_to_wir/emit.rs` | `WirEmitter`: converts `WirModule` to core Wasm bytes via `wasm_encoder` |
| `tir_to_wir/component.rs` | Component Model wrapping: delegates to `Codegen::build_component_from_core_module()` |

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
struct Point { x: i32, y: i32 }  // from ./geometry.wado

variant Shape {  // from ./shapes.wado
    Circle(f64),
    Point,
}

enum Color { Red = 0, Green = 1, Blue = 2 }  // from ./colors.wado

fn "Point::sum"(self: ref Point) -> i32 {  // from ./geometry.wado
    return i32.add(self.x, self.y);
}
```

Principles:

- Type definitions use Wado syntax (`struct`, `variant`, `enum`), not Wasm GC syntax
- Field access uses `self.x`, not `struct.get Point.x(self)`
- Instructions use WAT-style mnemonics (`i32.add`, `f64.mul`)
- Wado-level types in signatures (`bool`, `char`, `u8`, not `i32`)

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

Created `compile_with_wir(&Project) -> Vec<u8>` in `tir_to_wir/mod.rs` with `CompilerOptions.use_wir_backend` flag. Created parallel test harnesses:

- `tests/wir_e2e.rs` — runs individual fixtures through the WIR pipeline (gated by `WADO_WIR_TEST=1`)
- `tests/wir_progress.rs` — runs all fixtures and reports aggregate pass/fail counts

### Phase 3: Core Translation (in progress)

The main implementation work. Build `tir_to_wir` and `wir_emit` incrementally.

#### Step 3a: Type Registration

- [x] Struct types (fields, mutability, GC layout)
- [x] Variant types (base struct + case subtypes)
- [x] Enum types (i32 discriminant, no Wasm type entry)
- [ ] Flags types (i32 bitfield, no Wasm type entry)
- [x] Array types (element type, mutability)
- [x] Tuple types (anonymous structs)
- [ ] Closure types (funcref + captured environment struct)
- [x] Function types
- [x] Rec groups and topological sorting

#### Step 3b: Module Skeleton

- [x] Import section (WASI functions, bundled modules)
- [x] Global variables
- [x] Data section (string literals)
- [x] Element section (funcref tables)
- [x] Export section
- [x] Name section

#### Step 3c: Function Bodies — Basics

- [x] Constants (i32, i64, f32, f64)
- [x] Local variables (get, set, tee, declare)
- [x] Arithmetic (i32, i64, f32, f64 — all operators)
- [x] Comparison and logical operators
- [x] Type casts and conversions
- [x] Block, Loop, If/Else, Br, BrIf, BrTable
- [x] Return, Unreachable, Nop, Drop
- [x] Function calls (direct)

#### Step 3d: Function Bodies — GC and Compound

- [x] Struct construction (struct.new)
- [x] Field access (struct.get) and assignment (struct.set)
- [x] Array operations (new, get, set, len, copy, fill)
- [x] Reference operations (ref.null, ref.test, ref.cast, ref.eq)
- [ ] Value copy (ValueCopy compound instruction)
- [x] Match expressions (pattern dispatch, br_table)
- [ ] Closure creation and call_ref
- [x] Global get/set

#### Step 3e: Function Bodies — WASI and CM

If [TIR-Level CM Adapter Synthesis](./wep-2026-02-15-cm-adapter-synthesis.md) is completed before this step, CM adapter functions are already ordinary TIR functions — `tir_to_wir` handles them like any other function. Only the raw CM call builtins (`builtin::cm_raw_call__*`) need WIR-level support.

Without CM adapter synthesis:

- [ ] CM effect calls (canonical lift/lower)
- [ ] CM resource method calls
- [ ] CM payload lowering (string, list, record, variant, option, result)
- [ ] CM export glue functions
- [ ] Async CM (subtask handling, waitable sets)

#### Step 3f: Component Model Wrapper

- [x] WASI interface imports
- [x] Bundled module instantiation (fts, libm)
- [x] Core module + component composition
- [x] World export declarations

### Phase 4: Cutover

Once all E2E tests pass via the WIR pipeline:

- [ ] Verify behavioral equivalence across all fixtures and optimization levels
- [ ] Replace `Codegen::generate_wasm` calls with `compile_with_wir`
- [ ] Delete `codegen.rs` and `copy_context.rs`
- [ ] Promote `tests/wir_e2e.rs` to primary E2E test (or merge with `e2e.rs`)
- [ ] Merge `wasm_plan` into `tir_to_wir`. Pipeline becomes: `optimize → tir_to_wir → wir_emit`

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
  Passed:   251 / 651  (38.6%)
  Failed:   400
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
