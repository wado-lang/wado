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
lower → optimize → wasm_plan → tir_to_wir → wir_emit → wasm binary
                                    ↓
                                WirModule (inspectable via dump --wir)
```

`tir_to_wir` translates the optimized Project into a `WirModule` — a complete description of the Wasm module in WIR form. `wir_emit` translates `WirModule` into Wasm binary bytes. `wasm_plan` remains unchanged and provides `ComponentPlan` metadata consumed by `tir_to_wir`.

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
- **Not a semantic optimization target**: Semantic optimizations (inlining, SROA, reference elimination) happen on TIR where richer type information is available. WIR may host low-level Wasm optimizations (constant folding, LICM, peephole) in the future.
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

### Names

WIR entities at global scope (types, functions, globals) use `WirName` to carry both a short display name and a fully-qualified identity name. Local-scope names (locals, struct fields, enum/variant cases) remain plain `String`.

```rust
/// A globally-scoped name in WIR.
///
/// Carries both forms needed by different consumers:
/// - `display`: short name for unparse and error messages
/// - `fq`: module-qualified name for unique identity
///
/// The `tir_to_wir` phase constructs WirNames from name.rs objects
/// (StructName, FunctionId, etc.). WIR itself does not import name.rs types.
pub struct WirName {
    /// Short display name (e.g., "Point", "Array<i32>", "Point::sum")
    /// Used by unparse and error messages.
    pub display: String,
    /// Fully-qualified name (e.g., "core:prelude//Point", "./geometry.wado/Point::sum")
    /// Used as the unique identity key for name→index resolution during emission.
    pub fq: String,
}
```

Examples:

| Entity | `display` | `fq` |
| ------ | --------- | ---- |
| Struct Point from entry | `Point` | `<entry>//Point` |
| Array<i32> from prelude | `Array<i32>` | `core:prelude//Array<i32>` |
| Method Point::sum | `Point::sum` | `./geometry.wado/Point::sum` |
| Global counter | `counter` | `<entry>//counter` |
| Enum Ordering from prelude | `Ordering` | `core:prelude//Ordering` |

`WirName` is used on **definitions** (low frequency):

- **Type definitions**: `WirStructType.name`, `WirVariantType.name`, `WirEnumType.name`, etc.
- **Function definitions**: `WirFunction.name`
- **Global definitions and references**: `WirGlobal.name`, `GlobalGet { name: WirName }`

**Instructions** use `WirTypeId` / `WirFuncId` instead of `WirName` (see below).

Not used for local-scope names (no module qualification needed):

- Struct field names, enum/variant case names
- Local variable names (`DeclareLocal`, `LocalGet`, `LocalSet`)
- Block labels

### Type and Function IDs

WIR instructions reference types and functions via lightweight IDs (`WirTypeId`, `WirFuncId`) instead of embedding `WirName` in every instruction. GC instructions (struct access, array access, ref cast, etc.) and call instructions are extremely frequent — in a typical Wado program, every field access, every function call, and every type test requires a type or function reference. Using IDs avoids per-instruction string hashing during emission.

```rust
use std::rc::Rc;

/// Lightweight reference to a type definition in WirModule.types.
///
/// - `index`: indexes into WirModule.types (not the Wasm type section)
/// - `fq`: fully-qualified name shared via Rc<str> for Debug output
///
/// Eq and Hash use `index` only (O(1) integer operations).
/// Debug prints the fq name (e.g., "core:prelude//Array<i32>").
/// Clone is O(1) — Rc refcount increment (non-atomic, near zero cost).
#[derive(Clone)]
pub struct WirTypeId {
    index: u32,
    fq: Rc<str>,
}

/// Lightweight reference to a function.
/// Same design as WirTypeId.
#[derive(Clone)]
pub struct WirFuncId {
    index: u32,
    fq: Rc<str>,
}
```

For both types:

