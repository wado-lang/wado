//! WIR (Wasm IR) — tree-structured intermediate representation between TIR and Wasm binary.
//!
//! WIR is close to Wasm semantics but retains enough high-level information to be
//! readable and debuggable. It maps almost 1:1 to Wasm instructions with these
//! ergonomic improvements:
//!
//! - Named locals (not pre-allocated indices)
//! - Named types (Wado-level struct/variant/enum/flags, not Wasm type section entries)
//! - Wado-level value types (Bool, Char, I8, U8, etc. instead of Wasm's i32-for-everything)
//! - Structured control flow (tree nodes, not flat instruction sequences)
//! - Explicit value copy operations
//! - TIR metadata preserved for debugging and unparse
//!
//! See `docs/wep-2026-02-14-wir-layer.md` for the full design rationale.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::name::ModuleSource;
use crate::token::Span;

// =============================================================================
// Module Level
// =============================================================================

/// A complete Wasm module in WIR form.
/// Contains all information needed to emit a valid Wasm binary.
#[derive(Debug)]
pub struct WirModule {
    /// Type definitions: Wado-level types (struct, variant, enum, flags, array, func).
    /// Not a 1:1 mapping to the Wasm type section — the emit phase expands these.
    pub types: Vec<WirTypeDef>,
    /// Rec groups: which types form recursive groups.
    pub rec_groups: Vec<WirRecGroup>,
    /// Import section.
    pub imports: Vec<WirImport>,
    /// Function declarations with bodies.
    pub functions: Vec<WirFunction>,
    /// Global variables.
    pub globals: Vec<WirGlobal>,
    /// Export section.
    pub exports: Vec<WirExport>,
    /// Element section (for funcref tables).
    pub elements: Vec<WirElement>,
    /// Data section (string literals, etc.).
    pub data: Vec<WirData>,
    /// Branch hints (from likely/unlikely).
    pub branch_hints: Vec<WirBranchHint>,
    /// Name section entries.
    pub names: WirNames,
    /// Component Model wrapper info.
    pub component: WirComponent,
    /// Variant case type info: case WIR type index → (variant WIR type index, case index).
    /// Used by emitter to resolve case-specific struct types within variant rec groups.
    pub variant_case_info: IndexMap<u32, (u32, u32)>,
    /// Entry-point module path string (for display shortening in unparse).
    pub entry_point_path: Option<String>,
}

impl WirModule {
    /// Create an empty `WirModule` with no types, functions, or other content.
    pub fn empty() -> Self {
        Self {
            types: Vec::new(),
            rec_groups: Vec::new(),
            imports: Vec::new(),
            functions: Vec::new(),
            globals: Vec::new(),
            exports: Vec::new(),
            elements: Vec::new(),
            data: Vec::new(),
            branch_hints: Vec::new(),
            names: WirNames::default(),
            component: WirComponent::default(),
            variant_case_info: IndexMap::new(),
            entry_point_path: None,
        }
    }
}

// =============================================================================
// Names
// =============================================================================

/// A globally-scoped name in WIR.
///
/// Carries both forms needed by different consumers:
/// - `display`: short name for unparse and error messages
/// - `fq`: module-qualified name for unique identity
///
/// The `tir_to_wir` phase constructs `WirName`s from `name.rs` objects
/// (`StructName`, `FunctionId`, etc.). WIR itself does not import `name.rs` types.
#[derive(Debug, Clone)]
pub struct WirName {
    /// Short display name (e.g., "Point", "Array<i32>", "`Point::sum`").
    /// Used by unparse and error messages.
    pub display: String,
    /// Fully-qualified name (e.g., "core:prelude//Point", "./`geometry.wado/Point::sum`").
    /// Used as the unique identity key for name→index resolution during emission.
    pub fq: String,
}

impl fmt::Display for WirName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display)
    }
}

// =============================================================================
// Type and Function IDs
// =============================================================================

/// Lightweight reference to a type definition in `WirModule.types`.
///
/// - `index`: indexes into `WirModule.types` (not the Wasm type section)
/// - `fq`: fully-qualified name shared via `Rc<str>` for Debug output
///
/// `Eq` and `Hash` use `index` only (O(1) integer operations).
/// `Debug` prints the fq name (e.g., "core:prelude//Array<i32>").
/// `Clone` is O(1) — Rc refcount increment (non-atomic, near zero cost).
#[derive(Clone)]
pub struct WirTypeId {
    index: u32,
    fq: Rc<str>,
}

impl WirTypeId {
    /// Create a new `WirTypeId` with the given index and fully-qualified name.
    pub fn new(index: u32, fq: Rc<str>) -> Self {
        Self { index, fq }
    }

