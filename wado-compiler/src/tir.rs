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
use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};

use crate::name::{LocalMethodName, ModuleSource, TypeNameInfo, format_type_name};
use crate::token::Span;

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
    substitutions: IndexMap<u32, TypeId>,
}

impl SubstitutionContext {
    pub fn new() -> Self {
        Self {
            substitutions: IndexMap::new(),
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
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                ..
            } => {
                // Substitute the underlying type param to get the concrete type
                let concrete_id = self.substitute(param_id, type_table);
                // If the param resolved to a concrete type, look up the associated type binding
                if concrete_id != param_id
                    && let Some(resolved) = type_table.resolve_assoc_type(concrete_id, &assoc_name)
                {
                    return resolved;
                }
                // Fallback: return the concrete type (param substitution)
                concrete_id
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute(elem, type_table);
                type_table.make_builtin_array(new_elem)
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
            ResolvedType::StreamWritable(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.intern(ResolvedType::StreamWritable(new_inner))
            }
            ResolvedType::Future(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.intern(ResolvedType::Future(new_inner))
            }
            ResolvedType::FutureWritable(inner) => {
                let new_inner = self.substitute(inner, type_table);
                type_table.intern(ResolvedType::FutureWritable(new_inner))
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

impl PrimitiveType {
    /// Returns the string representation of the primitive type (e.g., "i32", "f64")
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::I128 => "i128",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
            Self::U128 => "u128",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Bool => "bool",
            Self::Char => "char",
        }
    }

    /// Check if a name is a primitive type name.
    #[must_use]
    pub fn is_primitive_name(name: &str) -> bool {
        matches!(
            name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "f32"
                | "f64"
                | "bool"
                | "char"
        )
    }
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
    /// Resource type - opaque handle (i32) to a Component Model resource
    Resource {
        name: String,
        module_source: ModuleSource,
    },
    Variant {
        name: String,
        module_source: ModuleSource,
    },
    // NOTE: Option<T> is no longer a dedicated type variant.
    // It is represented as GenericInstance { name: "Option", module_source: types(), type_args: [T] }.
    // Use TypeTable::as_option() to check if a type is Option<T>.
    //
    // TODO: Re-add NullableRef optimization for Option<T> where T is non-nullable.
    // When T is a reference type (struct, array, string, etc.), Option<T> can be
    // represented as (ref null T) — null means None, non-null means Some(T).
    // This avoids the SubtypeHierarchy overhead (discriminant struct + case subtypes).
    // Key requirement: Option<Option<T>> MUST NOT use NullableRef (ambiguous null).
    // See wep-2026-02-09-variant-independent-types.md for the general optimization strategy.
    Stream(TypeId),
    StreamWritable(TypeId),
    Future(TypeId),
    FutureWritable(TypeId),
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
    /// Associated type projection: `T::X` where T is a type parameter with a trait bound.
    /// During concrete instantiation this is resolved to the concrete associated type.
    AssocTypeProjection {
        /// The type-parameter `TypeId` (must be a `TypeParam` variant)
        param_id: TypeId,
        /// Name of the associated type (e.g., `"Value"` in `T::Value`)
        assoc_name: String,
        /// Trait bounds on this associated type (from the trait declaration)
        bounds: Vec<String>,
    },
    /// Raw GC array intrinsic (`builtin::array<T>`)
    /// This is the underlying storage type for String and Array<T> structs
    BuiltinArray(TypeId),
    /// Newtype: a distinct type wrapping a base type with the same representation.
    /// Created by `type T = U;` declarations.
    /// Newtypes are distinct from their base types but can be cast between them.
    Newtype {
        name: String,
        module_source: ModuleSource,
        /// The direct base type (may be another newtype for chained newtypes)
        base_type: TypeId,
    },
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
            | Self::GenericInstance { module_source, .. }
            | Self::Newtype { module_source, .. } => module_source.to_path(),
            _ => vec![],
        }
    }
}

#[derive(Debug, Clone)]
pub struct TypeTable {
    types: IndexMap<TypeId, ResolvedType>,
    intern_map: IndexMap<ResolvedType, TypeId>,
    next_id: u32,
    /// Module source of the canonical `Option<T>` variant, set via `#[comp_feature("option")]`.
    option_module_source: Option<ModuleSource>,
    /// Module source of the canonical `Result<T, E>` variant, set via `#[comp_feature("result")]`.
    result_module_source: Option<ModuleSource>,
    /// Associated type resolutions: `(concrete_type_id, assoc_name)` → `resolved_type_id`.
    /// Populated when impl blocks with associated type bindings are processed.
    assoc_type_resolutions: IndexMap<(TypeId, String), TypeId>,
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
    // STRING removed - String is now a user-defined struct in core:prelude/string.wado
    pub const UNKNOWN: TypeId = TypeId(16);
    pub const ERROR: TypeId = TypeId(17);

