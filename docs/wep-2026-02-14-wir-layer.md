# WEP: Wasm IR (WIR) Layer

## Context

`codegen.rs` is 14,000+ lines and mixes three concerns:

1. **Type layout decisions**: Mapping TIR types to Wasm GC types, registering struct/variant/array/tuple/closure types across 15+ phases with topological sorting and deferred registration
2. **Function-level analysis**: Pre-allocating locals, scalarization analysis, scratch local computation, copy context setup
3. **Instruction emission**: Translating TIR expressions to Wasm instructions

The existing `wasm_plan` phase (WEP 2026-02-03) moved Component Model analysis out of codegen, but the core problem remains: codegen is doing extensive TIR-to-Wasm translation and analysis that is tangled with low-level `wasm_encoder` API calls. This makes it hard to:

- Debug the compiler (no way to inspect the "planned" Wasm output before encoding)
- Test codegen logic (analysis and emission are inseparable)
- Add optimizations at the Wasm level (peephole, register allocation)
- Understand what the compiler is doing (TIR → binary is a black box)

### Current Pipeline

```
lower → optimize → wasm_plan → codegen → wasm binary
                       ↓
                   ComponentPlan
                   CmExportInfo
```

`codegen` receives TIR + metadata and produces Wasm bytes in one monolithic pass. There is no inspectable intermediate form between TIR and binary.

## Decision

Introduce **WIR (Wasm IR)** — a tree-structured intermediate representation between TIR and Wasm binary. WIR is close to Wasm semantics but retains enough high-level information to be readable and debuggable.

### New Pipeline

```
lower → optimize → wasm_plan → codegen(emit) → wasm binary
                       ↓
                   WirModule
```

The `wasm_plan` phase is expanded to produce a `WirModule` — a complete description of the Wasm module in WIR form. Codegen becomes a mechanical WIR → Wasm binary translation.

### What WIR Is

WIR is a tree-structured IR that maps almost 1:1 to Wasm instructions, but with these ergonomic improvements over raw Wasm:

1. **Named locals**: Variables are referenced by name, not pre-allocated indices. Locals can be declared inline (no pre-allocation pass needed).
2. **Named types**: Struct, enum, and variant types retain their source-level names and field/case names.
3. **Structured control flow**: Blocks, loops, if/else are tree nodes (not flat instruction sequences with labels).
4. **Explicit value copy**: Copy operations are explicit WIR nodes rather than inline instruction sequences.
5. **Unparse support**: WIR can be rendered as pseudo-Wado for inspection via `wado dump --wir --unparse`.

### What WIR Is Not

- **Not a CFG**: WIR preserves Wasm's structured control flow (block/loop/if), not a control-flow graph.
- **Not an optimization target**: WIR is a lowering target. Optimizations happen on TIR before WIR generation.
- **Not an abstraction over Wasm versions**: WIR targets specific Wasm features (GC, Component Model). It does not abstract away Wasm details.

## WIR Data Structures

### Module Level

```rust
/// A complete Wasm module in WIR form.
/// Contains all information needed to emit a valid Wasm binary.
pub struct WirModule {
    /// Type section: all Wasm GC types (structs, arrays, function types)
    pub types: Vec<WirTypeDef>,
    /// Rec groups: which types form recursive groups
    pub rec_groups: Vec<WirRecGroup>,
    /// Import section
    pub imports: Vec<WirImport>,
    /// Function declarations with bodies
    pub functions: Vec<WirFunction>,
    /// Global variables
    pub globals: Vec<WirGlobal>,
    /// Export section
    pub exports: Vec<WirExport>,
    /// Element section (for funcref tables)
    pub elements: Vec<WirElement>,
    /// Data section (string literals, etc.)
    pub data: Vec<WirData>,
    /// Branch hints (from likely/unlikely)
    pub branch_hints: Vec<WirBranchHint>,
    /// Name section entries
    pub names: WirNames,
    /// Component Model wrapper info
    pub component: WirComponent,
}
```

### Type Definitions

