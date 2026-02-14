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
2. **Named types**: Struct, variant, enum, and flags types retain their source-level names and field/case names. These are Wado-level type definitions, not Wasm type section entries — the emit phase expands them (e.g., a variant becomes N+1 Wasm struct types).
3. **Wado-level value types**: WIR uses `Bool`, `Char`, `I8`, `U8`, `I16`, `U16`, `Enum { type_name }`, `Flags { type_name }`, etc. instead of Wasm's `i32`-for-everything. The emit phase lowers to Wasm `ValType`/`StorageType`. This avoids the `ValType`/`StorageType` split at the WIR level and provides better debug output.
4. **Structured control flow**: Blocks, loops, if/else are tree nodes (not flat instruction sequences with labels).
5. **Explicit value copy**: Copy operations are explicit WIR nodes rather than inline instruction sequences.
6. **TIR metadata preserved**: Module source, source spans, attributes, generic instantiation info, and newtype origin are carried through for debugging and unparse. Newtypes are resolved (a `Meters` is `f64` at the WIR level), but their origin is recorded.
7. **Unparse support**: WIR can be rendered as pseudo-Wado for inspection via `wado dump --wir --unparse`.

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
    /// Type definitions: Wado-level types (struct, variant, enum, flags, array, func).
    /// Not a 1:1 mapping to the Wasm type section — the emit phase expands these:
    ///   Struct → 1 Wasm struct type
    ///   Variant → N+1 Wasm struct types (base + case subtypes)
    ///   Enum → none (i32 discriminant)
    ///   Flags → none (i32 bitfield)
    ///   Array → 1 Wasm array type
    ///   Func → 1 Wasm func type
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

### Metadata

WIR preserves TIR metadata for debugging and unparse output, even when not needed for code emission.

```rust
/// Source location and origin metadata, carried through from TIR.
pub struct WirMeta {
    /// Which module this entity was defined in
    pub module_source: Option<ModuleSource>,
    /// Source span in the original Wado source
    pub span: Option<Span>,
    /// Attributes (e.g., #[hidden])
    pub attributes: Vec<WirAttribute>,
}

/// Generic instantiation origin (e.g., Array<i32> from Array<T>)
pub struct WirGenericOrigin {
    /// Base generic name (e.g., "Array", "Box")
    pub base_name: String,
    /// Type arguments used for instantiation (e.g., ["i32"])
    pub type_args: Vec<String>,
}

/// Newtype origin — when a type was originally a newtype alias
pub struct WirNewtypeOrigin {
    /// Newtype name (e.g., "Meters")
    pub name: String,
    /// Module where the newtype was defined
    pub module_source: ModuleSource,
}
```

### Type Definitions

WIR type definitions are at the Wado source level, not the Wasm type section level. The emit phase expands them into actual Wasm type section entries.

```rust
/// A Wado-level type definition.
/// The emit phase expands these into Wasm type section entries:
///   Struct → 1 Wasm struct type
///   Variant → N+1 Wasm struct types (base with discriminant field + case subtypes)
///   Enum → none (represented as i32)
///   Flags → none (represented as i32 bitfield)
///   Array → 1 Wasm array type
///   Func → 1 Wasm func type
pub enum WirTypeDef {
    /// Struct type with named fields
    Struct(WirStructType),
    /// Variant type (sum type with payloads)
    Variant(WirVariantType),
    /// Enum type (discriminated values without payloads)
    Enum(WirEnumType),
    /// Flags type (bitfield)
    Flags(WirFlagsType),
    /// Array type
    Array(WirArrayType),
    /// Function type
    Func(WirFuncType),
}
```

#### Struct

```rust
pub struct WirStructType {
    /// Source-level name (e.g., "Point", "Array<i32>", "__Closure_3")
    pub name: String,
    /// Fields with names and types
    pub fields: Vec<WirField>,
    /// Metadata (module source, span, attributes)
    pub meta: WirMeta,
    /// Generic instantiation origin (None for non-generic types)
    pub generic_origin: Option<WirGenericOrigin>,
    /// If this type was a newtype in the source (resolved by WIR phase)
    pub newtype_origin: Option<WirNewtypeOrigin>,
}

pub struct WirField {
    /// Source-level field name (e.g., "x", "repr", "discriminant")
    pub name: String,
    /// WIR type (uses Wado-level primitives, not Wasm ValType)
    pub ty: WirType,
    /// Whether this field is mutable
    pub mutable: bool,
}
```

#### Variant

Variants are sum types with payloads. At the Wasm level, they expand to a subtype hierarchy: a base struct with a `discriminant` field, and per-case subtypes that add payload fields.

