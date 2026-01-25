//! Typed Intermediate Representation (TIR) for Wado
//!
//! TIR is the post-type-resolution representation used for lowering,
//! optimization, and code generation. Every expression has a resolved type.
//!
//! Key properties:
//! - All types resolved to `TypeId` (no string-based type names)
//! - All variable references resolved (local index known)
//! - All function calls resolved
//! - No syntactic sugar (desugared before TIR)

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::component_model::CmCallConvention;
use crate::name::{LocalMethodName, ModuleSource};
use crate::token::Span;

// ============================================================================
// Type Parameter Substitution System
// ============================================================================

/// Identifies the scope where a type parameter is defined
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeParamScope {
    /// Type parameter from struct/impl block (e.g., T in `impl Container<T>`)
    Impl,
    /// Type parameter from method signature (e.g., U in `fn transform<U>`)
    Method,
    /// Type parameter from free function (e.g., T in `fn identity<T>`)
    Function,
}

/// Identifies a type parameter with its scope and index
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeParamId {
    pub scope: TypeParamScope,
    pub index: u32,
}

impl TypeParamId {
    pub fn impl_param(index: u32) -> Self {
        Self {
            scope: TypeParamScope::Impl,
            index,
        }
    }

    pub fn method_param(index: u32) -> Self {
        Self {
            scope: TypeParamScope::Method,
            index,
        }
    }

    pub fn function_param(index: u32) -> Self {
        Self {
            scope: TypeParamScope::Function,
            index,
        }
    }
}

/// Unified substitution context for type parameter resolution
///
/// This handles the complexity of double generics (methods with both struct-level
/// and method-level type parameters) by building a combined substitution map.
///
/// For a method call like `container.method::<U, V>(args)` where container is `Container<T>`:
/// - Impl type args (T's concrete type) are added with `with_impl_args`
/// - Method type args (U, V's concrete types) are added with `with_method_args`
/// - The substitution correctly handles offset indices used for method type params
#[derive(Debug, Clone, Default)]
pub struct SubstitutionContext {
    /// Maps type param index to concrete type (index is as stored in `TypeParam`)
    /// For impl params: indices 0, 1, 2, ...
    /// For method params: indices offset, offset+1, ... (where offset = `impl_params.len()`)
    substitutions: HashMap<u32, TypeId>,
}

impl SubstitutionContext {
    pub fn new() -> Self {
        Self {
            substitutions: HashMap::new(),
        }
    }

    /// Add impl-level type args (e.g., T=i32 for Container<i32>)
    /// These are substituted at indices 0, 1, 2, ...
    pub fn with_impl_args(mut self, args: &[TypeId]) -> Self {
        for (i, &type_id) in args.iter().enumerate() {
            self.substitutions.insert(i as u32, type_id);
        }
        self
    }

    /// Add method-level type args (e.g., U=i64 for `transform::`<i64>)
    /// These are substituted at offset indices (offset, offset+1, ...)
    /// where offset is the number of impl type params
    pub fn with_method_args(mut self, args: &[TypeId], offset: u32) -> Self {
        for (i, &type_id) in args.iter().enumerate() {
            self.substitutions.insert(offset + i as u32, type_id);
        }
        self
    }

