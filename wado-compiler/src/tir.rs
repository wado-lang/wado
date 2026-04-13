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

use crate::hashmap::{IndexMap, IndexSet};

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

/// A resolved effect reference.
///
/// In AST, effects are bare strings. In TIR, they are resolved to include
/// the module source where the effect is defined, enabling cross-module
/// effect identity.
///
/// Equality and hashing are based on the effect name only, not the module source.
/// This is because effects with the same name but imported from different modules
/// (e.g., `Stdout` from `core:cli` vs `wasi:cli`) refer to the same effect.
#[derive(Debug, Clone)]
pub enum EffectRef {
    /// A concrete effect resolved to a module source (e.g., `Stdout` from `wasi:cli`)
    Concrete {
        name: String,
        module_source: ModuleSource,
    },
    /// An effect parameter from a generic effect declaration (e.g., `E` in `<effect E>`)
    Param { name: String },
}

impl PartialEq for EffectRef {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for EffectRef {}

impl std::hash::Hash for EffectRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name().hash(state);
    }
}

impl EffectRef {
    pub fn name(&self) -> &str {
        match self {
            EffectRef::Concrete { name, .. } | EffectRef::Param { name } => name,
        }
    }

    pub fn is_param(&self) -> bool {
        matches!(self, EffectRef::Param { .. })
    }
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
            substitutions: IndexMap::default(),
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
                if concrete_id != param_id {
                    // Direct lookup first
                    if let Some(resolved) = type_table.resolve_assoc_type(concrete_id, &assoc_name)
                    {
                        return resolved;
                    }
                    // Newtype fallback: newtypes inherit associated types from their base type.
                    let base_id = type_table.get_ultimate_base_type(concrete_id);
                    if base_id != concrete_id
                        && let Some(resolved) = type_table.resolve_assoc_type(base_id, &assoc_name)
                    {
                        return resolved;
                    }
                    // GenericInstance fallback: e.g. ArrayIter<i32>::Item -> i32
                    if let Some(resolved) =
                        type_table.resolve_generic_assoc_type(concrete_id, &assoc_name)
                    {
                        return resolved;
                    }
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
                stores,
            } => {
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.substitute(p, type_table))
                    .collect();
                let new_return = self.substitute(return_type, type_table);
                type_table.make_function(new_params, new_return, effects, stores)
            }
            ResolvedType::GenericResource {
                name,
                module_source,
                type_args,
            } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&a| self.substitute(a, type_table))
                    .collect();
                type_table.intern(ResolvedType::GenericResource {
                    name,
                    module_source,
                    type_args: new_args,
                })
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
    V128,
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
            Self::V128 => "v128",
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
                | "v128"
        )
    }

    pub fn all_primitive_names() -> &'static [&'static str] {
        &[
            "i8", "i16", "i32", "i64", "i128", "u8", "u16", "u32", "u64", "u128", "f32", "f64",
            "bool", "char", "v128",
        ]
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
    //
    // NOTE: Future<T>, Stream<T>, FutureWritable<T>, StreamWritable<T> are no longer dedicated
    // type variants. They are represented as GenericResource { name: "Future", ... }.
    // Use TypeTable::make_future() / as_future() etc. to create and inspect them.
    /// Generic resource instantiation (e.g., `Future<i32>`, `Stream<String>`).
    /// Represents opaque i32 handles to Component Model resources with type parameters.
    GenericResource {
        name: String,
        module_source: ModuleSource,
        type_args: Vec<TypeId>,
    },
    Ref(TypeId),
    MutRef(TypeId),
    Function {
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<EffectRef>,
        /// Positional indices of parameters the function may store.
        stores: Vec<u32>,
    },
    Reactive(TypeId),
    /// Type parameter (e.g., `T` in `struct Box<T>`)
    /// Used before monomorphization; should be substituted with concrete types
    TypeParam {
        name: String,
        /// Index of the type parameter in the generic definition (0 for first param)
        index: u32,
    },
    /// Type pack parameter (e.g., `..T` in `fn foo<..T>(x: [..T])`)
    /// Used inside tuples before monomorphization; expanded to concrete types during substitution
    TypePack {
        name: String,
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
        /// Resolved associated type bindings (e.g., [("Item", `u8_typeid`)] for `I::Iter`
        /// when I: `IntoIterator`<Item = u8> and `IntoIterator::Iter`: Iterator<Item = `Self::Item`>)
        assoc_type_bindings: Vec<(String, TypeId)>,
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
    /// Flags: a bitmask type over u32.
    /// Created by `flags F { A, B, C }` declarations.
    /// Distinct from Newtype so flags can be detected without name-based lookup.
    Flags {
        name: String,
        module_source: ModuleSource,
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
            | Self::GenericResource { module_source, .. }
            | Self::Newtype { module_source, .. }
            | Self::Flags { module_source, .. } => module_source.to_path(),
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
    /// Module source of the canonical `Default` trait, set via `#[comp_feature("default")]`.
    default_trait_module_source: Option<ModuleSource>,
    /// Module source of the canonical `From<T>` trait, set via `#[comp_feature("from")]`.
    from_trait_module_source: Option<ModuleSource>,
    /// Module source of the canonical `Box<T>` struct, set via `#[comp_feature("box")]`.
    pub box_module_source: Option<ModuleSource>,
    /// Module source of `RangeExclusive<T>`, set via `#[comp_feature("range_exclusive")]`.
    pub range_exclusive_module_source: Option<ModuleSource>,
    /// Module source of `RangeInclusive<T>`, set via `#[comp_feature("range_inclusive")]`.
    pub range_inclusive_module_source: Option<ModuleSource>,
    /// Module source that owns the tuple type family, set via `#[comp_feature("tuple")]`.
    tuple_module_source: Option<ModuleSource>,
    /// Associated type resolutions: `(concrete_type_id, assoc_name)` → `resolved_type_id`.
    /// Populated when impl blocks with associated type bindings are processed.
    assoc_type_resolutions: IndexMap<(TypeId, String), TypeId>,
    /// Generic associated type definitions: `(base_struct_name, assoc_name)` → `TypeId`.
    /// The `TypeId` is typically a `TypeParam` that can be substituted using the
    /// `GenericInstance`'s `type_args`. Populated when processing generic impl blocks
    /// (e.g., `impl Iterator for ArrayIter<T> { type Item = T; }`).
    /// Used by the monomorphizer to resolve associated types for `GenericInstance` types.
    generic_assoc_type_defs: IndexMap<(String, String), TypeId>,
    /// Erasure redirects: set by `erase_newtypes_and_flags()`.
    /// After erasure, `get(id)` for any erased `TypeId` returns the base type.
    /// Newtype → ultimate base type; Flags → u32.
    redirects: IndexMap<TypeId, TypeId>,
    /// Maps newtype names to their ultimate base type name.
    /// Populated by `erase_newtypes_and_flags()` and used by `wir_build` for name-based newtype resolution.
    newtype_to_base_name: IndexMap<String, String>,
    /// Index from (struct name, module source) to `TypeId` for O(1) lookup.
    /// Populated incrementally when Struct types are interned.
    struct_name_index: IndexMap<(String, ModuleSource), TypeId>,
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
    pub const V128: TypeId = TypeId(14);
    pub const UNIT: TypeId = TypeId(15);
    pub const NEVER: TypeId = TypeId(16);
    // STRING removed - String is now a user-defined struct in core:prelude/string.wado
    pub const UNKNOWN: TypeId = TypeId(17);
    pub const ERROR: TypeId = TypeId(18);

    /// Internal name for built-in tuple types in `GenericInstance`.
    /// Distinguished from user-defined structs named "Tuple" by `module_source.is_core()`.
    pub const TUPLE_TYPE_NAME: &'static str = "Tuple";

    /// Check if a name and `module_source` identify a built-in tuple type.
    pub fn is_tuple_type(name: &str, module_source: &ModuleSource) -> bool {
        name == Self::TUPLE_TYPE_NAME && module_source.is_core()
    }

    pub fn new() -> Self {
        let mut table = Self {
            types: IndexMap::default(),
            intern_map: IndexMap::default(),
            next_id: 0,
            option_module_source: None,
            result_module_source: None,
            default_trait_module_source: None,
            from_trait_module_source: None,
            box_module_source: None,
            range_exclusive_module_source: None,
            range_inclusive_module_source: None,
            tuple_module_source: None,
            assoc_type_resolutions: IndexMap::default(),
            generic_assoc_type_defs: IndexMap::default(),
            redirects: IndexMap::default(),
            newtype_to_base_name: IndexMap::default(),
            struct_name_index: IndexMap::default(),
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
        table.intern(ResolvedType::Primitive(PrimitiveType::V128));
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
        // Update struct name index for O(1) lookups by (name, module_source)
        if let ResolvedType::Struct {
            ref name,
            ref module_source,
            ..
        } = ty
        {
            self.struct_name_index
                .insert((name.clone(), module_source.clone()), id);
        }
        self.types.insert(id, ty.clone());
        self.intern_map.insert(ty, id);
        id
    }

    pub fn get(&self, id: TypeId) -> &ResolvedType {
        let id = self.redirects.get(&id).copied().unwrap_or(id);
        self.types
            .get(&id)
            .unwrap_or_else(|| panic!("TypeId {id:?} not found in TypeTable"))
    }

    /// Erase all newtypes from the `TypeTable`.
    /// Look up the ultimate base type name for a newtype by its name.
    /// Returns `None` if the name is not a known newtype.
    /// Available after `erase_newtypes_and_flags()` is called.
    pub fn get_newtype_ultimate_base_name(&self, name: &str) -> Option<&str> {
        self.newtype_to_base_name.get(name).map(String::as_str)
    }

    /// Iterate over all types in the type table.
    pub fn all_types(&self) -> impl Iterator<Item = (&TypeId, &ResolvedType)> {
        self.types.iter()
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

    /// Look up a struct `TypeId` by its name and module source.
    /// Returns `None` if no struct with that name exists in the given module.
    pub fn find_struct_by_name(&self, name: &str, module_source: &ModuleSource) -> Option<TypeId> {
        self.struct_name_index
            .get(&(name.to_string(), module_source.clone()))
            .copied()
    }

    /// Find the `module_source` where a type with the given name is defined.
    /// Searches `struct_name_index` first, then falls back to scanning for
    /// `GenericInstance` types (for generic struct base names like "`IterFilter`").
    pub fn find_struct_module_source(&self, name: &str) -> Option<ModuleSource> {
        // Try struct_name_index first (for concrete struct types)
        if let Some((_, ms)) = self.struct_name_index.keys().find(|(n, _)| n == name) {
            return Some(ms.clone());
        }
        // Fall back to scanning GenericInstance types (for generic templates)
        for id in self.types.keys() {
            if let ResolvedType::GenericInstance {
                name: gi_name,
                module_source,
                ..
            } = self.get(*id)
                && gi_name == name
            {
                return Some(module_source.clone());
            }
        }
        None
    }

    /// Remove all type entries whose `TypeId` is not in `keep`.
    ///
    /// This physically removes entries from the backing `IndexMap`s so that
    /// subsequent iterations (e.g. WIR type registration) no longer see them.
    /// The intern map and struct name index are rebuilt to stay consistent.
    pub fn retain(&mut self, keep: &IndexSet<TypeId>) {
        self.types.retain(|id, _| keep.contains(id));
        // Rebuild intern map from the surviving entries.
        self.intern_map.clear();
        self.struct_name_index.clear();
        for (&id, ty) in &self.types {
            self.intern_map.insert(ty.clone(), id);
            if let ResolvedType::Struct {
                name,
                module_source,
                ..
            } = ty
            {
                self.struct_name_index
                    .insert((name.clone(), module_source.clone()), id);
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

    /// Register the defining module source for a trait marked with `#[comp_feature("default")]`.
    pub fn register_comp_feature_trait(&mut self, comp_features: u32, module_source: ModuleSource) {
        if comp_features & crate::wir::COMP_FEATURE_DEFAULT != 0 {
            self.default_trait_module_source = Some(module_source.clone());
        }
        if comp_features & crate::wir::COMP_FEATURE_FROM != 0 {
            self.from_trait_module_source = Some(module_source);
        }
    }

    /// Register the module source for the tuple type family (`pub type [..T];`).
    pub fn register_tuple_module_source(&mut self, module_source: ModuleSource) {
        self.tuple_module_source = Some(module_source);
    }

    /// Get the module source that owns the tuple type family.
    pub fn tuple_module_source(&self) -> Option<&ModuleSource> {
        self.tuple_module_source.as_ref()
    }

    /// Get the module source where the `Default` trait is defined.
    pub fn default_trait_module_source(&self) -> Option<&ModuleSource> {
        self.default_trait_module_source.as_ref()
    }

    /// Get the module source where the `From<T>` trait is defined.
    pub fn from_trait_module_source(&self) -> Option<&ModuleSource> {
        self.from_trait_module_source.as_ref()
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

    /// Create a `Future<T>` generic resource type.
    pub fn make_future(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::GenericResource {
            name: "Future".to_string(),
            module_source: ModuleSource::types(),
            type_args: vec![inner],
        })
    }

    /// Create a `FutureWritable<T>` generic resource type.
    pub fn make_future_writable(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::GenericResource {
            name: "FutureWritable".to_string(),
            module_source: ModuleSource::types(),
            type_args: vec![inner],
        })
    }

    /// Create a `Stream<T>` generic resource type.
    pub fn make_stream(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::GenericResource {
            name: "Stream".to_string(),
            module_source: ModuleSource::types(),
            type_args: vec![inner],
        })
    }

    /// Create a `StreamWritable<T>` generic resource type.
    pub fn make_stream_writable(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::GenericResource {
            name: "StreamWritable".to_string(),
            module_source: ModuleSource::types(),
            type_args: vec![inner],
        })
    }

    /// If `type_id` is a `GenericResource`, return `(name, module_source, type_args)`.
    pub fn as_generic_resource(&self, type_id: TypeId) -> Option<(&str, &ModuleSource, &[TypeId])> {
        if let ResolvedType::GenericResource {
            name,
            module_source,
            type_args,
        } = self.get(type_id)
        {
            Some((name.as_str(), module_source, type_args.as_slice()))
        } else {
            None
        }
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
        let module_source = self
            .tuple_module_source
            .clone()
            .unwrap_or_else(|| ModuleSource::core("prelude"));
        self.intern(ResolvedType::GenericInstance {
            name: Self::TUPLE_TYPE_NAME.to_string(),
            module_source,
            type_args: elements,
        })
    }

    /// Check if a type is a built-in tuple (`GenericInstance` named "Tuple" from a core module).
    pub fn is_tuple(&self, id: TypeId) -> bool {
        matches!(
            self.get(id),
            ResolvedType::GenericInstance { name, module_source, .. }
                if Self::is_tuple_type(name, module_source)
        )
    }

    /// If the type is a built-in tuple, return its element types.
    pub fn as_tuple(&self, id: TypeId) -> Option<Vec<TypeId>> {
        if let ResolvedType::GenericInstance {
            name,
            module_source,
            type_args,
        } = self.get(id)
            && Self::is_tuple_type(name, module_source)
        {
            Some(type_args.clone())
        } else {
            None
        }
    }

    pub fn make_function(
        &mut self,
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<EffectRef>,
        stores: Vec<u32>,
    ) -> TypeId {
        self.intern(ResolvedType::Function {
            params,
            return_type,
            effects,
            stores,
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

    /// Find a named type (resource, enum, or variant) scoped to a WASI package.
    ///
    /// Tries resource → enum → variant in order, restricting matches to types
    /// whose `module_source` is `Wasi { interface }` where `interface` starts
    /// with `{wasi_package}/`.  Falls back to the unscoped lookup when no
    /// scoped match is found.
    pub fn find_named_type_by_wasi_package(
        &self,
        name: &str,
        wasi_package: &str,
    ) -> Option<TypeId> {
        let prefix = format!("{wasi_package}/");
        for (&type_id, resolved) in &self.types {
            let matches = match resolved {
                ResolvedType::Resource {
                    name: n,
                    module_source: ModuleSource::Wasi { interface },
                } => n == name && interface.starts_with(&prefix),
                ResolvedType::Enum {
                    name: n,
                    module_source: ModuleSource::Wasi { interface },
                } => n == name && interface.starts_with(&prefix),
                ResolvedType::Variant {
                    name: n,
                    module_source: ModuleSource::Wasi { interface },
                } => n == name && interface.starts_with(&prefix),
                _ => false,
            };
            if matches {
                return Some(type_id);
            }
        }
        None
    }

    /// Find a tuple type with the given element types.
    pub fn find_tuple(&self, elems: &[TypeId]) -> Option<TypeId> {
        self.find_generic_instance(Self::TUPLE_TYPE_NAME, elems)
    }

    /// Find a generic instance type with the given name and type args.
    pub fn find_generic_instance(&self, name: &str, type_args: &[TypeId]) -> Option<TypeId> {
        for (&type_id, resolved) in &self.types {
            if let ResolvedType::GenericInstance {
                name: gname,
                type_args: gargs,
                ..
            } = resolved
                && gname == name
                && gargs == type_args
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

    /// Peel through Ref/MutRef wrappers to get the underlying type.
    pub fn peel_refs(&self, mut type_id: TypeId) -> TypeId {
        loop {
            match self.get(type_id) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => type_id = *inner,
                _ => return type_id,
            }
        }
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

    /// Create a type pack parameter (e.g., `..T` in `fn foo<..T>(x: [..T])`)
    pub fn make_type_pack(&mut self, name: String, index: u32) -> TypeId {
        self.intern(ResolvedType::TypePack { name, index })
    }

    /// Create a simple associated type projection `T::X` with no bounds or bindings.
    /// Used in pre-pass registration of generic impl associated types.
    pub fn make_assoc_type_projection_simple(
        &mut self,
        param_id: TypeId,
        assoc_name: String,
    ) -> TypeId {
        self.intern(ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            bounds: vec![],
            assoc_type_bindings: vec![],
        })
    }

    /// Create an associated type projection: `T::X` where T is a type parameter.
    pub fn make_assoc_type_projection(
        &mut self,
        param_id: TypeId,
        assoc_name: String,
        bounds: Vec<String>,
        assoc_type_bindings: Vec<(String, TypeId)>,
    ) -> TypeId {
        self.intern(ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            bounds,
            assoc_type_bindings,
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

    /// Register a generic associated type definition.
    /// E.g., for `impl Iterator for ArrayIter<T> { type Item = T; }`,
    /// register `("ArrayIter", "Item") → TypeParam(0, "T")`.
    pub fn register_generic_assoc_type_def(
        &mut self,
        base_struct_name: String,
        assoc_name: String,
        type_param_id: TypeId,
    ) {
        self.generic_assoc_type_defs
            .insert((base_struct_name, assoc_name), type_param_id);
    }

    /// Resolve an associated type for a `GenericInstance` type using generic definitions.
    /// For `ArrayIter<i32>::Item`: looks up `("ArrayIter", "Item")` → `TypeParam(0)`,
    /// then substitutes using the instance's `type_args` to get `i32`.
    pub fn resolve_generic_assoc_type(
        &self,
        concrete_id: TypeId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let (base_name, type_args) = match self.get(concrete_id).clone() {
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => (name, type_args),
            _ => return None,
        };
        let def_type_id = *self
            .generic_assoc_type_defs
            .get(&(base_name, assoc_name.to_string()))?;
        match self.get(def_type_id).clone() {
            ResolvedType::TypeParam { index, .. } => type_args.get(index as usize).copied(),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name: inner_assoc_name,
                ..
            } => {
                // The def is `I::InnerName`. Substitute I with the concrete type arg.
                let inner_concrete_id =
                    if let ResolvedType::TypeParam { index, .. } = self.get(param_id).clone() {
                        type_args.get(index as usize).copied()?
                    } else {
                        param_id
                    };
                // Recursively resolve `inner_concrete_id::inner_assoc_name`.
                if let Some(resolved) =
                    self.resolve_assoc_type(inner_concrete_id, &inner_assoc_name)
                {
                    return Some(resolved);
                }
                self.resolve_generic_assoc_type(inner_concrete_id, &inner_assoc_name)
            }
            _ => Some(def_type_id),
        }
    }

    /// Substitute `TypeParam` and `TypePack` indices in `type_id` using `substitution`.
    ///
    /// Returns a new `TypeId` with the substitutions applied. Missing indices
    /// are permissive: unmatched `TypeParam`s remain in place so callers can
    /// perform partial substitution during type inference, or let downstream
    /// code report errors.
    ///
    /// Handles all container forms (`Ref`, `MutRef`, `BuiltinArray`,
    /// `Function`, `GenericInstance`, `GenericResource`) including `TypePack`
    /// expansion inside tuple `GenericInstance`s, and `AssocTypeProjection`
    /// resolution whenever the underlying parameter becomes fully concrete
    /// after substitution.
    pub fn substitute_type_params(
        &mut self,
        type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
    ) -> TypeId {
        if substitution.is_empty() {
            return type_id;
        }
        match self.get(type_id).clone() {
            ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                substitution.get(&index).copied().unwrap_or(type_id)
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type_params(elem, substitution);
                if new_elem == elem {
                    type_id
                } else {
                    self.intern(ResolvedType::BuiltinArray(new_elem))
                }
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type_params(inner, substitution);
                if new_inner == inner {
                    type_id
                } else {
                    self.make_ref(new_inner)
                }
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type_params(inner, substitution);
                if new_inner == inner {
                    type_id
                } else {
                    self.make_mut_ref(new_inner)
                }
            }
            ResolvedType::Function {
                params,
                return_type,
                effects,
                stores,
            } => {
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.substitute_type_params(p, substitution))
                    .collect();
                let new_return_type = self.substitute_type_params(return_type, substitution);
                if new_params == params && new_return_type == return_type {
                    type_id
                } else {
                    self.make_function(new_params, new_return_type, effects, stores)
                }
            }
            ResolvedType::GenericResource {
                name,
                module_source,
                type_args,
            } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&a| self.substitute_type_params(a, substitution))
                    .collect();
                if new_args == type_args {
                    type_id
                } else {
                    self.intern(ResolvedType::GenericResource {
                        name,
                        module_source,
                        type_args: new_args,
                    })
                }
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                if Self::is_tuple_type(&name, &module_source) {
                    // Tuples need TypePack expansion: splice pack elements
                    // into the tuple's type-arg list.
                    let mut new_elems: Vec<TypeId> = Vec::new();
                    for &e in &type_args {
                        match self.get(e).clone() {
                            ResolvedType::TypePack { index, .. } => {
                                if let Some(&pack_type) = substitution.get(&index) {
                                    if let Some(pack_elems) = self.as_tuple(pack_type) {
                                        new_elems.extend_from_slice(&pack_elems);
                                    } else {
                                        new_elems.push(pack_type);
                                    }
                                } else {
                                    new_elems.push(e);
                                }
                            }
                            _ => {
                                new_elems.push(self.substitute_type_params(e, substitution));
                            }
                        }
                    }
                    if new_elems == type_args {
                        type_id
                    } else {
                        self.make_tuple(new_elems)
                    }
                } else {
                    let new_args: Vec<TypeId> = type_args
                        .iter()
                        .map(|&a| self.substitute_type_params(a, substitution))
                        .collect();
                    if new_args == type_args {
                        type_id
                    } else {
                        self.make_generic_instance(name, module_source, new_args)
                    }
                }
            }
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                ..
            } => {
                // Substitute the parameter first; only attempt projection
                // resolution once the underlying type is fully concrete.
                let concrete = self.substitute_type_params(param_id, substitution);
                if !self.contains_type_param(concrete) {
                    if let Some(resolved) = self.resolve_assoc_type(concrete, &assoc_name) {
                        return resolved;
                    }
                    if let Some(resolved) = self.resolve_generic_assoc_type(concrete, &assoc_name) {
                        return resolved;
                    }
                }
                type_id
            }
            // Primitives, Unit, Never, Unknown, Error, Struct, Enum, Variant,
            // Resource, Newtype, Flags, Reactive — no embedded type params.
            _ => type_id,
        }
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

    /// Create a flags type (bitmask over u32)
    pub fn make_flags(&mut self, name: String, module_source: ModuleSource) -> TypeId {
        self.intern(ResolvedType::Flags {
            name,
            module_source,
        })
    }

    /// Erase all `Newtype` and `Flags` entries from the type table by populating
    /// the redirect map. After this call, `get(id)` for any erased `TypeId` returns
    /// its ultimate base type (`Newtype` chains) or `u32` (`Flags`).
    ///
    /// Must be called after resolve and synthesis, before monomorphize.
    pub fn erase_newtypes_and_flags(&mut self) {
        let ids: Vec<TypeId> = self.types.keys().copied().collect();
        for id in ids {
            let redirect = match self.types.get(&id).unwrap() {
                ResolvedType::Newtype { .. } => Some(self.get_ultimate_base_type(id)),
                ResolvedType::Flags { .. } => Some(TypeTable::U32),
                _ => None,
            };
            if let Some(target) = redirect {
                self.redirects.insert(id, target);
                // Populate name map for Newtype entries (used by wir_build for name-based lookup)
                if let ResolvedType::Newtype { name, .. } = self.types.get(&id).unwrap() {
                    let base_name = self.type_name(target);
                    self.newtype_to_base_name.insert(name.clone(), base_name);
                }
            }
        }
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
    /// `Flags` types ultimately resolve to `u32`.
    /// Returns the original type if it's not a newtype or flags.
    pub fn get_ultimate_base_type(&self, id: TypeId) -> TypeId {
        // Fast path: after erasure, redirects always point directly to the ultimate base.
        if let Some(&redirect) = self.redirects.get(&id) {
            return redirect;
        }
        let mut current = id;
        loop {
            match self
                .types
                .get(&current)
                .unwrap_or_else(|| panic!("TypeId {current:?} not found in TypeTable"))
            {
                ResolvedType::Newtype { base_type, .. } => {
                    // Use redirect if already computed; otherwise follow the raw chain.
                    current = self.redirects.get(base_type).copied().unwrap_or(*base_type);
                }
                ResolvedType::Flags { .. } => return TypeTable::U32,
                _ => return current,
            }
        }
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

    /// Check if a type contains UNKNOWN (undefined type that was not resolved).
    pub fn contains_unknown(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::Unknown => true,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Reactive(inner) => self.contains_unknown(*inner),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params.iter().any(|p| self.contains_unknown(*p))
                    || self.contains_unknown(*return_type)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => {
                type_args.iter().any(|t| self.contains_unknown(*t))
            }
            _ => false,
        }
    }

    /// Check if a type is or contains type parameters or unresolved types (Unknown/Error)
    pub fn contains_type_param(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => true,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Reactive(inner) => self.contains_type_param(*inner),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params.iter().any(|p| self.contains_type_param(*p))
                    || self.contains_type_param(*return_type)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => {
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
            ResolvedType::GenericResource {
                name, type_args, ..
            } => {
                let arg_names: Vec<String> = type_args.iter().map(|t| self.type_name(*t)).collect();
                format!("{}<{}>", name, arg_names.join(", "))
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
                name,
                module_source,
                type_args,
            } => {
                let arg_names: Vec<String> = type_args.iter().map(|t| self.type_name(*t)).collect();
                if Self::is_tuple_type(name, module_source) {
                    format!("[{}]", arg_names.join(", "))
                } else {
                    format!("{}<{}>", name, arg_names.join(", "))
                }
            }
            ResolvedType::Newtype { name, .. } | ResolvedType::Flags { name, .. } => name.clone(),
            ResolvedType::TypePack { name, .. } => format!("..{name}"),
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
    /// Resolve through newtypes/flags to find the base type.
    /// Returns the original `TypeId` if not a newtype or flags.
    ///
    /// Works both before and after `erase_newtypes_and_flags()`. After erasure,
    /// the redirect map is checked first so that `resolve_newtype_base(FieldName_id)` → `String_id`.
    #[must_use]
    pub fn resolve_newtype_base(&self, id: TypeId) -> TypeId {
        // Fast path: after erasure, redirects already point to the base.
        if let Some(&redirect) = self.redirects.get(&id) {
            return redirect;
        }
        let mut current = id;
        loop {
            match self
                .types
                .get(&current)
                .unwrap_or_else(|| panic!("TypeId {current:?} not found in TypeTable"))
            {
                ResolvedType::Newtype { base_type, .. } => {
                    current = self.redirects.get(base_type).copied().unwrap_or(*base_type);
                }
                ResolvedType::Flags { .. } => return TypeTable::U32,
                _ => return current,
            }
        }
    }

    /// Mangle a type name, resolving all newtypes/flags to their base types recursively.
    /// E.g., `Array<FieldName>` → `Array<String>` when `FieldName = String`.
    pub fn mangle_type_name_resolving_newtypes(&self, id: TypeId) -> String {
        let base = self.resolve_newtype_base(id);
        match self.get(base) {
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_name_resolving_newtypes(*t))
                    .collect();
                crate::name::mangle_generic_name(name, &args)
            }
            ResolvedType::BuiltinArray(elem) => {
                let elem_name = self.mangle_type_name_resolving_newtypes(*elem);
                crate::name::mangle_builtin_array_type(&elem_name)
            }
            _ => self.mangle_type_name(base),
        }
    }

    /// Recursively resolve newtypes inside compound types (tuples, generics, arrays).
    /// Returns the same `TypeId` if no newtypes are found, or a new `TypeId` with all
    /// newtypes replaced by their base types.
    pub fn resolve_newtypes_deep(&mut self, id: TypeId) -> TypeId {
        let base = self.resolve_newtype_base(id);
        match self.get(base).clone() {
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                let resolved: Vec<TypeId> = type_args
                    .iter()
                    .map(|t| self.resolve_newtypes_deep(*t))
                    .collect();
                if resolved == type_args {
                    base
                } else {
                    self.make_generic_instance(name, module_source, resolved)
                }
            }
            ResolvedType::BuiltinArray(elem) => {
                let resolved = self.resolve_newtypes_deep(elem);
                if resolved == elem {
                    base
                } else {
                    self.intern(ResolvedType::BuiltinArray(resolved))
                }
            }
            _ => base,
        }
    }

    /// Like `resolve_newtypes_deep` but non-mutating — only resolves if all
    /// intermediate types already exist. Returns the original `TypeId` if resolution
    /// would require creating new types.
    #[must_use]
    pub fn resolve_newtypes_deep_readonly(&self, id: TypeId) -> TypeId {
        let base = self.resolve_newtype_base(id);
        match self.get(base) {
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let resolved: Vec<TypeId> = type_args
                    .iter()
                    .map(|t| self.resolve_newtypes_deep_readonly(*t))
                    .collect();
                if resolved == *type_args {
                    base
                } else if let Some(existing) = self.find_generic_instance(name, &resolved) {
                    existing
                } else {
                    id // Can't create new type, return original
                }
            }
            _ => base,
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
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => self.base_type_name(*inner),
            _ => self.mangle_type_name(id),
        }
    }

    /// Extract the type arguments for a generic type.
    ///
    /// Works for both `GenericInstance` (which stores `type_args` directly) and
    /// monomorphized `Struct` types (which require searching for the original
    /// `GenericInstance` in the type table).
    ///
    /// Returns `None` for non-generic types.
    #[must_use]
    pub fn generic_type_args(&self, id: TypeId) -> Option<Vec<TypeId>> {
        match self.get(id) {
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => Some(type_args.clone()),
            ResolvedType::Struct {
                name,
                is_monomorphized: true,
                base_name: Some(base_name),
                ..
            } => {
                // Search for a GenericInstance with matching base name and mangled name
                for tid in self.iter_type_ids() {
                    if let ResolvedType::GenericInstance {
                        name: gi_name,
                        type_args,
                        ..
                    } = self.get(tid)
                        && gi_name == base_name
                        && self.mangle_type_name(tid) == *name
                    {
                        return Some(type_args.clone());
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Find type args for a struct/tuple by its mangled name (e.g., "Tuple<i32,String>").
    /// Used by the monomorphizer to extract impl type args for variadic impls.
    pub fn find_type_args_by_mangled_name(&self, mangled_name: &str) -> Option<Vec<TypeId>> {
        for tid in self.iter_type_ids() {
            if self.mangle_type_name(tid) == mangled_name {
                return self.generic_type_args(tid);
            }
        }
        None
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
            | ResolvedType::Flags { name, .. }
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
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => TypeNameInfo::Function {
                param_count: params.len(),
                return_type: self.mangle_type_name(*return_type),
            },
            ResolvedType::BuiltinArray(elem) => {
                TypeNameInfo::BuiltinArray(self.mangle_type_name(*elem))
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For references, use the inner type's name (strip reference)
                TypeNameInfo::Ref(self.mangle_type_name(*inner))
            }
            ResolvedType::GenericResource {
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
            ResolvedType::TypePack { name, .. } => TypeNameInfo::Named(format!("..{name}")),
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

#[derive(Debug, Clone)]
pub struct FunctionRef {
    pub module_source: ModuleSource,
    pub name: String,
    pub monomorph_info: Option<MonomorphInfo>,
    pub method_info: Option<LocalMethodName>,
}

impl FunctionRef {
    /// Create a `FunctionRef` by extracting metadata from a resolved `TirFunction`.
    pub fn from_resolved(func: &TirFunction, module_source: ModuleSource) -> Self {
        Self {
            module_source,
            name: func.name.clone(),
            monomorph_info: func.monomorph_info.clone(),
            method_info: func.method_info.clone(),
        }
    }

    /// Get the module path (for backwards compatibility)
    pub fn module_path(&self) -> Vec<String> {
        self.module_source.to_path()
    }

    /// Get the fully qualified function name including module path.
    pub fn full_name(&self) -> String {
        if let Some(info) = &self.method_info {
            info.to_mangled_name()
        } else if self.module_source.is_entry_point() {
            self.name.clone()
        } else {
            let path = self.module_source.to_path();
            format!("{}/{}", path.join("/"), &self.name)
        }
    }

    /// Get the builtin function name if this is a builtin call.
    /// Returns the qualified name (e.g., "`builtin::array_len`").
    pub fn builtin_name(&self) -> Option<String> {
        if self.module_source.is_core_builtin() && self.monomorph_info.is_none() {
            Some(format!("builtin::{}", &self.name))
        } else {
            None
        }
    }

    /// Get the monomorphized builtin name if this is a monomorphized builtin function.
    pub fn monomorphized_builtin_name(&self) -> Option<String> {
        let generic_name = self
            .monomorph_info
            .as_ref()
            .map(|i| i.generic_name.as_str())?;

        match generic_name {
            "array_get" | "array_set" | "array_new" | "array_len" | "array_copy" | "array_fill"
            | "select" => Some(format!("builtin::{generic_name}")),
            _ => None,
        }
    }

    /// Check if this function is monomorphized (instantiated from a generic)
    pub fn is_monomorphized(&self) -> bool {
        self.monomorph_info.is_some()
    }

    /// Get the base generic name if this is a monomorphized function.
    pub fn base_struct_name(&self) -> Option<String> {
        self.monomorph_info
            .as_ref()
            .and_then(|info| info.generic_name.split("::").next())
            .map(std::string::ToString::to_string)
    }

    /// Check if this is a method (instance or static) as opposed to a free function.
    pub fn is_method(&self) -> bool {
        self.method_info.is_some()
    }

    /// Check if this is a trait method.
    pub fn is_trait_method(&self) -> bool {
        self.method_info
            .as_ref()
            .is_some_and(LocalMethodName::is_trait_method)
    }
}

/// A function argument bundled with its parameter mutability metadata.
///
/// `is_mut` reflects whether the callee declares this parameter as `mut`.
/// It controls value-copy semantics at the call site in the WIR translation phase.
/// An empty `args` list or missing metadata defaults to conservative (copy).
#[derive(Debug, Clone)]
pub struct CallArg {
    pub expr: TirExpr,
    /// Whether the callee declares this parameter as `mut`.
    pub is_mut: bool,
}

impl CallArg {
    pub fn new(expr: TirExpr, is_mut: bool) -> Self {
        Self { expr, is_mut }
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
    /// Byte array literal from `#include_bytes`. Lowered to `Array<u8>` via data segment.
    BytesLiteral(Vec<u8>),
    Null,
    Unit,

    Local {
        index: u32,
        name: String,
    },
    FuncRef {
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

    /// Free function call (`foo(args)`) or static method call (`Type::method(args)`).
    ///
    /// Whether this is a static method call is encoded in `func.method_info`:
    /// `None` → free function, `Some(_)` → static method.
    /// Both map to the same `WirInstr::Call` and share identical semantics.
    Call {
        /// Function reference (resolved TIR function or external)
        func: FunctionRef,
        /// Explicit type arguments for generic functions: `identity::<i32>(x)`
        type_args: Vec<TypeId>,
        args: Vec<CallArg>,
    },
    /// Raw Component Model call to a lowered WASI import.
    ///
    /// Used inside synthesized CM binding functions to call the flat-ABI WASI function
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
        args: Vec<CallArg>,
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

    /// Spread a tuple expression into an enclosing `TupleLiteral`.
    /// Created by the resolver for `[..expr]` syntax. Expanded by monomorphization
    /// into individual `FieldAccess` elements once the concrete tuple arity is known.
    TupleSpread {
        expr: Box<TirExpr>,
    },

    /// Transpose a tuple-of-tuples: `[a, b].zip()` → `[[a.0, b.0], [a.1, b.1], ...]`.
    /// Created by the resolver for the `.zip()` pseudo-method on tuples.
    /// Expanded during monomorphization once concrete tuple arities are known.
    TupleZip {
        expr: Box<TirExpr>,
    },

    /// Type pack expansion: `[..T::method()]` inside a `TupleLiteral`.
    /// Expands at monomorphization to one call per concrete type in the pack:
    /// `[T_0::method(), T_1::method(), ...]`.
    ///
    /// The `call_expr` is a resolved Call whose receiver/return type references
    /// the `TypePack`. During monomorphization, the pack type is substituted
    /// with each concrete element type to produce individual calls.
    TypePackExpansion {
        /// The call expression template (resolved with `TypePack` type)
        call_expr: Box<TirExpr>,
        /// The `TypePack` type ID (index into type table, pre-substitution)
        pack_type_id: TypeId,
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
        /// Module where the closure was defined (needed for cross-module DCE)
        closure_module: ModuleSource,
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
    /// `push_str`, `Formatter`, and `Display`/inspect calls.
    TemplateString {
        parts: Vec<TirTemplatePart>,
    },
}

/// A part of a resolved template string.
#[derive(Debug, Clone)]
pub enum TirTemplatePart {
    /// A literal string segment.
    Literal(String),
    /// An interpolated expression with optional format specifier.
    Interpolation {
        expr: Box<TirExpr>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    RefEq,
    RefNotEq,
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
    Tuple(Vec<TirPattern>, /* has_rest */ bool),
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
    /// Or pattern: matches if any alternative matches
    Or(Vec<TirPattern>),
    /// Constant value pattern: compares scrutinee against a constant expression
    /// (immutable global variable or associated constant like `i32::MAX`)
    ConstantValue {
        expr: Box<TirExpr>,
    },
    /// Range pattern: `0..<10` or `'a'..='z'`
    Range {
        start: i128,
        end: i128,
        inclusive: bool,
        is_unsigned: bool,
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
    /// Eliminated by `synthesis::cm_binding` before lower/optimize phases.
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
    IfLet {
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
    LetDestructure {
        /// The pattern to bind (e.g., [a, b, c] or [x, [y, z]])
        pattern: TirPattern,
        /// Whether bindings are mutable
        is_mut: bool,
        /// The value expression (must be a tuple)
        value: TirExpr,
    },
    /// Deferred tuple for-of expansion for variadic type packs.
    ///
    /// Created when `for let v of iterable` where `iterable` has a tuple type containing
    /// `TypePack` elements. The monomorphizer expands this after type substitution resolves
    /// the `TypePack` to a concrete tuple.
    VariadicForOf {
        /// The tuple iterable expression (type contains `TypePack` before substitution)
        iterable: TirExpr,
        /// The loop variable name
        binding_name: String,
        /// Local index for the loop variable
        binding_local: u32,
        /// Whether the binding is mutable
        is_mut: bool,
        /// The body to execute for each element (resolved with TypePack-typed binding)
        body: TirBlock,
        /// Unique ID for generating labels
        unique_id: u32,
    },
}

/// Generic type parameter in TIR (from AST `GenericParam`)
#[derive(Debug, Clone)]
pub struct TirTypeParam {
    pub name: String,
    /// Whether this is an effect parameter (`effect E`)
    pub is_effect: bool,
    /// Whether this is a type pack parameter (`..T`)
    pub is_pack: bool,
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
    /// Impl-level type arguments (from the struct/type, e.g. `[i32]` for `Array<i32>`)
    pub impl_type_args: Vec<TypeId>,
    /// Method-level type arguments (from the method's own generics, e.g. `[String]` for `.transform::<String>()`)
    pub method_type_args: Vec<TypeId>,
    /// Whether this originates from a blanket impl (e.g., `impl<I: Iterator> IntoIterator for I`)
    pub is_blanket: bool,
}

/// Global variable declaration in TIR
#[derive(Debug, Clone)]
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
pub struct TirFunction {
    pub name: String,
    /// Module this function belongs to. Set by the link phase when flattening
    /// per-module TIR into flat lists; before link, the `module_source` is
    /// carried implicitly by the parent `TirModule`.
    pub module_source: ModuleSource,
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
    pub effects: Vec<EffectRef>,
    /// Parameter names declared in `stores[...]` — the function may store these references.
    pub stores: Vec<String>,
    pub body: Option<TirBlock>,
    pub span: Span,
    pub local_count: u32,
    pub local_types: Vec<TypeId>,
    /// Local indices that have their address taken (&x or &mut x).
    /// For mutable primitives, these locals are stored in box structs.
    pub address_taken_locals: IndexSet<u32>,

    /// Local indices whose references were stored by inlined `stores` functions.
    /// When inlining `fn f(x: &T) with stores[x]` with argument `&local`,
    /// `local` is added here to prevent SROA from decomposing it.
    pub stores_aliased_locals: IndexSet<u32>,

    /// Whether this function is a synthesized CM binding (generated by `synthesis::cm_binding`).
    /// The inliner and effect checker both skip CM bindings because they are ABI bridges
    /// between Wado GC types and CM linear memory with special effect semantics.
    pub is_cm_binding: bool,

    /// Whether this function is a synthesized CM *export* binding (world export wrapper).
    /// When true, the global initializer (`__initialize_modules`) is injected at the start
    /// of this function's body during lowering.
    pub is_cm_export: bool,

    /// Inline hint from `#[inline]`, `#[inline(always)]`, or `#[inline(never)]` attributes.
    pub inline_hint: InlineHint,

    /// Compiler feature bitflags from `#[comp_feature("...")]` attributes.
    pub comp_features: u32,

    /// Custom wasm export name from `#[export_name("...")]` attribute.
    pub export_name: Option<String>,

    /// Allocator tag from `#[allocator("...")]` attribute (e.g., `"bump"`, `"debug"`).
    pub allocator_tag: Option<String>,
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

    /// Returns true if this function has type params that need monomorphization
    /// (excludes effect params, which are erased at compile time).
    #[inline]
    pub fn has_real_type_params(&self) -> bool {
        self.type_params.iter().any(|p| !p.is_effect)
    }
}

#[derive(Debug, Clone)]
pub struct TirParam {
    pub name: String,
    pub type_id: TypeId,
    pub local_index: u32,
    pub is_mut: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirStruct {
    pub name: String,
    pub module_source: ModuleSource,
    pub is_pub: bool,
    /// Generic type parameters (empty for non-generic structs)
    pub type_params: Vec<TirTypeParam>,
    /// If this struct was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub fields: Vec<TirField>,
    pub span: Span,
    /// `#[serde(rename_all = "...")]` — naming strategy for all fields.
    pub serde_rename_all: Option<String>,
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
    /// `#[serde(rename = "name")]` — custom serialization name for this field.
    pub serde_rename: Option<String>,
    /// `#[serde(default)]` — use default value when field is missing during deserialization.
    pub serde_default: bool,
}

#[derive(Debug, Clone)]
pub struct TirEnum {
    pub name: String,
    pub module_source: ModuleSource,
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
/// Represented as `ResolvedType::Flags`; each member is a bitmask value (1 << index).
#[derive(Debug, Clone)]
pub struct TirFlags {
    pub name: String,
    pub module_source: ModuleSource,
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
    pub module_source: ModuleSource,
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
    pub module_source: ModuleSource,
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
    /// Per-test timeout in milliseconds (from `#[timeout_ms(N)]` attribute).
    /// `None` means use the default timeout (1 second).
    pub timeout_ms: Option<u64>,
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
    pub module_source: ModuleSource,
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

/// Tracks a requested instantiation of a generic item.
/// `name`, `module_source`, `impl_type_args`, and `method_type_args` are used for equality/hashing.
/// `method_info` is auxiliary metadata for name formatting.
#[derive(Debug, Clone)]
pub struct InstantiationKey {
    /// Name of the generic item (struct, function, or enum)
    pub name: String,
    /// Module where the generic item is defined.
    /// Distinguishes same-named generics from different modules.
    pub module_source: ModuleSource,
    /// Impl-level type arguments (from the struct/type)
    pub impl_type_args: Vec<TypeId>,
    /// Method-level type arguments (from the method's own generics)
    pub method_type_args: Vec<TypeId>,
    /// Method info for method instantiations (None for struct/enum instantiations)
    /// Not included in equality/hash - used only for name formatting
    pub method_info: Option<LocalMethodName>,
}

impl PartialEq for InstantiationKey {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.module_source == other.module_source
            && self.impl_type_args == other.impl_type_args
            && self.method_type_args == other.method_type_args
    }
}

impl Eq for InstantiationKey {}

impl std::hash::Hash for InstantiationKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.module_source.hash(state);
        self.impl_type_args.hash(state);
        self.method_type_args.hash(state);
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
    /// `#![wasm_module("name")]` — items in this module compile to a separate Wasm core module.
    pub wasm_module: Option<String>,
    pub string_literals: Vec<String>,
    /// Byte array literals from `#include_bytes` (for data segments)
    pub bytes_literals: Vec<Vec<u8>>,
    /// Map of (`module_source`, function name) to string literals it contains (for DCE)
    pub function_strings: IndexMap<(ModuleSource, String), Vec<String>>,
    /// Map of (`module_source`, function name) to its method info (for DCE), populated alongside `function_strings`
    pub function_method_info: IndexMap<(ModuleSource, String), Option<LocalMethodName>>,
    /// Generic struct definitions (before monomorphization)
    /// Key: (struct name, module source)
    pub generic_structs: IndexMap<(String, ModuleSource), TirStruct>,
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
            wasm_module: None,
            string_literals: Vec::new(),
            bytes_literals: Vec::new(),
            function_strings: IndexMap::default(),
            function_method_info: IndexMap::default(),
            generic_structs: IndexMap::default(),
            generic_functions: IndexMap::default(),
            instantiation_requests: IndexSet::default(),
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
            wasm_module: None,
            string_literals: Vec::new(),
            bytes_literals: Vec::new(),
            function_strings: IndexMap::default(),
            function_method_info: IndexMap::default(),
            generic_structs: IndexMap::default(),
            generic_functions: IndexMap::default(),
            instantiation_requests: IndexSet::default(),
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