    /// Get the index into `WirModule.types`.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Get the fully-qualified name.
    pub fn fq(&self) -> &str {
        &self.fq
    }
}

impl PartialEq for WirTypeId {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for WirTypeId {}

impl Hash for WirTypeId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl fmt::Debug for WirTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.fq)
    }
}

impl fmt::Display for WirTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.fq)
    }
}

/// Lightweight reference to a function.
/// Same design as `WirTypeId`.
#[derive(Clone)]
pub struct WirFuncId {
    index: u32,
    fq: Rc<str>,
}

impl WirFuncId {
    /// Create a new `WirFuncId` with the given index and fully-qualified name.
    pub fn new(index: u32, fq: Rc<str>) -> Self {
        Self { index, fq }
    }

    /// Get the index into `WirModule.functions`.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Get the fully-qualified name.
    pub fn fq(&self) -> &str {
        &self.fq
    }
}

impl PartialEq for WirFuncId {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for WirFuncId {}

impl Hash for WirFuncId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl fmt::Debug for WirFuncId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.fq)
    }
}

impl fmt::Display for WirFuncId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.fq)
    }
}

// =============================================================================
// Metadata
// =============================================================================

/// Source location and origin metadata, carried through from TIR.
#[derive(Debug, Clone, Default)]
pub struct WirMeta {
    /// Which module this entity was defined in.
    pub module_source: Option<ModuleSource>,
    /// Source span in the original Wado source.
    pub span: Option<Span>,
    /// Attributes (e.g., `#[hidden]`).
    pub attributes: Vec<WirAttribute>,
}

/// An attribute on a WIR entity.
#[derive(Debug, Clone)]
pub enum WirAttribute {
    /// `#[hidden]` — field not shown in debug stringify.
    Hidden,
}

/// Generic instantiation origin (e.g., `Array<i32>` from `Array<T>`).
#[derive(Debug, Clone)]
pub struct WirGenericOrigin {
    /// Base generic name (e.g., "Array", "Box").
    pub base_name: String,
    /// Type arguments used for instantiation (e.g., ["i32"]).
    pub type_args: Vec<String>,
}

/// Newtype origin — when a type was originally a newtype alias.
#[derive(Debug, Clone)]
pub struct WirNewtypeOrigin {
    /// Newtype name (e.g., "Meters").
    pub name: String,
    /// Module where the newtype was defined.
    pub module_source: ModuleSource,
}

// =============================================================================
// Type Definitions
// =============================================================================

/// A Wado-level type definition.
/// The emit phase expands these into Wasm type section entries:
///   Struct → 1 Wasm struct type
///   Variant → N+1 Wasm struct types (base with discriminant field + case subtypes)
///   Enum → none (represented as i32)
///   Flags → none (represented as i32 bitfield)
///   Array → 1 Wasm array type
///   Func → 1 Wasm func type
#[derive(Debug)]
pub enum WirTypeDef {
    /// Struct type with named fields.
    Struct(WirStructType),
    /// Variant type (sum type with payloads).
    Variant(WirVariantType),
    /// Enum type (discriminated values without payloads).
    Enum(WirEnumType),
    /// Flags type (bitfield).
    Flags(WirFlagsType),
    /// Array type.
    Array(WirArrayType),
    /// Function type.
    Func(WirFuncType),
}

// =============================================================================
// Struct
// =============================================================================

/// A struct type with named fields.
#[derive(Debug)]
pub struct WirStructType {
    /// Name (display: "Point", fq: "core:prelude//Point").
    pub name: WirName,
    /// Fields with names and types.
    pub fields: Vec<WirField>,
    /// Metadata (module source, span, attributes).
    pub meta: WirMeta,
    /// Generic instantiation origin (None for non-generic types).
    pub generic_origin: Option<WirGenericOrigin>,
    /// If this type was a newtype in the source (resolved by WIR phase).
    pub newtype_origin: Option<WirNewtypeOrigin>,
}

/// A field in a struct type.
#[derive(Debug)]
pub struct WirField {
    /// Source-level field name (e.g., "x", "repr", "discriminant").
    pub name: String,
    /// WIR type (uses Wado-level primitives, not Wasm `ValType`).
    pub ty: WirType,
    /// Whether this field is mutable.
    pub mutable: bool,
}

// =============================================================================
// Variant
// =============================================================================

/// A variant type (sum type with payloads).
/// At the Wasm level, expands to a subtype hierarchy: a base struct with a
/// `discriminant` field, and per-case subtypes that add payload fields.
#[derive(Debug)]
pub struct WirVariantType {
    /// Name (display: "Shape", fq: "./shapes.wado//Shape").
    pub name: WirName,
    /// Cases with names and optional payload types.
    pub cases: Vec<WirVariantCase>,
    /// Metadata (module source, span, attributes).
    pub meta: WirMeta,
    /// Generic instantiation origin (None for non-generic types).
    pub generic_origin: Option<WirGenericOrigin>,
    /// If this type was a newtype in the source (resolved by WIR phase).
    pub newtype_origin: Option<WirNewtypeOrigin>,
}