```rust
pub struct WirVariantType {
    /// Source-level name (e.g., "Shape", "Result<i32, String>")
    pub name: String,
    /// Cases with names and optional payload types
    pub cases: Vec<WirVariantCase>,
    /// Metadata (module source, span, attributes)
    pub meta: WirMeta,
    /// Generic instantiation origin (None for non-generic types)
    pub generic_origin: Option<WirGenericOrigin>,
    /// If this type was a newtype in the source (resolved by WIR phase)
    pub newtype_origin: Option<WirNewtypeOrigin>,
}

pub struct WirVariantCase {
    /// Case name (e.g., "Circle", "Ok")
    pub name: String,
    /// Case index (discriminant value)
    pub index: u32,
    /// Payload field types (empty for unit cases like "Point" or "None")
    pub payload: Vec<WirType>,
}
```

The emit phase generates:

- Base struct: `(type $Shape (struct (field $discriminant i32)))`
- Case subtypes: `(type $Shape::Circle (sub $Shape (struct (field $discriminant i32) (field $0 f64))))`
- Unit case subtypes: `(type $Shape::Point (sub $Shape (struct (field $discriminant i32))))`

For NullableRef variants (2 cases, 1 unit, payload is non-nullable ref), the emit phase may use a nullable reference instead of a subtype hierarchy.

#### Enum

Enums are discriminated values without payloads. They have no Wasm type section entry — values are i32 discriminants.

```rust
pub struct WirEnumType {
    /// Source-level name (e.g., "Color", "Ordering")
    pub name: String,
    /// Cases with names and discriminant values
    pub cases: Vec<WirEnumCase>,
    /// Metadata (module source, span, attributes)
    pub meta: WirMeta,
    /// Generic instantiation origin (None for non-generic types)
    pub generic_origin: Option<WirGenericOrigin>,
}

pub struct WirEnumCase {
    /// Case name (e.g., "Red", "Less")
    pub name: String,
    /// Discriminant value
    pub discriminant: i32,
}
```

#### Flags

Flags are bitfield types. Like enums, they have no Wasm type section entry — values are i32 bitfields.

```rust
pub struct WirFlagsType {
    /// Source-level name
    pub name: String,
    /// Flag bits with names and positions
    pub bits: Vec<WirFlagBit>,
    /// Metadata (module source, span, attributes)
    pub meta: WirMeta,
}

pub struct WirFlagBit {
    /// Flag name
    pub name: String,
    /// Bit position (0-indexed)
    pub position: u32,
}
```

#### Array and Func

```rust
pub struct WirArrayType {
    pub name: String,
    pub element_type: WirType,
    pub mutable: bool,
    pub meta: WirMeta,
    pub generic_origin: Option<WirGenericOrigin>,
}

pub struct WirFuncType {
    pub name: String,
    pub params: Vec<WirType>,
    pub results: Vec<WirType>,
}

/// Reference to a type by name (resolved to index during emission)
pub struct WirTypeRef(pub String);
```

### WIR Type System

WIR uses **Wado-level primitive types**, not Wasm's `ValType`/`StorageType` split. This preserves semantic information for debugging and simplifies the type representation. The emit phase lowers `WirType` to the appropriate Wasm `ValType` or `StorageType` depending on context (local vs. struct field).

```rust
/// WIR type — Wado-level primitives + GC references.
///
/// Unlike Wasm's ValType (i32/i64/f32/f64 only), WIR preserves the full
/// Wado type distinctions. The emit phase lowers these:
///   - I8/I16/U8/U16/Bool/Char → i32 (locals) or i8/i16 (packed struct fields)
///   - I32/U32 → i32
///   - I64/U64 → i64
///   - F32 → f32
///   - F64 → f64
///   - Enum/Flags → i32
pub enum WirType {
    // Signed integers
    I8,
    I16,
    I32,
    I64,
    // Unsigned integers
    U8,
    U16,
    U32,
    U64,
    // Floats
    F32,
    F64,
    // Other primitives
    Bool,
    Char,
    // Unit (no Wasm representation; zero-size)
    Unit,
    /// Named enum type (i32 at Wasm level)
    Enum { type_name: String },
    /// Named flags type (i32 at Wasm level)
    Flags { type_name: String },
    /// Reference to a named GC type
    Ref { type_name: String, nullable: bool },
    /// Abstract reference type (anyref, funcref, etc.)
    AbstractRef { heap_type: WirAbstractHeapType, nullable: bool },
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
    /// Type reference (function signature — param types and result types)
    pub type_ref: WirTypeRef,
    /// Parameter names (types come from the referenced WirFuncType)
    pub param_names: Vec<String>,
    /// Body (None for imported functions)
    pub body: Option<Vec<WirInstr>>,
    /// Metadata (module source, span, attributes)
    pub meta: WirMeta,
    /// Generic instantiation origin
    pub generic_origin: Option<WirGenericOrigin>,
    /// Effect requirements (for unparse display)
    pub effects: Vec<String>,
}
```