```rust
/// A Wasm GC type definition.
pub enum WirTypeDef {
    /// Struct type with named fields
    Struct(WirStructType),
    /// Array type
    Array(WirArrayType),
    /// Function type
    Func(WirFuncType),
}

pub struct WirStructType {
    /// Source-level name (e.g., "Point", "Array<i32>", "__Closure_3")
    pub name: String,
    /// Fields with names and types
    pub fields: Vec<WirField>,
    /// Supertype index (for variant subtypes)
    pub supertype: Option<WirTypeRef>,
    /// Whether this is a final type (no further subtypes allowed)
    pub is_final: bool,
}

pub struct WirField {
    /// Source-level field name (e.g., "x", "repr", "tag")
    pub name: String,
    /// Wasm storage type
    pub storage_type: WirStorageType,
    /// Whether this field is mutable
    pub mutable: bool,
}

pub struct WirArrayType {
    pub name: String,
    pub element_type: WirStorageType,
    pub mutable: bool,
}

pub struct WirFuncType {
    pub name: String,
    pub params: Vec<WirValType>,
    pub results: Vec<WirValType>,
}

/// Reference to a type by name (resolved to index during emission)
pub struct WirTypeRef(pub String);
```

### Value Types

```rust
/// Wasm value type — mirrors wasm_encoder::ValType but with named type refs.
pub enum WirValType {
    I32,
    I64,
    F32,
    F64,
    /// Reference to a named GC type
    Ref { type_name: String, nullable: bool },
    /// Abstract reference type (anyref, funcref, etc.)
    AbstractRef { heap_type: WirAbstractHeapType, nullable: bool },
}

pub enum WirStorageType {
    Val(WirValType),
    I8,
    I16,
}

pub enum WirAbstractHeapType {
    Any,
    Eq,
    Struct,
    Array,
    Func,
    None,
    NoFunc,
    Extern,
}
```

### Functions

```rust
pub struct WirFunction {
    /// Function name (for name section and local references)
    pub name: String,
    /// Type reference (function signature)
    pub type_ref: WirTypeRef,
    /// Parameters (named)
    pub params: Vec<WirLocal>,
    /// Body (None for imported functions)
    pub body: Option<WirBody>,
}

pub struct WirBody {
    /// Locals declared in the body (in declaration order)
    /// Unlike raw Wasm, locals can be declared inline at first use.
    pub locals: Vec<WirLocal>,
    /// Function body instructions
    pub code: Vec<WirInstr>,
}

pub struct WirLocal {
    pub name: String,
    pub ty: WirValType,
}
```

### Instructions

WIR instructions are tree-structured where operands are child nodes, not stack values. This makes the structure inspectable and allows inline local declaration.