/// A case in a variant type.
#[derive(Debug, Clone)]
pub struct WirVariantCase {
    /// Case name (e.g., "Circle", "Ok").
    pub name: String,
    /// Case index (discriminant value).
    pub index: u32,
    /// Payload field types (empty for unit cases like "Point" or "None").
    pub payload: Vec<WirType>,
}

// =============================================================================
// Enum
// =============================================================================

/// An enum type (discriminated values without payloads).
/// No Wasm type section entry — values are i32 discriminants.
#[derive(Debug)]
pub struct WirEnumType {
    /// Name (display: "Color", fq: "./colors.wado//Color").
    pub name: WirName,
    /// Cases with names and discriminant values.
    pub cases: Vec<WirEnumCase>,
    /// Metadata (module source, span, attributes).
    pub meta: WirMeta,
}

/// A case in an enum type.
#[derive(Debug)]
pub struct WirEnumCase {
    /// Case name (e.g., "Red", "Less").
    pub name: String,
    /// Discriminant value.
    pub discriminant: i32,
}

// =============================================================================
// Flags
// =============================================================================

/// A flags type (bitfield).
/// No Wasm type section entry — values are i32 bitfields.
#[derive(Debug)]
pub struct WirFlagsType {
    /// Name (display: "Permissions", fq: "./perms.wado//Permissions").
    pub name: WirName,
    /// Flag bits with names and positions.
    pub bits: Vec<WirFlagBit>,
    /// Metadata (module source, span, attributes).
    pub meta: WirMeta,
}

/// A flag bit in a flags type.
#[derive(Debug)]
pub struct WirFlagBit {
    /// Flag name.
    pub name: String,
    /// Bit position (0-indexed).
    pub position: u32,
}

// =============================================================================
// Array and Func Types
// =============================================================================

/// An array type (maps to 1 Wasm array type).
#[derive(Debug)]
pub struct WirArrayType {
    /// Name.
    pub name: WirName,
    /// Element type.
    pub element_type: WirType,
    /// Whether elements are mutable.
    pub mutable: bool,
    /// Metadata.
    pub meta: WirMeta,
    /// Generic instantiation origin.
    pub generic_origin: Option<WirGenericOrigin>,
}

/// A function type (maps to 1 Wasm func type).
#[derive(Debug)]
pub struct WirFuncType {
    /// Name.
    pub name: WirName,
    /// Parameter types.
    pub params: Vec<WirType>,
    /// Result types.
    pub results: Vec<WirType>,
}

// =============================================================================
// WIR Type System
// =============================================================================

/// WIR type — Wado-level primitives + GC references.
///
/// Unlike Wasm's `ValType` (i32/i64/f32/f64 only), WIR preserves the full
/// Wado type distinctions. The emit phase lowers these:
///   - I8/I16/U8/U16/Bool/Char → i32 (locals) or i8/i16 (packed struct fields)
///   - I32/U32 → i32
///   - I64/U64 → i64
///   - F32 → f32
///   - F64 → f64
///   - Enum/Flags → i32
#[derive(Debug, Clone)]
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
    /// Named enum type (i32 at Wasm level).
    Enum {
        type_id: WirTypeId,
    },
    /// Named flags type (i32 at Wasm level).
    Flags {
        type_id: WirTypeId,
    },
    /// Reference to a named GC type.
    Ref {
        type_id: WirTypeId,
        nullable: bool,
    },
    /// Abstract reference type (anyref, funcref, etc.).
    AbstractRef {
        heap_type: WirAbstractHeapType,
        nullable: bool,
    },
}

/// Abstract heap types for Wasm GC.
#[derive(Debug, Clone, PartialEq, Eq)]
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

// =============================================================================
// Functions
// =============================================================================

/// A function declaration with optional body.
#[derive(Debug)]
pub struct WirFunction {
    /// Function name (display: "`Point::sum`", fq: "./`geometry.wado/Point::sum`").
    pub name: WirName,
    /// Function type ID (references a `WirTypeDef::Func` in `WirModule.types`).
    pub type_id: WirTypeId,
    /// Parameter names (types come from the referenced `WirFuncType`).
    pub param_names: Vec<String>,
    /// Body (None for imported functions).
    pub body: Option<Vec<WirInstr>>,
    /// Metadata (module source, span, attributes).
    pub meta: WirMeta,
    /// Generic instantiation origin.
    pub generic_origin: Option<WirGenericOrigin>,
    /// Effect requirements (for unparse display).
    pub effects: Vec<String>,
}