Locals are declared via `DeclareLocal` instructions in the body. There is no separate locals list — the emit phase collects all `DeclareLocal`s and pre-allocates them as Wasm locals.

### Instructions

WIR instructions are tree-structured where operands are child nodes, not stack values. This makes the structure inspectable and allows inline local declaration.

```rust
pub enum WirInstr {
    // === Locals ===
    /// Declare a new local variable inline (not in Wasm; lowered to pre-allocated local)
    DeclareLocal { name: String, ty: WirType },
    /// local.get by name
    LocalGet { name: String },
    /// local.set by name
    LocalSet { name: String, value: Box<WirInstr> },
    /// local.tee by name
    LocalTee { name: String, value: Box<WirInstr> },

    // === Globals ===
    GlobalGet { name: String },
    GlobalSet { name: String, value: Box<WirInstr> },

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
    /// struct.get with named type and field (field index resolved by emitter)
    StructGet { type_name: String, field_name: String, expr: Box<WirInstr> },
    /// struct.set with named type and field (field index resolved by emitter)
    StructSet { type_name: String, field_name: String, expr: Box<WirInstr>, value: Box<WirInstr> },

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
    Block { label: Option<String>, result: Option<WirType>, body: Vec<WirInstr> },
    /// Loop with optional label
    Loop { label: Option<String>, body: Vec<WirInstr> },
    /// If/else with optional result type
    If { condition: Box<WirInstr>, result: Option<WirType>, then_body: Vec<WirInstr>, else_body: Option<Vec<WirInstr>> },
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
    Select { condition: Box<WirInstr>, if_true: Box<WirInstr>, if_false: Box<WirInstr>, ty: Option<WirType> },

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

WIR uses **string names** for all references, not numeric indices. During emission, names are resolved to indices via lookup tables built during the emission pass. This keeps WIR readable and decoupled from index allocation order.

All named references in WIR:

- **Types**: `WirTypeRef(String)` — struct, variant, array, func type names
- **Functions**: `WirFuncRef(String)` — function names in `Call`, `RefFunc`
- **Locals**: `String` — local variable names in `LocalGet`, `LocalSet`, `DeclareLocal`
- **Globals**: `String` — global variable names in `GlobalGet`, `GlobalSet`
- **Fields**: `String` — struct field names in `StructGet`, `StructSet` (resolved to field index by emitter)

```rust
/// Type references use the type's name
pub struct WirTypeRef(pub String);

/// Function references use the function's name
pub struct WirFuncRef(pub String);
```

## Unparse Format

WIR supports `--unparse` to output pseudo-Wado for debugging. The unparse output should look as close to Wado source as possible, using named field access and Wado-level type definitions rather than raw Wasm instructions.

```
// Struct definition
struct Point { x: i32, y: i32 }  // from ./geometry.wado

// Variant definition (source-level, not expanded to Wasm struct hierarchy)
variant Shape {  // from ./shapes.wado
    Circle(f64),
    Rectangle(f64, f64),
    Point,
}

// Enum definition (no Wasm type — i32 discriminant)
enum Color { Red = 0, Green = 1, Blue = 2 }  // from ./colors.wado

// Newtype (resolved to base type, origin recorded)
type Meters = f64  // newtype from ./physics.wado

// Generic instantiation
struct Array<i32> { repr: array<i32>, used: i32 }  // Array<T> with T=i32

// Function with module source and effects
fn "example"(a: i32, b: i32) -> i32 {  // from <entry>
    let result: i32;
    result = i32.add(a, b);
    return result;
}

// Wado-level types in signatures (bool, char, u8 etc.)
fn "is_ascii"(c: char) -> bool {  // from ./utils.wado
    return i32.lt_u(c, 128);
}

// Named field access (Wado-style, not struct.get)
fn "Point::sum"(self: ref Point) -> i32 {  // from ./geometry.wado
    return i32.add(self.x, self.y);
}

// Named field assignment (Wado-style, not struct.set)
fn "Point::reset"(self: ref mut Point) {  // from ./geometry.wado
    self.x = 0;
    self.y = 0;
}

// Enum type in signatures
fn "Color::is_primary"(self: Color) -> bool {  // from ./colors.wado
    return i32.or(
        i32.eq(self, 0),  // Red
        i32.eq(self, 2),  // Blue
    );
}