```rust
pub enum WirInstr {
    // === Locals ===
    /// Declare a new local variable inline (not in Wasm; lowered to pre-allocated local)
    DeclareLocal { name: String, ty: WirValType },
    /// local.get by name
    LocalGet { name: String },
    /// local.set by name
    LocalSet { name: String, value: Box<WirInstr> },
    /// local.tee by name
    LocalTee { name: String, value: Box<WirInstr> },

    // === Globals ===
    GlobalGet { index: u32 },
    GlobalSet { index: u32, value: Box<WirInstr> },

    // === Constants ===
    I32Const(i32),
    I64Const(i64),
    F32Const(f32),
    F64Const(f64),

    // === Arithmetic (i32) ===
    I32Add(Box<WirInstr>, Box<WirInstr>),
    I32Sub(Box<WirInstr>, Box<WirInstr>),
    I32Mul(Box<WirInstr>, Box<WirInstr>),
    I32DivS(Box<WirInstr>, Box<WirInstr>),
    I32DivU(Box<WirInstr>, Box<WirInstr>),
    I32RemS(Box<WirInstr>, Box<WirInstr>),
    I32RemU(Box<WirInstr>, Box<WirInstr>),
    I32And(Box<WirInstr>, Box<WirInstr>),
    I32Or(Box<WirInstr>, Box<WirInstr>),
    I32Xor(Box<WirInstr>, Box<WirInstr>),
    I32Shl(Box<WirInstr>, Box<WirInstr>),
    I32ShrS(Box<WirInstr>, Box<WirInstr>),
    I32ShrU(Box<WirInstr>, Box<WirInstr>),
    I32Eqz(Box<WirInstr>),
    I32Eq(Box<WirInstr>, Box<WirInstr>),
    I32Ne(Box<WirInstr>, Box<WirInstr>),
    I32LtS(Box<WirInstr>, Box<WirInstr>),
    I32LtU(Box<WirInstr>, Box<WirInstr>),
    I32GtS(Box<WirInstr>, Box<WirInstr>),
    I32GtU(Box<WirInstr>, Box<WirInstr>),
    I32LeS(Box<WirInstr>, Box<WirInstr>),
    I32LeU(Box<WirInstr>, Box<WirInstr>),
    I32GeS(Box<WirInstr>, Box<WirInstr>),
    I32GeU(Box<WirInstr>, Box<WirInstr>),
    I32WrapI64(Box<WirInstr>),
    I32Clz(Box<WirInstr>),
    I32Ctz(Box<WirInstr>),
    I32Popcnt(Box<WirInstr>),
    I32TruncF64S(Box<WirInstr>),
    I32TruncF64U(Box<WirInstr>),
    I32TruncF32S(Box<WirInstr>),
    I32TruncF32U(Box<WirInstr>),
    I32ReinterpretF32(Box<WirInstr>),
    I32Extend8S(Box<WirInstr>),
    I32Extend16S(Box<WirInstr>),

    // === Arithmetic (i64) ===
    I64Add(Box<WirInstr>, Box<WirInstr>),
    I64Sub(Box<WirInstr>, Box<WirInstr>),
    I64Mul(Box<WirInstr>, Box<WirInstr>),
    I64DivS(Box<WirInstr>, Box<WirInstr>),
    I64DivU(Box<WirInstr>, Box<WirInstr>),
    I64RemS(Box<WirInstr>, Box<WirInstr>),
    I64RemU(Box<WirInstr>, Box<WirInstr>),
    I64And(Box<WirInstr>, Box<WirInstr>),
    I64Or(Box<WirInstr>, Box<WirInstr>),
    I64Xor(Box<WirInstr>, Box<WirInstr>),
    I64Shl(Box<WirInstr>, Box<WirInstr>),
    I64ShrS(Box<WirInstr>, Box<WirInstr>),
    I64ShrU(Box<WirInstr>, Box<WirInstr>),
    I64Eqz(Box<WirInstr>),
    I64Eq(Box<WirInstr>, Box<WirInstr>),
    I64Ne(Box<WirInstr>, Box<WirInstr>),
    I64LtS(Box<WirInstr>, Box<WirInstr>),
    I64LtU(Box<WirInstr>, Box<WirInstr>),
    I64GtS(Box<WirInstr>, Box<WirInstr>),
    I64GtU(Box<WirInstr>, Box<WirInstr>),
    I64LeS(Box<WirInstr>, Box<WirInstr>),
    I64LeU(Box<WirInstr>, Box<WirInstr>),
    I64GeS(Box<WirInstr>, Box<WirInstr>),
    I64GeU(Box<WirInstr>, Box<WirInstr>),
    I64ExtendI32S(Box<WirInstr>),
    I64ExtendI32U(Box<WirInstr>),
    I64Clz(Box<WirInstr>),
    I64Ctz(Box<WirInstr>),
    I64Popcnt(Box<WirInstr>),
    I64TruncF64S(Box<WirInstr>),
    I64TruncF64U(Box<WirInstr>),
    I64TruncF32S(Box<WirInstr>),
    I64TruncF32U(Box<WirInstr>),
    I64ReinterpretF64(Box<WirInstr>),

    // === Arithmetic (f32) ===
    F32Add(Box<WirInstr>, Box<WirInstr>),
    F32Sub(Box<WirInstr>, Box<WirInstr>),
    F32Mul(Box<WirInstr>, Box<WirInstr>),
    F32Div(Box<WirInstr>, Box<WirInstr>),
    F32Neg(Box<WirInstr>),
    F32Abs(Box<WirInstr>),
    F32Ceil(Box<WirInstr>),
    F32Floor(Box<WirInstr>),
    F32Trunc(Box<WirInstr>),
    F32Nearest(Box<WirInstr>),
    F32Sqrt(Box<WirInstr>),
    F32Min(Box<WirInstr>, Box<WirInstr>),
    F32Max(Box<WirInstr>, Box<WirInstr>),
    F32Copysign(Box<WirInstr>, Box<WirInstr>),
    F32Eq(Box<WirInstr>, Box<WirInstr>),
    F32Ne(Box<WirInstr>, Box<WirInstr>),
    F32Lt(Box<WirInstr>, Box<WirInstr>),
    F32Gt(Box<WirInstr>, Box<WirInstr>),
    F32Le(Box<WirInstr>, Box<WirInstr>),
    F32Ge(Box<WirInstr>, Box<WirInstr>),
    F32ConvertI32S(Box<WirInstr>),
    F32ConvertI32U(Box<WirInstr>),
    F32ConvertI64S(Box<WirInstr>),
    F32ConvertI64U(Box<WirInstr>),
    F32DemoteF64(Box<WirInstr>),
    F32ReinterpretI32(Box<WirInstr>),

    // === Arithmetic (f64) ===
    F64Add(Box<WirInstr>, Box<WirInstr>),
    F64Sub(Box<WirInstr>, Box<WirInstr>),
    F64Mul(Box<WirInstr>, Box<WirInstr>),
    F64Div(Box<WirInstr>, Box<WirInstr>),
    F64Neg(Box<WirInstr>),
    F64Abs(Box<WirInstr>),
    F64Ceil(Box<WirInstr>),
    F64Floor(Box<WirInstr>),
    F64Trunc(Box<WirInstr>),
    F64Nearest(Box<WirInstr>),
    F64Sqrt(Box<WirInstr>),
    F64Min(Box<WirInstr>, Box<WirInstr>),
    F64Max(Box<WirInstr>, Box<WirInstr>),
    F64Copysign(Box<WirInstr>, Box<WirInstr>),
    F64Eq(Box<WirInstr>, Box<WirInstr>),
    F64Ne(Box<WirInstr>, Box<WirInstr>),
    F64Lt(Box<WirInstr>, Box<WirInstr>),
    F64Gt(Box<WirInstr>, Box<WirInstr>),
    F64Le(Box<WirInstr>, Box<WirInstr>),
    F64Ge(Box<WirInstr>, Box<WirInstr>),
    F64ConvertI32S(Box<WirInstr>),
    F64ConvertI32U(Box<WirInstr>),
    F64ConvertI64S(Box<WirInstr>),
    F64ConvertI64U(Box<WirInstr>),
    F64PromoteF32(Box<WirInstr>),
    F64ReinterpretI64(Box<WirInstr>),

    // === GC: Struct ===
    /// struct.new with named type
    StructNew { type_name: String, fields: Vec<WirInstr> },
    /// struct.get with named type and field
    StructGet { type_name: String, field_name: String, field_index: u32, expr: Box<WirInstr> },
    /// struct.set with named type and field
    StructSet { type_name: String, field_name: String, field_index: u32, expr: Box<WirInstr>, value: Box<WirInstr> },

    // === GC: Array ===
    ArrayNew { type_name: String, init: Box<WirInstr>, len: Box<WirInstr> },
    ArrayNewDefault { type_name: String, len: Box<WirInstr> },
    ArrayNewData { type_name: String, data_index: u32, offset: Box<WirInstr>, len: Box<WirInstr> },
    ArrayNewFixed { type_name: String, elements: Vec<WirInstr> },
    ArrayGet { type_name: String, array: Box<WirInstr>, index: Box<WirInstr> },
    ArrayGetS { type_name: String, array: Box<WirInstr>, index: Box<WirInstr> },
    ArrayGetU { type_name: String, array: Box<WirInstr>, index: Box<WirInstr> },
    ArraySet { type_name: String, array: Box<WirInstr>, index: Box<WirInstr>, value: Box<WirInstr> },
    ArrayLen(Box<WirInstr>),
    ArrayCopy { dest_type: String, src_type: String, dest: Box<WirInstr>, dest_offset: Box<WirInstr>, src: Box<WirInstr>, src_offset: Box<WirInstr>, len: Box<WirInstr> },
    ArrayFill { type_name: String, array: Box<WirInstr>, offset: Box<WirInstr>, value: Box<WirInstr>, len: Box<WirInstr> },

    // === GC: Reference ===
    RefNull { heap_type: WirAbstractHeapType },
    RefIsNull(Box<WirInstr>),
    RefAsNonNull(Box<WirInstr>),
    RefCast { type_name: String, nullable: bool, expr: Box<WirInstr> },
    RefTest { type_name: String, nullable: bool, expr: Box<WirInstr> },
    RefEq(Box<WirInstr>, Box<WirInstr>),
    RefI31(Box<WirInstr>),
    I31GetS(Box<WirInstr>),
    I31GetU(Box<WirInstr>),
    ExternInternalize(Box<WirInstr>),
    ExternExternalize(Box<WirInstr>),

    // === Control Flow ===
    /// Block with optional label and result type
    Block { label: Option<String>, result: Option<WirValType>, body: Vec<WirInstr> },
    /// Loop with optional label
    Loop { label: Option<String>, body: Vec<WirInstr> },
    /// If/else with optional result type
    If { condition: Box<WirInstr>, result: Option<WirValType>, then_body: Vec<WirInstr>, else_body: Option<Vec<WirInstr>> },
    /// Branch to label
    Br { depth: u32 },
    /// Conditional branch
    BrIf { depth: u32, condition: Box<WirInstr> },
    /// Branch table (switch)
    BrTable { index: Box<WirInstr>, targets: Vec<u32>, default: u32 },
    /// Return from function
    Return { value: Option<Box<WirInstr>> },
    /// Unreachable trap
    Unreachable,
    /// No operation (for structure)
    Nop,
    /// Drop a value
    Drop(Box<WirInstr>),
    /// Select between two values
    Select { condition: Box<WirInstr>, if_true: Box<WirInstr>, if_false: Box<WirInstr>, ty: Option<WirValType> },

    // === Calls ===
    Call { func_name: String, args: Vec<WirInstr> },
    CallIndirect { type_name: String, table: u32, index: Box<WirInstr>, args: Vec<WirInstr> },
    CallRef { type_name: String, func_ref: Box<WirInstr>, args: Vec<WirInstr> },
    RefFunc { func_name: String },

    // === Memory ===
    MemorySize,
    MemoryGrow(Box<WirInstr>),
    I32Load { offset: u64, align: u32, addr: Box<WirInstr> },
    I32Store { offset: u64, align: u32, addr: Box<WirInstr>, value: Box<WirInstr> },
    I64Load { offset: u64, align: u32, addr: Box<WirInstr> },
    I64Store { offset: u64, align: u32, addr: Box<WirInstr>, value: Box<WirInstr> },

    // === Table ===
    TableGet { table: u32, index: Box<WirInstr> },
    TableSet { table: u32, index: Box<WirInstr>, value: Box<WirInstr> },

    // === High-level compound instructions (lowered to sequences during emission) ===

    /// Deep copy of a value type (struct, array, variant, option, tuple).
    /// Lowered to field-by-field copy, array loop, etc. during emission.
    ValueCopy { type_name: String, source_type: WirCopyType, expr: Box<WirInstr> },

    /// Sequence of instructions (for statement blocks)
    Seq(Vec<WirInstr>),
}
```