// =============================================================================
// Instructions
// =============================================================================

/// WIR instructions are tree-structured where operands are child nodes,
/// not stack values. This makes the structure inspectable and allows inline
/// local declaration.
#[derive(Debug, Clone)]
pub enum WirInstr {
    // === Locals ===
    /// Declare a new local variable inline (not in Wasm; lowered to pre-allocated local).
    DeclareLocal {
        name: String,
        ty: WirType,
    },
    /// `local.get` by name.
    LocalGet {
        name: String,
    },
    /// `local.set` by name.
    LocalSet {
        name: String,
        value: Box<WirInstr>,
    },
    /// `local.tee` by name.
    LocalTee {
        name: String,
        value: Box<WirInstr>,
    },

    // === Globals ===
    /// `global.get` by name.
    GlobalGet {
        name: WirName,
    },
    /// `global.set` by name.
    GlobalSet {
        name: WirName,
        value: Box<WirInstr>,
    },

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

    /// Emit a multi-value instruction and wrap the results in a struct.
    /// Used for `i64.add128`, `i64.sub128`, `i64.mul_wide_u/s` which push two i64
    /// values on the stack, then `StructNew` wraps them into a tuple struct.
    MultiValueStructNew {
        type_id: WirTypeId,
        instr: Box<WirInstr>,
    },

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
    /// `struct.new` with type ID.
    StructNew {
        type_id: WirTypeId,
        fields: Vec<WirInstr>,
    },
    /// `struct.get` with type ID and field name (field index resolved by emitter).
    StructGet {
        type_id: WirTypeId,
        field_name: String,
        expr: Box<WirInstr>,
    },
    /// `struct.set` with type ID and field name (field index resolved by emitter).
    StructSet {
        type_id: WirTypeId,
        field_name: String,
        expr: Box<WirInstr>,
        value: Box<WirInstr>,
    },

    // === GC: Array ===
    ArrayNew {
        type_id: WirTypeId,
        init: Box<WirInstr>,
        len: Box<WirInstr>,
    },
    ArrayNewDefault {
        type_id: WirTypeId,
        len: Box<WirInstr>,
    },
    ArrayNewData {
        type_id: WirTypeId,
        data_index: u32,
        offset: Box<WirInstr>,
        len: Box<WirInstr>,
    },
    ArrayNewFixed {
        type_id: WirTypeId,
        elements: Vec<WirInstr>,
    },
    ArrayGet {
        type_id: WirTypeId,
        array: Box<WirInstr>,
        index: Box<WirInstr>,
    },
    ArrayGetS {
        type_id: WirTypeId,
        array: Box<WirInstr>,
        index: Box<WirInstr>,
    },
    ArrayGetU {
        type_id: WirTypeId,
        array: Box<WirInstr>,
        index: Box<WirInstr>,
    },
    ArraySet {
        type_id: WirTypeId,
        array: Box<WirInstr>,
        index: Box<WirInstr>,
        value: Box<WirInstr>,
    },
    ArrayLen(Box<WirInstr>),
    ArrayCopy {
        dest_type_id: WirTypeId,
        src_type_id: WirTypeId,
        dest: Box<WirInstr>,
        dest_offset: Box<WirInstr>,
        src: Box<WirInstr>,
        src_offset: Box<WirInstr>,
        len: Box<WirInstr>,
    },
    ArrayFill {
        type_id: WirTypeId,
        array: Box<WirInstr>,
        offset: Box<WirInstr>,
        value: Box<WirInstr>,
        len: Box<WirInstr>,
    },

    // === GC: Reference ===
    RefNull {
        heap_type: WirAbstractHeapType,
    },
    RefIsNull(Box<WirInstr>),
    RefAsNonNull(Box<WirInstr>),
    RefCast {
        type_id: WirTypeId,
        nullable: bool,
        expr: Box<WirInstr>,
    },
    RefTest {
        type_id: WirTypeId,
        nullable: bool,
        expr: Box<WirInstr>,
    },
    RefEq(Box<WirInstr>, Box<WirInstr>),
    RefI31(Box<WirInstr>),
    I31GetS(Box<WirInstr>),
    I31GetU(Box<WirInstr>),
    ExternInternalize(Box<WirInstr>),
    ExternExternalize(Box<WirInstr>),

