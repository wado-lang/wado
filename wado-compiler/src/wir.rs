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

use crate::hashmap::{IndexMap, IndexSet};

use crate::name::ModuleSource;
use crate::token::Span;

/// A complete Wasm module in WIR form.
/// Contains all information needed to emit a valid Wasm binary.
#[derive(Debug)]
pub struct WirPackage {
    /// Type definitions: Wado-level types (struct, variant, enum, flags, array, func).
    /// Not a 1:1 mapping to the Wasm type section — the emit phase expands these.
    pub types: Vec<WirTypeDef>,
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
    /// Defined memories (for standalone modules like the memory module).
    pub memories: Vec<WirMemory>,
    /// Data section (string literals, etc.).
    pub data: Vec<WirData>,
    /// Name section entries.
    pub names: WirNames,
    /// Component Model wrapper info.
    pub component: WirComponent,
    /// Variant case type info: case WIR type index → (variant WIR type index, case index).
    /// Used by emitter to resolve case-specific struct types within variant rec groups.
    pub variant_case_info: IndexMap<u32, (u32, u32)>,
    /// Separate Wasm core modules extracted from `#![wasm_module("name")]` sources.
    /// Key: wasm module name (e.g., "mem").
    pub wasm_modules: IndexMap<String, WasmModuleInfo>,
    /// Type indices that were extracted into `wasm_modules` and should be skipped by the emitter.
    pub dead_type_indices: IndexSet<u32>,
    /// Function indices (into `functions`) extracted into `wasm_modules`; skipped by emitter.
    pub dead_func_indices: IndexSet<u32>,
    /// Global indices (into `globals`) extracted into `wasm_modules`; skipped by emitter.
    pub dead_global_indices: IndexSet<u32>,
    /// CM canonical intrinsics needed by this module.
    /// Populated during WIR translation via `WirContext::ensure_canonical`.
    /// Used by the component codegen to determine which canonical imports to generate.
    pub needed_canonicals: IndexSet<CanonicalIntrinsic>,
}

/// Functions and globals extracted from a `#![wasm_module("name")]` source module.
/// Compiled into a separate Wasm core module in the component.
#[derive(Debug)]
pub struct WasmModuleInfo {
    /// Extracted functions with their export names and bodies.
    pub functions: Vec<WasmModuleFunc>,
    /// Extracted globals.
    pub globals: Vec<WirGlobal>,
    /// Mapping from WIR global FQ names to module-local global indices.
    pub global_name_to_index: IndexMap<String, u32>,
}

/// A function in a `#![wasm_module]` separate core module.
#[derive(Debug)]
pub struct WasmModuleFunc {
    /// The wasm export name for this function (e.g., "realloc").
    pub export_name: String,
    /// Parameter names.
    pub param_names: Vec<String>,
    /// Result types from the original function type.
    pub results: Vec<WirType>,
    /// The WIR body instructions.
    pub body: Vec<WirInstr>,
    /// Original function index in the main WIR module (for remapping calls).
    pub original_func_index: u32,
    /// Whether this function is exported from the wasm module.
    pub is_exported: bool,
}

impl WasmModuleInfo {
    /// Convert this extracted module info into a standalone `WirPackage`
    /// that can be emitted via `emit_core_module`.
    pub fn to_wir_package(&self, strip_names: bool, memory: WirMemory) -> WirPackage {
        let mut wir = WirPackage::empty();

        // Build func index remap: original global index → local module index
        let func_index_remap: IndexMap<u32, u32> = self
            .functions
            .iter()
            .enumerate()
            .map(|(local_idx, func)| (func.original_func_index, u32::try_from(local_idx).unwrap()))
            .collect();

        // Memory definition
        wir.memories.push(memory);

        // One func type per function
        for (i, func) in self.functions.iter().enumerate() {
            let fq: Rc<str> = Rc::from(format!("__wasm_mod_type_{i}"));
            wir.types.push(WirTypeDef::Func(WirFuncType {
                name: WirName { fq: fq.to_string() },
                params: vec![WirType::I32; func.param_names.len()],
                results: func.results.clone(),
            }));
        }

        // Functions (with remapped call targets)
        for (i, func) in self.functions.iter().enumerate() {
            let type_id = WirTypeId::new(
                u32::try_from(i).unwrap(),
                Rc::from(format!("__wasm_mod_type_{i}")),
            );
            let func_fq: Rc<str> = Rc::from(format!("__wasm_mod_func_{i}"));
            let mut body = func.body.clone();
            for instr in &mut body {
                remap_func_ids_in_instr(instr, &func_index_remap);
            }
            wir.functions.push(WirFunction {
                name: WirName {
                    fq: func_fq.to_string(),
                },
                type_id,
                param_names: func.param_names.clone(),
                body: Some(body),
                meta: WirMeta {
                    module_source: None,
                    span: None,
                    attributes: Vec::new(),
                },
                generic_origin: None,
                effects: Vec::new(),
                stores: Vec::new(),
                comp_features: 0,
                export_name: None,
            });

            // Only export functions that are marked as exported
            if func.is_exported {
                wir.exports.push(WirExport {
                    name: func.export_name.clone(),
                    desc: WirExportDesc::Func {
                        func_id: WirFuncId::new(
                            u32::try_from(i).unwrap(),
                            Rc::from(format!("__wasm_mod_func_{i}")),
                        ),
                    },
                });
            }
        }

        // Globals
        wir.globals.clone_from(&self.globals);

        // Export memory
        wir.exports.push(WirExport {
            name: "memory".to_string(),
            desc: WirExportDesc::Memory,
        });

        // Names
        if !strip_names {
            for (i, func) in self.functions.iter().enumerate() {
                wir.names
                    .function_names
                    .push((u32::try_from(i).unwrap(), func.export_name.clone()));
            }
        }

        wir
    }
}

/// Recursively remap `WirFuncId` indices in a `WirInstr` tree.
fn remap_func_ids_in_instr(instr: &mut WirInstr, remap: &IndexMap<u32, u32>) {
    match instr {
        WirInstr::Call { func_id, args } => {
            if let Some(&new_idx) = remap.get(&func_id.index()) {
                *func_id = WirFuncId::new(new_idx, Rc::from(func_id.fq()));
            }
            for arg in args {
                remap_func_ids_in_instr(arg, remap);
            }
        }
        WirInstr::RefFunc { func_id } => {
            if let Some(&new_idx) = remap.get(&func_id.index()) {
                *func_id = WirFuncId::new(new_idx, Rc::from(func_id.fq()));
            }
        }
        _ => {
            instr.for_each_boxed_child_mut(&mut |child| {
                remap_func_ids_in_instr(child, remap);
            });
        }
    }
}

impl WirPackage {
    /// Create an empty `WirPackage` with no types, functions, or other content.
    pub fn empty() -> Self {
        Self {
            types: Vec::new(),
            imports: Vec::new(),
            functions: Vec::new(),
            globals: Vec::new(),
            exports: Vec::new(),
            elements: Vec::new(),
            memories: Vec::new(),
            data: Vec::new(),
            names: WirNames::default(),
            component: WirComponent::default(),
            variant_case_info: IndexMap::default(),
            wasm_modules: IndexMap::default(),
            dead_type_indices: IndexSet::default(),
            dead_func_indices: IndexSet::default(),
            dead_global_indices: IndexSet::default(),
            needed_canonicals: IndexSet::default(),
        }
    }
}

/// A CM (Component Model) scalar value type for parameterized canonical intrinsics.
///
/// Used to specify the element type of `future<T>` and (in the future) `stream<T>`.
/// Maps 1:1 to `wasm_encoder::PrimitiveValType` in codegen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CmScalarType {
    S8,
    S16,
    S32,
    S64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Bool,
    Char,
}

impl fmt::Display for CmScalarType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::S8 => write!(f, "s8"),
            Self::S16 => write!(f, "s16"),
            Self::S32 => write!(f, "s32"),
            Self::S64 => write!(f, "s64"),
            Self::U8 => write!(f, "u8"),
            Self::U16 => write!(f, "u16"),
            Self::U32 => write!(f, "u32"),
            Self::U64 => write!(f, "u64"),
            Self::F32 => write!(f, "float32"),
            Self::F64 => write!(f, "float64"),
            Self::Bool => write!(f, "bool"),
            Self::Char => write!(f, "char"),
        }
    }
}

/// The element type of a CM `stream<T>` canonical intrinsic.
///
/// Distinguishes between distinct stream types at the Component Model level:
/// - `U8` = `stream<u8>` (default for file I/O, stdin/stdout)
/// - `Record(name)` = `stream<T>` where T is a CM record type (e.g., directory-entry)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmStreamPayload {
    /// `stream<u8>` — the default stream type
    U8,
    /// `stream<T>` where T is a CM record type, identified by CM kebab-case name
    Record(String),
}