    /// Substitute type parameters in a type
    pub fn substitute(&self, type_id: TypeId, type_table: &mut TypeTable) -> TypeId {
        match type_table.get(type_id).clone() {
            ResolvedType::TypeParam { index, .. } => {
                // Direct substitution: TypeParam at index -> concrete type
                self.substitutions.get(&index).copied().unwrap_or(type_id)
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute(elem, type_table);
                type_table.make_builtin_array(new_elem)
            }
            ResolvedType::Option(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.make_option(new_inner)
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.make_ref(new_inner)
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.make_mut_ref(new_inner)
            }
            ResolvedType::Tuple(elems) => {
                let new_elems: Vec<TypeId> = elems
                    .iter()
                    .map(|&e| self.substitute(e, type_table))
                    .collect();
                type_table.make_tuple(new_elems)
            }
            ResolvedType::Result { ok, err } => {
                let new_ok = self.substitute(ok, type_table);
                let new_err = self.substitute(err, type_table);
                type_table.make_result(new_ok, new_err)
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // Recursively substitute in nested generic instances
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute(arg, type_table))
                    .collect();
                type_table.make_generic_instance(name, module_source, new_args)
            }
            ResolvedType::Function {
                params,
                return_type,
                effects,
            } => {
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.substitute(p, type_table))
                    .collect();
                let new_return = self.substitute(return_type, type_table);
                type_table.make_function(new_params, new_return, effects)
            }
            ResolvedType::Stream(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.intern(ResolvedType::Stream(new_inner))
            }
            ResolvedType::Future(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.intern(ResolvedType::Future(new_inner))
            }
            ResolvedType::Reactive(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.intern(ResolvedType::Reactive(new_inner))
            }
            // Other types don't contain type parameters
            _ => type_id,
        }
    }

    /// Check if this context has any substitutions
    pub fn is_empty(&self) -> bool {
        self.substitutions.is_empty()
    }
}

// ============================================================================
// Type System
// ============================================================================

/// Type identifier for resolved types in TIR.
/// This is a newtype wrapper to prevent misuse of raw integers as `TypeId`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TypeId(pub u32);

impl std::fmt::Display for TypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

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
    Struct {
        name: String,
        module_source: ModuleSource,
        /// Whether this struct was created by monomorphizing a generic struct.
        /// If true, `base_name` contains the original generic struct name.
        is_monomorphized: bool,
        /// For monomorphized structs, the original generic name (e.g., "`TreeMap`" for "`TreeMap`<String,i32>").
        /// None for non-monomorphized structs.
        base_name: Option<String>,
    },
    Enum {
        name: String,
        module_source: ModuleSource,
    },
    Variant {
        name: String,
        module_source: ModuleSource,
    },
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
        module_source: ModuleSource,
        /// Concrete type arguments (e.g., [i32])
        type_args: Vec<TypeId>,
    },
    /// Raw GC array intrinsic (`builtin::array<T>`)
    /// This is the underlying storage type for String and Array<T> structs
    BuiltinArray(TypeId),
    Unknown,
    Error,
}