    // === Control Flow ===
    /// Block with optional label and result type.
    Block {
        label: Option<String>,
        result: Option<WirType>,
        body: Vec<WirInstr>,
    },
    /// Loop with optional label.
    Loop {
        label: Option<String>,
        body: Vec<WirInstr>,
    },
    /// If/else with optional result type.
    If {
        condition: Box<WirInstr>,
        result: Option<WirType>,
        then_body: Vec<WirInstr>,
        else_body: Option<Vec<WirInstr>>,
    },
    /// Branch hint annotation (from `builtin::likely`/`builtin::unlikely`).
    /// Wraps a condition expression; consumed by the emitter when it appears
    /// as the condition of an `If` instruction.
    BranchHint {
        likely: bool,
        expr: Box<WirInstr>,
    },
    /// Branch to label.
    Br {
        depth: u32,
    },
    /// Conditional branch.
    BrIf {
        depth: u32,
        condition: Box<WirInstr>,
    },
    /// Branch table (switch).
    BrTable {
        index: Box<WirInstr>,
        targets: Vec<u32>,
        default: u32,
    },
    /// Return from function.
    Return {
        value: Option<Box<WirInstr>>,
    },
    /// Unreachable trap.
    Unreachable,
    /// No operation (for structure).
    Nop,
    /// Drop a value.
    Drop(Box<WirInstr>),
    /// Select between two values.
    Select {
        condition: Box<WirInstr>,
        if_true: Box<WirInstr>,
        if_false: Box<WirInstr>,
        ty: Option<WirType>,
    },

    // === Calls ===
    Call {
        func_id: WirFuncId,
        args: Vec<WirInstr>,
    },
    CallIndirect {
        type_id: WirTypeId,
        table: u32,
        index: Box<WirInstr>,
        args: Vec<WirInstr>,
    },
    CallRef {
        type_id: WirTypeId,
        func_ref: Box<WirInstr>,
        args: Vec<WirInstr>,
    },
    RefFunc {
        func_id: WirFuncId,
    },

    // === Memory ===
    MemorySize,
    MemoryGrow(Box<WirInstr>),
    I32Load {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
    },
    I32Load8U {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
    },
    I32Load8S {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
    },
    I32Load16U {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
    },
    I32Load16S {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
    },
    I32Store {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
        value: Box<WirInstr>,
    },
    I32Store8 {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
        value: Box<WirInstr>,
    },
    I32Store16 {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
        value: Box<WirInstr>,
    },
    I64Load {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
    },
    I64Store {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
        value: Box<WirInstr>,
    },

    // === Table ===
    TableGet {
        table: u32,
        index: Box<WirInstr>,
    },
    TableSet {
        table: u32,
        index: Box<WirInstr>,
        value: Box<WirInstr>,
    },

    // === High-level compound instructions (lowered to sequences during emission) ===
    /// Deep copy of a value type (struct, array, variant, option, tuple).
    /// Lowered to field-by-field copy, array loop, etc. during emission.
    ValueCopy {
        type_id: WirTypeId,
        source_type: WirCopyType,
        expr: Box<WirInstr>,
    },

    /// Multi-value instruction with direct local binding (tuple elision).
    /// The instruction pushes N values on the stack; they are bound directly
    /// to locals without intermediate struct allocation.
    /// Locals are in source order (index 0 = bottom of stack).
    /// `None` entries represent wildcards (dropped values).
    MultiValueLocalBind {
        instr: Box<WirInstr>,
        locals: Vec<Option<String>>,
    },

    /// Sequence of instructions (for statement blocks).
    Seq(Vec<WirInstr>),
}