/// The element type of a CM `future<T>` canonical intrinsic.
///
/// Distinguishes between distinct future types at the Component Model level:
/// - `Trailers` = `future<result<option<trailers>, error-code>>` (HTTP body trailers)
/// - `Transmission` = `future<result<_, error-code>>` (HTTP transmission result)
/// - `Scalar(s)` = `future<T>` where T is a primitive scalar type
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CmFuturePayload {
    /// The HTTP trailers pattern: `future<result<option<trailers>, error-code>>`
    Trailers,
    /// `future<result<_, error-code>>` — error-code type identified by
    /// the WASI package that defines it (e.g., "cli", "filesystem", "http").
    /// Each package's error-code is a distinct CM type, so each needs
    /// its own component-level `future<result<_, E>>` definition.
    Transmission(String),
    /// A scalar value type like `future<s32>`
    Scalar(CmScalarType),
}

/// A canonical intrinsic needed by the compiled module.
///
/// Replaces the previous string-based approach (e.g., `"future-new:s32"`) with
/// structured metadata. The future type parameter is stored as `CmFuturePayload`
/// instead of being encoded in the name string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CanonicalIntrinsic {
    StreamNew(CmStreamPayload),
    StreamRead(CmStreamPayload),
    StreamWrite(CmStreamPayload),
    StreamDropReadable(CmStreamPayload),
    StreamDropWritable(CmStreamPayload),
    StreamCancelRead(CmStreamPayload),
    StreamCancelWrite(CmStreamPayload),
    FutureNew(CmFuturePayload),
    FutureRead(CmFuturePayload),
    FutureWrite(CmFuturePayload),
    FutureDropReadable(CmFuturePayload),
    FutureDropWritable(CmFuturePayload),
    FutureCancelRead(CmFuturePayload),
    FutureCancelWrite(CmFuturePayload),
    WaitableSetNew,
    WaitableSetWait,
    WaitableSetPoll,
    WaitableSetDrop,
    WaitableJoin,
    SubtaskDrop,
    SubtaskCancel,
    ErrorContextNew,
    ErrorContextDebugMessage,
    ErrorContextDrop,
    TaskReturn,
}

impl CanonicalIntrinsic {
    /// The import name used in the core Wasm module.
    ///
    /// For parameterized intrinsics, includes a type suffix (e.g., `"future-new:s32"`).
    /// This is only used for the core-level import name; component codegen uses
    /// the structured enum directly.
    pub fn import_name(&self) -> String {
        match self {
            Self::StreamNew(p) => format_stream_name("stream-new", p),
            Self::StreamRead(p) => format_stream_name("stream-read", p),
            Self::StreamWrite(p) => format_stream_name("stream-write", p),
            Self::StreamDropReadable(p) => format_stream_name("stream-drop-readable", p),
            Self::StreamDropWritable(p) => format_stream_name("stream-drop-writable", p),
            Self::StreamCancelRead(p) => format_stream_name("stream-cancel-read", p),
            Self::StreamCancelWrite(p) => format_stream_name("stream-cancel-write", p),
            Self::FutureNew(p) => format_future_name("future-new", p.clone()),
            Self::FutureRead(p) => format_future_name("future-read", p.clone()),
            Self::FutureWrite(p) => format_future_name("future-write", p.clone()),
            Self::FutureDropReadable(p) => format_future_name("future-drop-readable", p.clone()),
            Self::FutureDropWritable(p) => format_future_name("future-drop-writable", p.clone()),
            Self::FutureCancelRead(p) => format_future_name("future-cancel-read", p.clone()),
            Self::FutureCancelWrite(p) => format_future_name("future-cancel-write", p.clone()),
            Self::WaitableSetNew => "waitable-set-new".to_string(),
            Self::WaitableSetWait => "waitable-set-wait".to_string(),
            Self::WaitableSetPoll => "waitable-set-poll".to_string(),
            Self::WaitableSetDrop => "waitable-set-drop".to_string(),
            Self::WaitableJoin => "waitable-join".to_string(),
            Self::SubtaskDrop => "subtask-drop".to_string(),
            Self::SubtaskCancel => "subtask-cancel".to_string(),
            Self::ErrorContextNew => "error-context-new".to_string(),
            Self::ErrorContextDebugMessage => "error-context-debug-message".to_string(),
            Self::ErrorContextDrop => "error-context-drop".to_string(),
            Self::TaskReturn => "task-return".to_string(),
        }
    }

    /// Parse a canonical intrinsic from a WASI import name.
    ///
    /// Used for TIR-level WASI imports (e.g., "task-return") that are registered
    /// before WIR translation. Only handles non-parameterized intrinsics since
    /// parameterized ones (future with type) are created directly in WIR translation.
    pub fn from_import_name(name: &str) -> Option<Self> {
        Some(match name {
            _ if name.starts_with("stream-") => {
                return parse_stream_intrinsic(name);
            }
            "future-new" => Self::FutureNew(CmFuturePayload::Trailers),
            "future-read" => Self::FutureRead(CmFuturePayload::Trailers),
            "future-write" => Self::FutureWrite(CmFuturePayload::Trailers),
            "future-drop-readable" => Self::FutureDropReadable(CmFuturePayload::Trailers),
            "future-drop-writable" => Self::FutureDropWritable(CmFuturePayload::Trailers),
            "future-cancel-read" => Self::FutureCancelRead(CmFuturePayload::Trailers),
            "future-cancel-write" => Self::FutureCancelWrite(CmFuturePayload::Trailers),
            "waitable-set-new" => Self::WaitableSetNew,
            "waitable-set-wait" => Self::WaitableSetWait,
            "waitable-set-poll" => Self::WaitableSetPoll,
            "waitable-set-drop" => Self::WaitableSetDrop,
            "waitable-join" => Self::WaitableJoin,
            "subtask-drop" => Self::SubtaskDrop,
            "subtask-cancel" => Self::SubtaskCancel,
            "error-context-new" => Self::ErrorContextNew,
            "error-context-debug-message" => Self::ErrorContextDebugMessage,
            "error-context-drop" => Self::ErrorContextDrop,
            "task-return" => Self::TaskReturn,
            _ => return None,
        })
    }

    /// Extract the future payload type, if this is a future intrinsic.
    pub fn future_payload(&self) -> Option<CmFuturePayload> {
        match self {
            Self::FutureNew(p)
            | Self::FutureRead(p)
            | Self::FutureWrite(p)
            | Self::FutureDropReadable(p)
            | Self::FutureDropWritable(p)
            | Self::FutureCancelRead(p)
            | Self::FutureCancelWrite(p) => Some(p.clone()),
            _ => None,
        }
    }

    /// Extract the stream payload type, if this is a stream intrinsic.
    pub fn stream_payload(&self) -> Option<CmStreamPayload> {
        match self {
            Self::StreamNew(p)
            | Self::StreamRead(p)
            | Self::StreamWrite(p)
            | Self::StreamDropReadable(p)
            | Self::StreamDropWritable(p)
            | Self::StreamCancelRead(p)
            | Self::StreamCancelWrite(p) => Some(p.clone()),
            _ => None,
        }
    }
}

fn parse_stream_intrinsic(name: &str) -> Option<CanonicalIntrinsic> {
    // Parse "stream-read:directory-entry" → StreamRead(Record("directory-entry"))
    // Parse "stream-read" → StreamRead(U8)
    let (base, payload) = if let Some((b, suffix)) = name.split_once(':') {
        (b, CmStreamPayload::Record(suffix.to_string()))
    } else {
        (name, CmStreamPayload::U8)
    };
    Some(match base {
        "stream-new" => CanonicalIntrinsic::StreamNew(payload),
        "stream-read" => CanonicalIntrinsic::StreamRead(payload),
        "stream-write" => CanonicalIntrinsic::StreamWrite(payload),
        "stream-drop-readable" => CanonicalIntrinsic::StreamDropReadable(payload),
        "stream-drop-writable" => CanonicalIntrinsic::StreamDropWritable(payload),
        "stream-cancel-read" => CanonicalIntrinsic::StreamCancelRead(payload),
        "stream-cancel-write" => CanonicalIntrinsic::StreamCancelWrite(payload),
        _ => return None,
    })
}

fn format_stream_name(base: &str, payload: &CmStreamPayload) -> String {
    match payload {
        CmStreamPayload::U8 => base.to_string(),
        CmStreamPayload::Record(name) => format!("{base}:{name}"),
    }
}

fn format_future_name(base: &str, payload: CmFuturePayload) -> String {
    match payload {
        CmFuturePayload::Trailers => base.to_string(),
        CmFuturePayload::Transmission(ref source) => format!("{base}:transmission-{source}"),
        CmFuturePayload::Scalar(scalar) => format!("{base}:{scalar}"),
    }
}

/// A globally-scoped name in WIR.
///
/// Carries both forms needed by different consumers:
/// - `fq`: fully-qualified name for identity, unparse, and error messages
/// - `fq`: module-qualified name for unique identity
///
/// The `tir_to_wir` phase constructs `WirName`s from `name.rs` objects
/// (`StructName`, `FunctionId`, etc.). WIR itself does not import `name.rs` types.
#[derive(Debug, Clone)]
pub struct WirName {
    /// Fully-qualified name (e.g., "core:prelude//Point", "./`geometry.wado/Point::sum`").
    /// Used as the unique identity key for name→index resolution during emission,
    /// and also for display in unparse output and error messages.
    pub fq: String,
}