- `PartialEq` / `Eq`: compares `index` only
- `Hash`: hashes `index` only
- `fmt::Debug`: prints `fq` (e.g., `core:prelude//Point` instead of `WirTypeId(17)`)
- All references to the same type/function share the same `Rc<str>` allocation

`tir_to_wir` creates name→ID maps during type and function registration. When generating instructions, it embeds the pre-resolved ID. `wir_emit` builds `WirTypeId.index → Wasm type index` and `WirFuncId.index → Wasm func index` tables once, then resolves every reference via O(1) integer indexing.

| Property | `WirName` (definitions) | `WirTypeId` / `WirFuncId` (instructions) |
| --- | --- | --- |
| Eq / Hash | O(n) string hash | O(1) integer |
| Clone | O(n) string copy | O(1) Rc refcount |
| Debug | shows display name | shows fq name |
| Stack size | 48 bytes (2× String) | 24 bytes (u32 + Rc\<str\>) |
| Used in | Type/function definitions | Instructions (high frequency) |

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
```

`module_source` is also encoded in `WirName.fq`, but kept as a separate `ModuleSource` value for programmatic use (e.g., `is_core()`, `is_wasi()`, grouping by module). Parsing `fq` strings to extract module origin would violate name.rs conventions.

```rust
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
    /// Name (display: "Point", fq: "core:prelude//Point")
    pub name: WirName,
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
    /// Name (display: "Shape", fq: "./shapes.wado//Shape")
    pub name: WirName,
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
    /// Name (display: "Color", fq: "./colors.wado//Color")
    pub name: WirName,
    /// Cases with names and discriminant values
    pub cases: Vec<WirEnumCase>,
    /// Metadata (module source, span, attributes)
    pub meta: WirMeta,
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
    /// Name (display: "Permissions", fq: "./perms.wado//Permissions")
    pub name: WirName,
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
    pub name: WirName,
    pub element_type: WirType,
    pub mutable: bool,
    pub meta: WirMeta,
    pub generic_origin: Option<WirGenericOrigin>,
}