impl WirInstr {
    /// Visit all child instructions of this node (non-recursive).
    /// Used by the emitter for pre-scanning (e.g., collecting `DeclareLocal`).
    pub fn for_each_child(&self, f: &mut impl FnMut(&WirInstr)) {
        match self {
            // Leaf nodes
            Self::I32Const(_)
            | Self::I64Const(_)
            | Self::F32Const(_)
            | Self::F64Const(_)
            | Self::LocalGet { .. }
            | Self::GlobalGet { .. }
            | Self::RefNull { .. }
            | Self::Nop
            | Self::Unreachable
            | Self::MemorySize
            | Self::Br { .. }
            | Self::DeclareLocal { .. }
            | Self::RefFunc { .. }
            | Self::Return { value: None } => {}
            Self::Return { value } => {
                if let Some(v) = value {
                    f(v);
                }
            }
            // Unary Box<WirInstr>
            Self::LocalSet { value, .. }
            | Self::LocalTee { value, .. }
            | Self::GlobalSet { value, .. } => f(value),
            Self::StructGet { expr, .. }
            | Self::RefCast { expr, .. }
            | Self::RefTest { expr, .. }
            | Self::ValueCopy { expr, .. } => f(expr),
            Self::BrIf { condition, .. } | Self::BranchHint { expr: condition, .. } => {
                f(condition);
            }
            Self::BrTable { index, .. } => f(index),
            Self::ArrayNewDefault { len, .. } => f(len),
            Self::Drop(o)
            | Self::MemoryGrow(o)
            | Self::I32Eqz(o)
            | Self::I64Eqz(o)
            | Self::I32WrapI64(o)
            | Self::I64ExtendI32S(o)
            | Self::I64ExtendI32U(o)
            | Self::I32Clz(o)
            | Self::I32Ctz(o)
            | Self::I32Popcnt(o)
            | Self::I64Clz(o)
            | Self::I64Ctz(o)
            | Self::I64Popcnt(o)
            | Self::I32TruncF64S(o)
            | Self::I32TruncF64U(o)
            | Self::I32TruncF32S(o)
            | Self::I32TruncF32U(o)
            | Self::I64TruncF64S(o)
            | Self::I64TruncF64U(o)
            | Self::I64TruncF32S(o)
            | Self::I64TruncF32U(o)
            | Self::I32ReinterpretF32(o)
            | Self::F32ReinterpretI32(o)
            | Self::I64ReinterpretF64(o)
            | Self::F64ReinterpretI64(o)
            | Self::I32Extend8S(o)
            | Self::I32Extend16S(o)
            | Self::F32Neg(o)
            | Self::F32Abs(o)
            | Self::F32Ceil(o)
            | Self::F32Floor(o)
            | Self::F32Trunc(o)
            | Self::F32Nearest(o)
            | Self::F32Sqrt(o)
            | Self::F32ConvertI32S(o)
            | Self::F32ConvertI32U(o)
            | Self::F32ConvertI64S(o)
            | Self::F32ConvertI64U(o)
            | Self::F32DemoteF64(o)
            | Self::F64Neg(o)
            | Self::F64Abs(o)
            | Self::F64Ceil(o)
            | Self::F64Floor(o)
            | Self::F64Trunc(o)
            | Self::F64Nearest(o)
            | Self::F64Sqrt(o)
            | Self::F64ConvertI32S(o)
            | Self::F64ConvertI32U(o)
            | Self::F64ConvertI64S(o)
            | Self::F64ConvertI64U(o)
            | Self::F64PromoteF32(o)
            | Self::RefIsNull(o)
            | Self::RefAsNonNull(o)
            | Self::RefI31(o)
            | Self::I31GetS(o)
            | Self::I31GetU(o)
            | Self::ExternInternalize(o)
            | Self::ExternExternalize(o)
            | Self::ArrayLen(o) => f(o),
            // Binary Box<WirInstr>
            Self::I32Add(l, r)
            | Self::I32Sub(l, r)
            | Self::I32Mul(l, r)
            | Self::I32DivS(l, r)
            | Self::I32DivU(l, r)
            | Self::I32RemS(l, r)
            | Self::I32RemU(l, r)
            | Self::I32And(l, r)
            | Self::I32Or(l, r)
            | Self::I32Xor(l, r)
            | Self::I32Shl(l, r)
            | Self::I32ShrS(l, r)
            | Self::I32ShrU(l, r)
            | Self::I32Eq(l, r)
            | Self::I32Ne(l, r)
            | Self::I32LtS(l, r)
            | Self::I32LtU(l, r)
            | Self::I32GtS(l, r)
            | Self::I32GtU(l, r)
            | Self::I32LeS(l, r)
            | Self::I32LeU(l, r)
            | Self::I32GeS(l, r)
            | Self::I32GeU(l, r)
            | Self::I64Add(l, r)
            | Self::I64Sub(l, r)
            | Self::I64Mul(l, r)
            | Self::I64DivS(l, r)
            | Self::I64DivU(l, r)
            | Self::I64RemS(l, r)
            | Self::I64RemU(l, r)
            | Self::I64And(l, r)
            | Self::I64Or(l, r)
            | Self::I64Xor(l, r)
            | Self::I64Shl(l, r)
            | Self::I64ShrS(l, r)
            | Self::I64ShrU(l, r)
            | Self::I64Eq(l, r)
            | Self::I64Ne(l, r)
            | Self::I64LtS(l, r)
            | Self::I64LtU(l, r)
            | Self::I64GtS(l, r)
            | Self::I64GtU(l, r)
            | Self::I64LeS(l, r)
            | Self::I64LeU(l, r)
            | Self::I64GeS(l, r)
            | Self::I64GeU(l, r)
            | Self::I64MulWideU(l, r)
            | Self::I64MulWideS(l, r)
            | Self::F32Add(l, r)
            | Self::F32Sub(l, r)
            | Self::F32Mul(l, r)
            | Self::F32Div(l, r)
            | Self::F32Min(l, r)
            | Self::F32Max(l, r)
            | Self::F32Copysign(l, r)
            | Self::F32Eq(l, r)
            | Self::F32Ne(l, r)
            | Self::F32Lt(l, r)
            | Self::F32Gt(l, r)
            | Self::F32Le(l, r)
            | Self::F32Ge(l, r)
            | Self::F64Add(l, r)
            | Self::F64Sub(l, r)
            | Self::F64Mul(l, r)
            | Self::F64Div(l, r)
            | Self::F64Min(l, r)
            | Self::F64Max(l, r)
            | Self::F64Copysign(l, r)
            | Self::F64Eq(l, r)
            | Self::F64Ne(l, r)
            | Self::F64Lt(l, r)
            | Self::F64Gt(l, r)
            | Self::F64Le(l, r)
            | Self::F64Ge(l, r)
            | Self::RefEq(l, r) => {
                f(l);
                f(r);
            }
            Self::StructSet { expr, value, .. } => {
                f(expr);
                f(value);
            }
            Self::ArrayNew { init, len, .. }
            | Self::ArrayNewData {
                offset: init, len, ..
            } => {
                f(init);
                f(len);
            }
            Self::ArrayGet { array, index, .. }
            | Self::ArrayGetS { array, index, .. }
            | Self::ArrayGetU { array, index, .. } => {
                f(array);
                f(index);
            }
            Self::ArraySet {
                array,
                index,
                value,
                ..
            } => {
                f(array);
                f(index);
                f(value);
            }
            Self::ArrayFill {
                array,
                offset,
                value,
                len,
                ..
            } => {
                f(array);
                f(offset);
                f(value);
                f(len);
            }
            Self::ArrayCopy {
                dest,
                dest_offset,
                src,
                src_offset,
                len,
                ..
            } => {
                f(dest);
                f(dest_offset);
                f(src);
                f(src_offset);
                f(len);
            }
            Self::Select {
                condition,
                if_true,
                if_false,
                ..
            } => {
                f(condition);
                f(if_true);
                f(if_false);
            }
            Self::I64Add128(a, b, c, d) | Self::I64Sub128(a, b, c, d) => {
                f(a);
                f(b);
                f(c);
                f(d);
            }
            Self::MultiValueStructNew { instr, .. }
            | Self::MultiValueLocalBind { instr, .. } => f(instr),
            Self::TableGet { index, .. } => f(index),
            Self::TableSet { index, value, .. } => {
                f(index);
                f(value);
            }
            // Memory operations
            Self::I32Load { addr, .. }
            | Self::I32Load8U { addr, .. }
            | Self::I32Load8S { addr, .. }
            | Self::I32Load16U { addr, .. }
            | Self::I32Load16S { addr, .. }
            | Self::I64Load { addr, .. } => f(addr),
            Self::I32Store { addr, value, .. }
            | Self::I32Store8 { addr, value, .. }
            | Self::I32Store16 { addr, value, .. }
            | Self::I64Store { addr, value, .. } => {
                f(addr);
                f(value);
            }
            // Vec<WirInstr> children
            Self::StructNew { fields, .. }
            | Self::ArrayNewFixed {
                elements: fields, ..
            } => {
                for child in fields {
                    f(child);
                }
            }
            Self::Call { args, .. } => {
                for arg in args {
                    f(arg);
                }
            }
            Self::CallIndirect { args, index, .. } => {
                for arg in args {
                    f(arg);
                }
                f(index);
            }
            Self::CallRef { args, func_ref, .. } => {
                for arg in args {
                    f(arg);
                }
                f(func_ref);
            }
            // Control flow with body
            Self::Block { body, .. } | Self::Loop { body, .. } => {
                for child in body {
                    f(child);
                }
            }
            Self::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                f(condition);
                for child in then_body {
                    f(child);
                }
                if let Some(eb) = else_body {
                    for child in eb {
                        f(child);
                    }
                }
            }
            Self::Seq(body) => {
                for child in body {
                    f(child);
                }
            }
        }
    }
}