impl fmt::Display for WirName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.fq)
    }
}

/// Lightweight reference to a type definition in `WirPackage.types`.
///
/// - `index`: indexes into `WirPackage.types` (not the Wasm type section)
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

    /// Get the index into `WirPackage.types`.
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

    /// Get the index into `WirPackage.functions`.
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

/// A struct type with named fields.
#[derive(Debug)]
pub struct WirStructType {
    /// Name (fq: "core:prelude//Point").
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

/// Runtime representation of a variant type.
#[derive(Debug, Clone, Default)]
pub enum WirVariantRepr {
    /// Subtype hierarchy: base struct with `discriminant: i32` + per-case subtypes.
    /// This is the general-purpose representation used when no null niche is available.
    #[default]
    SubtypeHierarchy,
    /// Nullable reference: the variant value IS the payload reference (or null for the unit case).
    ///
    /// Applicable when the variant has exactly 2 cases: one unit case (no payload) and one
    /// payload case whose type is a non-nullable reference (struct, array, `SubtypeHierarchy`
    /// variant, i128/u128). The unit case maps to `ref.null none`; the payload case is the
    /// value itself. No Wasm types are emitted for the variant.
    ///
    /// Example: `Option<String>` → represented as `(ref null $String)`.
    NullableRef {
        /// Index (in `cases`) of the payload case (e.g. `Some`/`Ok`).
        payload_case: u32,
    },
}

/// A variant type (sum type with payloads).
/// At the Wasm level, expands to a subtype hierarchy: a base struct with a
/// `discriminant` field, and per-case subtypes that add payload fields.
/// Unless `repr` is `NullableRef`, in which case no Wasm types are emitted.
#[derive(Debug)]
pub struct WirVariantType {
    /// Name (fq: "./shapes.wado//Shape").
    pub name: WirName,
    /// Cases with names and optional payload types.
    pub cases: Vec<WirVariantCase>,
    /// Runtime representation strategy.
    pub repr: WirVariantRepr,
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

/// An enum type (discriminated values without payloads).
/// No Wasm type section entry — values are i32 discriminants.
#[derive(Debug)]
pub struct WirEnumType {
    /// Name (fq: "./colors.wado//Color").
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

/// A flags type (bitfield).
/// No Wasm type section entry — values are i32 bitfields.
#[derive(Debug)]
pub struct WirFlagsType {
    /// Name (fq: "./perms.wado//Permissions").
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
#[derive(Debug, Clone, PartialEq)]
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
    // SIMD
    V128,
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

impl WirType {
    /// Returns a non-nullable version of this type.
    /// Only affects `Ref` and `AbstractRef` variants; other types are returned unchanged.
    pub fn as_nonnull(self) -> Self {
        match self {
            Self::Ref {
                type_id,
                nullable: true,
            } => Self::Ref {
                type_id,
                nullable: false,
            },
            Self::AbstractRef {
                heap_type,
                nullable: true,
            } => Self::AbstractRef {
                heap_type,
                nullable: false,
            },
            other => other,
        }
    }

    /// Returns true if this type is a non-nullable reference.
    pub fn is_nonnull_ref(&self) -> bool {
        matches!(
            self,
            Self::Ref {
                nullable: false,
                ..
            } | Self::AbstractRef {
                nullable: false,
                ..
            }
        )
    }

    /// Returns a nullable version of this type.
    /// Only affects `Ref` and `AbstractRef` variants; other types are returned unchanged.
    pub fn as_nullable(self) -> Self {
        match self {
            Self::Ref {
                type_id,
                nullable: false,
            } => Self::Ref {
                type_id,
                nullable: true,
            },
            Self::AbstractRef {
                heap_type,
                nullable: false,
            } => Self::AbstractRef {
                heap_type,
                nullable: true,
            },
            other => other,
        }
    }
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

/// Compiler feature flag: function implements `Array<T>::push`.
pub const COMP_FEATURE_ARRAY_PUSH: u32 = 1 << 0;
/// Compiler feature flag: function implements `String::push_str`.
pub const COMP_FEATURE_STRING_PUSH_STR: u32 = 1 << 1;
/// Compiler feature flag: function implements `String::push_char`.
pub const COMP_FEATURE_STRING_PUSH_CHAR: u32 = 1 << 2;
/// Compiler feature flag: variant is the canonical `Option<T>` type.
pub const COMP_FEATURE_OPTION: u32 = 1 << 3;
/// Compiler feature flag: variant is the canonical `Result<T, E>` type.
pub const COMP_FEATURE_RESULT: u32 = 1 << 4;
/// Compiler feature flag: trait is the canonical `Default` trait.
pub const COMP_FEATURE_DEFAULT: u32 = 1 << 5;
/// Compiler feature flag: trait is the canonical `From<T>` trait.
pub const COMP_FEATURE_FROM: u32 = 1 << 6;
/// Compiler feature flag: declares ownership of the tuple type family (`pub type [..T];`).
pub const COMP_FEATURE_TUPLE: u32 = 1 << 7;
/// Compiler feature flag: struct is the canonical `Box<T>` type for primitive boxing.
pub const COMP_FEATURE_BOX: u32 = 1 << 8;
pub const COMP_FEATURE_RANGE_EXCLUSIVE: u32 = 1 << 9;
pub const COMP_FEATURE_RANGE_INCLUSIVE: u32 = 1 << 10;

/// A function declaration with optional body.
#[derive(Debug)]
pub struct WirFunction {
    /// Function name (fq: "./`geometry.wado//Point::sum`").
    pub name: WirName,
    /// Function type ID (references a `WirTypeDef::Func` in `WirPackage.types`).
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
    pub effects: Vec<crate::tir::EffectRef>,
    /// Parameter names declared in `stores[...]` — the function may store these references.
    /// Used by WIR optimizations for stores-aware alias analysis.
    pub stores: Vec<String>,
    /// Compiler feature bitflags (e.g., `COMP_FEATURE_ARRAY_PUSH`).
    /// Set via `#[comp_feature("array_push")]` attribute in Wado source.
    pub comp_features: u32,
    /// Custom wasm export name from `#[export_name("...")]` attribute.
    pub export_name: Option<String>,
}

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
        result_ty: WirType,
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
        result_ty: WirType,
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