    pub fn new() -> Self {
        let mut table = Self {
            types: IndexMap::new(),
            intern_map: IndexMap::new(),
            next_id: 0,
            option_module_source: None,
            result_module_source: None,
            assoc_type_resolutions: IndexMap::new(),
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
        let id = TypeId(self.next_id);
        self.next_id += 1;
        self.types.insert(id, ty.clone());
        self.intern_map.insert(ty, id);
        id
    }

    pub fn get(&self, id: TypeId) -> &ResolvedType {
        self.types
            .get(&id)
            .unwrap_or_else(|| panic!("TypeId {id:?} not found in TypeTable"))
    }

    /// Iterate over all types in the type table.
    pub fn all_types(&self) -> impl Iterator<Item = (&TypeId, &ResolvedType)> {
        self.types.iter()
    }

    /// Try to get a type by ID, returning `None` if the type doesn't exist.
    /// Useful when types may have been removed by DCE.
    pub fn try_get(&self, id: TypeId) -> Option<&ResolvedType> {
        self.types.get(&id)
    }

    /// Try to mangle a type name, returning `None` if the type doesn't exist.
    /// Useful when types may have been removed by DCE.
    pub fn try_mangle_type_name(&self, id: TypeId) -> Option<String> {
        self.try_get(id)?;
        Some(self.mangle_type_name(id))
    }

    pub fn is_integer(&self, id: TypeId) -> bool {
        // Follow newtype chain to get ultimate base type
        let base_id = self.get_ultimate_base_type(id);
        matches!(
            self.get(base_id),
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
        // Follow newtype chain to get ultimate base type
        let base_id = self.get_ultimate_base_type(id);
        matches!(
            self.get(base_id),
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
    pub fn iter_type_ids(&self) -> impl Iterator<Item = TypeId> + '_ {
        self.types.keys().copied()
    }

    /// Retain only types that satisfy the predicate.
    /// Used by DCE to remove unreachable types.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(TypeId, &ResolvedType) -> bool,
    {
        // Collect types to remove
        let to_remove: Vec<TypeId> = self
            .types
            .iter()
            .filter(|(id, ty)| !f(**id, ty))
            .map(|(id, _)| *id)
            .collect();

        // Remove from both maps
        for id in to_remove {
            if let Some(ty) = self.types.shift_remove(&id) {
                self.intern_map.shift_remove(&ty);
            }
        }
    }

    /// Create a raw GC array type (`builtin::array<T>`)
    pub fn make_builtin_array(&mut self, element: TypeId) -> TypeId {
        self.intern(ResolvedType::BuiltinArray(element))
    }

    /// Register the defining module source for a variant marked with `#[comp_feature]`.
    ///
    /// Called by the resolver when it encounters a variant with `COMP_FEATURE_OPTION` or
    /// `COMP_FEATURE_RESULT` so that `make_option`/`make_result` can use the real path.
    pub fn register_comp_feature_variant(
        &mut self,
        comp_features: u32,
        module_source: ModuleSource,
    ) {
        if comp_features & crate::wir::COMP_FEATURE_OPTION != 0 {
            self.option_module_source = Some(module_source.clone());
        }
        if comp_features & crate::wir::COMP_FEATURE_RESULT != 0 {
            self.result_module_source = Some(module_source);
        }
    }

    /// Create an `Option<T>` type using the module source registered via `#[comp_feature("option")]`.
    pub fn make_option(&mut self, inner: TypeId) -> TypeId {
        let module_source = self
            .option_module_source
            .clone()
            .expect("Option module source not registered; missing #[comp_feature(\"option\")] on Option variant");
        self.make_generic_instance("Option".to_string(), module_source, vec![inner])
    }

    /// Create a `Result<T, E>` type using the module source registered via `#[comp_feature("result")]`.
    pub fn make_result(&mut self, ok: TypeId, err: TypeId) -> TypeId {
        let module_source = self
            .result_module_source
            .clone()
            .expect("Result module source not registered; missing #[comp_feature(\"result\")] on Result variant");
        self.make_generic_instance("Result".to_string(), module_source, vec![ok, err])
    }

    /// Check if a type is `Option<T>`, returning the inner type if so.
    pub fn as_option(&self, type_id: TypeId) -> Option<TypeId> {
        if let ResolvedType::GenericInstance {
            name, type_args, ..
        } = self.get(type_id)
            && name == "Option"
            && type_args.len() == 1
        {
            return Some(type_args[0]);
        }
        None
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

    /// Find a variant type by name (scanning all types).
    /// Returns the first matching `ResolvedType::Variant` with the given name.
    pub fn find_variant_type_by_name(&self, name: &str) -> Option<TypeId> {
        for (&type_id, resolved) in &self.types {
            if let ResolvedType::Variant { name: vname, .. } = resolved
                && vname == name
            {
                return Some(type_id);
            }
        }
        None
    }

    /// Find a resource type by name (scanning all types).
    /// Returns the first matching `ResolvedType::Resource` with the given name.
    pub fn find_resource_type_by_name(&self, name: &str) -> Option<TypeId> {
        for (&type_id, resolved) in &self.types {
            if let ResolvedType::Resource { name: rname, .. } = resolved
                && rname == name
            {
                return Some(type_id);
            }
        }
        None
    }

    /// Find an enum type by name (scanning all types).
    /// Returns the first matching `ResolvedType::Enum` with the given name.
    pub fn find_enum_type_by_name(&self, name: &str) -> Option<TypeId> {
        for (&type_id, resolved) in &self.types {
            if let ResolvedType::Enum { name: ename, .. } = resolved
                && ename == name
            {
                return Some(type_id);
            }
        }
        None
    }

    pub fn make_enum(&mut self, name: String, module_source: ModuleSource) -> TypeId {
        self.intern(ResolvedType::Enum {
            name,
            module_source,
        })
    }

    pub fn make_resource(&mut self, name: String, module_source: ModuleSource) -> TypeId {
        self.intern(ResolvedType::Resource {
            name,
            module_source,
        })
    }

    /// Replace the type at an existing `TypeId` with a new type.
    /// Used by the boxing lowering pass to rewrite `Ref(primitive)` → `Struct(Box<T>)`.
    /// Removes the old type from the intern map so it won't be found by future `intern()` calls.
    pub fn replace_type(&mut self, id: TypeId, new_ty: ResolvedType) {
        if let Some(old_ty) = self.types.get(&id).cloned() {
            // Only remove from intern_map if this TypeId was the canonical one
            if self.intern_map.get(&old_ty) == Some(&id) {
                self.intern_map.shift_remove(&old_ty);
            }
        }
        self.types.insert(id, new_ty);
    }

    /// Check if a type is a primitive (including following newtypes).
    pub fn is_primitive_like(&self, id: TypeId) -> bool {
        let base = self.get_ultimate_base_type(id);
        matches!(self.get(base), ResolvedType::Primitive(_))
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

    /// Create an associated type projection: `T::X` where T is a type parameter.
    pub fn make_assoc_type_projection(
        &mut self,
        param_id: TypeId,
        assoc_name: String,
        bounds: Vec<String>,
    ) -> TypeId {
        self.intern(ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            bounds,
        })
    }

    /// Register an associated type resolution: for concrete type `concrete_id` (e.g., `JsonSerializer`),
    /// the associated type `assoc_name` (e.g., `"StructSerializer"`) resolves to `resolved_id`
    /// (e.g., `JsonStructSerializer`).
    pub fn register_assoc_type_resolution(
        &mut self,
        concrete_id: TypeId,
        assoc_name: String,
        resolved_id: TypeId,
    ) {
        self.assoc_type_resolutions
            .insert((concrete_id, assoc_name), resolved_id);
    }

    /// Resolve an associated type for a concrete type.
    /// Returns `Some(resolved_id)` if a resolution is registered.
    pub fn resolve_assoc_type(&self, concrete_id: TypeId, assoc_name: &str) -> Option<TypeId> {
        self.assoc_type_resolutions
            .get(&(concrete_id, assoc_name.to_string()))
            .copied()
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
        self.make_generic_instance("Array".to_string(), ModuleSource::array(), vec![element])
    }

    /// Create a newtype wrapping a base type
    pub fn make_newtype(
        &mut self,
        name: String,
        module_source: ModuleSource,
        base_type: TypeId,
    ) -> TypeId {
        self.intern(ResolvedType::Newtype {
            name,
            module_source,
            base_type,
        })
    }

    /// Get the base type if this is a newtype, or None otherwise
    pub fn get_newtype_base(&self, id: TypeId) -> Option<TypeId> {
        if let ResolvedType::Newtype { base_type, .. } = self.get(id) {
            Some(*base_type)
        } else {
            None
        }
    }

    /// Get the ultimate base type by following the chain of newtypes.
    /// Returns the original type if it's not a newtype.
    pub fn get_ultimate_base_type(&self, id: TypeId) -> TypeId {
        let mut current = id;
        while let ResolvedType::Newtype { base_type, .. } = self.get(current) {
            current = *base_type;
        }
        current
    }

    /// Check if two types share a common base type (for cast validation).
    /// Types share a common base if:
    /// - They are the same type
    /// - One is a newtype of the other
    /// - Both are newtypes with the same ultimate base type
    pub fn share_common_base(&self, a: TypeId, b: TypeId) -> bool {
        self.get_ultimate_base_type(a) == self.get_ultimate_base_type(b)
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
            ResolvedType::TypeParam { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => true,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Stream(inner)
            | ResolvedType::StreamWritable(inner)
            | ResolvedType::Future(inner)
            | ResolvedType::FutureWritable(inner)
            | ResolvedType::Reactive(inner) => self.contains_type_param(*inner),
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
            ResolvedType::Primitive(p) => p.as_str().to_string(),
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
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::Enum { name, .. } => name.clone(),
            ResolvedType::Resource { name, .. } => name.clone(),
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
            ResolvedType::StreamWritable(inner) => {
                format!("StreamWritable<{}>", self.type_name(*inner))
            }
            ResolvedType::Future(inner) => format!("Future<{}>", self.type_name(*inner)),
            ResolvedType::FutureWritable(inner) => {
                format!("FutureWritable<{}>", self.type_name(*inner))
            }
            ResolvedType::Reactive(inner) => format!("Reactive<{}>", self.type_name(*inner)),
            ResolvedType::TypeParam { name, .. } => name.clone(),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                ..
            } => {
                format!("{}::{}", self.type_name(*param_id), assoc_name)
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let arg_names: Vec<String> = type_args.iter().map(|t| self.type_name(*t)).collect();
                format!("{}<{}>", name, arg_names.join(", "))
            }
            ResolvedType::Newtype { name, .. } => name.clone(),
        }
    }

    /// Get a human-readable name for a type, returning a fallback for missing `TypeIds`.
    /// Useful in diagnostic/unparse contexts where types may not be in the local `TypeTable`.
    pub fn try_type_name(&self, id: TypeId) -> String {
        if self.try_get(id).is_some() {
            self.type_name(id)
        } else {
            format!("<TypeId({})>", id.0)
        }
    }

    /// Get a mangled name for a type suitable for use in struct/function names.
    ///
    /// Unlike `type_name` which returns human-readable names (e.g., `[i32, String]`),
    /// this returns mangled names suitable for monomorphization (e.g., `Tuple<i32,String>`).
    ///
    /// The format is:
    /// - Primitives: `i32`, `f64`, `bool`, etc.
    /// - Unit: `unit`
    /// - Struct: struct name
    /// - Tuple: `Tuple<T1,T2,...>`
    /// - Option: `Option<T>`
    /// - Result: `Result<T,E>`
    /// - Array: `Array<T>`
    /// - Function: `Fn<paramCount,returnType>`
    /// - `GenericInstance`: `Name<T1,T2,...>`
    /// - Ref/MutRef: inner type (references are stripped for mangling)
    ///
    /// Resolve through newtypes to find the base type.
    /// Returns the original `TypeId` if not a newtype.
    #[must_use]
    pub fn resolve_newtype_base(&self, id: TypeId) -> TypeId {
        let mut current = id;
        loop {
            match self.get(current) {
                ResolvedType::Newtype { base_type, .. } => current = *base_type,
                _ => return current,
            }
        }
    }

    #[must_use]
    pub fn mangle_type_name(&self, id: TypeId) -> String {
        let info = self.get_type_name_info(id);
        format_type_name(info)
    }

    /// Return the base type name without type arguments.
    ///
    /// For `GenericInstance { name: "Option", type_args: [String] }` → `"Option"`.
    /// For monomorphized `Struct { base_name: Some("Option"), .. }` → `"Option"`.
    /// For everything else, falls back to `mangle_type_name`.
    #[must_use]
    pub fn base_type_name(&self, id: TypeId) -> String {
        match self.get(id) {
            ResolvedType::GenericInstance { name, .. } => name.clone(),
            ResolvedType::Struct {
                base_name: Some(base),
                ..
            } => base.clone(),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.base_type_name(*inner)
            }
            _ => self.mangle_type_name(id),
        }
    }

    /// Convert a resolved type to its name info for formatting.
    ///
    /// This separates type resolution (here in tir.rs) from name formatting
    /// (in name.rs), following the principle that name format details belong
    /// in name.rs.
    fn get_type_name_info(&self, id: TypeId) -> TypeNameInfo {
        match self.get(id) {
            ResolvedType::Primitive(prim) => TypeNameInfo::Primitive(prim.as_str().to_string()),
            ResolvedType::Unit => TypeNameInfo::Unit,
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Resource { name, .. }
            | ResolvedType::Variant { name, .. }
            | ResolvedType::Newtype { name, .. }
            | ResolvedType::TypeParam { name, .. } => TypeNameInfo::Named(name.clone()),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_name(*t))
                    .collect();
                TypeNameInfo::Generic {
                    name: name.clone(),
                    args,
                }
            }
            ResolvedType::Tuple(elems) => {
                let elem_names: Vec<String> =
                    elems.iter().map(|t| self.mangle_type_name(*t)).collect();
                TypeNameInfo::Tuple(elem_names)
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => TypeNameInfo::Function {
                param_count: params.len(),
                return_type: self.mangle_type_name(*return_type),
            },
            ResolvedType::BuiltinArray(elem) => TypeNameInfo::Array(self.mangle_type_name(*elem)),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For references, use the inner type's name (strip reference)
                TypeNameInfo::Ref(self.mangle_type_name(*inner))
            }
            ResolvedType::Stream(inner) => TypeNameInfo::Stream(self.mangle_type_name(*inner)),
            ResolvedType::StreamWritable(inner) => {
                TypeNameInfo::StreamWritable(self.mangle_type_name(*inner))
            }
            ResolvedType::Future(inner) => TypeNameInfo::Future(self.mangle_type_name(*inner)),
            ResolvedType::FutureWritable(inner) => {
                TypeNameInfo::FutureWritable(self.mangle_type_name(*inner))
            }
            ResolvedType::Reactive(inner) => TypeNameInfo::Reactive(self.mangle_type_name(*inner)),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                ..
            } => TypeNameInfo::Named(format!(
                "{}::{}",
                self.mangle_type_name(*param_id),
                assoc_name
            )),
            ResolvedType::Never | ResolvedType::Unknown | ResolvedType::Error => {
                TypeNameInfo::Unknown
            }
        }
    }
}

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

