//! Typed Intermediate Representation (TIR) for Wado
//!
//! TIR is the post-type-resolution representation used for lowering,
//! optimization, and code generation. Every expression has a resolved type.
//!
//! Key properties:
//! - All types resolved to TypeId (no string-based type names)
//! - All variable references resolved (local index known)
//! - All function calls resolved
//! - No syntactic sugar (desugared before TIR)

use std::collections::HashMap;

use crate::token::Span;

// ============================================================================
// Type System
// ============================================================================

pub type TypeId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimitiveType {
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
    F32,
    F64,
    Bool,
    Char,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResolvedType {
    Primitive(PrimitiveType),
    Unit,
    Never,
    String,
    Struct {
        name: String,
        module_path: Vec<String>,
    },
    Enum {
        name: String,
        module_path: Vec<String>,
    },
    Variant {
        name: String,
        module_path: Vec<String>,
    },
    Array(TypeId),
    Option(TypeId),
    Result {
        ok: TypeId,
        err: TypeId,
    },
    Stream(TypeId),
    Future(TypeId),
    Ref(TypeId),
    MutRef(TypeId),
    Function {
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<String>,
    },
    Tuple(Vec<TypeId>),
    Dict {
        key: TypeId,
        value: TypeId,
    },
    Reactive(TypeId),
    /// Type parameter (e.g., `T` in `struct Box<T>`)
    /// Used before monomorphization; should be substituted with concrete types
    TypeParam {
        name: String,
        /// Index of the type parameter in the generic definition (0 for first param)
        index: u32,
    },
    /// Generic struct instantiation (e.g., `Box<i32>`)
    /// Used to track instantiation sites before monomorphization
    GenericInstance {
        /// Base generic type name (e.g., "Box")
        name: String,
        module_path: Vec<String>,
        /// Concrete type arguments (e.g., [i32])
        type_args: Vec<TypeId>,
    },
    /// Raw GC array intrinsic (`builtin::array<T>`)
    /// This is the underlying storage type for String and Array<T> structs
    BuiltinArray(TypeId),
    Unknown,
    Error,
}

#[derive(Debug, Clone)]
pub struct TypeTable {
    types: Vec<ResolvedType>,
    intern_map: HashMap<ResolvedType, TypeId>,
}

impl Default for TypeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeTable {
    pub const I8: TypeId = 0;
    pub const I16: TypeId = 1;
    pub const I32: TypeId = 2;
    pub const I64: TypeId = 3;
    pub const I128: TypeId = 4;
    pub const U8: TypeId = 5;
    pub const U16: TypeId = 6;
    pub const U32: TypeId = 7;
    pub const U64: TypeId = 8;
    pub const U128: TypeId = 9;
    pub const F32: TypeId = 10;
    pub const F64: TypeId = 11;
    pub const BOOL: TypeId = 12;
    pub const CHAR: TypeId = 13;
    pub const UNIT: TypeId = 14;
    pub const NEVER: TypeId = 15;
    // STRING removed - String is now a user-defined struct in core/prelude
    pub const UNKNOWN: TypeId = 16;
    pub const ERROR: TypeId = 17;

    pub fn new() -> Self {
        let mut table = Self {
            types: Vec::new(),
            intern_map: HashMap::new(),
        };

        // Pre-populate primitive types matching the constants above
        table.intern(ResolvedType::Primitive(PrimitiveType::I8));
        table.intern(ResolvedType::Primitive(PrimitiveType::I16));
        table.intern(ResolvedType::Primitive(PrimitiveType::I32));
        table.intern(ResolvedType::Primitive(PrimitiveType::I64));
        table.intern(ResolvedType::Primitive(PrimitiveType::I128));
        table.intern(ResolvedType::Primitive(PrimitiveType::U8));
        table.intern(ResolvedType::Primitive(PrimitiveType::U16));
        table.intern(ResolvedType::Primitive(PrimitiveType::U32));
        table.intern(ResolvedType::Primitive(PrimitiveType::U64));
        table.intern(ResolvedType::Primitive(PrimitiveType::U128));
        table.intern(ResolvedType::Primitive(PrimitiveType::F32));
        table.intern(ResolvedType::Primitive(PrimitiveType::F64));
        table.intern(ResolvedType::Primitive(PrimitiveType::Bool));
        table.intern(ResolvedType::Primitive(PrimitiveType::Char));
        table.intern(ResolvedType::Unit);
        table.intern(ResolvedType::Never);
        // ResolvedType::String removed - String is now a user-defined struct
        table.intern(ResolvedType::Unknown);
        table.intern(ResolvedType::Error);

        table
    }

    pub fn intern(&mut self, ty: ResolvedType) -> TypeId {
        if let Some(&id) = self.intern_map.get(&ty) {
            return id;
        }
        let id = self.types.len() as TypeId;
        self.types.push(ty.clone());
        self.intern_map.insert(ty, id);
        id
    }

    pub fn get(&self, id: TypeId) -> &ResolvedType {
        &self.types[id as usize]
    }

    pub fn is_integer(&self, id: TypeId) -> bool {
        matches!(
            self.get(id),
            ResolvedType::Primitive(
                PrimitiveType::I8
                    | PrimitiveType::I16
                    | PrimitiveType::I32
                    | PrimitiveType::I64
                    | PrimitiveType::I128
                    | PrimitiveType::U8
                    | PrimitiveType::U16
                    | PrimitiveType::U32
                    | PrimitiveType::U64
                    | PrimitiveType::U128
            )
        )
    }

    pub fn is_float(&self, id: TypeId) -> bool {
        matches!(
            self.get(id),
            ResolvedType::Primitive(PrimitiveType::F32 | PrimitiveType::F64)
        )
    }

    pub fn is_numeric(&self, id: TypeId) -> bool {
        self.is_integer(id) || self.is_float(id)
    }

    pub fn len(&self) -> usize {
        self.types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.types.len() <= 19
    }

    pub fn make_array(&mut self, element: TypeId) -> TypeId {
        self.intern(ResolvedType::Array(element))
    }

    /// Create a raw GC array type (`builtin::array<T>`)
    pub fn make_builtin_array(&mut self, element: TypeId) -> TypeId {
        self.intern(ResolvedType::BuiltinArray(element))
    }

    pub fn make_option(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::Option(inner))
    }

    pub fn make_result(&mut self, ok: TypeId, err: TypeId) -> TypeId {
        self.intern(ResolvedType::Result { ok, err })
    }

    pub fn make_tuple(&mut self, elements: Vec<TypeId>) -> TypeId {
        self.intern(ResolvedType::Tuple(elements))
    }

    pub fn make_function(
        &mut self,
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<String>,
    ) -> TypeId {
        self.intern(ResolvedType::Function {
            params,
            return_type,
            effects,
        })
    }

    pub fn make_struct(&mut self, name: String, module_path: Vec<String>) -> TypeId {
        self.intern(ResolvedType::Struct { name, module_path })
    }

    /// Find the type_id for a struct by name and module path (O(1) lookup via intern_map)
    pub fn find_struct_type(&self, name: &str, module_path: &[String]) -> Option<TypeId> {
        // Use the existing intern_map for O(1) lookup
        let key = ResolvedType::Struct {
            name: name.to_string(),
            module_path: module_path.to_vec(),
        };
        self.intern_map.get(&key).copied()
    }

    pub fn make_enum(&mut self, name: String, module_path: Vec<String>) -> TypeId {
        self.intern(ResolvedType::Enum { name, module_path })
    }

    pub fn make_ref(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::Ref(inner))
    }

    pub fn make_mut_ref(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::MutRef(inner))
    }

    /// Create a type parameter (e.g., `T` in `struct Box<T>`)
    pub fn make_type_param(&mut self, name: String, index: u32) -> TypeId {
        self.intern(ResolvedType::TypeParam { name, index })
    }

    /// Create a generic instance (e.g., `Box<i32>`)
    pub fn make_generic_instance(
        &mut self,
        name: String,
        module_path: Vec<String>,
        type_args: Vec<TypeId>,
    ) -> TypeId {
        self.intern(ResolvedType::GenericInstance {
            name,
            module_path,
            type_args,
        })
    }

    /// Check if a type is or contains type parameters
    pub fn contains_type_param(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. } => true,
            ResolvedType::Array(inner)
            | ResolvedType::BuiltinArray(inner)
            | ResolvedType::Option(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Stream(inner)
            | ResolvedType::Future(inner)
            | ResolvedType::Reactive(inner) => self.contains_type_param(*inner),
            ResolvedType::Result { ok, err }
            | ResolvedType::Dict {
                key: ok,
                value: err,
            } => self.contains_type_param(*ok) || self.contains_type_param(*err),
            ResolvedType::Tuple(elems) => elems.iter().any(|e| self.contains_type_param(*e)),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params.iter().any(|p| self.contains_type_param(*p))
                    || self.contains_type_param(*return_type)
            }
            ResolvedType::GenericInstance { type_args, .. } => {
                type_args.iter().any(|t| self.contains_type_param(*t))
            }
            _ => false,
        }
    }

    /// Get a human-readable name for a type
    pub fn type_name(&self, id: TypeId) -> String {
        match self.get(id) {
            ResolvedType::Primitive(p) => match p {
                PrimitiveType::I8 => "i8".to_string(),
                PrimitiveType::I16 => "i16".to_string(),
                PrimitiveType::I32 => "i32".to_string(),
                PrimitiveType::I64 => "i64".to_string(),
                PrimitiveType::I128 => "i128".to_string(),
                PrimitiveType::U8 => "u8".to_string(),
                PrimitiveType::U16 => "u16".to_string(),
                PrimitiveType::U32 => "u32".to_string(),
                PrimitiveType::U64 => "u64".to_string(),
                PrimitiveType::U128 => "u128".to_string(),
                PrimitiveType::F32 => "f32".to_string(),
                PrimitiveType::F64 => "f64".to_string(),
                PrimitiveType::Bool => "bool".to_string(),
                PrimitiveType::Char => "char".to_string(),
            },
            ResolvedType::Unit => "()".to_string(),
            ResolvedType::Never => "!".to_string(),
            ResolvedType::String => "String".to_string(),
            ResolvedType::Unknown => "unknown".to_string(),
            ResolvedType::Error => "error".to_string(),
            ResolvedType::Array(elem) => format!("Array<{}>", self.type_name(*elem)),
            ResolvedType::BuiltinArray(elem) => {
                format!("builtin::array<{}>", self.type_name(*elem))
            }
            ResolvedType::Tuple(elems) => {
                let elem_names: Vec<String> = elems.iter().map(|e| self.type_name(*e)).collect();
                format!("[{}]", elem_names.join(", "))
            }
            ResolvedType::Option(inner) => format!("Option<{}>", self.type_name(*inner)),
            ResolvedType::Result { ok, err } => {
                format!("Result<{}, {}>", self.type_name(*ok), self.type_name(*err))
            }
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::Enum { name, .. } => name.clone(),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                let param_names: Vec<String> = params.iter().map(|p| self.type_name(*p)).collect();
                format!(
                    "fn({}) -> {}",
                    param_names.join(", "),
                    self.type_name(*return_type)
                )
            }
            ResolvedType::Ref(inner) => format!("&{}", self.type_name(*inner)),
            ResolvedType::MutRef(inner) => format!("&mut {}", self.type_name(*inner)),
            ResolvedType::Variant { name, .. } => name.clone(),
            ResolvedType::Stream(inner) => format!("Stream<{}>", self.type_name(*inner)),
            ResolvedType::Future(inner) => format!("Future<{}>", self.type_name(*inner)),
            ResolvedType::Dict { key, value } => {
                format!("Dict<{}, {}>", self.type_name(*key), self.type_name(*value))
            }
            ResolvedType::Reactive(inner) => format!("Reactive<{}>", self.type_name(*inner)),
            ResolvedType::TypeParam { name, .. } => name.clone(),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let arg_names: Vec<String> = type_args.iter().map(|t| self.type_name(*t)).collect();
                format!("{}<{}>", name, arg_names.join(", "))
            }
        }
    }
}