// Value copy shown explicitly
fn "copy_array"(src: ref Array<i32>) -> ref Array<i32> {
    return value_copy Array<i32>(src);
}
```

### Unparse Principles

- **Type definitions**: Use Wado syntax (`struct`, `variant`, `enum`) not Wasm GC syntax
- **Field access**: Use `self.x` not `struct.get Point.x(self)`
- **Field assignment**: Use `self.x = value` not `struct.set Point.x(self, value)`
- **Enum/flags values**: Show discriminant as i32 constant (since the Wasm level is i32)
- **Variant construction**: Show `Shape::Circle(5.0)` or `struct.new Shape::Circle(0, 5.0)` depending on context
- **Instructions**: Use WAT-style mnemonics for arithmetic (`i32.add`, `f64.mul`, etc.)

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

- [ ] **Step 3a**: Move type layout decisions into WIR generation. The 15+ type registration phases produce `Vec<WirTypeDef>` (structs, variants, enums, flags, arrays, func types) instead of calling `wasm_encoder` directly.
- [ ] **Step 3b**: `wir_emit` translates `WirTypeDef` → Wasm type section entries. This includes expanding `WirVariantType` into base struct + subtype structs, and skipping `WirEnumType`/`WirFlagsType` (no Wasm type section entry needed).
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

// Unparse: Wado-style field access
// i32.add(self.x, self.y)

// Wasm (emitted): field indices resolved from type definition
// local.get $self
// struct.get $Point 0   ;; "x" → index 0
// local.get $self
// struct.get $Point 1   ;; "y" → index 1
// i32.add
```

Flattening trees to stack-machine instructions is trivial (post-order traversal). The reverse is not.

### Why Named References (Not Indices)?

Using names **uniformly** for all references keeps WIR independent of emission order and self-documenting:

- Types, functions, globals, locals, struct fields — all referenced by name
- Type registration order can change without invalidating WIR
- Functions and globals can be reordered freely
- Struct field indices are resolved from type definitions during emission
- WIR is self-documenting (no need for a separate name section to read it)

The cost is a name→index lookup during emission, which is O(1) with `IndexMap`.

### Why `ValueCopy` as a Compound Instruction?

Value copy involves complex dispatching (struct copy, array loop, variant discriminant check, etc.). Keeping it as a single WIR node:

- Preserves the semantic intent ("copy this value")
- Allows the emitter to choose the most efficient lowering strategy
- Keeps `tir_to_wir` focused on semantics, not emission details

### Why Wado-Level Value Types (Not Wasm ValType)?

Wasm has only 4 numeric types: `i32`, `i64`, `f32`, `f64`. Wado has `bool`, `char`, `i8`, `u8`, `i16`, `u16`, `i32`, `u32`, `i64`, `u64`, `f32`, `f64`, plus enum and flags types — all of which collapse to `i32` at the Wasm level. In the current codegen, this collapse happens early, losing semantic information.

WIR keeps the Wado types and lets the emit phase lower them:

| WIR Type      | As Wasm local (ValType) | As struct field (StorageType) |
| ------------- | ----------------------- | ----------------------------- |
| Bool          | i32                     | i8                            |
| Char          | i32                     | i32                           |
| I8            | i32                     | i8                            |
| U8            | i32                     | i8                            |
| I16           | i32                     | i16                           |
| U16           | i32                     | i16                           |
| I32, U32      | i32                     | i32                           |
| I64, U64      | i64                     | i64                           |
| F32           | f32                     | f32                           |
| F64           | f64                     | f64                           |
| Enum { .. }   | i32                     | i32                           |
| Flags { .. }  | i32                     | i32                           |

This eliminates the `ValType`/`StorageType` split at the WIR level — there is just `WirType`. The emit phase knows the context (local vs. struct field) and picks the right Wasm encoding. It also makes unparse output more readable: `bool` instead of `i32`, `Color` instead of `i32`.

### Why Preserve TIR Metadata?

WIR carries metadata (module source, spans, attributes, generic origin, newtype origin) even though the emit phase does not need most of it. This is intentional:

- **Unparse**: `wado dump --wir --unparse` can show `// from ./geometry.wado` comments, display newtype origins, and annotate generic instantiations.
- **Error messages**: If the emit phase detects a problem, it can report source locations.
- **Debugging**: When investigating codegen issues, knowing where a type or function came from is critical.
- **Newtypes**: Resolved by the WIR phase (a `Meters` field is `F64` in WIR), but `newtype_origin` records that it was `Meters` from `./physics.wado`. This avoids polluting the emit phase with newtype logic while keeping debug info.

The metadata is lightweight (references and optional fields) and does not affect WIR→Wasm correctness.

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