/// Reference to a function, either resolved to TIR or external
#[derive(Debug, Clone)]
pub enum FunctionRef {
    /// Reference to a resolved TIR function
    Resolved {
        func: Rc<RefCell<TirFunction>>,
        module_source: ModuleSource,
    },
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
            FunctionRef::Resolved { func, .. } => func.borrow().name.clone(),
            FunctionRef::External { name, .. } => name.clone(),
        }
    }

    /// Get the module source
    pub fn module_source(&self) -> ModuleSource {
        match self {
            FunctionRef::Resolved { module_source, .. } => module_source.clone(),
            FunctionRef::External { module_source, .. } => module_source.clone(),
        }
    }

    /// Get the module path (for backwards compatibility)
    pub fn module_path(&self) -> Vec<String> {
        self.module_source().to_path()
    }

    /// Check if this is a resolved reference
    pub fn is_resolved(&self) -> bool {
        matches!(self, FunctionRef::Resolved { .. })
    }

    /// Whether this refers to a synthesized CM adapter function.
    pub fn is_cm_adapter(&self) -> bool {
        match self {
            FunctionRef::Resolved { func, .. } => func.borrow().is_cm_adapter,
            FunctionRef::External { .. } => false,
        }
    }

    /// Get the resolved function if available
    pub fn as_resolved(&self) -> Option<&Rc<RefCell<TirFunction>>> {
        match self {
            FunctionRef::Resolved { func, .. } => Some(func),
            FunctionRef::External { .. } => None,
        }
    }

    /// Get the fully qualified function name including module path.
    /// For external functions, this returns "{`module_source/{name`}".
    /// For resolved functions, this returns the method-qualified name if available.
    pub fn full_name(&self) -> String {
        match self {
            FunctionRef::Resolved { func, .. } => {
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
            FunctionRef::Resolved { .. } => None,
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
            FunctionRef::Resolved { func, .. } => func
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
            "array_get" | "array_set" | "array_new" | "array_len" | "array_copy" | "array_fill"
            | "select" => Some(format!("builtin::{generic_name}")),
            _ => None,
        }
    }

    /// Check if this function is monomorphized (instantiated from a generic)
    pub fn is_monomorphized(&self) -> bool {
        match self {
            FunctionRef::Resolved { func, .. } => func.borrow().monomorph_info.is_some(),
            FunctionRef::External { monomorph_info, .. } => monomorph_info.is_some(),
        }
    }

    /// Get the base generic name if this is a monomorphized function.
    /// For "Box<i32>`::get`", returns "Box".
    /// For "Container<i32>`::transform`<i64>", returns "Container".
    pub fn base_struct_name(&self) -> Option<String> {
        match self {
            FunctionRef::Resolved { func, .. } => {
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
            FunctionRef::Resolved { func, .. } => func.borrow().method_info.clone(),
            FunctionRef::External { method_info, .. } => method_info.clone(),
        }
    }

    /// Check if this is a method (instance or static) as opposed to a free function.
    pub fn is_method(&self) -> bool {
        match self {
            FunctionRef::Resolved { func, .. } => func.borrow().is_method(),
            FunctionRef::External { method_info, .. } => method_info.is_some(),
        }
    }

    /// Check if this is a trait method.
    pub fn is_trait_method(&self) -> bool {
        match self {
            FunctionRef::Resolved { func, .. } => func.borrow().is_trait_method(),
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
    /// Read a global variable
    GlobalVarGet {
        module_source: ModuleSource,
        name: String,
    },
    /// Write to a global variable
    GlobalVarSet {
        module_source: ModuleSource,
        name: String,
        value: Box<TirExpr>,
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
    /// Raw Component Model call to a lowered WASI import.
    ///
    /// Used inside synthesized CM adapter functions to call the flat-ABI WASI function
    /// directly, bypassing the normal effect call mechanism. Args are already lowered
    /// to flat CM types (i32, i64, f32, f64).
    CmRawCall {
        /// Full WASI local alias name (e.g., "wasi:cli/stdout@0.3.0/write-via-stream")
        local_name: String,
        /// Flat ABI arguments (already lowered to core Wasm types)
        args: Vec<TirExpr>,
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
        /// Pre-desugar source text for inspect output.
        source_text: Option<String>,
    },

    /// Indirect call through a callable value (closure or funcref)
    IndirectCall {
        /// The callee expression (closure struct or funcref)
        callee: Box<TirExpr>,
        /// Arguments to pass to the callee
        args: Vec<TirExpr>,
    },

    /// Convert a functor struct to canonical closure representation.
    /// Generated by lower phase for closures that need fn-type compatibility
    /// but weren't handled by fn-param specialization.
    ClosureToCanonical {
        /// The functor struct expression (`__Closure_N` literal)
        functor: Box<TirExpr>,
        /// Functor ID for looking up the `__call` method
        functor_id: u32,
        /// Target function type (for canonical closure type lookup)
        target_fn_type: TypeId,
    },

    /// Custom variant construction: `Shape::Circle(5.0)` or `MyVariant::Unit`
    VariantConstruct {
        /// The variant type (e.g., `ResolvedType::Variant` { name: "Shape", ... })
        variant_type: TypeId,
        /// The case index (0-based position in variant declaration)
        case_index: u32,
        /// The case name (for debugging/error messages)
        case_name: String,
        /// Payload value (None for unit variants constructed without explicit payload)
        payload: Option<Box<TirExpr>>,
    },

    /// Enum construction: `Color::Red`
    /// Enums have no payload, just a discriminant value.
    EnumConstruct {
        /// The enum type (e.g., `ResolvedType::Enum` { name: "Color", ... })
        enum_type: TypeId,
        /// The case index (0-based position in enum declaration)
        case_index: u32,
        /// The case name (for debugging/error messages)
        case_name: String,
    },

    /// Labeled block expression: `label: { ... }` that produces a value
    /// The value must be returned via `break label: expr;`
    LabeledBlock {
        label: String,
        block: TirBlock,
        /// The type of value this block produces (from break expressions)
        result_type: TypeId,
    },

    /// Get the discriminant (tag) of a variant value.
    /// Generated from match expressions on variants.
    /// Result type is i32.
    VariantTag {
        expr: Box<TirExpr>,
    },

    /// Test if a variant value is of a specific case.
    /// Generated from if-let patterns on custom variants.
    /// For unit variants: checks discriminator == `case_index`
    /// For payload variants: uses ref.test on the case type
    /// Result type is bool.
    VariantTest {
        expr: Box<TirExpr>,
        /// The case index to test for
        case_index: u32,
        /// The case name (for error messages)
        case_name: String,
    },

    /// Extract the payload from a variant value at a specific case index.
    /// Generated from match expressions on variants.
    VariantPayload {
        expr: Box<TirExpr>,
        /// The case index to extract payload from
        case_index: u32,
        /// The payload type for this case
        payload_type: TypeId,
    },

    /// Switch expression for O(1) dispatch using `br_table`.
    /// Generated by lower phase for dense integer match expressions.
    /// Each arm index corresponds to (`scrutinee_value` - `min_value`).
    Switch {
        /// The scrutinee expression (must be integer type)
        scrutinee: Box<TirExpr>,
        /// Minimum value in the switch range
        min_value: i64,
        /// Arms in order: arm[i] handles value (`min_value` + i)
        /// Length determines the range: `[min_value, min_value + arms.len() - 1]`
        arms: Vec<TirBlock>,
        /// Default arm for values outside the range
        default: TirBlock,
    },

    /// Unresolved template string expression.
    ///
    /// Created by the resolver with resolved sub-expressions but without
    /// expanding to formatting code. The synthesis phase (pre-monomorphize)
    /// expands this into the `__tmpl` labeled block with `String::with_capacity`,
    /// `append`, `Formatter`, and `Display`/inspect calls.
    TemplateString {
        parts: Vec<TirTemplatePart>,
    },
}

/// A part of a resolved template string.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum TirTemplatePart {
    /// A literal string segment.
    Literal(String),
    /// An interpolated expression with optional format specifier.
    Interpolation {
        expr: TirExpr,
        format_spec: Option<TemplateFormatSpec>,
    },
}

/// Parsed format specification from a template string interpolation.
/// Syntax: `[[fill]align][sign][#][0][width][.precision]type`
#[derive(Debug, Clone)]
pub struct TemplateFormatSpec {
    pub fill: Option<char>,
    pub align: Option<char>,
    pub sign_plus: bool,
    pub alternate: bool,
    pub zero_pad: bool,
    pub width: Option<i64>,
    pub precision: Option<i64>,
    /// Type character: `b`, `o`, `x`, `X`, `e`, `E`, `?`
    pub type_char: Option<char>,
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
    /// Optional guard expression (the condition after `&&`)
    pub guard: Option<TirExpr>,
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
        /// Payload type for the matched variant case (unit for no-payload cases)
        payload_type: TypeId,
    },
    /// Enum case pattern (enums are simple i32 discriminants with no payload)
    Enum {
        enum_type: TypeId,
        case_name: String,
        case_index: u32,
    },
    /// Struct destructuring pattern: `{ x, y }` or `Point { x, y }`
    Struct {
        struct_type: TypeId,
        fields: Vec<TirStructPatternField>,
        has_rest: bool,
    },
}

#[derive(Debug, Clone)]
pub struct TirStructPatternField {
    pub field_name: String,
    pub field_index: u32,
    pub pattern: TirPattern,
}

#[derive(Debug, Clone)]
pub enum TirLiteralPattern {
    /// Signed integer literal (covers i8, i16, i32, i64, i128)
    I128(i128),
    /// Unsigned integer literal (covers u8, u16, u32, u64, u128)
    U128(u128),
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
        /// When true, the WIR builder skips deep value-copy for this binding.
        /// Set by LICM for hoisted variables whose source field is verified
        /// non-mutated in the loop, making aliasing safe.
        skip_value_copy: bool,
    },
    Expr(TirExpr),
    Return {
        value: Option<TirExpr>,
    },
    /// `task return expr;` — delivers the async task result without terminating the function.
    /// Eliminated by `synthesis::cm_adapter` before lower/optimize phases.
    TaskReturn {
        value: TirExpr,
    },
    If {
        condition: TirExpr,
        then_block: TirBlock,
        else_block: Option<TirBlock>,
    },
    /// Canonical loop: `loop { ... }` - infinite loop exited via break
    Loop {
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
    /// Tuple destructuring let statement: `let [a, b] = tuple_expr;`
    LetPattern {
        /// The pattern to bind (e.g., [a, b, c] or [x, [y, z]])
        pattern: TirPattern,
        /// Whether bindings are mutable
        is_mut: bool,
        /// The value expression (must be a tuple)
        value: TirExpr,
    },
}

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
    /// Whether this originates from a blanket impl (e.g., `impl<I: Iterator> IntoIterator for I`)
    pub is_blanket: bool,
}

/// Global variable declaration in TIR
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct TirGlobal {
    pub name: String,
    pub ty: TypeId,
    pub initializer: TirExpr,
    pub mutable: bool,
    /// Whether the user declared this global as `global mut`.
    /// Preserved across lowering so the optimizer can promote lazy-init globals
    /// back to immutable when their initializers fold to constants.
    pub wado_mutable: bool,
    pub is_pub: bool,
    /// Module where this global is defined
    pub module_source: ModuleSource,
    pub span: Span,
    /// True if this global's Wasm type should be nullable.
    /// Set by lower phase for lazy-initialized reference type globals.
    pub is_nullable: bool,
    /// Local variable types used by the initializer expression.
    /// Populated when the initializer is non-trivial (e.g., `SequenceLiteralBuilder` coercion).
    pub local_types: Vec<TypeId>,
}

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct TirFunction {
    pub name: String,
    pub is_pub: bool,
    /// Whether this function is exported at the Component Model boundary (world export)
    pub is_export: bool,
    /// Whether this is an async function (`export async fn`).
    /// Async functions use `task return` instead of `return` to deliver results.
    pub is_async: bool,
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
    pub address_taken_locals: IndexSet<u32>,

    /// Whether this function is a synthesized CM adapter (generated by `synthesis::cm_adapter`).
    /// Used by DCE to avoid module remapping for adapter functions.
    pub is_cm_adapter: bool,

    /// Inline hint from `#[inline]`, `#[inline(always)]`, or `#[inline(never)]` attributes.
    pub inline_hint: InlineHint,

    /// Compiler feature bitflags from `#[comp_feature("...")]` attributes.
    pub comp_features: u32,
}

/// Inline hint for a function, extracted from `#[inline(...)]` attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InlineHint {
    /// No hint — the optimizer decides based on heuristics.
    #[default]
    Auto,
    /// `#[inline]` — suggest inlining (raises the threshold).
    Hint,
    /// `#[inline(always)]` — always inline regardless of size.
    Always,
    /// `#[inline(never)]` — never inline.
    Never,
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
    pub is_pub: bool,
    pub type_id: TypeId,
    pub index: u32,
    pub span: Span,
    /// `#[hidden]` — field not shown in debug inspect output.
    pub is_hidden: bool,
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

/// A flags type declaration (bitmask type, like WIT flags)
/// e.g., `flags PathFlags { SymlinkFollow }`
/// Represented as a newtype over u32; each member is a bitmask value (1 << index).
#[derive(Debug, Clone)]
pub struct TirFlags {
    pub name: String,
    pub is_pub: bool,
    /// The newtype `TypeId` (base type is u32)
    pub type_id: TypeId,
    pub members: Vec<TirFlagsMember>,
    pub span: Span,
}

/// A member of a flags type
#[derive(Debug, Clone)]
pub struct TirFlagsMember {
    pub name: String,
    /// Bitmask value: `1 << index`
    pub bitmask: u32,
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
    /// Compiler feature bitflags from `#[comp_feature("...")]` attributes.
    pub comp_features: u32,
    pub span: Span,
}

/// A case in a variant declaration
/// e.g., `Circle(f64)` or `Point`
///
/// Each variant case has exactly one payload type:
/// - Unit variants: `None` → payload is `()` (unit type)
/// - Scalar payloads: `Some(T)` → payload is `T`
/// - Tuple payloads: `Rectangle([f64, f64])` → payload is `[f64, f64]`
#[derive(Debug, Clone)]
pub struct TirVariantCase {
    pub name: String,
    /// Case index (0-based)
    pub index: u32,
    /// Payload type for this case. Unit variants have `()` (unit type) payload.
    pub payload: TypeId,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirNewtype {
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
    /// Generated function name (e.g., "__`test_0`", "__`test_trap_0`", or "__`test_todo_0`")
    pub function_name: String,
    /// Source line number for unnamed test identification
    pub line: usize,
    pub span: Span,
    /// Whether this test is expected to trap (from `#[expect_trap]` attribute)
    pub expect_trap: bool,
    /// Whether this test is a TODO placeholder (from `#[TODO]` attribute).
    /// Like `expect_trap`, the test passes when the body traps, but the runner emits
    /// a distinct message when the body unexpectedly passes, reminding the developer
    /// to remove the `#[TODO]` attribute.
    pub is_todo: bool,
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

/// `impl Trait for Type;` — request the compiler to synthesize the trait implementation.
#[derive(Debug, Clone)]
pub struct SynthesisRequest {
    pub trait_name: String,
    pub target_type_name: String,
    pub target_type_id: TypeId,
    /// Type parameters: `(name, index, type_id)`
    pub type_params: Vec<(String, u32, TypeId)>,
    pub span: Span,
}

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
    /// Type ID of the generated functor struct (bare struct type for definitions)
    pub struct_type_id: TypeId,
    /// Type ID of reference to functor struct (for expression/local types)
    /// Functors are reference types, so variables holding them have this type.
    pub ref_type_id: TypeId,
    /// The `__call` method for this closure (with body transformed:
    /// Capture nodes become `FieldAccess` on self)
    pub call_method: Rc<RefCell<TirFunction>>,
    /// Captures from the original closure
    pub captures: Vec<TirCapture>,
}

/// External function import from Component Model canonical builtins.
/// These are functions that need to be imported at the Wasm level.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TirImport {
    /// Import namespace ("wasi" or "env")
    pub namespace: String,
    /// Canonical name for the import (e.g., "stream-new", "`libm_sin`")
    pub canonical_name: String,
    /// Internal function name (e.g., "`stream_new`", "`f64_sin`")
    pub func_name: String,
    /// Parameter types
    pub params: Vec<TypeId>,
    /// Return type
    pub return_type: TypeId,
}

/// Tracks a requested instantiation of a generic item
/// Note: Only `name` and `type_args` are used for equality/hashing.
/// `method_info` is auxiliary metadata for name formatting.
#[derive(Debug, Clone)]
pub struct InstantiationKey {
    /// Name of the generic item (struct, function, or enum)
    pub name: String,
    /// Concrete type arguments for instantiation
    pub type_args: Vec<TypeId>,
    /// Method info for method instantiations (None for struct/enum instantiations)
    /// Not included in equality/hash - used only for name formatting
    pub method_info: Option<LocalMethodName>,
}

impl PartialEq for InstantiationKey {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.type_args == other.type_args
    }
}