// ============================================================================
// Expressions
// ============================================================================

#[derive(Debug, Clone)]
pub struct TirExpr {
    pub kind: TirExprKind,
    pub type_id: TypeId,
    pub span: Span,
}

impl TirExpr {
    pub fn new(kind: TirExprKind, type_id: TypeId, span: Span) -> Self {
        Self {
            kind,
            type_id,
            span,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TirExprKind {
    IntLiteral {
        value: u64,
        repr: String,
    },
    FloatLiteral {
        value: f64,
        repr: String,
    },
    BoolLiteral(bool),
    CharLiteral(char),
    StringLiteral(String),
    Null,
    Unit,

    Local {
        index: u32,
        name: String,
    },
    Global {
        module_path: Vec<String>,
        name: String,
    },

    Binary {
        left: Box<TirExpr>,
        op: TirBinaryOp,
        right: Box<TirExpr>,
    },
    Unary {
        op: TirUnaryOp,
        expr: Box<TirExpr>,
    },
    Assign {
        target: Box<TirExpr>,
        value: Box<TirExpr>,
    },
    Cast {
        expr: Box<TirExpr>,
        target_type: TypeId,
    },

    Call {
        module_path: Vec<String>,
        func_name: String,
        /// Explicit type arguments for generic functions: `identity::<i32>(x)`
        type_args: Vec<TypeId>,
        args: Vec<TirExpr>,
    },
    EffectCall {
        effect_name: String,
        op_name: String,
        args: Vec<TirExpr>,
    },
    MethodCall {
        receiver: Box<TirExpr>,
        method_name: String,
        /// Explicit type arguments for generic methods: `obj.method::<i32>()`
        type_args: Vec<TypeId>,
        args: Vec<TirExpr>,
    },

    FieldAccess {
        expr: Box<TirExpr>,
        field_index: u32,
        field_name: String,
    },
    Index {
        expr: Box<TirExpr>,
        index: Box<TirExpr>,
    },

    Block(TirBlock),
    If {
        condition: Box<TirExpr>,
        then_branch: TirBlock,
        else_branch: Option<TirBlock>,
    },
    Match {
        expr: Box<TirExpr>,
        arms: Vec<TirMatchArm>,
    },

    StructLiteral {
        struct_type: TypeId,
        struct_name: String,
        fields: Vec<TirStructField>,
    },
    ArrayLiteral {
        elements: Vec<TirExpr>,
    },
    TupleLiteral {
        elements: Vec<TirExpr>,
    },

    Closure {
        params: Vec<(String, TypeId)>,
        body: Box<TirExpr>,
        captures: Vec<TirCapture>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TirBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TirUnaryOp {
    Neg,
    Not,
    BitNot,
    Ref,
    MutRef,
    Deref,
}

#[derive(Debug, Clone)]
pub struct TirMatchArm {
    pub pattern: TirPattern,
    pub body: TirExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TirPattern {
    Wildcard,
    Binding {
        name: String,
        local_index: u32,
    },
    Literal(TirLiteralPattern),
    Tuple(Vec<TirPattern>),
    Variant {
        enum_type: TypeId,
        variant_name: String,
        bindings: Vec<TirPattern>,
    },
}

#[derive(Debug, Clone)]
pub enum TirLiteralPattern {
    Int(u64),
    Bool(bool),
    Char(char),
    String(String),
    Null,
}

#[derive(Debug, Clone)]
pub struct TirStructField {
    pub name: String,
    pub value: TirExpr,
    pub field_index: u32,
}

#[derive(Debug, Clone)]
pub struct TirCapture {
    pub name: String,
    pub outer_index: u32,
    pub type_id: TypeId,
    pub is_mut: bool,
}

// ============================================================================
// Statements
// ============================================================================

#[derive(Debug, Clone)]
pub struct TirBlock {
    pub stmts: Vec<TirStmt>,
    pub span: Span,
}

impl TirBlock {
    pub fn new(stmts: Vec<TirStmt>, span: Span) -> Self {
        Self { stmts, span }
    }

    pub fn empty(span: Span) -> Self {
        Self {
            stmts: Vec::new(),
            span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TirStmt {
    pub kind: TirStmtKind,
    pub span: Span,
}

impl TirStmt {
    pub fn new(kind: TirStmtKind, span: Span) -> Self {
        Self { kind, span }
    }
}

#[derive(Debug, Clone)]
pub enum TirStmtKind {
    Let {
        name: String,
        local_index: u32,
        is_mut: bool,
        is_reactive: bool,
        type_id: TypeId,
        value: TirExpr,
    },
    Expr(TirExpr),
    Return {
        value: Option<TirExpr>,
    },
    If {
        condition: TirExpr,
        then_block: TirBlock,
        else_block: Option<TirBlock>,
    },
    While {
        condition: TirExpr,
        body: TirBlock,
    },
    /// C-style for loop: continue executes update, break exits loop
    For {
        condition: Option<TirExpr>,
        body: TirBlock,
        update: Option<TirExpr>,
    },
    Loop {
        body: TirBlock,
    },
    /// For-of loop: `for let item of array { ... }`
    ForOf {
        /// Local index for the loop binding variable
        binding_local: u32,
        /// Type of the binding (element type of the array)
        binding_type: TypeId,
        /// Whether the binding is mutable
        is_mut: bool,
        /// The array expression to iterate over
        iterable: TirExpr,
        /// Type of the iterable (should be Array<T>)
        iterable_type: TypeId,
        body: TirBlock,
    },
    Break,
    Continue,
    Assert {
        condition: TirExpr,
        condition_source: String,
        message: Option<TirExpr>,
        intermediates: Vec<(String, TirExpr, TypeId)>,
    },
}

// ============================================================================
// Items (Top-level Declarations)
// ============================================================================

/// Generic type parameter in TIR (from AST GenericParam)
#[derive(Debug, Clone)]
pub struct TirTypeParam {
    pub name: String,
    pub bounds: Vec<String>,
    pub index: u32,
}

/// Information about monomorphization origin for instantiated items
#[derive(Debug, Clone)]
pub struct MonomorphInfo {
    /// Original generic name (e.g., "Box" for "Box$i32")
    pub generic_name: String,
    /// Concrete type arguments used for this instantiation
    pub type_args: Vec<TypeId>,
}

#[derive(Debug, Clone)]
pub struct TirFunction {
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters (empty for non-generic functions)
    pub type_params: Vec<TirTypeParam>,
    /// If this function was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub params: Vec<TirParam>,
    pub return_type: TypeId,
    pub effects: Vec<String>,
    pub body: Option<TirBlock>,
    pub span: Span,
    pub local_count: u32,
    pub local_types: Vec<TypeId>,
    /// Local indices that have their address taken (&x or &mut x).
    /// For mutable primitives, these locals are stored in box structs.
    pub address_taken_locals: std::collections::HashSet<u32>,
}

#[derive(Debug, Clone)]
pub struct TirParam {
    pub name: String,
    pub type_id: TypeId,
    pub local_index: u32,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirStruct {
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters (empty for non-generic structs)
    pub type_params: Vec<TirTypeParam>,
    /// If this struct was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub fields: Vec<TirField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirField {
    pub name: String,
    pub type_id: TypeId,
    pub index: u32,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirEnum {
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters (empty for non-generic enums)
    pub type_params: Vec<TirTypeParam>,
    /// If this enum was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub variants: Vec<TirVariant>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirVariant {
    pub name: String,
    pub index: u32,
    pub fields: Vec<TypeId>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirTypeAlias {
    pub name: String,
    pub is_pub: bool,
    pub type_id: TypeId,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirEffect {
    pub name: String,
    pub is_pub: bool,
    pub operations: Vec<TirEffectOp>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirEffectOp {
    pub name: String,
    pub params: Vec<TirParam>,
    pub return_type: TypeId,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirImpl {
    /// Generic type parameters for the impl block (e.g., `impl<T> Box<T>`)
    pub type_params: Vec<TirTypeParam>,
    pub target_type: TypeId,
    pub methods: Vec<TirFunction>,
    pub span: Span,
}

// ============================================================================
// Module
// ============================================================================

/// Tracks a requested instantiation of a generic item
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstantiationKey {
    /// Name of the generic item (struct, function, or enum)
    pub name: String,
    /// Concrete type arguments for instantiation
    pub type_args: Vec<TypeId>,
}

#[derive(Debug, Clone)]
pub struct TirModule {
    pub path: Vec<String>,
    pub type_table: TypeTable,
    pub functions: Vec<TirFunction>,
    pub structs: Vec<TirStruct>,
    pub enums: Vec<TirEnum>,
    pub type_aliases: Vec<TirTypeAlias>,
    pub effects: Vec<TirEffect>,
    pub impls: Vec<TirImpl>,
    pub data_section: Option<String>,
    pub string_literals: Vec<String>,
    /// Generic struct definitions (before monomorphization)
    /// Key: struct name
    pub generic_structs: HashMap<String, TirStruct>,
    /// Generic function definitions (before monomorphization)
    /// Key: function name
    pub generic_functions: HashMap<String, TirFunction>,
    /// Requested instantiations (populated during resolution, processed in lower)
    pub instantiation_requests: std::collections::HashSet<InstantiationKey>,
}

impl TirModule {
    pub fn new(path: Vec<String>) -> Self {
        Self {
            path,
            type_table: TypeTable::new(),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            type_aliases: Vec::new(),
            effects: Vec::new(),
            impls: Vec::new(),
            data_section: None,
            string_literals: Vec::new(),
            generic_structs: HashMap::new(),
            generic_functions: HashMap::new(),
            instantiation_requests: std::collections::HashSet::new(),
        }
    }

    pub fn with_type_table(path: Vec<String>, type_table: TypeTable) -> Self {
        Self {
            path,
            type_table,
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            type_aliases: Vec::new(),
            effects: Vec::new(),
            impls: Vec::new(),
            data_section: None,
            string_literals: Vec::new(),
            generic_structs: HashMap::new(),
            generic_functions: HashMap::new(),
            instantiation_requests: std::collections::HashSet::new(),
        }
    }

    pub fn with_data_section(mut self, data_section: Option<String>) -> Self {
        self.data_section = data_section;
        self
    }

    pub fn data_section(&self) -> Option<&str> {
        self.data_section.as_deref()
    }

    pub fn add_function(&mut self, func: TirFunction) {
        self.functions.push(func);
    }

    pub fn add_struct(&mut self, s: TirStruct) {
        self.structs.push(s);
    }

    pub fn add_enum(&mut self, e: TirEnum) {
        self.enums.push(e);
    }

    pub fn add_type_alias(&mut self, alias: TirTypeAlias) {
        self.type_aliases.push(alias);
    }

    pub fn add_effect(&mut self, effect: TirEffect) {
        self.effects.push(effect);
    }

    pub fn add_impl(&mut self, impl_block: TirImpl) {
        self.impls.push(impl_block);
    }

    pub fn find_function(&self, name: &str) -> Option<&TirFunction> {
        self.functions.iter().find(|f| f.name == name)
    }

    pub fn find_struct(&self, name: &str) -> Option<&TirStruct> {
        self.structs.iter().find(|s| s.name == name)
    }

    pub fn find_enum(&self, name: &str) -> Option<&TirEnum> {
        self.enums.iter().find(|e| e.name == name)
    }
}

#[derive(Debug)]
pub struct TirProgram {
    pub main_module: TirModule,
    pub dependencies: Vec<TirModule>,
    pub type_table: TypeTable,
}

impl TirProgram {
    pub fn new(main_module: TirModule) -> Self {
        Self {
            type_table: TypeTable::new(),
            main_module,
            dependencies: Vec::new(),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_primitive_constants() {
        let table = TypeTable::new();
        assert!(matches!(
            table.get(TypeTable::I32),
            ResolvedType::Primitive(PrimitiveType::I32)
        ));
        assert!(matches!(
            table.get(TypeTable::BOOL),
            ResolvedType::Primitive(PrimitiveType::Bool)
        ));
        // Note: String is now a user-defined struct, not a builtin type
        assert!(matches!(table.get(TypeTable::UNIT), ResolvedType::Unit));
    }

    #[test]
    fn test_intern_deduplication() {
        let mut table = TypeTable::new();
        let arr1 = table.make_array(TypeTable::I32);
        let arr2 = table.make_array(TypeTable::I32);
        assert_eq!(arr1, arr2);
    }

    #[test]
    fn test_composite_types() {
        let mut table = TypeTable::new();
        let option_i32 = table.make_option(TypeTable::I32);
        // Use I64 as error type since String is now a user-defined struct
        let result_i32_i64 = table.make_result(TypeTable::I32, TypeTable::I64);

        assert!(matches!(
            table.get(option_i32),
            ResolvedType::Option(id) if *id == TypeTable::I32
        ));
        assert!(matches!(
            table.get(result_i32_i64),
            ResolvedType::Result { ok, err } if *ok == TypeTable::I32 && *err == TypeTable::I64
        ));
    }
}