### Copy Types

```rust
/// What kind of value copy to perform
pub enum WirCopyType {
    Struct { fields: Vec<WirCopyField> },
    Array { element_copy: Option<Box<WirCopyType>> },
    Variant { cases: Vec<WirCopyCase> },
    Option { inner_copy: Box<WirCopyType> },
    Tuple { field_copies: Vec<Option<WirCopyType>> },
}

pub struct WirCopyField {
    pub index: u32,
    pub needs_copy: bool,
    pub copy_type: Option<WirCopyType>,
}

pub struct WirCopyCase {
    pub index: u32,
    pub name: String,
    pub payload_copy: Option<WirCopyType>,
}
```

### Component Model

```rust
/// Component Model wrapper information
pub struct WirComponent {
    /// WASI interfaces to import
    pub wasi_imports: Vec<WirWasiImport>,
    /// Bundled modules (fts, libm)
    pub bundled_modules: Vec<WirBundledModule>,
    /// World exports
    pub world_exports: Vec<WirWorldExport>,
    /// Memory module configuration
    pub memory: WirMemoryConfig,
}
```

### Names and References

WIR uses **string names** for type and function references, not numeric indices. During emission, names are resolved to indices via lookup tables built during the emission pass. This keeps WIR readable and decoupled from index allocation order.