    V128Const(i128),
    V128Not(Box<WirInstr>),
    V128And(Box<WirInstr>, Box<WirInstr>),
    V128Or(Box<WirInstr>, Box<WirInstr>),
    V128Xor(Box<WirInstr>, Box<WirInstr>),
    V128Bitselect(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    I8x16Splat(Box<WirInstr>),
    I8x16ExtractLaneS(u8, Box<WirInstr>),
    I8x16ExtractLaneU(u8, Box<WirInstr>),
    I8x16ReplaceLane(u8, Box<WirInstr>, Box<WirInstr>),
    I8x16Add(Box<WirInstr>, Box<WirInstr>),
    I8x16Sub(Box<WirInstr>, Box<WirInstr>),
    I8x16Neg(Box<WirInstr>),
    I8x16Eq(Box<WirInstr>, Box<WirInstr>),
    I8x16Ne(Box<WirInstr>, Box<WirInstr>),
    I8x16LtS(Box<WirInstr>, Box<WirInstr>),
    I8x16GtS(Box<WirInstr>, Box<WirInstr>),
    I8x16LeS(Box<WirInstr>, Box<WirInstr>),
    I8x16GeS(Box<WirInstr>, Box<WirInstr>),
    I8x16LtU(Box<WirInstr>, Box<WirInstr>),
    I8x16GtU(Box<WirInstr>, Box<WirInstr>),
    I8x16LeU(Box<WirInstr>, Box<WirInstr>),
    I8x16GeU(Box<WirInstr>, Box<WirInstr>),
    I8x16Shl(Box<WirInstr>, Box<WirInstr>),
    I8x16ShrS(Box<WirInstr>, Box<WirInstr>),
    I8x16ShrU(Box<WirInstr>, Box<WirInstr>),
    I8x16Swizzle(Box<WirInstr>, Box<WirInstr>),
    I8x16Shuffle([u8; 16], Box<WirInstr>, Box<WirInstr>),
    I16x8Splat(Box<WirInstr>),
    I16x8ExtractLaneS(u8, Box<WirInstr>),
    I16x8ExtractLaneU(u8, Box<WirInstr>),
    I16x8ReplaceLane(u8, Box<WirInstr>, Box<WirInstr>),
    I16x8Add(Box<WirInstr>, Box<WirInstr>),
    I16x8Sub(Box<WirInstr>, Box<WirInstr>),
    I16x8Mul(Box<WirInstr>, Box<WirInstr>),
    I16x8Neg(Box<WirInstr>),
    I16x8Eq(Box<WirInstr>, Box<WirInstr>),
    I16x8Ne(Box<WirInstr>, Box<WirInstr>),
    I16x8LtS(Box<WirInstr>, Box<WirInstr>),
    I16x8GtS(Box<WirInstr>, Box<WirInstr>),
    I16x8LeS(Box<WirInstr>, Box<WirInstr>),
    I16x8GeS(Box<WirInstr>, Box<WirInstr>),
    I16x8LtU(Box<WirInstr>, Box<WirInstr>),
    I16x8GtU(Box<WirInstr>, Box<WirInstr>),
    I16x8LeU(Box<WirInstr>, Box<WirInstr>),
    I16x8GeU(Box<WirInstr>, Box<WirInstr>),
    I16x8Shl(Box<WirInstr>, Box<WirInstr>),
    I16x8ShrS(Box<WirInstr>, Box<WirInstr>),
    I16x8ShrU(Box<WirInstr>, Box<WirInstr>),
    I32x4Splat(Box<WirInstr>),
    I32x4ExtractLane(u8, Box<WirInstr>),
    I32x4ReplaceLane(u8, Box<WirInstr>, Box<WirInstr>),
    I32x4Add(Box<WirInstr>, Box<WirInstr>),
    I32x4Sub(Box<WirInstr>, Box<WirInstr>),
    I32x4Mul(Box<WirInstr>, Box<WirInstr>),
    I32x4Neg(Box<WirInstr>),
    I32x4Eq(Box<WirInstr>, Box<WirInstr>),
    I32x4Ne(Box<WirInstr>, Box<WirInstr>),
    I32x4LtS(Box<WirInstr>, Box<WirInstr>),
    I32x4GtS(Box<WirInstr>, Box<WirInstr>),
    I32x4LeS(Box<WirInstr>, Box<WirInstr>),
    I32x4GeS(Box<WirInstr>, Box<WirInstr>),
    I32x4LtU(Box<WirInstr>, Box<WirInstr>),
    I32x4GtU(Box<WirInstr>, Box<WirInstr>),
    I32x4LeU(Box<WirInstr>, Box<WirInstr>),
    I32x4GeU(Box<WirInstr>, Box<WirInstr>),
    I32x4Shl(Box<WirInstr>, Box<WirInstr>),
    I32x4ShrS(Box<WirInstr>, Box<WirInstr>),
    I32x4ShrU(Box<WirInstr>, Box<WirInstr>),
    I64x2Splat(Box<WirInstr>),
    I64x2ExtractLane(u8, Box<WirInstr>),
    I64x2ReplaceLane(u8, Box<WirInstr>, Box<WirInstr>),
    I64x2Add(Box<WirInstr>, Box<WirInstr>),
    I64x2Sub(Box<WirInstr>, Box<WirInstr>),
    I64x2Mul(Box<WirInstr>, Box<WirInstr>),
    I64x2Neg(Box<WirInstr>),
    I64x2Eq(Box<WirInstr>, Box<WirInstr>),
    I64x2Ne(Box<WirInstr>, Box<WirInstr>),
    I64x2LtS(Box<WirInstr>, Box<WirInstr>),
    I64x2GtS(Box<WirInstr>, Box<WirInstr>),
    I64x2LeS(Box<WirInstr>, Box<WirInstr>),
    I64x2GeS(Box<WirInstr>, Box<WirInstr>),
    I64x2Shl(Box<WirInstr>, Box<WirInstr>),
    I64x2ShrS(Box<WirInstr>, Box<WirInstr>),
    I64x2ShrU(Box<WirInstr>, Box<WirInstr>),
    F32x4Splat(Box<WirInstr>),
    F32x4ExtractLane(u8, Box<WirInstr>),
    F32x4ReplaceLane(u8, Box<WirInstr>, Box<WirInstr>),
    F32x4Add(Box<WirInstr>, Box<WirInstr>),
    F32x4Sub(Box<WirInstr>, Box<WirInstr>),
    F32x4Mul(Box<WirInstr>, Box<WirInstr>),
    F32x4Div(Box<WirInstr>, Box<WirInstr>),
    F32x4Neg(Box<WirInstr>),
    F32x4Sqrt(Box<WirInstr>),
    F32x4Abs(Box<WirInstr>),
    F32x4Eq(Box<WirInstr>, Box<WirInstr>),
    F32x4Ne(Box<WirInstr>, Box<WirInstr>),
    F32x4Lt(Box<WirInstr>, Box<WirInstr>),
    F32x4Gt(Box<WirInstr>, Box<WirInstr>),
    F32x4Le(Box<WirInstr>, Box<WirInstr>),
    F32x4Ge(Box<WirInstr>, Box<WirInstr>),
    F32x4Min(Box<WirInstr>, Box<WirInstr>),
    F32x4Max(Box<WirInstr>, Box<WirInstr>),
    F64x2Splat(Box<WirInstr>),
    F64x2ExtractLane(u8, Box<WirInstr>),
    F64x2ReplaceLane(u8, Box<WirInstr>, Box<WirInstr>),
    F64x2Add(Box<WirInstr>, Box<WirInstr>),
    F64x2Sub(Box<WirInstr>, Box<WirInstr>),
    F64x2Mul(Box<WirInstr>, Box<WirInstr>),
    F64x2Div(Box<WirInstr>, Box<WirInstr>),
    F64x2Neg(Box<WirInstr>),
    F64x2Sqrt(Box<WirInstr>),
    F64x2Abs(Box<WirInstr>),
    F64x2Eq(Box<WirInstr>, Box<WirInstr>),
    F64x2Ne(Box<WirInstr>, Box<WirInstr>),
    F64x2Lt(Box<WirInstr>, Box<WirInstr>),
    F64x2Gt(Box<WirInstr>, Box<WirInstr>),
    F64x2Le(Box<WirInstr>, Box<WirInstr>),
    F64x2Ge(Box<WirInstr>, Box<WirInstr>),
    F64x2Min(Box<WirInstr>, Box<WirInstr>),
    F64x2Max(Box<WirInstr>, Box<WirInstr>),
    V128AndNot(Box<WirInstr>, Box<WirInstr>),
    V128AnyTrue(Box<WirInstr>),
    I8x16Abs(Box<WirInstr>),
    I8x16AddSatS(Box<WirInstr>, Box<WirInstr>),
    I8x16AddSatU(Box<WirInstr>, Box<WirInstr>),
    I8x16SubSatS(Box<WirInstr>, Box<WirInstr>),
    I8x16SubSatU(Box<WirInstr>, Box<WirInstr>),
    I8x16MinS(Box<WirInstr>, Box<WirInstr>),
    I8x16MinU(Box<WirInstr>, Box<WirInstr>),
    I8x16MaxS(Box<WirInstr>, Box<WirInstr>),
    I8x16MaxU(Box<WirInstr>, Box<WirInstr>),
    I8x16AvgrU(Box<WirInstr>, Box<WirInstr>),
    I8x16AllTrue(Box<WirInstr>),
    I8x16Bitmask(Box<WirInstr>),
    I8x16NarrowI16x8S(Box<WirInstr>, Box<WirInstr>),
    I8x16NarrowI16x8U(Box<WirInstr>, Box<WirInstr>),
    I8x16Popcnt(Box<WirInstr>),
    I16x8Abs(Box<WirInstr>),
    I16x8AddSatS(Box<WirInstr>, Box<WirInstr>),
    I16x8AddSatU(Box<WirInstr>, Box<WirInstr>),
    I16x8SubSatS(Box<WirInstr>, Box<WirInstr>),
    I16x8SubSatU(Box<WirInstr>, Box<WirInstr>),
    I16x8MinS(Box<WirInstr>, Box<WirInstr>),
    I16x8MinU(Box<WirInstr>, Box<WirInstr>),
    I16x8MaxS(Box<WirInstr>, Box<WirInstr>),
    I16x8MaxU(Box<WirInstr>, Box<WirInstr>),
    I16x8AvgrU(Box<WirInstr>, Box<WirInstr>),
    I16x8AllTrue(Box<WirInstr>),
    I16x8Bitmask(Box<WirInstr>),
    I16x8NarrowI32x4S(Box<WirInstr>, Box<WirInstr>),
    I16x8NarrowI32x4U(Box<WirInstr>, Box<WirInstr>),
    I16x8ExtendLowI8x16S(Box<WirInstr>),
    I16x8ExtendHighI8x16S(Box<WirInstr>),
    I16x8ExtendLowI8x16U(Box<WirInstr>),
    I16x8ExtendHighI8x16U(Box<WirInstr>),
    I16x8ExtMulLowI8x16S(Box<WirInstr>, Box<WirInstr>),
    I16x8ExtMulHighI8x16S(Box<WirInstr>, Box<WirInstr>),
    I16x8ExtMulLowI8x16U(Box<WirInstr>, Box<WirInstr>),
    I16x8ExtMulHighI8x16U(Box<WirInstr>, Box<WirInstr>),
    I16x8ExtAddPairwiseI8x16S(Box<WirInstr>),
    I16x8ExtAddPairwiseI8x16U(Box<WirInstr>),
    I16x8Q15MulrSatS(Box<WirInstr>, Box<WirInstr>),
    I32x4Abs(Box<WirInstr>),
    I32x4AllTrue(Box<WirInstr>),
    I32x4Bitmask(Box<WirInstr>),
    I32x4MinS(Box<WirInstr>, Box<WirInstr>),
    I32x4MinU(Box<WirInstr>, Box<WirInstr>),
    I32x4MaxS(Box<WirInstr>, Box<WirInstr>),
    I32x4MaxU(Box<WirInstr>, Box<WirInstr>),
    I32x4DotI16x8S(Box<WirInstr>, Box<WirInstr>),
    I32x4ExtendLowI16x8S(Box<WirInstr>),
    I32x4ExtendHighI16x8S(Box<WirInstr>),
    I32x4ExtendLowI16x8U(Box<WirInstr>),
    I32x4ExtendHighI16x8U(Box<WirInstr>),
    I32x4ExtMulLowI16x8S(Box<WirInstr>, Box<WirInstr>),
    I32x4ExtMulHighI16x8S(Box<WirInstr>, Box<WirInstr>),
    I32x4ExtMulLowI16x8U(Box<WirInstr>, Box<WirInstr>),
    I32x4ExtMulHighI16x8U(Box<WirInstr>, Box<WirInstr>),
    I32x4ExtAddPairwiseI16x8S(Box<WirInstr>),
    I32x4ExtAddPairwiseI16x8U(Box<WirInstr>),
    I32x4TruncSatF32x4S(Box<WirInstr>),
    I32x4TruncSatF32x4U(Box<WirInstr>),
    I32x4TruncSatF64x2SZero(Box<WirInstr>),
    I32x4TruncSatF64x2UZero(Box<WirInstr>),
    I64x2Abs(Box<WirInstr>),
    I64x2AllTrue(Box<WirInstr>),
    I64x2Bitmask(Box<WirInstr>),
    I64x2ExtendLowI32x4S(Box<WirInstr>),
    I64x2ExtendHighI32x4S(Box<WirInstr>),
    I64x2ExtendLowI32x4U(Box<WirInstr>),
    I64x2ExtendHighI32x4U(Box<WirInstr>),
    I64x2ExtMulLowI32x4S(Box<WirInstr>, Box<WirInstr>),
    I64x2ExtMulHighI32x4S(Box<WirInstr>, Box<WirInstr>),
    I64x2ExtMulLowI32x4U(Box<WirInstr>, Box<WirInstr>),
    I64x2ExtMulHighI32x4U(Box<WirInstr>, Box<WirInstr>),
    F32x4Ceil(Box<WirInstr>),
    F32x4Floor(Box<WirInstr>),
    F32x4Trunc(Box<WirInstr>),
    F32x4Nearest(Box<WirInstr>),
    F32x4PMin(Box<WirInstr>, Box<WirInstr>),
    F32x4PMax(Box<WirInstr>, Box<WirInstr>),
    F32x4ConvertI32x4S(Box<WirInstr>),
    F32x4ConvertI32x4U(Box<WirInstr>),
    F32x4DemoteF64x2Zero(Box<WirInstr>),
    F64x2Ceil(Box<WirInstr>),
    F64x2Floor(Box<WirInstr>),
    F64x2Trunc(Box<WirInstr>),
    F64x2Nearest(Box<WirInstr>),
    F64x2PMin(Box<WirInstr>, Box<WirInstr>),
    F64x2PMax(Box<WirInstr>, Box<WirInstr>),
    F64x2ConvertLowI32x4S(Box<WirInstr>),
    F64x2ConvertLowI32x4U(Box<WirInstr>),
    F64x2PromoteLowF32x4(Box<WirInstr>),
    I8x16RelaxedSwizzle(Box<WirInstr>, Box<WirInstr>),
    I8x16RelaxedLaneselect(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    I16x8RelaxedLaneselect(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    I32x4RelaxedLaneselect(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    I64x2RelaxedLaneselect(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    F32x4RelaxedMadd(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    F32x4RelaxedNmadd(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    F64x2RelaxedMadd(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    F64x2RelaxedNmadd(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),
    F32x4RelaxedMin(Box<WirInstr>, Box<WirInstr>),
    F32x4RelaxedMax(Box<WirInstr>, Box<WirInstr>),
    F64x2RelaxedMin(Box<WirInstr>, Box<WirInstr>),
    F64x2RelaxedMax(Box<WirInstr>, Box<WirInstr>),
    I32x4RelaxedTruncF32x4S(Box<WirInstr>),
    I32x4RelaxedTruncF32x4U(Box<WirInstr>),
    I32x4RelaxedTruncF64x2SZero(Box<WirInstr>),
    I32x4RelaxedTruncF64x2UZero(Box<WirInstr>),
    I16x8RelaxedQ15mulrS(Box<WirInstr>, Box<WirInstr>),
    I16x8RelaxedDotI8x16I7x16S(Box<WirInstr>, Box<WirInstr>),
    I32x4RelaxedDotI8x16I7x16AddS(Box<WirInstr>, Box<WirInstr>, Box<WirInstr>),

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
        result_ty: WirType,
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
        result_ty: WirType,
    },
    ArrayGetS {
        type_id: WirTypeId,
        array: Box<WirInstr>,
        index: Box<WirInstr>,
        result_ty: WirType,
    },
    ArrayGetU {
        type_id: WirTypeId,
        array: Box<WirInstr>,
        index: Box<WirInstr>,
        result_ty: WirType,
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
    MemoryFill {
        dst: Box<WirInstr>,
        value: Box<WirInstr>,
        len: Box<WirInstr>,
    },
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
    V128Load {
        offset: u64,
        align: u32,
        addr: Box<WirInstr>,
    },
    V128Store {
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
    /// When `nullable` is true, codegen emits a null guard (`ref.is_null` + if/else).
    ValueCopy {
        type_id: WirTypeId,
        source_type: WirCopyType,
        expr: Box<WirInstr>,
        nullable: bool,
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
    /// Returns true if this instruction always diverges (all execution paths
    /// end with `return` or `unreachable` before producing a value).
    pub fn always_diverges(&self) -> bool {
        match self {
            Self::Return { .. } | Self::Unreachable => true,
            Self::If {
                then_body,
                else_body,
                ..
            } => {
                Self::seq_always_diverges(then_body)
                    && else_body
                        .as_ref()
                        .is_some_and(|eb| Self::seq_always_diverges(eb))
            }
            Self::Seq(body) => Self::seq_always_diverges(body),
            Self::Drop(inner) => inner.always_diverges(),
            _ => false,
        }
    }

    fn seq_always_diverges(instrs: &[Self]) -> bool {
        instrs.iter().any(Self::always_diverges)
    }

    /// Returns true if this instruction leaves a value on the Wasm stack.
    /// Used to guard `Drop` emission — a `Block{result: None}` produces no value.
    pub fn produces_stack_value(&self) -> bool {
        match self {
            Self::Block { result, .. } | Self::If { result, .. } => result.is_some(),
            Self::Loop { .. } => false,
            Self::Seq(body) => body.last().is_some_and(WirInstr::produces_stack_value),
            Self::Drop(_)
            | Self::LocalSet { .. }
            | Self::GlobalSet { .. }
            | Self::Return { .. }
            | Self::Unreachable
            | Self::Br { .. }
            | Self::BrIf { .. }
            | Self::BrTable { .. }
            | Self::DeclareLocal { .. }
            | Self::Nop => false,
            _ => true,
        }
    }

    /// Returns true if this instruction is guaranteed to produce a non-null
    /// reference at the Wasm level. Used to skip redundant `RefAsNonNull` wrappers.
    pub fn is_nonnull_result(&self) -> bool {
        match self {
            Self::LocalGet { result_ty, .. }
            | Self::GlobalGet { result_ty, .. }
            | Self::StructGet { result_ty, .. }
            | Self::ArrayGet { result_ty, .. }
            | Self::ArrayGetS { result_ty, .. }
            | Self::ArrayGetU { result_ty, .. } => result_ty.is_nonnull_ref(),
            _ => matches!(
                self,
                Self::StructNew { .. }
                    | Self::ArrayNew { .. }
                    | Self::ArrayNewDefault { .. }
                    | Self::ArrayNewData { .. }
                    | Self::ArrayNewFixed { .. }
                    | Self::RefAsNonNull(_)
                    | Self::RefI31(_)
            ),
        }
    }

    /// Visit all child instructions of this node (non-recursive).
    /// Used by the emitter for pre-scanning (e.g., collecting `DeclareLocal`).
    pub fn for_each_child(&self, f: &mut impl FnMut(&WirInstr)) {
        match self {
            // Leaf nodes
            Self::I32Const(_)
            | Self::I64Const(_)
            | Self::F32Const(_)
            | Self::F64Const(_)
            | Self::V128Const(_)
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
            Self::BrIf { condition, .. }
            | Self::BranchHint {
                expr: condition, ..
            } => {
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
            | Self::ArrayLen(o)
            | Self::V128Not(o)
            | Self::I8x16Splat(o)
            | Self::I8x16Neg(o)
            | Self::I16x8Splat(o)
            | Self::I16x8Neg(o)
            | Self::I32x4Splat(o)
            | Self::I32x4Neg(o)
            | Self::I64x2Splat(o)
            | Self::I64x2Neg(o)
            | Self::F32x4Splat(o)
            | Self::F32x4Neg(o)
            | Self::F32x4Sqrt(o)
            | Self::F32x4Abs(o)
            | Self::F64x2Splat(o)
            | Self::F64x2Neg(o)
            | Self::F64x2Sqrt(o)
            | Self::F64x2Abs(o)
            | Self::V128AnyTrue(o)
            | Self::I8x16Abs(o)
            | Self::I8x16AllTrue(o)
            | Self::I8x16Bitmask(o)
            | Self::I8x16Popcnt(o)
            | Self::I16x8Abs(o)
            | Self::I16x8AllTrue(o)
            | Self::I16x8Bitmask(o)
            | Self::I16x8ExtendLowI8x16S(o)
            | Self::I16x8ExtendHighI8x16S(o)
            | Self::I16x8ExtendLowI8x16U(o)
            | Self::I16x8ExtendHighI8x16U(o)
            | Self::I16x8ExtAddPairwiseI8x16S(o)
            | Self::I16x8ExtAddPairwiseI8x16U(o)
            | Self::I32x4Abs(o)
            | Self::I32x4AllTrue(o)
            | Self::I32x4Bitmask(o)
            | Self::I32x4ExtendLowI16x8S(o)
            | Self::I32x4ExtendHighI16x8S(o)
            | Self::I32x4ExtendLowI16x8U(o)
            | Self::I32x4ExtendHighI16x8U(o)
            | Self::I32x4ExtAddPairwiseI16x8S(o)
            | Self::I32x4ExtAddPairwiseI16x8U(o)
            | Self::I32x4TruncSatF32x4S(o)
            | Self::I32x4TruncSatF32x4U(o)
            | Self::I32x4TruncSatF64x2SZero(o)
            | Self::I32x4TruncSatF64x2UZero(o)
            | Self::I64x2Abs(o)
            | Self::I64x2AllTrue(o)
            | Self::I64x2Bitmask(o)
            | Self::I64x2ExtendLowI32x4S(o)
            | Self::I64x2ExtendHighI32x4S(o)
            | Self::I64x2ExtendLowI32x4U(o)
            | Self::I64x2ExtendHighI32x4U(o)
            | Self::F32x4Ceil(o)
            | Self::F32x4Floor(o)
            | Self::F32x4Trunc(o)
            | Self::F32x4Nearest(o)
            | Self::F32x4ConvertI32x4S(o)
            | Self::F32x4ConvertI32x4U(o)
            | Self::F32x4DemoteF64x2Zero(o)
            | Self::F64x2Ceil(o)
            | Self::F64x2Floor(o)
            | Self::F64x2Trunc(o)
            | Self::F64x2Nearest(o)
            | Self::F64x2ConvertLowI32x4S(o)
            | Self::F64x2ConvertLowI32x4U(o)
            | Self::F64x2PromoteLowF32x4(o)
            | Self::I32x4RelaxedTruncF32x4S(o)
            | Self::I32x4RelaxedTruncF32x4U(o)
            | Self::I32x4RelaxedTruncF64x2SZero(o)
            | Self::I32x4RelaxedTruncF64x2UZero(o) => f(o),
            Self::I8x16ExtractLaneS(_, o)
            | Self::I8x16ExtractLaneU(_, o)
            | Self::I16x8ExtractLaneS(_, o)
            | Self::I16x8ExtractLaneU(_, o)
            | Self::I32x4ExtractLane(_, o)
            | Self::I64x2ExtractLane(_, o)
            | Self::F32x4ExtractLane(_, o)
            | Self::F64x2ExtractLane(_, o) => f(o),
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
            | Self::RefEq(l, r)
            | Self::V128And(l, r)
            | Self::V128Or(l, r)
            | Self::V128Xor(l, r)
            | Self::I8x16Add(l, r)
            | Self::I8x16Sub(l, r)
            | Self::I8x16Eq(l, r)
            | Self::I8x16Ne(l, r)
            | Self::I8x16LtS(l, r)
            | Self::I8x16GtS(l, r)
            | Self::I8x16LeS(l, r)
            | Self::I8x16GeS(l, r)
            | Self::I8x16LtU(l, r)
            | Self::I8x16GtU(l, r)
            | Self::I8x16LeU(l, r)
            | Self::I8x16GeU(l, r)
            | Self::I8x16Shl(l, r)
            | Self::I8x16ShrS(l, r)
            | Self::I8x16ShrU(l, r)
            | Self::I8x16Swizzle(l, r)
            | Self::I16x8Add(l, r)
            | Self::I16x8Sub(l, r)
            | Self::I16x8Mul(l, r)
            | Self::I16x8Eq(l, r)
            | Self::I16x8Ne(l, r)
            | Self::I16x8LtS(l, r)
            | Self::I16x8GtS(l, r)
            | Self::I16x8LeS(l, r)
            | Self::I16x8GeS(l, r)
            | Self::I16x8LtU(l, r)
            | Self::I16x8GtU(l, r)
            | Self::I16x8LeU(l, r)
            | Self::I16x8GeU(l, r)
            | Self::I16x8Shl(l, r)
            | Self::I16x8ShrS(l, r)
            | Self::I16x8ShrU(l, r)
            | Self::I32x4Add(l, r)
            | Self::I32x4Sub(l, r)
            | Self::I32x4Mul(l, r)
            | Self::I32x4Eq(l, r)
            | Self::I32x4Ne(l, r)
            | Self::I32x4LtS(l, r)
            | Self::I32x4GtS(l, r)
            | Self::I32x4LeS(l, r)
            | Self::I32x4GeS(l, r)
            | Self::I32x4LtU(l, r)
            | Self::I32x4GtU(l, r)
            | Self::I32x4LeU(l, r)
            | Self::I32x4GeU(l, r)
            | Self::I32x4Shl(l, r)
            | Self::I32x4ShrS(l, r)
            | Self::I32x4ShrU(l, r)
            | Self::I64x2Add(l, r)
            | Self::I64x2Sub(l, r)
            | Self::I64x2Mul(l, r)
            | Self::I64x2Eq(l, r)
            | Self::I64x2Ne(l, r)
            | Self::I64x2LtS(l, r)
            | Self::I64x2GtS(l, r)
            | Self::I64x2LeS(l, r)
            | Self::I64x2GeS(l, r)
            | Self::I64x2Shl(l, r)
            | Self::I64x2ShrS(l, r)
            | Self::I64x2ShrU(l, r)
            | Self::F32x4Add(l, r)
            | Self::F32x4Sub(l, r)
            | Self::F32x4Mul(l, r)
            | Self::F32x4Div(l, r)
            | Self::F32x4Eq(l, r)
            | Self::F32x4Ne(l, r)
            | Self::F32x4Lt(l, r)
            | Self::F32x4Gt(l, r)
            | Self::F32x4Le(l, r)
            | Self::F32x4Ge(l, r)
            | Self::F32x4Min(l, r)
            | Self::F32x4Max(l, r)
            | Self::F64x2Add(l, r)
            | Self::F64x2Sub(l, r)
            | Self::F64x2Mul(l, r)
            | Self::F64x2Div(l, r)
            | Self::F64x2Eq(l, r)
            | Self::F64x2Ne(l, r)
            | Self::F64x2Lt(l, r)
            | Self::F64x2Gt(l, r)
            | Self::F64x2Le(l, r)
            | Self::F64x2Ge(l, r)
            | Self::F64x2Min(l, r)
            | Self::F64x2Max(l, r)
            | Self::V128AndNot(l, r)
            | Self::I8x16AddSatS(l, r)
            | Self::I8x16AddSatU(l, r)
            | Self::I8x16SubSatS(l, r)
            | Self::I8x16SubSatU(l, r)
            | Self::I8x16MinS(l, r)
            | Self::I8x16MinU(l, r)
            | Self::I8x16MaxS(l, r)
            | Self::I8x16MaxU(l, r)
            | Self::I8x16AvgrU(l, r)
            | Self::I8x16NarrowI16x8S(l, r)
            | Self::I8x16NarrowI16x8U(l, r)
            | Self::I16x8AddSatS(l, r)
            | Self::I16x8AddSatU(l, r)
            | Self::I16x8SubSatS(l, r)
            | Self::I16x8SubSatU(l, r)
            | Self::I16x8MinS(l, r)
            | Self::I16x8MinU(l, r)
            | Self::I16x8MaxS(l, r)
            | Self::I16x8MaxU(l, r)
            | Self::I16x8AvgrU(l, r)
            | Self::I16x8NarrowI32x4S(l, r)
            | Self::I16x8NarrowI32x4U(l, r)
            | Self::I16x8ExtMulLowI8x16S(l, r)
            | Self::I16x8ExtMulHighI8x16S(l, r)
            | Self::I16x8ExtMulLowI8x16U(l, r)
            | Self::I16x8ExtMulHighI8x16U(l, r)
            | Self::I16x8Q15MulrSatS(l, r)
            | Self::I32x4MinS(l, r)
            | Self::I32x4MinU(l, r)
            | Self::I32x4MaxS(l, r)
            | Self::I32x4MaxU(l, r)
            | Self::I32x4DotI16x8S(l, r)
            | Self::I32x4ExtMulLowI16x8S(l, r)
            | Self::I32x4ExtMulHighI16x8S(l, r)
            | Self::I32x4ExtMulLowI16x8U(l, r)
            | Self::I32x4ExtMulHighI16x8U(l, r)
            | Self::I64x2ExtMulLowI32x4S(l, r)
            | Self::I64x2ExtMulHighI32x4S(l, r)
            | Self::I64x2ExtMulLowI32x4U(l, r)
            | Self::I64x2ExtMulHighI32x4U(l, r)
            | Self::F32x4PMin(l, r)
            | Self::F32x4PMax(l, r)
            | Self::F64x2PMin(l, r)
            | Self::F64x2PMax(l, r)
            | Self::I8x16RelaxedSwizzle(l, r)
            | Self::F32x4RelaxedMin(l, r)
            | Self::F32x4RelaxedMax(l, r)
            | Self::F64x2RelaxedMin(l, r)
            | Self::F64x2RelaxedMax(l, r)
            | Self::I16x8RelaxedQ15mulrS(l, r)
            | Self::I16x8RelaxedDotI8x16I7x16S(l, r) => {
                f(l);
                f(r);
            }
            Self::I8x16ReplaceLane(_, v, val)
            | Self::I16x8ReplaceLane(_, v, val)
            | Self::I32x4ReplaceLane(_, v, val)
            | Self::I64x2ReplaceLane(_, v, val)
            | Self::F32x4ReplaceLane(_, v, val)
            | Self::F64x2ReplaceLane(_, v, val) => {
                f(v);
                f(val);
            }
            Self::V128Bitselect(a, b, c)
            | Self::I8x16RelaxedLaneselect(a, b, c)
            | Self::I16x8RelaxedLaneselect(a, b, c)
            | Self::I32x4RelaxedLaneselect(a, b, c)
            | Self::I64x2RelaxedLaneselect(a, b, c)
            | Self::F32x4RelaxedMadd(a, b, c)
            | Self::F32x4RelaxedNmadd(a, b, c)
            | Self::F64x2RelaxedMadd(a, b, c)
            | Self::F64x2RelaxedNmadd(a, b, c)
            | Self::I32x4RelaxedDotI8x16I7x16AddS(a, b, c) => {
                f(a);
                f(b);
                f(c);
            }
            Self::I8x16Shuffle(_, a, b) => {
                f(a);
                f(b);
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
            Self::MultiValueStructNew { instr, .. } | Self::MultiValueLocalBind { instr, .. } => {
                f(instr);
            }
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
            | Self::I64Load { addr, .. }
            | Self::V128Load { addr, .. } => f(addr),
            Self::I32Store { addr, value, .. }
            | Self::I32Store8 { addr, value, .. }
            | Self::I32Store16 { addr, value, .. }
            | Self::I64Store { addr, value, .. }
            | Self::V128Store { addr, value, .. } => {
                f(addr);
                f(value);
            }
            Self::MemoryFill { dst, value, len } => {
                f(dst);
                f(value);
                f(len);
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

    /// Visit all child instructions of this node mutably (non-recursive).
    /// Used by WIR optimization passes for in-place tree rewriting.
    pub fn for_each_boxed_child_mut(&mut self, f: &mut impl FnMut(&mut WirInstr)) {
        match self {
            // Leaf nodes
            Self::I32Const(_)
            | Self::I64Const(_)
            | Self::F32Const(_)
            | Self::F64Const(_)
            | Self::V128Const(_)
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
            Self::BrIf { condition, .. }
            | Self::BranchHint {
                expr: condition, ..
            } => {
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
            | Self::ArrayLen(o)
            | Self::V128Not(o)
            | Self::I8x16Splat(o)
            | Self::I8x16Neg(o)
            | Self::I16x8Splat(o)
            | Self::I16x8Neg(o)
            | Self::I32x4Splat(o)
            | Self::I32x4Neg(o)
            | Self::I64x2Splat(o)
            | Self::I64x2Neg(o)
            | Self::F32x4Splat(o)
            | Self::F32x4Neg(o)
            | Self::F32x4Sqrt(o)
            | Self::F32x4Abs(o)
            | Self::F64x2Splat(o)
            | Self::F64x2Neg(o)
            | Self::F64x2Sqrt(o)
            | Self::F64x2Abs(o)
            | Self::V128AnyTrue(o)
            | Self::I8x16Abs(o)
            | Self::I8x16AllTrue(o)
            | Self::I8x16Bitmask(o)
            | Self::I8x16Popcnt(o)
            | Self::I16x8Abs(o)
            | Self::I16x8AllTrue(o)
            | Self::I16x8Bitmask(o)
            | Self::I16x8ExtendLowI8x16S(o)
            | Self::I16x8ExtendHighI8x16S(o)
            | Self::I16x8ExtendLowI8x16U(o)
            | Self::I16x8ExtendHighI8x16U(o)
            | Self::I16x8ExtAddPairwiseI8x16S(o)
            | Self::I16x8ExtAddPairwiseI8x16U(o)
            | Self::I32x4Abs(o)
            | Self::I32x4AllTrue(o)
            | Self::I32x4Bitmask(o)
            | Self::I32x4ExtendLowI16x8S(o)
            | Self::I32x4ExtendHighI16x8S(o)
            | Self::I32x4ExtendLowI16x8U(o)
            | Self::I32x4ExtendHighI16x8U(o)
            | Self::I32x4ExtAddPairwiseI16x8S(o)
            | Self::I32x4ExtAddPairwiseI16x8U(o)
            | Self::I32x4TruncSatF32x4S(o)
            | Self::I32x4TruncSatF32x4U(o)
            | Self::I32x4TruncSatF64x2SZero(o)
            | Self::I32x4TruncSatF64x2UZero(o)
            | Self::I64x2Abs(o)
            | Self::I64x2AllTrue(o)
            | Self::I64x2Bitmask(o)
            | Self::I64x2ExtendLowI32x4S(o)
            | Self::I64x2ExtendHighI32x4S(o)
            | Self::I64x2ExtendLowI32x4U(o)
            | Self::I64x2ExtendHighI32x4U(o)
            | Self::F32x4Ceil(o)
            | Self::F32x4Floor(o)
            | Self::F32x4Trunc(o)
            | Self::F32x4Nearest(o)
            | Self::F32x4ConvertI32x4S(o)
            | Self::F32x4ConvertI32x4U(o)
            | Self::F32x4DemoteF64x2Zero(o)
            | Self::F64x2Ceil(o)
            | Self::F64x2Floor(o)
            | Self::F64x2Trunc(o)
            | Self::F64x2Nearest(o)
            | Self::F64x2ConvertLowI32x4S(o)
            | Self::F64x2ConvertLowI32x4U(o)
            | Self::F64x2PromoteLowF32x4(o)
            | Self::I32x4RelaxedTruncF32x4S(o)
            | Self::I32x4RelaxedTruncF32x4U(o)
            | Self::I32x4RelaxedTruncF64x2SZero(o)
            | Self::I32x4RelaxedTruncF64x2UZero(o) => f(o),
            Self::I8x16ExtractLaneS(_, o)
            | Self::I8x16ExtractLaneU(_, o)
            | Self::I16x8ExtractLaneS(_, o)
            | Self::I16x8ExtractLaneU(_, o)
            | Self::I32x4ExtractLane(_, o)
            | Self::I64x2ExtractLane(_, o)
            | Self::F32x4ExtractLane(_, o)
            | Self::F64x2ExtractLane(_, o) => f(o),
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
            | Self::RefEq(l, r)
            | Self::V128And(l, r)
            | Self::V128Or(l, r)
            | Self::V128Xor(l, r)
            | Self::I8x16Add(l, r)
            | Self::I8x16Sub(l, r)
            | Self::I8x16Eq(l, r)
            | Self::I8x16Ne(l, r)
            | Self::I8x16LtS(l, r)
            | Self::I8x16GtS(l, r)
            | Self::I8x16LeS(l, r)
            | Self::I8x16GeS(l, r)
            | Self::I8x16LtU(l, r)
            | Self::I8x16GtU(l, r)
            | Self::I8x16LeU(l, r)
            | Self::I8x16GeU(l, r)
            | Self::I8x16Shl(l, r)
            | Self::I8x16ShrS(l, r)
            | Self::I8x16ShrU(l, r)
            | Self::I8x16Swizzle(l, r)
            | Self::I16x8Add(l, r)
            | Self::I16x8Sub(l, r)
            | Self::I16x8Mul(l, r)
            | Self::I16x8Eq(l, r)
            | Self::I16x8Ne(l, r)
            | Self::I16x8LtS(l, r)
            | Self::I16x8GtS(l, r)
            | Self::I16x8LeS(l, r)
            | Self::I16x8GeS(l, r)
            | Self::I16x8LtU(l, r)
            | Self::I16x8GtU(l, r)
            | Self::I16x8LeU(l, r)
            | Self::I16x8GeU(l, r)
            | Self::I16x8Shl(l, r)
            | Self::I16x8ShrS(l, r)
            | Self::I16x8ShrU(l, r)
            | Self::I32x4Add(l, r)
            | Self::I32x4Sub(l, r)
            | Self::I32x4Mul(l, r)
            | Self::I32x4Eq(l, r)
            | Self::I32x4Ne(l, r)
            | Self::I32x4LtS(l, r)
            | Self::I32x4GtS(l, r)
            | Self::I32x4LeS(l, r)
            | Self::I32x4GeS(l, r)
            | Self::I32x4LtU(l, r)
            | Self::I32x4GtU(l, r)
            | Self::I32x4LeU(l, r)
            | Self::I32x4GeU(l, r)
            | Self::I32x4Shl(l, r)
            | Self::I32x4ShrS(l, r)
            | Self::I32x4ShrU(l, r)
            | Self::I64x2Add(l, r)
            | Self::I64x2Sub(l, r)
            | Self::I64x2Mul(l, r)
            | Self::I64x2Eq(l, r)
            | Self::I64x2Ne(l, r)
            | Self::I64x2LtS(l, r)
            | Self::I64x2GtS(l, r)
            | Self::I64x2LeS(l, r)
            | Self::I64x2GeS(l, r)
            | Self::I64x2Shl(l, r)
            | Self::I64x2ShrS(l, r)
            | Self::I64x2ShrU(l, r)
            | Self::F32x4Add(l, r)
            | Self::F32x4Sub(l, r)
            | Self::F32x4Mul(l, r)
            | Self::F32x4Div(l, r)
            | Self::F32x4Eq(l, r)
            | Self::F32x4Ne(l, r)
            | Self::F32x4Lt(l, r)
            | Self::F32x4Gt(l, r)
            | Self::F32x4Le(l, r)
            | Self::F32x4Ge(l, r)
            | Self::F32x4Min(l, r)
            | Self::F32x4Max(l, r)
            | Self::F64x2Add(l, r)
            | Self::F64x2Sub(l, r)
            | Self::F64x2Mul(l, r)
            | Self::F64x2Div(l, r)
            | Self::F64x2Eq(l, r)
            | Self::F64x2Ne(l, r)
            | Self::F64x2Lt(l, r)
            | Self::F64x2Gt(l, r)
            | Self::F64x2Le(l, r)
            | Self::F64x2Ge(l, r)
            | Self::F64x2Min(l, r)
            | Self::F64x2Max(l, r)
            | Self::V128AndNot(l, r)
            | Self::I8x16AddSatS(l, r)
            | Self::I8x16AddSatU(l, r)
            | Self::I8x16SubSatS(l, r)
            | Self::I8x16SubSatU(l, r)
            | Self::I8x16MinS(l, r)
            | Self::I8x16MinU(l, r)
            | Self::I8x16MaxS(l, r)
            | Self::I8x16MaxU(l, r)
            | Self::I8x16AvgrU(l, r)
            | Self::I8x16NarrowI16x8S(l, r)
            | Self::I8x16NarrowI16x8U(l, r)
            | Self::I16x8AddSatS(l, r)
            | Self::I16x8AddSatU(l, r)
            | Self::I16x8SubSatS(l, r)
            | Self::I16x8SubSatU(l, r)
            | Self::I16x8MinS(l, r)
            | Self::I16x8MinU(l, r)
            | Self::I16x8MaxS(l, r)
            | Self::I16x8MaxU(l, r)
            | Self::I16x8AvgrU(l, r)
            | Self::I16x8NarrowI32x4S(l, r)
            | Self::I16x8NarrowI32x4U(l, r)
            | Self::I16x8ExtMulLowI8x16S(l, r)
            | Self::I16x8ExtMulHighI8x16S(l, r)
            | Self::I16x8ExtMulLowI8x16U(l, r)
            | Self::I16x8ExtMulHighI8x16U(l, r)
            | Self::I16x8Q15MulrSatS(l, r)
            | Self::I32x4MinS(l, r)
            | Self::I32x4MinU(l, r)
            | Self::I32x4MaxS(l, r)
            | Self::I32x4MaxU(l, r)
            | Self::I32x4DotI16x8S(l, r)
            | Self::I32x4ExtMulLowI16x8S(l, r)
            | Self::I32x4ExtMulHighI16x8S(l, r)
            | Self::I32x4ExtMulLowI16x8U(l, r)
            | Self::I32x4ExtMulHighI16x8U(l, r)
            | Self::I64x2ExtMulLowI32x4S(l, r)
            | Self::I64x2ExtMulHighI32x4S(l, r)
            | Self::I64x2ExtMulLowI32x4U(l, r)
            | Self::I64x2ExtMulHighI32x4U(l, r)
            | Self::F32x4PMin(l, r)
            | Self::F32x4PMax(l, r)
            | Self::F64x2PMin(l, r)
            | Self::F64x2PMax(l, r)
            | Self::I8x16RelaxedSwizzle(l, r)
            | Self::F32x4RelaxedMin(l, r)
            | Self::F32x4RelaxedMax(l, r)
            | Self::F64x2RelaxedMin(l, r)
            | Self::F64x2RelaxedMax(l, r)
            | Self::I16x8RelaxedQ15mulrS(l, r)
            | Self::I16x8RelaxedDotI8x16I7x16S(l, r) => {
                f(l);
                f(r);
            }
            Self::I8x16ReplaceLane(_, v, val)
            | Self::I16x8ReplaceLane(_, v, val)
            | Self::I32x4ReplaceLane(_, v, val)
            | Self::I64x2ReplaceLane(_, v, val)
            | Self::F32x4ReplaceLane(_, v, val)
            | Self::F64x2ReplaceLane(_, v, val) => {
                f(v);
                f(val);
            }
            Self::V128Bitselect(a, b, c)
            | Self::I8x16RelaxedLaneselect(a, b, c)
            | Self::I16x8RelaxedLaneselect(a, b, c)
            | Self::I32x4RelaxedLaneselect(a, b, c)
            | Self::I64x2RelaxedLaneselect(a, b, c)
            | Self::F32x4RelaxedMadd(a, b, c)
            | Self::F32x4RelaxedNmadd(a, b, c)
            | Self::F64x2RelaxedMadd(a, b, c)
            | Self::F64x2RelaxedNmadd(a, b, c)
            | Self::I32x4RelaxedDotI8x16I7x16AddS(a, b, c) => {
                f(a);
                f(b);
                f(c);
            }
            Self::I8x16Shuffle(_, a, b) => {
                f(a);
                f(b);
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
            Self::MultiValueStructNew { instr, .. } | Self::MultiValueLocalBind { instr, .. } => {
                f(instr);
            }
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
            | Self::I64Load { addr, .. }
            | Self::V128Load { addr, .. } => f(addr),
            Self::I32Store { addr, value, .. }
            | Self::I32Store8 { addr, value, .. }
            | Self::I32Store16 { addr, value, .. }
            | Self::I64Store { addr, value, .. }
            | Self::V128Store { addr, value, .. } => {
                f(addr);
                f(value);
            }
            Self::MemoryFill { dst, value, len } => {
                f(dst);
                f(value);
                f(len);
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

/// A defined linear memory.
#[derive(Debug, Clone)]
pub struct WirMemory {
    /// Minimum size in pages.
    pub min: u32,
    /// Maximum size in pages (optional).
    pub max: Option<u32>,
}

/// A global variable.
#[derive(Debug, Clone)]
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

/// Component Model wrapper information.
#[derive(Debug, Default)]
pub struct WirComponent {
    /// Component Model interfaces to import.
    pub cm_imports: Vec<WirCmImport>,
    /// Bundled modules (fts, libm).
    pub bundled_modules: Vec<WirBundledModule>,
    /// World exports.
    pub world_exports: Vec<WirWorldExport>,
    /// Memory module configuration.
    pub memory: WirMemoryConfig,
}

/// A Component Model interface import.
#[derive(Debug)]
pub struct WirCmImport {
    /// Interface name (e.g., "wasi:cli/stdout@0.2.0").
    pub interface: String,
    /// Functions imported from this interface.
    pub functions: Vec<WirCmFunc>,
}

/// A function from a CM interface.
#[derive(Debug)]
pub struct WirCmFunc {
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