pub struct WirFuncType {
    pub name: WirName,
    pub params: Vec<WirType>,
    pub results: Vec<WirType>,
}
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
    Enum { type_id: WirTypeId },
    /// Named flags type (i32 at Wasm level)
    Flags { type_id: WirTypeId },
    /// Reference to a named GC type
    Ref { type_id: WirTypeId, nullable: bool },
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
    /// Function name (display: "Point::sum", fq: "./geometry.wado/Point::sum")
    pub name: WirName,
    /// Function type ID (references a WirTypeDef::Func in WirModule.types)
    pub type_id: WirTypeId,
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
    GlobalGet { name: WirName },
    GlobalSet { name: WirName, value: Box<WirInstr> },

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

    // === Arithmetic (i128 via i64 pairs, Wasm 3.0) ===
    I64Add128(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    I64Sub128(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    I64MulWideU(Box<WirInstr>, Box<WirInstr>),
    I64MulWideS(Box<WirInstr>, Box<WirInstr>),

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
    /// struct.new with type ID
    StructNew { type_id: WirTypeId, fields: Vec<WirInstr> },
    /// struct.get with type ID and field name (field index resolved by emitter)
    StructGet { type_id: WirTypeId, field_name: String, expr: Box<WirInstr> },
    /// struct.set with type ID and field name (field index resolved by emitter)
    StructSet { type_id: WirTypeId, field_name: String, expr: Box<WirInstr>, value: Box<WirInstr> },

    // === GC: Array ===
    ArrayNew { type_id: WirTypeId, init: Box<WirInstr>, len: Box<WirInstr> },
    ArrayNewDefault { type_id: WirTypeId, len: Box<WirInstr> },
    ArrayNewData { type_id: WirTypeId, data_index: u32, offset: Box<WirInstr>, len: Box<WirInstr> },
    ArrayNewFixed { type_id: WirTypeId, elements: Vec<WirInstr> },
    ArrayGet { type_id: WirTypeId, array: Box<WirInstr>, index: Box<WirInstr> },
    ArrayGetS { type_id: WirTypeId, array: Box<WirInstr>, index: Box<WirInstr> },
    ArrayGetU { type_id: WirTypeId, array: Box<WirInstr>, index: Box<WirInstr> },
    ArraySet { type_id: WirTypeId, array: Box<WirInstr>, index: Box<WirInstr>, value: Box<WirInstr> },
    ArrayLen(Box<WirInstr>),
    ArrayCopy { dest_type_id: WirTypeId, src_type_id: WirTypeId, dest: Box<WirInstr>, dest_offset: Box<WirInstr>, src: Box<WirInstr>, src_offset: Box<WirInstr>, len: Box<WirInstr> },
    ArrayFill { type_id: WirTypeId, array: Box<WirInstr>, offset: Box<WirInstr>, value: Box<WirInstr>, len: Box<WirInstr> },

    // === GC: Reference ===
    RefNull { heap_type: WirAbstractHeapType },
    RefIsNull(Box<WirInstr>),
    RefAsNonNull(Box<WirInstr>),
    RefCast { type_id: WirTypeId, nullable: bool, expr: Box<WirInstr> },
    RefTest { type_id: WirTypeId, nullable: bool, expr: Box<WirInstr> },
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
    Call { func_id: WirFuncId, args: Vec<WirInstr> },
    CallIndirect { type_id: WirTypeId, table: u32, index: Box<WirInstr>, args: Vec<WirInstr> },
    CallRef { type_id: WirTypeId, func_ref: Box<WirInstr>, args: Vec<WirInstr> },
    RefFunc { func_id: WirFuncId },

    // === Memory ===
    MemorySize,
    MemoryGrow(Box<WirInstr>),
    I32Load { offset: u64, align: u32, addr: Box<WirInstr> },
    I32Load8U { offset: u64, align: u32, addr: Box<WirInstr> },
    I32Load8S { offset: u64, align: u32, addr: Box<WirInstr> },
    I32Load16U { offset: u64, align: u32, addr: Box<WirInstr> },
    I32Load16S { offset: u64, align: u32, addr: Box<WirInstr> },
    I32Store { offset: u64, align: u32, addr: Box<WirInstr>, value: Box<WirInstr> },
    I32Store8 { offset: u64, align: u32, addr: Box<WirInstr>, value: Box<WirInstr> },
    I32Store16 { offset: u64, align: u32, addr: Box<WirInstr>, value: Box<WirInstr> },
    I64Load { offset: u64, align: u32, addr: Box<WirInstr> },
    I64Store { offset: u64, align: u32, addr: Box<WirInstr>, value: Box<WirInstr> },

    // === Table ===
    TableGet { table: u32, index: Box<WirInstr> },
    TableSet { table: u32, index: Box<WirInstr>, value: Box<WirInstr> },

    // === High-level compound instructions (lowered to sequences during emission) ===

    /// Deep copy of a value type (struct, array, variant, option, tuple).
    /// Lowered to field-by-field copy, array loop, etc. during emission.
    ValueCopy { type_id: WirTypeId, source_type: WirCopyType, expr: Box<WirInstr> },

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

WIR uses three reference mechanisms depending on frequency and scope:

| Scope | Reference type | Used in | Eq / Hash |
| ----- | -------------- | ------- | --------- |
| Types (in instructions) | `WirTypeId` | `StructGet`, `ArrayNew`, `RefCast`, `CallRef`, etc. | O(1) integer |
| Functions (in instructions) | `WirFuncId` | `Call`, `RefFunc` | O(1) integer |
| Definitions | `WirName` | `WirStructType.name`, `WirFunction.name`, `WirGlobal.name` | O(n) string |
| Globals (in instructions) | `WirName` | `GlobalGet`, `GlobalSet` | O(n) string |
| Local scope | `String` | `LocalGet`, `StructGet.field_name`, labels, case names | N/A |

Type and function references in instructions use `WirTypeId` / `WirFuncId` because they are high-frequency (every GC instruction and every call). Global variable references use `WirName` because they are comparatively rare. Local-scope names use plain `String` (no module qualification needed).

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

### Strategy: Strangler Fig Pattern

The migration builds a complete WIR pipeline **alongside** the existing codegen, without modifying `codegen.rs`. Both pipelines consume the same `&Project` after `wasm_plan`. E2E tests validate the new pipeline against the same fixtures. Once all tests pass, the old codegen is replaced and deleted.

```
                                  ┌→ codegen → wasm binary (existing, untouched)
lower → optimize → wasm_plan ─────┤
                                  └→ tir_to_wir → wir_emit → wasm binary (new, tested in parallel)
```

This is a departure from the original plan that required decomposing `codegen.rs` first (Phase 0). That approach was:

- **High risk**: modifying `codegen.rs` while it is the only backend
- **Big-bang**: each step must produce identical output or tests break
- **Previously attempted and abandoned**

The strangler fig approach eliminates these risks:

- **Zero risk to existing tests**: `codegen.rs` is never modified, so all tests always pass
- **No Phase 0**: `codegen.rs` decomposition is unnecessary since we never modify it
- **Living reference**: `codegen.rs` serves as the "correct answer" throughout development
- **Incremental progress**: each feature added to `tir_to_wir` makes more WIR E2E tests pass
- **Inspectable**: `wado dump --wir --unparse` provides visibility into the new pipeline at all times

### Phase 1: Scaffolding and Inspection

Set up the WIR data structures, unparse output, and dump integration. No code generation yet.

- [ ] **Step 1a**: Create `wir.rs` with the WIR data structure definitions (types, instructions, module structure). No code uses it yet.
- [ ] **Step 1b**: Create `wir_unparse.rs` for WIR → pseudo-Wado output.
- [ ] **Step 1c**: Add `--wir` flag to the dump command (`wado dump --wir --unparse`). Initially outputs an empty `WirModule`.
- [ ] **Step 1d**: Move `effect_wait` from `builtin.wado` to `internal.wado` — `generate_effect_wait()` in codegen is a multi-instruction sequence. Refactor it into a Wado function `internal::effect_wait(subtask: i32)`. This removes hard-coded codegen logic and simplifies future WIR translation. (This is the only change that touches existing code, and it is a semantic no-op.)

After this phase: `wado dump --wir --unparse` works but shows an empty module.

### Phase 2: Parallel E2E Test Infrastructure

Set up the mechanism to run the same E2E test fixtures through the WIR pipeline.

- [ ] **Step 2a**: Create `compile_with_wir(&Project) -> Vec<u8>` in a new module. This is the WIR pipeline entry point: calls `tir_to_wir` → `wir_emit` and returns Wasm bytes. Initially returns a minimal valid Wasm component (stub).
- [ ] **Step 2b**: Create `tests/wir_e2e.rs` — a parallel E2E test harness that uses `compile_with_wir` instead of `Codegen::generate_wasm`. Same fixtures, same `__DATA__` specs. Gated by `WADO_WIR_TEST=1` so normal `make test` is unaffected.
- [ ] **Step 2c**: Add a progress tracking mechanism — e.g., a test that counts how many fixtures pass vs. fail through the WIR pipeline, printed as a summary.

After this phase: `WADO_WIR_TEST=1 cargo test --test wir_e2e` runs but all tests fail. The scaffolding for incremental progress is in place.

### Phase 3: Core Translation

The main implementation work. Build `tir_to_wir` and `wir_emit` incrementally. Each feature added makes more E2E tests pass.

#### Step 3a: Type Registration

Translate TIR type definitions to `Vec<WirTypeDef>`. Implement `wir_emit` type section generation.

- [ ] Struct types (fields, mutability, GC layout)
- [ ] Variant types (base struct + case subtypes)
- [ ] Enum types (i32 discriminant, no Wasm type entry)
- [ ] Flags types (i32 bitfield, no Wasm type entry)
- [ ] Array types (element type, mutability)
- [ ] Tuple types (anonymous structs)
- [ ] Closure types (funcref + captured environment struct)
- [ ] Function types
- [ ] Rec groups and topological sorting

#### Step 3b: Module Skeleton

Translate module-level structure to `WirModule`.

- [ ] Import section (WASI functions, bundled modules)
- [ ] Global variables
- [ ] Data section (string literals)
- [ ] Element section (funcref tables)
- [ ] Export section
- [ ] Name section

#### Step 3c: Function Bodies — Basics

Implement `tir_to_wir` expression translation for core constructs.

- [ ] Constants (i32, i64, f32, f64)
- [ ] Local variables (get, set, tee, declare)
- [ ] Arithmetic (i32, i64, f32, f64 — all operators)
- [ ] Comparison and logical operators
- [ ] Type casts and conversions
- [ ] Block, Loop, If/Else, Br, BrIf, BrTable
- [ ] Return, Unreachable, Nop, Drop
- [ ] Function calls (direct)

#### Step 3d: Function Bodies — GC and Compound

- [ ] Struct construction (struct.new)
- [ ] Field access (struct.get) and assignment (struct.set)
- [ ] Array operations (new, get, set, len, copy, fill)
- [ ] Reference operations (ref.null, ref.test, ref.cast, ref.eq)
- [ ] Value copy (ValueCopy compound instruction)
- [ ] Match expressions (pattern dispatch, br_table optimization)
- [ ] Closure creation and call_ref
- [ ] Global get/set

#### Step 3e: Function Bodies — WASI and CM

- [ ] CM effect calls (canonical lift/lower)
- [ ] CM resource method calls
- [ ] CM payload lowering (string, list, record, variant, option, result)
- [ ] CM export glue functions
- [ ] Async CM (subtask handling, waitable sets)

#### Step 3f: Component Model Wrapper

Translate `ComponentPlan` to `WirComponent` and emit the CM wrapper.

- [ ] WASI interface imports
- [ ] Bundled module instantiation (fts, libm)
- [ ] Core module + component composition
- [ ] World export declarations

After this phase: all (or nearly all) E2E tests pass through the WIR pipeline.

### Phase 4: Cutover

Once all E2E tests pass via the WIR pipeline:

- [ ] **Step 4a**: Verify behavioral equivalence across all fixtures and optimization levels.
- [ ] **Step 4b**: Replace `Codegen::generate_wasm` calls in `compile_with_options` with `compile_with_wir`.
- [ ] **Step 4c**: Delete `codegen.rs` and `copy_context.rs`.
- [ ] **Step 4d**: Promote `tests/wir_e2e.rs` to be the primary `e2e.rs` (or remove it if identical).
- [ ] **Step 4e**: Merge `wasm_plan` analysis into `tir_to_wir` where appropriate. The pipeline becomes: `optimize → tir_to_wir → wir_emit`.

### Phase 5 (Future): Optimizer Migration

After WIR is stable, migrate low-level optimizations from TIR to WIR:

- [ ] **Step 5a**: Move constant folding to `wir_optimize`. Pattern: `I32Add(I32Const, I32Const)` → `I32Const`. No TIR dependency.
- [ ] **Step 5b**: Move LICM to `wir_optimize`. Pattern: `Loop` + `StructGet` on non-modified locals. No TIR dependency.
- [ ] **Step 5c**: Move ValueCopy analysis from `optimize_rewrite.rs` to `tir_to_wir`. Fresh value detection and copy type resolution happen during WIR generation instead of as a post-optimization rewrite. Remove `needed_copy_types`, `copy_source_types`, and `Move` from `TirFunction`.
- [ ] **Step 5d**: Move `CopyContext` (scratch local pre-allocation) into `wir_emit`. WIR uses named locals, so pre-allocation is purely an emission concern.
- [ ] **Step 5e**: Add peephole optimizations in `wir_optimize` (redundant `LocalSet`/`LocalGet` elimination, dead `Drop` removal, etc.).

### Final State

```
lower → optimize → tir_to_wir → wir_emit → wasm binary
                        ↓
                    WirModule (inspectable via dump --wir)
```

- `tir_to_wir` (~5000 lines): TIR + Project → WirModule. All analysis, type layout, function translation, ValueCopy insertion.
- `wir_emit` (~2000 lines): WirModule → Wasm binary. Mechanical translation, index allocation, `wasm_encoder` calls, CopyContext local pre-allocation.
- `wir.rs` (~500 lines): Data structure definitions.
- `wir_unparse.rs` (~500 lines): WIR → pseudo-Wado for debugging.

#### Future State (with optimizer migration)

```
lower → tir_optimize → tir_to_wir → wir_optimize → wir_emit → wasm binary
                             ↓
                         WirModule (inspectable via dump --wir)
```

- `tir_optimize`: Semantic optimizations (inlining, DCE, SROA, ref-elim, copy-prop). Operates on TIR with full TypeTable access.
- `tir_to_wir`: TIR → WirModule. Includes ValueCopy insertion (freshness analysis + copy type resolution).
- `wir_optimize`: Wasm-level optimizations (constant folding, LICM, peephole). Operates on WIR where types are embedded in instruction names.
- `wir_emit`: WirModule → Wasm binary. CopyContext, index allocation, `wasm_encoder` calls.

## Design Rationale

### Why Tree-Structured (Not Flat Instructions)?

Wasm is a stack machine, but flat instruction sequences are hard to inspect and manipulate. WIR uses trees where operands are children:

```rust
// WIR (tree): readable, inspectable
// Debug output shows fq names from WirTypeId (not integer indices)
I32Add(
    StructGet { type_id: <entry>//Point, field_name: "x", expr: LocalGet("self") },
    StructGet { type_id: <entry>//Point, field_name: "y", expr: LocalGet("self") },
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

### Why `WirTypeId` / `WirFuncId` with `Rc<str>` (Not Raw Indices or `WirName`)?

WIR instructions reference types and functions at very high frequency (every struct field access, every call, every ref cast). The reference design must balance three goals: emission speed, debuggability, and independence from emission order.

Three alternatives were considered:

1. **Raw `u32` index**: Fastest, but `Debug` output shows `WirTypeId(17)` — unreadable without a lookup table. Debugging the compiler requires constantly cross-referencing indices.
2. **`WirName` (two owned `String`s)**: Most readable, but every clone copies two strings. In a program with thousands of instructions, this means thousands of string allocations. `Eq` and `Hash` require O(n) string operations.
3. **`WirTypeId { index: u32, fq: Rc<str> }` (chosen)**: `Eq`/`Hash` are O(1) via integer comparison. `Debug` prints the fq name for readability. `Clone` is O(1) — just an `Rc` refcount increment (non-atomic, near zero cost). All references to the same type share one `Rc<str>` allocation.

Additional design properties:

- `WirTypeId.index` indexes into `WirModule.types`, not the Wasm type section — WIR remains independent of emission order
- Type and function definitions retain full `WirName` (with `display` + `fq`) for unparse output
- Local-scope names (locals, fields, labels) use plain `String`
- Struct field indices are resolved from type definitions during emission

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

- **Unparse**: `wado dump --wir --unparse` can show `// from ./geometry.wado` comments (from `WirMeta.module_source`), display newtype origins, and annotate generic instantiations.
- **Error messages**: If the emit phase detects a problem, it can report source locations via `WirMeta.span`.
- **Debugging**: When investigating codegen issues, knowing where a type or function came from is critical. `WirMeta.module_source` provides structured access (`is_core()`, `is_wasi()`, etc.); `WirName.fq` provides the serialized identity.
- **Newtypes**: Resolved by the WIR phase (a `Meters` field is `F64` in WIR), but `newtype_origin` records that it was `Meters` from `./physics.wado`. This avoids polluting the emit phase with newtype logic while keeping debug info.

The metadata is lightweight (references and optional fields) and does not affect WIR→Wasm correctness.

### Future: Optimizer on WIR

The current optimizer runs entirely on TIR. The long-term plan is to split optimizations into two levels:

```
Current:  lower → optimize → tir_to_wir → wir_emit
Future:   lower → tir_optimize → tir_to_wir → wir_optimize → wir_emit
```

#### Responsibility Split

**`tir_optimize` (semantic optimizations)** — remains on TIR where richer type information is available:

- Inlining: requires purity analysis (`effects.is_empty()`), return type checks (`ResolvedType::Never`), nested generics detection (`has_nested_generics()`), expression counting
- SROA: requires `Let` + `StructLiteral` pattern matching, `is_mut` on let bindings, escape analysis
- Reference elimination: requires `Let` + `Ref(Local)` pattern, `address_taken_locals`, field access tracking
- Copy propagation: requires `Let` + `Local/Literal` pattern, `is_mut`, use counting
- DCE: works on both TIR and WIR, but entry point analysis uses TIR metadata

**`wir_optimize` (Wasm-level optimizations)** — operates on WIR where types are embedded in instructions:

- Constant folding: `I32Add(I32Const(1), I32Const(2))` → `I32Const(3)`. Type is encoded in instruction names — no TypeTable needed.
- LICM: `Loop { body }` + `StructGet` hoisting. Modified locals detectable from `LocalSet`. Loop structure is explicit.
- Peephole: redundant `LocalSet`/`LocalGet` pairs, dead `Drop`, etc.

#### Why This Split Works

WIR does not need to carry `is_mut`, `address_taken_locals`, or `TypeTable` — those are only needed by semantic optimizations that stay on TIR. WIR-level optimizations rely on information already embedded in instruction names and structure.

#### ValueCopy Belongs in `tir_to_wir`, Not the Optimizer

The current `optimize_rewrite.rs` performs three tasks that are not optimizations:

1. **Fresh value detection** (`is_fresh_value()`): classifies whether an expression produces a new value (literal, call result, constructor) vs. referencing an existing value (local, field access)
2. **Copy type collection** (`collect_value_copy_types_in_*()`): determines which types need deep copy operations
3. **Move insertion** (`insert_moves_in_*()`): wraps fresh values in `Move { expr }` to suppress unnecessary copies

These are **lowering concerns** — they implement Wasm GC value semantics, not performance optimizations. They are placed in the optimizer solely because they must run after inlining stabilizes function bodies.

In the future pipeline, these move into `tir_to_wir`:

- `tir_to_wir` emits `ValueCopy { type_name, source_type, expr }` for non-fresh assignments to value types
- `tir_to_wir` omits `ValueCopy` for fresh values (the TIR `Move` wrapper or WIR-level freshness analysis)
- The `ValueCopy` compound instruction carries its own `WirCopyType`, which `wir_emit` lowers to copy instructions
- `CopyContext` (scratch local pre-allocation) moves entirely into `wir_emit`, since WIR uses named locals and index allocation is an emission concern

This eliminates the current coupling between the optimizer and codegen via `needed_copy_types` / `copy_source_types` / `Move` on `TirFunction`.

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
- **No Wasm-level optimization yet**: TIR optimization is sufficient for now. A `wir_optimize` pass can be added later without architectural changes.

### Risks

- **Scope creep**: WIR should stay close to Wasm. Resist adding high-level abstractions.
- **`WirTypeId` is not `Copy`**: `Rc<str>` prevents `Copy`. Mitigated by cheap `Clone` (non-atomic refcount increment) and instructions being heap-allocated (`Box<WirInstr>`) regardless.
- **Two code paths during migration**: The strangler fig approach intentionally maintains two pipelines (`codegen` and `tir_to_wir` + `wir_emit`) until cutover. This is safe because the existing pipeline is never modified — it always produces correct output. The new pipeline is tested independently via `wir_e2e.rs`. The risk is that migration stalls and the duplicate code persists indefinitely; mitigated by Phase 2's progress tracking which gives clear visibility into how close the WIR pipeline is to completion.