```rust
/// Type references use the type's name
pub struct WirTypeRef(pub String);

/// Function references use the function's name
pub struct WirFuncRef(pub String);
```

## Unparse Format

WIR supports `--unparse` to output pseudo-Wado/WAT hybrid for debugging:

```
// Type definitions
type Point = struct { x: i32, y: i32 }
type Array<i32> = struct { repr: array<i32>, used: i32 }

// Function
fn "example"(a: i32, b: i32) -> i32 {
    let result: i32;
    result = i32.add(a, b);
    return result;
}

// GC operations shown with type names
fn "Point::sum"(self: ref Point) -> i32 {
    return i32.add(
        struct.get Point.x(self),
        struct.get Point.y(self),
    );
}

// Value copy shown explicitly
fn "copy_array"(src: ref Array<i32>) -> ref Array<i32> {
    return value_copy Array<i32>(src);
}
```

## Migration Plan

The migration is incremental. Each step produces a working compiler.

### Preparation: Extract Submodules from codegen.rs

Before introducing WIR, split codegen.rs into manageable files:

- [ ] **Step 0a**: Extract `type_registration.rs` — the 15+ type registration phases from `build_main_module()` (lines 700–1365). This is pure analysis + `wasm_encoder` type section building. ~700 lines.
- [ ] **Step 0b**: Extract `value_copy.rs` — `generate_value_copy()`, `generate_struct_copy()`, `generate_array_copy()`, `generate_variant_copy()`, `generate_option_copy()`, `needs_value_copy()`. ~500 lines.
- [ ] **Step 0c**: Extract `match_codegen.rs` — `generate_match_expr()`, `generate_match_arms()`, `generate_match_br_table()`, `generate_match_pattern_check()`, `generate_match_pattern_binding()`, `analyze_for_br_table()`. ~600 lines.
- [ ] **Step 0d**: Extract `cm_codegen.rs` — `generate_cm_effect_call()`, `generate_cm_resource_method_call()`, `generate_effect_wait()`, CM payload lowering functions. ~800 lines.
- [ ] **Step 0e**: Extract `scalarization.rs` — `collect_scalarization_candidates()`, `preallocate_scalarized_locals()`, related analysis. ~300 lines.