impl ResolvedType {
    /// Get the module path as a Vec<String> for backwards compatibility.
    /// This is a transitional helper during the migration to `ModuleSource`.
    #[must_use]
    pub fn module_path(&self) -> Vec<String> {
        match self {
            Self::Struct { module_source, .. }
            | Self::Enum { module_source, .. }
            | Self::Variant { module_source, .. }
            | Self::GenericInstance { module_source, .. } => module_source.to_path(),
            _ => vec![],
        }
    }
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
    pub const I8: TypeId = TypeId(0);
    pub const I16: TypeId = TypeId(1);
    pub const I32: TypeId = TypeId(2);
    pub const I64: TypeId = TypeId(3);
    pub const I128: TypeId = TypeId(4);
    pub const U8: TypeId = TypeId(5);
    pub const U16: TypeId = TypeId(6);
    pub const U32: TypeId = TypeId(7);
    pub const U64: TypeId = TypeId(8);
    pub const U128: TypeId = TypeId(9);
    pub const F32: TypeId = TypeId(10);
    pub const F64: TypeId = TypeId(11);
    pub const BOOL: TypeId = TypeId(12);
    pub const CHAR: TypeId = TypeId(13);
    pub const UNIT: TypeId = TypeId(14);
    pub const NEVER: TypeId = TypeId(15);
    // STRING removed - String is now a user-defined struct in core/prelude
    pub const UNKNOWN: TypeId = TypeId(16);
    pub const ERROR: TypeId = TypeId(17);

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
        let id = TypeId(self.types.len() as u32);
        self.types.push(ty.clone());
        self.intern_map.insert(ty, id);
        id
    }

    pub fn get(&self, id: TypeId) -> &ResolvedType {
        &self.types[id.0 as usize]
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

    /// Iterate over all type IDs in the table
    pub fn iter_type_ids(&self) -> impl Iterator<Item = TypeId> {
        (0..self.types.len() as u32).map(TypeId)
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

    pub fn make_struct(&mut self, name: String, module_source: ModuleSource) -> TypeId {
        self.intern(ResolvedType::Struct {
            name,
            module_source,
            is_monomorphized: false,
            base_name: None,
        })
    }

    /// Create a monomorphized struct type (e.g., "Box<i32>")
    ///
    /// - `name`: The fully mangled name (e.g., "`TreeMap`<String,i32>")
    /// - `base_name`: The original generic struct name (e.g., "`TreeMap`")
    pub fn make_monomorphized_struct(
        &mut self,
        name: String,
        module_source: ModuleSource,
        base_name: String,
    ) -> TypeId {
        self.intern(ResolvedType::Struct {
            name,
            module_source,
            is_monomorphized: true,
            base_name: Some(base_name),
        })
    }

    pub fn make_variant(&mut self, name: String, module_source: ModuleSource) -> TypeId {
        self.intern(ResolvedType::Variant {
            name,
            module_source,
        })
    }

    /// Find the `type_id` for a non-monomorphized struct by name and module source (O(1) lookup via `intern_map`)
    pub fn find_struct_type(&self, name: &str, module_source: &ModuleSource) -> Option<TypeId> {
        // Use the existing intern_map for O(1) lookup
        let key = ResolvedType::Struct {
            name: name.to_string(),
            module_source: module_source.clone(),
            is_monomorphized: false,
            base_name: None,
        };
        self.intern_map.get(&key).copied()
    }

    pub fn make_enum(&mut self, name: String, module_source: ModuleSource) -> TypeId {
        self.intern(ResolvedType::Enum {
            name,
            module_source,
        })
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
        module_source: ModuleSource,
        type_args: Vec<TypeId>,
    ) -> TypeId {
        self.intern(ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
        })
    }

    /// Create an Array<T> type (`GenericInstance` { name: "Array", ... })
    pub fn make_array(&mut self, element: TypeId) -> TypeId {
        self.make_generic_instance(
            "Array".to_string(),
            ModuleSource::core("prelude"),
            vec![element],
        )
    }

    /// Check if a type is Array<T> and return the element type if so.
    /// Also unwraps Ref/MutRef types to check the inner type.
    pub fn as_array(&self, id: TypeId) -> Option<TypeId> {
        match self.get(id) {
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Array" && type_args.len() == 1 => Some(type_args[0]),
            // Unwrap references and check the inner type
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => self.as_array(*inner),
            _ => None,
        }
    }

    /// Check if a type is or contains type parameters or unresolved types (Unknown/Error)
    pub fn contains_type_param(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. } | ResolvedType::Unknown | ResolvedType::Error => true,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Option(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Stream(inner)
            | ResolvedType::Future(inner)
            | ResolvedType::Reactive(inner) => self.contains_type_param(*inner),
            ResolvedType::Result { ok, err } => {
                self.contains_type_param(*ok) || self.contains_type_param(*err)
            }
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

    /// Check if a type has nested generic types.
    /// Returns true if the type is parameterized and any of its type arguments
    /// is also a parameterized type (e.g., `Option<BTreeNode<K,V>>`).
    ///
    /// TODO: This is used to detect types that may have monomorphization issues during inlining.
    /// Currently we skip inlining functions with such types. In the future, we should fix
    /// the underlying type normalization issues to allow inlining these functions.
    pub fn has_nested_generics(&self, id: TypeId) -> bool {
        match self.get(id) {
            // For Ref/MutRef, just check if the inner type has nested generics
            // (references themselves don't add nesting)
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.has_nested_generics(*inner)
            }
            // For Option, Stream, Future, etc., the inner type is a type argument
            // Check if that type argument is itself generic (i.e., nested)
            ResolvedType::Option(inner)
            | ResolvedType::Stream(inner)
            | ResolvedType::Future(inner)
            | ResolvedType::Reactive(inner)
            | ResolvedType::BuiltinArray(inner) => self.is_generic_type(*inner),
            // For Result, check both type args
            ResolvedType::Result { ok, err } => {
                self.is_generic_type(*ok) || self.is_generic_type(*err)
            }
            // For Tuple, check all elements
            ResolvedType::Tuple(elems) => elems.iter().any(|e| self.is_generic_type(*e)),
            // For GenericInstance, check if any type argument is itself generic
            ResolvedType::GenericInstance { type_args, .. } => {
                type_args.iter().any(|t| self.is_generic_type(*t))
            }
            // For Function types, check params and return type
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params.iter().any(|p| self.is_generic_type(*p))
                    || self.is_generic_type(*return_type)
            }
            // Non-parameterized types don't have nested generics
            _ => false,
        }
    }

    /// Check if a type is a generic/parameterized type (has type arguments).
    /// This includes `GenericInstance`, Option, Result, Tuple with generics, etc.
    /// Also includes monomorphized structs (whose names contain type parameters).
    fn is_generic_type(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::GenericInstance { .. }
            | ResolvedType::Option(_)
            | ResolvedType::Result { .. }
            | ResolvedType::Stream(_)
            | ResolvedType::Future(_)
            | ResolvedType::BuiltinArray(_) => true,
            ResolvedType::Struct {
                is_monomorphized, ..
            } => *is_monomorphized,
            // References are generic if inner is generic
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => self.is_generic_type(*inner),
            // Tuples are generic if they contain generic types
            ResolvedType::Tuple(elems) => elems.iter().any(|e| self.is_generic_type(*e)),
            // Reactive wrapping a generic type
            ResolvedType::Reactive(inner) => self.is_generic_type(*inner),
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
            ResolvedType::Unknown => "unknown".to_string(),
            ResolvedType::Error => "error".to_string(),
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

// ============================================================================
// Function References
// ============================================================================

/// Reference to a function, either resolved to TIR or external
#[derive(Debug, Clone)]
pub enum FunctionRef {
    /// Reference to a resolved TIR function
    Resolved(Rc<RefCell<TirFunction>>),
    /// Reference to an external/unresolved function
    External {
        module_source: ModuleSource,
        name: String,
        /// Monomorphization info for external monomorphized functions
        monomorph_info: Option<MonomorphInfo>,
        /// Parsed method info for external methods (None for free functions)
        method_info: Option<LocalMethodName>,
    },
}

impl FunctionRef {
    /// Get the function name
    pub fn name(&self) -> String {
        match self {
            FunctionRef::Resolved(func) => func.borrow().name.clone(),
            FunctionRef::External { name, .. } => name.clone(),
        }
    }

    /// Get the module source
    pub fn module_source(&self) -> ModuleSource {
        match self {
            FunctionRef::Resolved(_) => ModuleSource::entry_point(),
            FunctionRef::External { module_source, .. } => module_source.clone(),
        }
    }

    /// Get the module path (for backwards compatibility)
    pub fn module_path(&self) -> Vec<String> {
        self.module_source().to_path()
    }

    /// Check if this is a resolved reference
    pub fn is_resolved(&self) -> bool {
        matches!(self, FunctionRef::Resolved(_))
    }

    /// Get the resolved function if available
    pub fn as_resolved(&self) -> Option<&Rc<RefCell<TirFunction>>> {
        match self {
            FunctionRef::Resolved(func) => Some(func),
            FunctionRef::External { .. } => None,
        }
    }

    /// Get the fully qualified function name including module path.
    /// For external functions, this returns "{`module_source}/{name`}".
    /// For resolved functions, this returns the method-qualified name if available.
    pub fn full_name(&self) -> String {
        match self {
            FunctionRef::Resolved(func) => {
                let func = func.borrow();
                // Use method_info to create a unique name for methods
                if let Some(info) = &func.method_info {
                    info.to_mangled_name()
                } else {
                    func.name.clone()
                }
            }
            FunctionRef::External {
                module_source,
                name,
                ..
            } => {
                let path = module_source.to_path();
                if path.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", path.join("/"), name)
                }
            }
        }
    }

    /// Get the builtin function name if this is a builtin call.
    /// Returns the qualified name (e.g., "`builtin::array_len`").
    /// Builtin functions have `module_source` == Core { name: "builtin" }.
    pub fn builtin_name(&self) -> Option<String> {
        match self {
            FunctionRef::Resolved(_) => None,
            FunctionRef::External {
                module_source,
                name,
                ..
            } if module_source.is_core_builtin() => Some(format!("builtin::{name}")),
            FunctionRef::External { .. } => None,
        }
    }

    /// Get the monomorphized builtin name if this is a monomorphized builtin function.
    /// Returns the qualified name (e.g., "`builtin::array_get`") if the `generic_name`
    /// is a known builtin function like "`array_get`", "`array_set`", etc.
    pub fn monomorphized_builtin_name(&self) -> Option<String> {
        let generic_name = match self {
            FunctionRef::Resolved(func) => func
                .borrow()
                .monomorph_info
                .as_ref()
                .map(|i| i.generic_name.clone()),
            FunctionRef::External { monomorph_info, .. } => {
                monomorph_info.as_ref().map(|i| i.generic_name.clone())
            }
        }?;

        // Check if the generic name is a known builtin
        match generic_name.as_str() {
            "array_get" | "array_set" | "array_new" | "array_len" => {
                Some(format!("builtin::{generic_name}"))
            }
            _ => None,
        }
    }

    /// Check if this function is monomorphized (instantiated from a generic)
    pub fn is_monomorphized(&self) -> bool {
        match self {
            FunctionRef::Resolved(func) => func.borrow().monomorph_info.is_some(),
            FunctionRef::External { monomorph_info, .. } => monomorph_info.is_some(),
        }
    }

    /// Get the base generic name if this is a monomorphized function.
    /// For "Box<i32>`::get`", returns "Box".
    /// For "Container<i32>`::transform`<i64>", returns "Container".
    pub fn base_struct_name(&self) -> Option<String> {
        match self {
            FunctionRef::Resolved(func) => {
                let func = func.borrow();
                func.monomorph_info
                    .as_ref()
                    .and_then(|info| info.generic_name.split("::").next())
                    .map(std::string::ToString::to_string)
            }
            FunctionRef::External { monomorph_info, .. } => monomorph_info
                .as_ref()
                .and_then(|info| info.generic_name.split("::").next())
                .map(std::string::ToString::to_string),
        }
    }

    /// Get parsed method info if this is a method reference.
    /// Returns `None` for free functions.
    pub fn method_info(&self) -> Option<LocalMethodName> {
        match self {
            FunctionRef::Resolved(func) => func.borrow().method_info.clone(),
            FunctionRef::External { method_info, .. } => method_info.clone(),
        }
    }

    /// Check if this is a method (instance or static) as opposed to a free function.
    pub fn is_method(&self) -> bool {
        match self {
            FunctionRef::Resolved(func) => func.borrow().is_method(),
            FunctionRef::External { method_info, .. } => method_info.is_some(),
        }
    }

    /// Check if this is a trait method.
    pub fn is_trait_method(&self) -> bool {
        match self {
            FunctionRef::Resolved(func) => func.borrow().is_trait_method(),
            FunctionRef::External { method_info, .. } => method_info
                .as_ref()
                .is_some_and(LocalMethodName::is_trait_method),
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
        module_source: ModuleSource,
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
        /// Function reference (resolved TIR function or external)
        func: FunctionRef,
        /// Explicit type arguments for generic functions: `identity::<i32>(x)`
        type_args: Vec<TypeId>,
        args: Vec<TirExpr>,
    },
    EffectCall {
        effect_name: String,
        op_name: String,
        args: Vec<TirExpr>,
        /// Component Model call convention (if this is a CM effect call)
        /// Contains all ABI information for codegen - no WASI-specific knowledge needed
        cm_convention: Option<CmCallConvention>,
        /// Full local alias name for CM call (e.g., "`wasi:cli/Environment::get_arguments`")
        cm_local_name: Option<String>,
    },
    MethodCall {
        receiver: Box<TirExpr>,
        /// Method reference (resolved TIR function or external)
        func: FunctionRef,
        /// Explicit type arguments for generic methods: `obj.method::<i32>()`
        type_args: Vec<TypeId>,
        args: Vec<TirExpr>,
    },
    /// Static method call: `Array::<i32>::with_capacity(100)` or `Point::origin()`
    StaticCall {
        /// Function reference (resolved TIR function or external)
        func: FunctionRef,
        /// Arguments to the static method
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

    /// Access to a captured variable inside a closure body
    Capture {
        /// Index into the closure's captures array
        index: u32,
        /// Variable name (for debugging)
        name: String,
    },

    Closure {
        params: Vec<(String, TypeId)>,
        body: Box<TirExpr>,
        captures: Vec<TirCapture>,
        /// Optional functor ID assigned during lowering.
        /// Used by monomorphize phase to look up the corresponding `ClosureFunctor`.
        functor_id: Option<u32>,
    },

    /// Indirect call through a callable value (closure or funcref)
    IndirectCall {
        /// The callee expression (closure struct or funcref)
        callee: Box<TirExpr>,
        /// Arguments to pass to the callee
        args: Vec<TirExpr>,
    },

    /// `Option::Some(value)` construction
    OptionSome {
        value: Box<TirExpr>,
    },

    /// Custom variant construction: `Shape::Circle(5.0)` or `MyVariant::Unit`
    VariantConstruct {
        /// The variant type (e.g., `ResolvedType::Variant` { name: "Shape", ... })
        variant_type: TypeId,
        /// The case index (0-based position in variant declaration)
        case_index: u32,
        /// The case name (for debugging/error messages)
        case_name: String,
        /// Field values (empty for unit variants)
        fields: Vec<TirExpr>,
    },

    /// Move semantics: the inner value is moved without copying.
    /// Inserted by the optimizer for expressions that produce fresh values
    /// (e.g., function call results, literals).
    /// Future: When user-facing, `move x` invalidates `x` after the expression.
    Move {
        value: Box<TirExpr>,
    },

    /// Labeled block expression: `label: { ... }` that produces a value
    /// The value must be returned via `break label: expr;`
    LabeledBlock {
        label: String,
        block: TirBlock,
        /// The type of value this block produces (from break expressions)
        result_type: TypeId,
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
        type_id: TypeId,
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
        /// Init statements (e.g., `let mut i = 0`)
        init: Vec<TirStmt>,
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
    /// Break statement: `break;`, `break label;`, or `break label: expr;`
    Break {
        /// Optional label to break to (for labeled blocks)
        label: Option<String>,
        /// Optional value to return from the labeled block
        value: Option<TirExpr>,
    },
    Continue,
    /// Labeled block: `LABEL: { ... }` - creates a new scope with local bindings
    LabeledBlock {
        label: String,
        block: TirBlock,
    },
    /// Pattern match in if condition: `if let Some(x) = expr { ... } else { ... }`
    IfPattern {
        /// The expression being matched against
        scrutinee: TirExpr,
        /// The pattern to match
        pattern: TirPattern,
        /// Block executed when pattern matches
        then_block: TirBlock,
        /// Optional else block when pattern doesn't match
        else_block: Option<TirBlock>,
    },
    /// Pattern match in while condition: `while let Some(x) = expr { ... }`
    WhilePattern {
        /// The expression being matched against (evaluated each iteration)
        scrutinee: TirExpr,
        /// The pattern to match
        pattern: TirPattern,
        /// Block executed when pattern matches
        body: TirBlock,
    },
    /// Pattern match in for condition: `for init; let Some(x) = expr; update { ... }`
    ForPattern {
        /// Init statements (e.g., `let mut iter = arr.iter()`)
        init: Vec<TirStmt>,
        /// The expression being matched against (evaluated each iteration)
        scrutinee: TirExpr,
        /// The pattern to match
        pattern: TirPattern,
        /// Block executed when pattern matches
        body: TirBlock,
        /// Optional update expression (executed after body, before next iteration)
        update: Option<TirExpr>,
    },
}

// ============================================================================
// Items (Top-level Declarations)
// ============================================================================

/// Generic type parameter in TIR (from AST `GenericParam`)
#[derive(Debug, Clone)]
pub struct TirTypeParam {
    pub name: String,
    pub bounds: Vec<String>,
    /// Default type if specified (e.g., `Effects = []`)
    pub default: Option<TypeId>,
    pub index: u32,
}

/// Information about monomorphization origin for instantiated items
#[derive(Debug, Clone)]
pub struct MonomorphInfo {
    /// Original generic name (e.g., "Box" for "Box<i32>", or "`BTreeNode`<`K,V>::insert`" for methods)
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
    /// Type parameters from the impl block (for methods on generic structs)
    /// e.g., for a method in `impl Counter<T>`, this contains T's info
    pub impl_type_params: Vec<TirTypeParam>,
    /// If this function was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    /// Parsed method info for methods (None for free functions)
    /// Contains `struct_name`, `trait_name`, and `method_name` extracted from the function name.
    pub method_info: Option<LocalMethodName>,
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
    /// Types that need value copy operations in this function.
    /// Populated by the optimizer after all transformations.
    pub needed_copy_types: std::collections::HashSet<TypeId>,
}

impl TirFunction {
    /// Returns true if this is a method (belongs to a struct)
    #[inline]
    pub fn is_method(&self) -> bool {
        self.method_info.is_some()
    }

    /// Returns true if this is a trait method (implements a trait)
    #[inline]
    pub fn is_trait_method(&self) -> bool {
        self.method_info
            .as_ref()
            .is_some_and(super::name::LocalMethodName::is_trait_method)
    }
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
    pub cases: Vec<TirEnumCase>,
    pub span: Span,
}

/// A case in a TIR enum.
/// Unlike `TirVariantCase`, enum cases have no payload.
#[derive(Debug, Clone)]
pub struct TirEnumCase {
    pub name: String,
    pub index: u32,
    pub span: Span,
}

/// A variant type declaration (tagged union, distinct from enum)
/// e.g., `variant Shape { Circle(f64), Rectangle(f64, f64), Point }`
#[derive(Debug, Clone)]
pub struct TirVariantDecl {
    pub name: String,
    pub is_pub: bool,
    /// Generic type parameters (e.g., `T` in `variant Option<T>`)
    pub type_params: Vec<TirTypeParam>,
    /// Cases of the variant (e.g., Some, None for Option)
    pub cases: Vec<TirVariantCase>,
    pub span: Span,
}

/// A case in a variant declaration
/// e.g., `Circle(f64)` or `Point`
#[derive(Debug, Clone)]
pub struct TirVariantCase {
    pub name: String,
    /// Case index (0-based)
    pub index: u32,
    /// Payload field types (empty for unit cases like `None`)
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

/// Test declaration metadata
/// The actual test code is stored as a `TirFunction` in the functions list.
#[derive(Debug, Clone)]
pub struct TirTest {
    /// The original test name from source (None if unnamed)
    pub name: Option<String>,
    /// Generated function name (e.g., "__`test_0`" or "__`test_addition_works`")
    pub function_name: String,
    /// Source line number for unnamed test identification
    pub line: usize,
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

/// Trait declaration
#[derive(Debug, Clone)]
pub struct TirTrait {
    pub name: String,
    pub is_pub: bool,
    pub type_params: Vec<TirTypeParam>,
    pub methods: Vec<TirTraitMethod>,
    pub span: Span,
}

/// A method signature in a trait
#[derive(Debug, Clone)]
pub struct TirTraitMethod {
    pub name: String,
    pub params: Vec<TirParam>,
    pub return_type: TypeId,
    pub has_default_body: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirImpl {
    /// Generic type parameters for the impl block (e.g., `impl<T> Box<T>`)
    pub type_params: Vec<TirTypeParam>,
    /// The trait being implemented, if any (e.g., "Display" for `impl Display for Type`)
    pub trait_name: Option<String>,
    pub target_type: TypeId,
    pub methods: Vec<TirFunction>,
    pub span: Span,
}

// ============================================================================
// Closure Metadata (for optimizer)
// ============================================================================

/// Metadata about a closure for optimization (especially inlining).
///
/// This is populated by the lower phase and used by the optimizer to inline
/// closure calls when the closure is known at compile time.
#[derive(Debug, Clone)]
pub struct ClosureFunctor {
    /// Unique closure ID (matches the order closures are visited in the module)
    pub id: u32,
    /// Name of the generated functor struct (e.g., `__Closure_0`)
    pub struct_name: String,
    /// Type ID of the generated functor struct
    pub struct_type_id: TypeId,
    /// The `__call` method for this closure (with body transformed:
    /// Capture nodes become `FieldAccess` on self)
    pub call_method: Rc<RefCell<TirFunction>>,
    /// Captures from the original closure
    pub captures: Vec<TirCapture>,
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
    pub module_source: ModuleSource,
    /// Shared type table across all modules (enables cross-module type references)
    pub type_table: Rc<RefCell<TypeTable>>,
    pub functions: Vec<Rc<RefCell<TirFunction>>>,
    pub structs: Vec<TirStruct>,
    pub enums: Vec<TirEnum>,
    /// Custom variant declarations (tagged unions with payloads)
    pub variants: Vec<TirVariantDecl>,
    pub type_aliases: Vec<TirTypeAlias>,
    pub effects: Vec<TirEffect>,
    pub traits: Vec<TirTrait>,
    pub impls: Vec<TirImpl>,
    /// Test declarations with their metadata
    pub tests: Vec<TirTest>,
    pub data_section: Option<String>,
    pub string_literals: Vec<String>,
    /// Map of function name to string literals it contains (for DCE)
    pub function_strings: HashMap<String, Vec<String>>,
    /// Generic struct definitions (before monomorphization)
    /// Key: struct name
    pub generic_structs: HashMap<String, TirStruct>,
    /// Generic function definitions (before monomorphization)
    /// Key: function name
    pub generic_functions: HashMap<String, Rc<RefCell<TirFunction>>>,
    /// Requested instantiations (populated during resolution, processed in lower)
    pub instantiation_requests: std::collections::HashSet<InstantiationKey>,
    /// Closure metadata for optimization (populated by lower phase).
    /// Maps closure ID to functor info including the `__call` method for inlining.
    pub closure_functors: Vec<ClosureFunctor>,
}

impl TirModule {
    pub fn new(module_source: ModuleSource) -> Self {
        Self {
            module_source,
            type_table: Rc::new(RefCell::new(TypeTable::new())),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            variants: Vec::new(),
            type_aliases: Vec::new(),
            effects: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            tests: Vec::new(),
            data_section: None,
            string_literals: Vec::new(),
            function_strings: HashMap::new(),
            generic_structs: HashMap::new(),
            generic_functions: HashMap::new(),
            instantiation_requests: std::collections::HashSet::new(),
            closure_functors: Vec::new(),
        }
    }

    pub fn with_type_table(
        module_source: ModuleSource,
        type_table: Rc<RefCell<TypeTable>>,
    ) -> Self {
        Self {
            module_source,
            type_table,
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            variants: Vec::new(),
            type_aliases: Vec::new(),
            effects: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            tests: Vec::new(),
            data_section: None,
            string_literals: Vec::new(),
            function_strings: HashMap::new(),
            generic_structs: HashMap::new(),
            generic_functions: HashMap::new(),
            instantiation_requests: std::collections::HashSet::new(),
            closure_functors: Vec::new(),
        }
    }

    pub fn with_data_section(mut self, data_section: Option<String>) -> Self {
        self.data_section = data_section;
        self
    }

    pub fn data_section(&self) -> Option<&str> {
        self.data_section.as_deref()
    }

    pub fn add_function(&mut self, func: TirFunction) -> Rc<RefCell<TirFunction>> {
        let func_rc = Rc::new(RefCell::new(func));
        self.functions.push(Rc::clone(&func_rc));
        func_rc
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

    pub fn add_trait(&mut self, trait_decl: TirTrait) {
        self.traits.push(trait_decl);
    }

    pub fn add_impl(&mut self, impl_block: TirImpl) {
        self.impls.push(impl_block);
    }

    pub fn find_function(&self, name: &str) -> Option<Rc<RefCell<TirFunction>>> {
        self.functions
            .iter()
            .find(|f| f.borrow().name == name)
            .cloned()
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
        // Test that interning the same type returns the same TypeId
        let arr1 = table.make_array(TypeTable::I32);
        let arr2 = table.make_array(TypeTable::I32);
        assert_eq!(arr1, arr2);
        // Verify as_array works
        assert_eq!(table.as_array(arr1), Some(TypeTable::I32));
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