// =============================================================================
// Copy Types
// =============================================================================

/// What kind of value copy to perform.
#[derive(Debug, Clone)]
pub enum WirCopyType {
    Struct {
        fields: Vec<WirCopyField>,
    },
    Array {
        element_copy: Option<Box<WirCopyType>>,
    },
    Variant {
        cases: Vec<WirCopyCase>,
    },
    Option {
        inner_copy: Box<WirCopyType>,
    },
    Tuple {
        field_copies: Vec<Option<WirCopyType>>,
    },
}

/// A field in a struct copy.
#[derive(Debug, Clone)]
pub struct WirCopyField {
    pub index: u32,
    pub needs_copy: bool,
    pub copy_type: Option<WirCopyType>,
}

/// A case in a variant copy.
#[derive(Debug, Clone)]
pub struct WirCopyCase {
    pub index: u32,
    pub name: String,
    pub payload_copy: Option<WirCopyType>,
}

// =============================================================================
// Module-level Sections
// =============================================================================

/// Rec group: a set of mutually-recursive type definitions.
#[derive(Debug)]
pub struct WirRecGroup {
    /// Indices into `WirModule.types`.
    pub type_indices: Vec<u32>,
}

/// An import entry.
#[derive(Debug)]
pub struct WirImport {
    /// Import module name.
    pub module: String,
    /// Import field name.
    pub field: String,
    /// What is being imported.
    pub desc: WirImportDesc,
}