After this step, codegen.rs is split into ~6 files but the architecture is unchanged. This is pure refactoring.

### Phase 1: Define WIR Data Structures

- [ ] **Step 1a**: Create `wir.rs` with the WIR data structure definitions (types, instructions, module structure). No code uses it yet.
- [ ] **Step 1b**: Create `wir_unparse.rs` for WIR → pseudo-Wado output. Hook into `wado dump --wir --unparse`.
- [ ] **Step 1c**: Add `--wir` flag to the dump command. Pipeline: TIR → (existing codegen analysis) → WIR → display.

### Phase 2: WIR Emission for Function Bodies

The core migration: replace TIR expression codegen with WIR generation.

- [ ] **Step 2a**: Create `tir_to_wir.rs` — translates TIR expressions to WIR instructions. This extracts the logic from `generate_expr()` but produces WIR nodes instead of `wasm_encoder::Instruction`.
- [ ] **Step 2b**: Create `wir_emit.rs` — translates WIR instructions to `wasm_encoder::Instruction`. This is a mechanical mapping (WIR names → Wasm indices, WIR tree → flat instruction sequence).
- [ ] **Step 2c**: Wire it together: `tir_to_wir` produces `WirFunction` bodies, `wir_emit` produces `wasm_encoder::Function`. The old `generate_expr()` / `generate_function()` can be replaced incrementally (one TirExprKind at a time).
- [ ] **Step 2d**: Delete the old `generate_expr()` once all expression kinds are covered by `tir_to_wir` + `wir_emit`.

### Phase 3: WIR Emission for Type Section

- [ ] **Step 3a**: Move type layout decisions into WIR generation. The 15+ type registration phases produce `Vec<WirTypeDef>` instead of calling `wasm_encoder` directly.
- [ ] **Step 3b**: `wir_emit` translates `WirTypeDef` → `wasm_encoder` type section entries.
- [ ] **Step 3c**: Delete the old type registration code in codegen.

### Phase 4: WIR Emission for Module Structure