impl Eq for InstantiationKey {}

impl std::hash::Hash for InstantiationKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.type_args.hash(state);
    }
}

#[derive(Debug, Clone)]
pub struct TirModule {
    pub module_source: ModuleSource,
    /// Shared type table across all modules (enables cross-module type references)
    pub type_table: Rc<RefCell<TypeTable>>,
    /// External function imports (canonical builtins from wasi/env namespaces)
    pub imports: Vec<TirImport>,
    pub functions: Vec<Rc<RefCell<TirFunction>>>,
    pub structs: Vec<TirStruct>,
    pub enums: Vec<TirEnum>,
    /// Flags type declarations (bitmask types, newtypes over u32)
    pub flags: Vec<TirFlags>,
    /// Custom variant declarations (tagged unions with payloads)
    pub variants: Vec<TirVariantDecl>,
    pub newtypes: Vec<TirNewtype>,
    pub effects: Vec<TirEffect>,
    pub traits: Vec<TirTrait>,
    pub impls: Vec<TirImpl>,
    /// `impl Trait for Type;` — synthesis requests (populated by resolver, consumed by synthesis)
    pub synthesis_requests: Vec<SynthesisRequest>,
    /// Test declarations with their metadata
    pub tests: Vec<TirTest>,
    /// Global variable declarations
    pub globals: Vec<TirGlobal>,
    pub data_section: Option<String>,
    pub string_literals: Vec<String>,
    /// Map of function name to string literals it contains (for DCE)
    pub function_strings: IndexMap<String, Vec<String>>,
    /// Map of function name to its method info (for DCE), populated alongside `function_strings`
    pub function_method_info: IndexMap<String, Option<LocalMethodName>>,
    /// Generic struct definitions (before monomorphization)
    /// Key: struct name
    pub generic_structs: IndexMap<String, TirStruct>,
    /// Generic function definitions (before monomorphization)
    /// Key: function name
    pub generic_functions: IndexMap<String, Rc<RefCell<TirFunction>>>,
    /// Requested instantiations (populated during resolution, processed in lower)
    pub instantiation_requests: IndexSet<InstantiationKey>,
    /// Closure metadata for optimization (populated by lower phase).
    /// Maps closure ID to functor info including the `__call` method for inlining.
    pub closure_functors: Vec<ClosureFunctor>,
}

impl TirModule {
    pub fn new(module_source: ModuleSource) -> Self {
        Self {
            module_source,
            type_table: Rc::new(RefCell::new(TypeTable::new())),
            imports: Vec::new(),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            flags: Vec::new(),
            variants: Vec::new(),
            newtypes: Vec::new(),
            effects: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            string_literals: Vec::new(),
            function_strings: IndexMap::new(),
            function_method_info: IndexMap::new(),
            generic_structs: IndexMap::new(),
            generic_functions: IndexMap::new(),
            instantiation_requests: IndexSet::new(),
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
            imports: Vec::new(),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            flags: Vec::new(),
            variants: Vec::new(),
            newtypes: Vec::new(),
            effects: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            string_literals: Vec::new(),
            function_strings: IndexMap::new(),
            function_method_info: IndexMap::new(),
            generic_structs: IndexMap::new(),
            generic_functions: IndexMap::new(),
            instantiation_requests: IndexSet::new(),
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

    pub fn add_flags(&mut self, f: TirFlags) {
        self.flags.push(f);
    }

    pub fn add_newtype(&mut self, newtype: TirNewtype) {
        self.newtypes.push(newtype);
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
}