/// Import descriptor.
#[derive(Debug)]
pub enum WirImportDesc {
    Func {
        type_id: WirTypeId,
        name: WirName,
    },
    Global {
        ty: WirType,
        mutable: bool,
    },
    Memory {
        min: u32,
        max: Option<u32>,
    },
    Table {
        ty: WirType,
        min: u32,
        max: Option<u32>,
    },
}

/// A global variable.
#[derive(Debug)]
pub struct WirGlobal {
    /// Global name.
    pub name: WirName,
    /// Value type.
    pub ty: WirType,
    /// Whether the global is mutable.
    pub mutable: bool,
    /// Initial value expression.
    pub init: WirInstr,
    /// Metadata.
    pub meta: WirMeta,
}

/// An export entry.
#[derive(Debug)]
pub struct WirExport {
    /// Export name (as seen by the host).
    pub name: String,
    /// What is being exported.
    pub desc: WirExportDesc,
}

/// Export descriptor.
#[derive(Debug)]
pub enum WirExportDesc {
    Func { func_id: WirFuncId },
    Global { name: WirName },
    Memory,
    Table { index: u32 },
}

/// An element segment (for funcref tables).
#[derive(Debug)]
pub struct WirElement {
    /// Table index (usually 0).
    pub table: u32,
    /// Offset expression.
    pub offset: WirInstr,
    /// Function references.
    pub func_ids: Vec<WirFuncId>,
}

/// A data segment.
#[derive(Debug)]
pub struct WirData {
    /// Segment data bytes.
    pub bytes: Vec<u8>,
    /// Optional memory offset (active segment). None for passive segments.
    pub offset: Option<WirInstr>,
}

/// A branch hint annotation.
#[derive(Debug)]
pub struct WirBranchHint {
    /// Function index.
    pub func_index: u32,
    /// Instruction offset within the function.
    pub instr_offset: u32,
    /// Hint: true = likely, false = unlikely.
    pub likely: bool,
}

/// Name section entries for debugging.
#[derive(Debug, Default)]
pub struct WirNames {
    /// Module name.
    pub module_name: Option<String>,
    /// Function names: index → name.
    pub function_names: Vec<(u32, String)>,
    /// Local names: `function_index` → vec of (`local_index`, name).
    pub local_names: Vec<(u32, Vec<(u32, String)>)>,
    /// Type names: index → name.
    pub type_names: Vec<(u32, String)>,
    /// Global names: index → name.
    pub global_names: Vec<(u32, String)>,
}

// =============================================================================
// Component Model
// =============================================================================

/// Component Model wrapper information.
#[derive(Debug, Default)]
pub struct WirComponent {
    /// WASI interfaces to import.
    pub wasi_imports: Vec<WirWasiImport>,
    /// Bundled modules (fts, libm).
    pub bundled_modules: Vec<WirBundledModule>,
    /// World exports.
    pub world_exports: Vec<WirWorldExport>,
    /// Memory module configuration.
    pub memory: WirMemoryConfig,
}

/// A WASI interface import.
#[derive(Debug)]
pub struct WirWasiImport {
    /// Interface name (e.g., "wasi:cli/stdout@0.2.0").
    pub interface: String,
    /// Functions imported from this interface.
    pub functions: Vec<WirWasiFunc>,
}

/// A function from a WASI interface.
#[derive(Debug)]
pub struct WirWasiFunc {
    /// WIT-level function name (e.g., "write-via-stream").
    pub wit_name: String,
    /// Internal function name in the core module.
    pub core_name: String,
}

/// A bundled module (e.g., fts, libm).
#[derive(Debug)]
pub struct WirBundledModule {
    /// Module name (e.g., "fts", "libm").
    pub name: String,
    /// Whether this module is needed.
    pub needed: bool,
}

/// A world export.
#[derive(Debug)]
pub struct WirWorldExport {
    /// Export name in the component model.
    pub name: String,
    /// Core function to export.
    pub core_func: String,
}

/// Memory module configuration.
#[derive(Debug, Default)]
pub struct WirMemoryConfig {
    /// Whether to include a linear memory.
    pub has_memory: bool,
    /// Minimum memory pages.
    pub min_pages: u32,
}