- [ ] **Step 4a**: Move import/export/global/data section generation to produce WIR structures.
- [ ] **Step 4b**: Move Component Model wrapper generation to produce `WirComponent`.
- [ ] **Step 4c**: `wir_emit` translates the complete `WirModule` → Wasm binary.

### Phase 5: Merge wasm_plan into WIR Generation

At this point, `wasm_plan` and `tir_to_wir` are doing related work. Consolidate:

- [ ] **Step 5a**: Move `ComponentPlan` generation into WIR generation (it becomes `WirComponent`).
- [ ] **Step 5b**: The pipeline becomes: `optimize → wir_gen → wir_emit`.
- [ ] **Step 5c**: Delete the separate `wasm_plan` phase. Its analysis functions (topological sort, etc.) move into `wir_gen` or stay as utilities.

### Final State

```
lower → optimize → wir_gen → wir_emit → wasm binary
                      ↓
                  WirModule (inspectable via dump --wir)
```

- `wir_gen` (~5000 lines): TIR + Project → WirModule. All analysis, type layout, function translation.
- `wir_emit` (~2000 lines): WirModule → Wasm binary. Mechanical translation, index allocation, `wasm_encoder` calls.
- `wir.rs` (~500 lines): Data structure definitions.
- `wir_unparse.rs` (~500 lines): WIR → pseudo-Wado for debugging.

## Design Rationale

### Why Tree-Structured (Not Flat Instructions)?

Wasm is a stack machine, but flat instruction sequences are hard to inspect and manipulate. WIR uses trees where operands are children:

```rust
// WIR (tree): readable, inspectable
I32Add(
    StructGet { type_name: "Point", field_name: "x", expr: LocalGet("self") },
    StructGet { type_name: "Point", field_name: "y", expr: LocalGet("self") },
)

// Wasm (flat): requires mental stack tracking
// local.get $self
// struct.get $Point 0
// local.get $self
// struct.get $Point 1
// i32.add
```

Flattening trees to stack-machine instructions is trivial (post-order traversal). The reverse is not.

### Why Named References (Not Indices)?

Using names keeps WIR independent of emission order:

- Type registration order can change without invalidating WIR
- Functions can be reordered freely
- WIR is self-documenting (no need for a separate name section to read it)

The cost is a name→index lookup during emission, which is O(1) with `IndexMap`.

### Why `ValueCopy` as a Compound Instruction?

Value copy involves complex dispatching (struct copy, array loop, variant tag check, etc.). Keeping it as a single WIR node:

- Preserves the semantic intent ("copy this value")
- Allows the emitter to choose the most efficient lowering strategy
- Keeps `tir_to_wir` focused on semantics, not emission details

### Why Not a Separate Optimization Pass on WIR?

WIR is intentionally a thin layer. Optimizations belong on TIR where semantic information is richer. WIR's purpose is debuggability and separation of concerns, not optimization.

If Wasm-level optimizations become needed (peephole, etc.), they can be added as a `wir_opt` pass later without changing the architecture.

## Consequences

### Benefits

- **Debuggability**: `wado dump --wir --unparse` shows exactly what Wasm will be generated, with readable names
- **Testability**: WIR generation and WIR emission can be tested independently
- **Maintainability**: codegen.rs (14k lines) splits into focused modules (~5k + ~2k + ~1k)
- **Extensibility**: New Wasm features (SIMD, stack switching) are added as WIR nodes, not interleaved with encoding logic
- **Golden testing**: WIR output can be used for golden file testing (more stable than binary Wasm)

### Trade-offs

- **Additional IR**: One more representation to maintain. Mitigated by WIR being close to Wasm (not a novel abstraction).
- **Memory**: WIR trees allocate more than flat instruction streams. Acceptable since Wado programs are not extremely large.
- **Migration effort**: Incremental migration spans multiple steps. Each step is independently shippable.
- **No Wasm-level optimization**: Intentional. TIR optimization is sufficient for now.

### Risks

- **Scope creep**: WIR should stay close to Wasm. Resist adding high-level abstractions.
- **Name resolution overhead**: String-based name lookup during emission. Mitigated by single-pass name table construction.
- **Incomplete migration**: If migration stalls midway, we'd have two code paths. Mitigated by the incremental approach where each step is a complete working state.
