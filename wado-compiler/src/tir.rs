//! Typed Intermediate Representation — the post-type-resolution form used for
//! lowering, optimization, and codegen. Every type is a `TypeId`, every variable
//! reference a local index, every call a resolved target, and all syntactic
//! sugar is already desugared.

use std::cell::RefCell;
use std::rc::Rc;

use crate::canonical::CmCallTarget;
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::{LocalMethodName, RefKind, TypeNameInfo, format_type_name};
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

/// A resolved effect reference — a bare string in the AST, carrying its defining
/// module here. Identity is `(module_source, name)` for `Concrete` and `name`
/// alone for `Param`. The elaborator canonicalises the module, so `with Stdout`
/// imported from `wasi:cli` and from `core:cli` compare equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EffectRef {
    /// A concrete effect resolved to a module source (e.g., `Stdout` from `wasi:cli`)
    Concrete {
        name: String,
        module_source: ModuleSource,
    },
    /// An effect parameter from a generic effect declaration (e.g., `E` in `<effect E>`)
    Param { name: String },
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

/// What a use site chose for a declaration's type parameters.
///
/// Keyed by the parameter itself. A slot carries its own index, and only the
/// declaration knows which index each of its parameters holds — a generic,
/// `&`-target, blanket or variadic-tuple impl numbers its slots differently,
/// and a partially concrete target leaves gaps no positional list can express.
/// Keying by position asks the caller to reconstruct that, which it cannot;
/// `impl<T, ..F> Emit for T` is enough to break the reconstruction.
/// `instantiate_call` takes the same view.
#[derive(Debug, Clone, Default)]
pub struct SubstitutionContext {
    substitutions: IndexMap<TypeId, TypeId>,
}

impl SubstitutionContext {
    pub fn new() -> Self {
        Self {
            substitutions: IndexMap::default(),
        }
    }

    /// Bind a declaration's type parameters to the arguments a use site chose.
    ///
    /// `params` are the parameters as the declaration holds them: a lookup
    /// reports them, a declaration record carries them. Nothing recomputes
    /// them. Parameters past the end of `args` stay unbound.
    pub fn bind(mut self, params: &[TypeId], args: &[TypeId]) -> Self {
        for (&param, &arg) in params.iter().zip(args.iter()) {
            self.substitutions.insert(param, arg);
        }
        self
    }

    /// Substitute type parameters in a type
    pub fn substitute(&self, type_id: TypeId, type_table: &mut TypeTable) -> TypeId {
        match type_table.get(type_id).clone() {
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. } => {
                self.substitutions.get(&type_id).copied().unwrap_or(type_id)
            }
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                owning_trait,
                bounds,
                assoc_type_bindings,
            } => {
                // Substitute the underlying type param to get the concrete type
                let concrete_id = self.substitute(param_id, type_table);
                if let Some(resolved) =
                    type_table.resolve_assoc_type_qualified(concrete_id, &owning_trait, &assoc_name)
                {
                    return resolved;
                }
                // Newtype fallback: newtypes inherit associated types from their base type.
                let base_id = type_table.get_ultimate_base_type(concrete_id);
                if base_id != concrete_id
                    && let Some(resolved) =
                        type_table.resolve_assoc_type_qualified(base_id, &owning_trait, &assoc_name)
                {
                    return resolved;
                }
                // GenericInstance fallback: e.g. ListIter<i32>::Item -> i32.
                // Use the monomorphizing variant so a reference / nested
                // associated type (`&T`, `I::Item`) is substituted with the
                // instance's type args instead of returned verbatim.
                if let Some(resolved) =
                    type_table.resolve_generic_assoc_type_mono(concrete_id, &assoc_name)
                {
                    return resolved;
                }
                // The associated type cannot be resolved yet because the
                // projected parameter is still abstract (e.g. substituting
                // `I::Item` with `I -> I`, or `I -> some other type param`).
                // Preserve the projection over the substituted parameter
                // rather than collapsing to the bare parameter, so later
                // monomorphization can resolve `concrete::assoc_name`.
                if concrete_id == param_id {
                    type_id
                } else {
                    type_table.intern(ResolvedType::AssocTypeProjection {
                        param_id: concrete_id,
                        assoc_name,
                        owning_trait,
                        bounds,
                        assoc_type_bindings,
                    })
                }
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
            ResolvedType::GenericInstance { def, type_args } => {
                // Recursively substitute in nested generic instances
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute(arg, type_table))
                    .collect();
                type_table.make_generic_instance(def, new_args)
            }
            ResolvedType::Function {
                is_mut,
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
                type_table.make_function_with_mut(is_mut, new_params, new_return, effects, stores)
            }
            ResolvedType::GenericResource { def, type_args } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&a| self.substitute(a, type_table))
                    .collect();
                type_table.intern(ResolvedType::GenericResource {
                    def,
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

/// Identity of an inference variable — see [`ResolvedType::InferVar`].
///
/// Minted per module by the elaborator, so two uses of the same polymorphic
/// signature get distinct variables. Unlike a [`ResolvedType::TypeParam`]
/// index, this is not positional: it names one unknown, not a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InferVarId(pub u32);

impl std::fmt::Display for InferVarId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "?{}", self.0)
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

/// An anonymous struct's shape, interned by its field list.
///
/// Minted only by [`TypeTable::intern_anon_struct`]. Two literals of the same
/// shape reach one of these, which is why the shape is the identity and no
/// declaration is involved.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AnonStructId(u32);

impl std::fmt::Debug for AnonStructId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Anon({})", self.0)
    }
}

/// A struct shape the compiler minted rather than source declared.
///
/// A struct literal is identified by its fields, because two literals of the
/// same shape are one type. A closure environment is identified by the name
/// lowering assigns it, because two closures with identical captures are
/// still two environments.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum AnonShape {
    Fields(Vec<(String, TypeId)>),
    Synthetic(String),
}

/// What a struct type's head is.
///
/// A struct literal with no type name has no declaration and no node to
/// identify it by — two literals of the same shape intern to one type on
/// purpose — so the head says which of the two it is rather than carrying a
/// `DefId` that would have to be invented for the second.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StructDef {
    Decl(crate::defs::DefId),
    Anon(AnonStructId),
}

impl StructDef {
    /// The declaration this head names, or `None` for a shape that names none.
    #[must_use]
    pub fn decl(self) -> Option<crate::defs::DefId> {
        match self {
            Self::Decl(def) => Some(def),
            Self::Anon(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResolvedType {
    Primitive(PrimitiveType),
    Unit,
    Never,
    Struct {
        /// The declaration this was written from, or the shape it was built
        /// from. Never a spelling: a monomorphized struct used to store the
        /// rendered `TreeMap<String,i32>` and a `base_name` to recover the head
        /// from, which is what let its spelling pass as a declaration name
        /// wherever the two were matched together.
        def: StructDef,
        /// What this instantiation was made with; empty for a declaration
        /// written as such. The rendered spelling is derived from the two —
        /// see [`TypeTable::struct_rendered_name`].
        type_args: Vec<TypeId>,
    },
    Enum {
        def: crate::defs::DefId,
    },
    /// Resource type - opaque handle (i32) to a Component Model resource
    Resource {
        def: crate::defs::DefId,
    },
    Variant {
        def: crate::defs::DefId,
    },
    // `Option<T>` is a `GenericInstance`, not a variant here — see
    // `TypeTable::as_option`. `Future<T>` / `Stream<T>` and their writable twins
    // are `GenericResource`, built by `make_future` / `make_future_writable`.
    //
    // TODO: represent `Option<T>` as `ref null T` when `T` is a non-nullable
    // reference, dropping the discriminant struct. `Option<Option<T>>` must not
    // take that route — its null would be ambiguous.
    /// Generic resource instantiation (e.g., `Future<i32>`, `Stream<String>`).
    /// Represents opaque i32 handles to Component Model resources with type parameters.
    GenericResource {
        def: crate::defs::DefId,
        type_args: Vec<TypeId>,
    },
    Ref(TypeId),
    MutRef(TypeId),
    Function {
        /// `true` for `fn mut(...)` (closure type that may mutate its captures).
        /// `false` for `fn(...)` (read-only).
        is_mut: bool,
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<EffectRef>,
        /// Positional indices of parameters the function may store.
        stores: Vec<u32>,
    },
    Reactive(TypeId),
    /// Type parameter (e.g., `T` in `struct Box<T>`) — a *rigid* variable.
    ///
    /// It stands for whatever a caller instantiates the binding item with, so
    /// inside that item it is opaque: nothing but itself is assignable to it.
    /// It appears only within the item that binds it; a *use* of a polymorphic
    /// signature instantiates these into [`ResolvedType::InferVar`]s.
    TypeParam {
        name: String,
        /// Index of the type parameter in the generic definition (0 for first param)
        index: u32,
    },
    /// Inference variable — a *flexible* variable standing for a type the
    /// solver has yet to determine. Minted when a use site instantiates a
    /// polymorphic signature: where a rigid parameter rejects anything but
    /// itself, a variable accepts and records.
    ///
    /// Reaches no recorded fact — `finalize_infer_holes` substitutes every
    /// one away. The intermediate types built on one stay interned, as every
    /// type ever considered does, so a pass enumerating
    /// [`TypeTable::all_types`] must select with [`TypeTable::is_concrete`].
    InferVar(InferVarId),
    /// Type pack parameter (`..T` in `fn foo<..T>(x: [..T])`), living inside
    /// tuples until substitution expands it. `mapped_elem` separates an identity
    /// pack (`None` — element `i` is `F_i`) from a mapped one (`Some(R)` —
    /// element `i` is `R[F := F_i]`, degenerating to `|F|` copies of `R` when
    /// `R` does not mention the pack).
    TypePack {
        name: String,
        index: u32,
        mapped_elem: Option<TypeId>,
    },
    /// Generic struct instantiation (e.g., `Box<i32>`)
    /// Used to track instantiation sites before monomorphization
    GenericInstance {
        /// The generic declaration this instantiates.
        def: crate::defs::DefId,
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
        /// The trait declaring `assoc_name`: `Self::Err` inside
        /// `trait FromStr` is `<Self as FromStr>::Err`. An identity, so a
        /// projection built under one module's `FromStr` cannot be answered by
        /// another's. `None` where the builder had no trait, which makes
        /// resolution require the name to be unambiguous.
        owning_trait: Option<crate::defs::DefId>,
        /// Trait bounds on this associated type, named by the declarations the
        /// trait's own `type A: Bound` references resolve to. A projection
        /// outlives the frame that built it, so a spelling here would be read
        /// back from a vantage that never wrote it.
        bounds: Vec<crate::name::FqTraitName>,
        /// Resolved associated type bindings (e.g., [("Item", `u8_typeid`)] for `I::Iter`
        /// when I: `IntoIterator`<Item = u8> and `IntoIterator::Iter`: Iterator<Item = `Self::Item`>)
        assoc_type_bindings: Vec<(String, TypeId)>,
    },
    /// Raw GC array intrinsic (`Array<T>`)
    /// This is the underlying storage type for `String` and `List<T>` structs
    BuiltinArray(TypeId),
    /// Newtype: a distinct type wrapping a base type with the same representation.
    /// Created by `type T = U;` declarations.
    /// Newtypes are distinct from their base types but can be cast between them.
    Newtype {
        def: crate::defs::DefId,
        /// What this instantiation was made with; empty for a declaration
        /// written as such. The same head/arguments split `Struct` has — the
        /// stored name used to bake them into the head (`MyArray<i32>`), which
        /// is a fused spelling no `impl` header writes.
        type_args: Vec<TypeId>,
        /// The direct base type (may be another newtype for chained newtypes)
        base_type: TypeId,
    },
    /// Flags: a bitmask type over u32.
    /// Created by `flags F { A, B, C }` declarations.
    /// Distinct from Newtype so flags can be detected without name-based lookup.
    Flags {
        def: crate::defs::DefId,
    },
    Unknown,
    Error,
}

/// A dense map from [`TypeId`] to `V`, backed by a `Vec` indexed by `TypeId.0`.
/// `TypeId`s are dense and sequential, so every access is a hash-free array
/// index — [`TypeTable::get`] is the compiler's hottest accessor. The newtype
/// keeps the lone `TypeId`→`usize` conversion in one place. Erased entries are
/// `None`: [`TypeMap::retain`] punches holes rather than renumbering.
#[derive(Debug, Clone)]
pub(crate) struct TypeMap<V> {
    slots: Vec<Option<V>>,
}

impl<V> Default for TypeMap<V> {
    fn default() -> Self {
        Self { slots: Vec::new() }
    }
}

impl<V> TypeMap<V> {
    /// The `TypeId` the next [`Self::push`] will occupy.
    pub(crate) fn next_id(&self) -> TypeId {
        TypeId(self.slots.len() as u32)
    }

    /// Live value at `id`, or `None` if absent, erased, or out of range.
    pub(crate) fn get(&self, id: TypeId) -> Option<&V> {
        self.slots.get(id.0 as usize).and_then(Option::as_ref)
    }

    /// Append a value at the next dense `TypeId` (== [`Self::next_id`]).
    pub(crate) fn push(&mut self, value: V) {
        self.slots.push(Some(value));
    }

    /// Set `id`'s value in place; `id` must already be in range.
    pub(crate) fn replace(&mut self, id: TypeId, value: V) {
        self.slots[id.0 as usize] = Some(value);
    }

    /// Set `id`'s value, growing the backing storage with empty slots as
    /// needed. Used for sparse maps such as erasure redirects.
    pub(crate) fn set_growing(&mut self, id: TypeId, value: V) {
        let idx = id.0 as usize;
        if idx >= self.slots.len() {
            self.slots.resize_with(idx + 1, || None);
        }
        self.slots[idx] = Some(value);
    }

    /// Drop every live slot for which `keep(id, &value)` returns false.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(TypeId, &V) -> bool) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if let Some(value) = slot
                && !keep(TypeId(i as u32), value)
            {
                *slot = None;
            }
        }
    }

    /// Iterate live `(TypeId, &value)` pairs, skipping erased holes.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (TypeId, &V)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|v| (TypeId(i as u32), v)))
    }

    /// Iterate the `TypeId`s of every live slot.
    pub(crate) fn ids(&self) -> impl Iterator<Item = TypeId> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| slot.as_ref().map(|_| TypeId(i as u32)))
    }
}

/// A dense set of [`TypeId`], backed by a bitset — the set-shaped companion to
/// [`TypeMap`]. One bit per type, so a transient "visited" set over the type
/// graph costs a small `Vec<u64>` instead of a hash set that reallocates as it
/// grows. Iteration yields ascending `TypeId` order, *not* insertion order.
#[derive(Debug, Clone, Default)]
pub(crate) struct TypeSet {
    words: Vec<u64>,
}

impl TypeSet {
    fn slot(id: TypeId) -> (usize, u64) {
        ((id.0 / 64) as usize, 1u64 << (id.0 % 64))
    }

    /// Insert `id`, returning `true` if it was not already present.
    pub(crate) fn insert(&mut self, id: TypeId) -> bool {
        let (word, mask) = Self::slot(id);
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let newly = self.words[word] & mask == 0;
        self.words[word] |= mask;
        newly
    }

    pub(crate) fn contains(&self, id: TypeId) -> bool {
        let (word, mask) = Self::slot(id);
        self.words.get(word).is_some_and(|w| w & mask != 0)
    }
}

#[derive(Debug, Clone)]
pub struct TypeTable {
    /// `TypeId` → `ResolvedType`. See [`TypeMap`]; `get` reads this on
    /// essentially every type query, so it is a hash-free `Vec` index.
    types: TypeMap<ResolvedType>,
    intern_map: IndexMap<ResolvedType, TypeId>,
    /// Registry of stdlib items the compiler is allowed to reference
    /// (Box, Option, Default, `push_str`, …). Populated during the
    /// annotate pass from `#[compiler_item("...")]` attributes; see
    /// [`crate::compiler_item`].
    compiler_items: crate::compiler_item::CompilerItems,
    /// Associated type resolutions:
    /// `(concrete_type_id, declaring trait, assoc_name)` → `resolved_type_id`.
    /// Populated when impl blocks with associated type bindings are processed.
    ///
    /// Keyed by trait because one type may implement several declaring the
    /// same name — `f32` has both `FromStr::Err` and `LenientFromStr::Err`.
    assoc_type_resolutions: IndexMap<(TypeId, crate::defs::DefId, String), TypeId>,
    /// Generic associated type definitions:
    /// `(base decl, declaring trait, assoc_name)` → `TypeId`.
    /// The `TypeId` is typically a `TypeParam` that can be substituted using the
    /// `GenericInstance`'s `type_args`. Populated when processing generic impl blocks
    /// (e.g., `impl Iterator for ListIter<T> { type Item = T; }`).
    /// Used by the monomorphizer to resolve associated types for `GenericInstance` types.
    generic_assoc_type_defs: IndexMap<(crate::ast::AstId, crate::defs::DefId, String), TypeId>,
    /// Erasure redirects: set by `erase_newtypes_and_flags()`.
    /// After erasure, `get(id)` for any erased `TypeId` returns the base type.
    /// Newtype → ultimate base type; Flags → u32.
    ///
    /// A sparse [`TypeMap`] (most types are not erased): `Some(target)` is a
    /// live redirect, an absent slot means "no redirect". `get` consults it
    /// on every call, so the hash-free index matters here too.
    redirects: TypeMap<TypeId>,
    /// Reverse mapping `Box<T>` `TypeId` → `T`'s `TypeId`, populated by the
    /// boxing pass (`lower/plan/boxing.rs`). The pass rewrites `&T` /
    /// `&mut T` into `Box<T>` wrapper structs for primitives, variants and
    /// function types. Sites that read receiver / argument types in
    /// post-boxing IR (DCE inspect scanning, dispatch synthesis, etc.) use
    /// [`Self::peel_refs_and_box`] to look through both the reference
    /// layer and any boxing wrapper in a single step.
    ///
    /// A sparse [`TypeMap`] keyed by the wrapper `TypeId`.
    box_payload_types: TypeMap<TypeId>,
    /// The wrapper `TypeId`s the boxing pass redefined from a *shared* `&T`.
    /// `&T` and `&mut T` get the same `Box<T>` content, so this is the only
    /// surviving record of which boxed handles cannot be written through — see
    /// [`Self::is_mut_box`].
    shared_box_type_ids: TypeSet,
    /// Index from (struct name, module source) to `TypeId` for O(1) lookup.
    /// Populated incrementally when Struct types are interned.
    struct_name_index: IndexMap<(String, ModuleSource), TypeId>,
    /// `(name, module) -> TypeId` for the nominal declarations that are not
    /// structs. `find_decl_type_by_name` scanned every interned type for these,
    /// which an instantiation now pays on the way in — see
    /// `make_generic_instance`.
    decl_name_index: IndexMap<(String, ModuleSource), TypeId>,
    /// Canonical map: declared-type symbol → `TypeId`, populated whenever the
    /// elaborator mints a decl-backed type, so an LSP-style query can go from an
    /// [`AstId`](crate::ast::AstId) to its type without searching by name.
    /// Monomorphized instances are not entered — the base generic's key still
    /// resolves to the base id; `symbol_of_type` walks the other way.
    type_by_symbol: IndexMap<crate::ast::AstId, TypeId>,
    /// Inverse of `type_by_symbol` plus monomorphization tracking: every
    /// decl-backed `TypeId` — including monomorphized instances —
    /// maps to the [`AstId`](crate::ast::AstId) of its declaring AST node.
    ///
    /// A sparse [`TypeMap`] keyed by the decl-backed `TypeId`.
    symbol_by_type: TypeMap<crate::ast::AstId>,
    /// `(receiver head, module, trait)` triples that satisfied a `Serialize` /
    /// `Deserialize` / `Eq` / `Ord` bound structurally during elaboration
    /// (bound-driven synthesis, WEP 2026-06-25). Keyed by the receiver's head
    /// rather than by `TypeId`, so a generic records once against its
    /// declaration. Lives on the shared `TypeTable` because elaboration runs one
    /// `Elaborator` per module.
    bound_driven_synth_requests:
        IndexSet<(crate::name::TypeHead, ModuleSource, crate::defs::DefId)>,
    /// Variant case templates: `(variant name, module)` → `(case name, case
    /// index, payload TypeId)`. Payload ids are in the declaring template's
    /// terms; unit cases use `TypeTable::UNIT`.
    variant_case_index: IndexMap<crate::defs::DefId, Vec<(String, u32, TypeId)>>,
    /// Every struct shape the compiler minted, by [`AnonStructId`].
    anon_structs: Vec<(ModuleSource, AnonShape)>,
    /// Dedup for the above: the same shape in the same module is one id.
    anon_struct_index: IndexMap<(ModuleSource, AnonShape), AnonStructId>,
    /// `(WIT name, generated module)` of each type declaration, for
    /// [`Self::cm_decl_in`]. Built with [`Self::attach_defs`], so it answers at
    /// any point in the pipeline rather than only after a declaration's type is
    /// interned.
    decl_index: IndexMap<(String, ModuleSource), crate::defs::DefId>,
    /// Every declaration in the program, for rendering a nominal type's head.
    ///
    /// A name comes out of an identity and never goes back in. Attached where
    /// [`crate::resolve::Resolutions`] is built, and again on the snapshot
    /// restore path with the seeded table — which is what keeps a `DefId` a
    /// cached type carries pointing at the same declaration.
    defs: std::sync::Arc<crate::defs::DefTable>,
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

    /// The primitive spelling → well-known `TypeId` mapping, the single
    /// source for every by-name primitive resolution. `i128` / `u128` are
    /// deliberately absent: they are struct-backed in the prelude, so their
    /// names resolve through the declaration path, not to the raw
    /// `PrimitiveType` ids.
    pub fn primitive_by_name(name: &str) -> Option<TypeId> {
        match name {
            "i8" => Some(Self::I8),
            "i16" => Some(Self::I16),
            "i32" => Some(Self::I32),
            "i64" => Some(Self::I64),
            "u8" => Some(Self::U8),
            "u16" => Some(Self::U16),
            "u32" => Some(Self::U32),
            "u64" => Some(Self::U64),
            "f32" => Some(Self::F32),
            "f64" => Some(Self::F64),
            "bool" => Some(Self::BOOL),
            "char" => Some(Self::CHAR),
            "v128" => Some(Self::V128),
            "()" => Some(Self::UNIT),
            "!" => Some(Self::NEVER),
            _ => None,
        }
    }

    /// Reserved `GenericInstance` base name of the built-in tuple. Not a
    /// writable type name, so it can never collide with a user-defined
    /// `struct Tuple` — that is what makes the name-only [`Self::is_tuple_type`]
    /// check sound. User-facing spelling is `[T1, T2, …]`.
    pub const TUPLE_TYPE_NAME: &'static str = crate::name::TUPLE_TYPE_NAME;

    /// Canonical name for the unit type `()` used in method lookup and impl indexing.
    /// Must match what `format_type_name(TypeNameInfo::Unit)` returns, and matches
    /// the source-level syntax `()` so error messages and mangled names line up.
    pub const UNIT_TYPE_NAME: &'static str = "()";

    /// Canonical user-facing name of the raw GC array (`ResolvedType::BuiltinArray`).
    /// Single source of truth for both the resolver arms that recognise the
    /// `Array<T>` spelling and the dispatch arms that report it as the
    /// method-owner base name (`impl Array<T>` in `core:prelude/array.wado`),
    /// so those scattered sites cannot drift out of agreement.
    pub const ARRAY_TYPE_NAME: &'static str = "Array";

    /// The `(base name, struct type args)` a generic container (`GenericInstance`
    /// or the raw GC array `Array<T>`, whose methods live in `impl Array<T>`)
    /// dispatches under. A trait-method call site
    /// (`synthesis::template::method_name_for_type`) and the monomorphizer's
    /// `get_struct_info_from_type` must agree on this, or a call mangles to a
    /// name no impl was registered under. `None` for every other type (their
    /// dispatch name is derived differently).
    pub fn generic_dispatch_components(&self, type_id: TypeId) -> Option<(String, Vec<TypeId>)> {
        match self.get(type_id) {
            ResolvedType::GenericInstance { def, type_args } => {
                Some((self.def_name(*def).to_string(), type_args.clone()))
            }
            ResolvedType::BuiltinArray(elem) => {
                Some((Self::ARRAY_TYPE_NAME.to_string(), vec![*elem]))
            }
            _ => None,
        }
    }

    /// Whether a `GenericInstance` base name is the built-in tuple.
    pub fn is_tuple_type(name: &str) -> bool {
        name == Self::TUPLE_TYPE_NAME
    }

    pub fn new() -> Self {
        let mut table = Self {
            types: TypeMap::default(),
            intern_map: IndexMap::default(),
            compiler_items: crate::compiler_item::CompilerItems::new(),
            assoc_type_resolutions: IndexMap::default(),
            generic_assoc_type_defs: IndexMap::default(),
            redirects: TypeMap::default(),
            box_payload_types: TypeMap::default(),
            shared_box_type_ids: TypeSet::default(),
            struct_name_index: IndexMap::default(),
            decl_name_index: IndexMap::default(),
            type_by_symbol: IndexMap::default(),
            symbol_by_type: TypeMap::default(),
            bound_driven_synth_requests: IndexSet::default(),
            variant_case_index: IndexMap::default(),
            anon_structs: Vec::new(),
            anon_struct_index: IndexMap::default(),
            decl_index: IndexMap::default(),
            defs: std::sync::Arc::default(),
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
        let id = self.types.next_id();
        // Update struct name index for O(1) lookups by (name, module_source).
        // Keyed on the *rendered* spelling: every instantiation of a generic
        // struct is a distinct type, and keying on the declaration would
        // collapse them all onto one entry.
        if let ResolvedType::Struct {
            ref def,
            ref type_args,
        } = ty
        {
            let rendered = self.struct_rendered_name(*def, type_args);
            let module_source = self.struct_head_module(*def).clone();
            self.struct_name_index.insert((rendered, module_source), id);
        }
        if let Some(def) = Self::nominal_key(&ty) {
            let key = (self.def_name(def).to_string(), self.def_module(def).clone());
            self.decl_name_index.insert(key, id);
        }
        self.types.push(ty.clone());
        self.intern_map.insert(ty, id);
        id
    }

    /// Mint a brand-new `TypeId` for `ty`, bypassing `intern`'s structural dedup.
    /// `intern_map` / `struct_name_index` are deliberately not updated — keyed by
    /// `(name, module_source)`, a second same-named entry would overwrite the
    /// first. This is what a local type declaration mints
    /// through: its identity is the caller's `AstId`, not this type's name.
    pub fn push_fresh(&mut self, ty: ResolvedType) -> TypeId {
        let id = self.types.next_id();
        self.types.push(ty);
        id
    }

    pub fn get(&self, id: TypeId) -> &ResolvedType {
        let id = self.redirects.get(id).copied().unwrap_or(id);
        self.types
            .get(id)
            .unwrap_or_else(|| panic!("TypeId {id:?} not found in TypeTable"))
    }

    /// [`Self::get`] before the newtype / flags erasure applied ahead of
    /// monomorphize.
    ///
    /// Erasure is a representation choice — a `flags` value is a `u32` at
    /// runtime — but `impl Trait for Perms` is still keyed under `Perms`. A
    /// name that has to match an impl must read the identity; only code that
    /// cares how the value is stored should read [`Self::get`].
    #[must_use]
    pub fn get_unerased(&self, id: TypeId) -> &ResolvedType {
        self.types
            .get(id)
            .unwrap_or_else(|| panic!("TypeId {id:?} not found in TypeTable"))
    }

    /// [`Self::get`] returning `None` for ids pruned by DCE's `retain`.
    pub fn get_pruned(&self, id: TypeId) -> Option<&ResolvedType> {
        let id = self.redirects.get(id).copied().unwrap_or(id);
        self.types.get(id)
    }

    /// True when `id` resolves to the never type `!`. An expression of this type
    /// diverges and never yields a value (`panic`, `unreachable`, …).
    pub fn is_never(&self, id: TypeId) -> bool {
        matches!(self.get(id), ResolvedType::Never)
    }

    /// Iterate over all live types in the type table. Erased slots (`None`,
    /// produced by [`Self::retain`]) are skipped.
    pub fn all_types(&self) -> impl Iterator<Item = (TypeId, &ResolvedType)> {
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

    /// Iterate over all live type IDs in the table. Erased slots are skipped.
    pub fn iter_type_ids(&self) -> impl Iterator<Item = TypeId> + '_ {
        self.types.ids()
    }

    /// Look up a struct `TypeId` by its name and module source.
    /// Returns `None` if no struct with that name exists in the given module.
    pub fn find_struct_by_name(&self, name: &str, module_source: &ModuleSource) -> Option<TypeId> {
        self.struct_name_index
            .get(&(name.to_string(), module_source.clone()))
            .copied()
    }

    /// Register the `(AstId -> TypeId)` mapping for a declared type.
    ///
    /// Called by the elaborator right after constructing the `TypeId` that
    /// represents a user-declared type (struct, enum, variant, flags,
    /// newtype, resource). Both directions of the map are populated:
    /// forward `type_by_symbol[key] = type_id` and inverse
    /// `symbol_by_type[type_id] = key`.
    /// Attach the program's declarations, so a nominal type can render its
    /// head once it carries one instead of a spelling.
    pub fn attach_defs(&mut self, defs: std::sync::Arc<crate::defs::DefTable>) {
        // A module-level declaration is entered first and kept: a
        // function-local item shares its module, and a spelling that reaches
        // both means the module-level one everywhere this index is consulted.
        self.decl_index = IndexMap::default();
        for def in defs.iter() {
            if !defs.kind(def).is_type() {
                continue;
            }
            self.decl_index
                .entry((defs.name(def).to_string(), defs.module(def).clone()))
                .or_insert(def);
        }
        self.defs = defs;
    }

    /// Every declaration in the program.
    #[must_use]
    pub fn defs(&self) -> &crate::defs::DefTable {
        &self.defs
    }

    /// The name `def` writes — a rendering, for a diagnostic or a mangle.
    #[must_use]
    pub fn def_name(&self, def: crate::defs::DefId) -> &str {
        self.defs.name(def)
    }

    /// The name `def` renders to — its declared name, with a function-local
    /// declaration's disambiguator applied.
    ///
    /// [`Self::def_name`] is the *declared* name, which is what an `impl`
    /// header spells and what a by-name declaration lookup keys on. This is
    /// the *rendered* one, which is what a mangle embeds and what every
    /// name-keyed downstream registry stores. They differ for exactly one kind
    /// of declaration: two sibling functions may each declare a `struct
    /// Point`, and a registry keyed on the declared name would hold one entry
    /// for two types.
    #[must_use]
    pub fn decl_render_name(&self, def: crate::defs::DefId) -> String {
        let name = self.defs.name(def);
        if self.defs.is_function_local(def) {
            return crate::name::mangle_local_item_name(name, self.defs.ast_id(def));
        }
        name.to_string()
    }

    /// The module that declares `def`.
    #[must_use]
    pub fn def_module(&self, def: crate::defs::DefId) -> &ModuleSource {
        self.defs.module(def)
    }

    /// Intern an anonymous struct's shape. The fields are the identity, so two
    /// literals writing the same shape in the same module reach one id.
    pub fn intern_anon_struct(
        &mut self,
        module_source: ModuleSource,
        fields: Vec<(String, TypeId)>,
    ) -> AnonStructId {
        self.intern_shape(module_source, AnonShape::Fields(fields))
    }

    /// A struct the compiler mints under a name it assigns — a closure
    /// environment, which names no declaration but is not identified by its
    /// captures either.
    pub fn intern_synthetic_struct(
        &mut self,
        module_source: ModuleSource,
        name: String,
    ) -> AnonStructId {
        self.intern_shape(module_source, AnonShape::Synthetic(name))
    }

    fn intern_shape(&mut self, module_source: ModuleSource, shape: AnonShape) -> AnonStructId {
        let key = (module_source, shape);
        if let Some(&id) = self.anon_struct_index.get(&key) {
            return id;
        }
        let id = AnonStructId(u32::try_from(self.anon_structs.len()).expect("anon struct space"));
        self.anon_structs.push(key.clone());
        self.anon_struct_index.insert(key, id);
        id
    }

    /// The fields of an anonymous struct shape; empty for a synthetic one.
    #[must_use]
    pub fn anon_struct_fields(&self, id: AnonStructId) -> &[(String, TypeId)] {
        match &self.anon_structs[id.0 as usize].1 {
            AnonShape::Fields(fields) => fields,
            AnonShape::Synthetic(_) => &[],
        }
    }

    /// The module the shape was written in.
    #[must_use]
    pub fn anon_struct_module(&self, id: AnonStructId) -> &ModuleSource {
        &self.anon_structs[id.0 as usize].0
    }

    /// The spelling an anonymous struct renders to, derived from its fields —
    /// the same `__anon_{x:i32,y:i32}` form the synthesized name used to be.
    #[must_use]
    pub fn anon_struct_name(&self, id: AnonStructId) -> String {
        match &self.anon_structs[id.0 as usize].1 {
            AnonShape::Synthetic(name) => name.clone(),
            AnonShape::Fields(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(n, ty)| format!("{n}:{}", self.type_name(*ty)))
                    .collect();
                format!("__anon_{{{}}}", parts.join(","))
            }
        }
    }

    /// The name a struct head renders to: the declaration's, or the shape's.
    #[must_use]
    pub fn struct_head_name(&self, head: StructDef) -> String {
        match head {
            StructDef::Decl(def) => self.decl_render_name(def),
            StructDef::Anon(id) => self.anon_struct_name(id),
        }
    }

    /// A struct head as a mangled name embeds it: the declaration when it names
    /// one, the interned shape otherwise.
    #[must_use]
    pub fn fq_struct_head(&self, head: StructDef) -> crate::name::FqTypeName {
        match head {
            StructDef::Decl(def) => crate::name::FqTypeName::declared(&self.defs, def),
            StructDef::Anon(id) => crate::name::FqTypeName::shape(
                self.anon_struct_module(id),
                &self.anon_struct_name(id),
            ),
        }
    }

    /// The head as source spells it: the declared name, with no storage
    /// disambiguator. [`Self::struct_head_name`]'s counterpart in the
    /// declaration namespace — what a diagnostic shows, never a lookup key.
    #[must_use]
    pub fn struct_head_decl_name(&self, head: StructDef) -> String {
        match head {
            StructDef::Decl(def) => self.def_name(def).to_string(),
            StructDef::Anon(id) => self.anon_struct_name(id),
        }
    }

    /// The module a struct head belongs to.
    #[must_use]
    pub fn struct_head_module(&self, head: StructDef) -> &ModuleSource {
        match head {
            StructDef::Decl(def) => self.def_module(def),
            StructDef::Anon(id) => self.anon_struct_module(id),
        }
    }

    /// The declaration a compiler item names.
    ///
    /// The registry records the declaring node for every kind that names a
    /// type of its own, so this answers at any point in the pipeline and for
    /// any stdlib type the compiler knows — no site has to spell one.
    #[must_use]
    pub fn compiler_item_def(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> Option<crate::defs::DefId> {
        self.compiler_items
            .decl(item)
            .and_then(|ast| self.defs.of_ast_id(ast))
    }

    /// Like [`Self::compiler_item_def`], but ICEs rather than answering `None`
    /// — for the items the compiler requires to be registered.
    #[must_use]
    pub fn require_compiler_item_def(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> crate::defs::DefId {
        self.compiler_item_def(item)
            .unwrap_or_else(|| panic!("compiler item `{item}` names no declaration"))
    }

    /// The declaration a nominal type was written from, if it names one.
    ///
    /// This is identity: compare these, never the names below.
    #[must_use]
    pub fn nominal_def(&self, id: TypeId) -> Option<crate::defs::DefId> {
        match self.get(id) {
            ResolvedType::Struct { def, .. } => def.decl(),
            ResolvedType::Enum { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Flags { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::GenericInstance { def, .. }
            | ResolvedType::GenericResource { def, .. } => Some(*def),
            _ => None,
        }
    }

    /// What a nominal type's head renders to — for a diagnostic, a mangle, or
    /// a consumer still keyed on the pair. A rendering out of an identity, and
    /// never a way back to one.
    #[must_use]
    pub fn nominal_head(&self, id: TypeId) -> Option<(String, ModuleSource)> {
        match self.get(id) {
            ResolvedType::Struct { def, .. } => Some((
                self.struct_head_name(*def),
                self.struct_head_module(*def).clone(),
            )),
            ResolvedType::Enum { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Flags { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::GenericInstance { def, .. }
            | ResolvedType::GenericResource { def, .. } => {
                Some((self.decl_render_name(*def), self.def_module(*def).clone()))
            }
            _ => None,
        }
    }

    pub fn register_decl_type(&mut self, key: crate::ast::AstId, type_id: TypeId) {
        self.type_by_symbol.insert(key, type_id);
        self.symbol_by_type.set_growing(type_id, key);
    }

    /// Register a monomorphized `TypeId` as pointing at its generic base symbol.
    ///
    /// Monomorphized types do not have their own `AstId` — they are synthesized
    /// from a generic declaration. Registering `(mono_type_id -> base_key)` on
    /// `symbol_by_type` lets LSP queries walk any decl-backed `TypeId` back to
    /// the declaring AST node. The forward `type_by_symbol` index is NOT
    /// updated: that keeps the base generic's `TypeId` as the canonical entry.
    pub fn register_mono_type(&mut self, base_key: crate::ast::AstId, type_id: TypeId) {
        self.symbol_by_type.set_growing(type_id, base_key);
    }

    /// Canonical `TypeId` for a declared-type [`AstId`](crate::ast::AstId).
    ///
    /// Returns `None` if the symbol is not a decl-backed type, or if the
    /// elaborator has not yet created a `TypeId` for it.
    pub fn type_of_symbol(&self, key: &crate::ast::AstId) -> Option<TypeId> {
        self.type_by_symbol.get(key).copied()
    }

    /// Canonical `TypeId` for a declared-type [`AstId`](crate::ast::AstId).
    /// Prefer this over re-deriving one from `(name, module_source)`: the
    /// `AstId` is a cheap `Copy` key, and it stays unique even where name+module
    /// does not — a local type can share another's name.
    ///
    /// Panics if `collect_types` has not run for this declaration; both that and
    /// a non-decl-backed `AstId` are compiler bugs, not recoverable conditions.
    pub fn type_id_of_decl(&self, key: crate::ast::AstId) -> TypeId {
        self.type_of_symbol(&key).unwrap_or_else(|| {
            panic!(
                "type_id_of_decl: no TypeId registered for {key:?} — \
                 collect_types must run before type resolution queries it"
            )
        })
    }

    /// Walk a decl-backed `TypeId` (including monomorphizations) back to the
    /// declaring [`AstId`](crate::ast::AstId).
    pub fn symbol_of_type(&self, type_id: TypeId) -> Option<&crate::ast::AstId> {
        self.symbol_by_type.get(type_id)
    }

    /// The declaring [`AstId`](crate::ast::AstId) behind `type_id`, whether it
    /// is a plain declaration, a monomorphization, or a `GenericInstance`.
    ///
    /// `Node<i32>` and `Node<String>` answer with the one `Node` they were
    /// spelled from; a `Node` in another module answers with a different id.
    pub fn decl_of_type(&self, type_id: TypeId) -> Option<crate::ast::AstId> {
        let type_id = self.peel_refs(type_id);
        if let Some(key) = self.symbol_by_type.get(type_id) {
            return Some(*key);
        }
        // An instantiation records the declaration it came from, so the
        // answer is read off the type rather than re-derived from a spelling
        // whose base may already have been pruned.
        let ResolvedType::GenericInstance { def, .. } = self.get(type_id) else {
            return None;
        };
        Some(self.defs.ast_id(*def))
    }

    /// The declaring [`AstId`](crate::ast::AstId) of the type named `name` in
    /// `module_source`.
    pub fn decl_by_name(
        &self,
        name: &str,
        module_source: &ModuleSource,
    ) -> Option<crate::ast::AstId> {
        let type_id = self.find_decl_type_by_name(name, module_source)?;
        self.symbol_by_type.get(type_id).copied()
    }

    /// Whether `decl` is one of the four reflection member handles, whose own
    /// `Members` would mention `StructField<Self, …>` and grow `Self` without
    /// bound (WEP 2026-06-13).
    ///
    /// Matched by declaration, so a user type spelled `StructField` stays
    /// reflectable.
    pub fn is_sealed_reflect_member(&self, decl: crate::ast::AstId) -> bool {
        use crate::compiler_item::CompilerItem;
        [
            CompilerItem::ReflectStructField,
            CompilerItem::ReflectVariantCase,
            CompilerItem::ReflectEnumCase,
            CompilerItem::ReflectFlagsBit,
        ]
        .into_iter()
        .filter_map(|item| self.compiler_items().struct_owned_opt(item))
        .filter_map(|(module_source, name)| self.find_struct_by_name(&name, &module_source))
        .any(|sealed| self.symbol_by_type.get(sealed) == Some(&decl))
    }

    /// The `TypeId` of `Array<element>`, when the program already has one.
    /// Lookup only: a reader holds the table by shared reference and cannot
    /// mint a type, and an array a constant describes is one the program
    /// declared.
    #[must_use]
    pub fn find_builtin_array(&self, element: TypeId) -> Option<TypeId> {
        self.intern_map
            .get(&ResolvedType::BuiltinArray(element))
            .copied()
    }

    /// Whether `type_id` is the `String` / `List` container the lower phase
    /// writes as `{ repr, used }`. Shape does not identify one: a struct over
    /// an array and an `i32` looks the same and means something else, so the
    /// answer comes from the registered item rather than the field positions.
    ///
    /// By declared name, as everywhere else the compiler asks this — a
    /// monomorphized instance carries the instantiation's module, not the one
    /// the generic was declared in.
    #[must_use]
    pub fn is_seq_container(&self, type_id: TypeId) -> bool {
        self.is_string(type_id) || self.is_list(type_id)
    }

    /// Find the `TypeId` of a user-declared type (struct, enum, variant, flags,
    /// newtype, resource) by its source-level name and owning module. Returns
    /// only non-monomorphized declarations — monomorphized generic instances
    /// are skipped because they do not correspond to an `AstId` of their own.
    pub fn find_decl_type_by_name(
        &self,
        name: &str,
        module_source: &ModuleSource,
    ) -> Option<TypeId> {
        if let Some(id) = self.find_struct_by_name(name, module_source) {
            return Some(id);
        }
        self.decl_name_index
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
        for id in self.iter_type_ids() {
            if let ResolvedType::GenericInstance { def, .. } = self.get(id)
                && self.def_name(*def) == name
            {
                return Some(self.def_module(*def).clone());
            }
        }
        None
    }

    /// Remove all type entries whose `TypeId` is not in `keep`. Erased entries
    /// become `None` holes rather than being renumbered away, so surviving ids
    /// keep their indices, and the intern map and secondary indices are rebuilt.
    /// `get(id)` must not panic for a surviving id, so `keep` is implicitly
    /// closed under `redirects` and stale redirect entries are dropped.
    pub fn retain(&mut self, keep: &IndexSet<TypeId>) {
        // Implicit closure under `redirects`: every kept id whose `get`
        // result lives at a different id must keep that target alive too.
        let mut effective_keep: IndexSet<TypeId> = keep.clone();
        let mut queue: Vec<TypeId> = Vec::new();
        let keep_id = |set: &mut IndexSet<TypeId>, queue: &mut Vec<TypeId>, id: TypeId| {
            if set.insert(id) {
                queue.push(id);
            }
        };
        for &id in keep {
            if let Some(&target) = self.redirects.get(id) {
                keep_id(&mut effective_keep, &mut queue, target);
            }
            queue.push(id);
        }
        // A surviving struct spells itself with its instantiation arguments,
        // so those must resolve too. The reachability walk reaches a type
        // through its erased view and never through the pre-erasure arguments
        // a monomorphized struct records, so nothing else keeps them alive.
        while let Some(id) = queue.pop() {
            let Some(ResolvedType::Struct { type_args, .. }) = self.types.get(id) else {
                continue;
            };
            for arg in type_args.clone() {
                keep_id(&mut effective_keep, &mut queue, arg);
                if let Some(&target) = self.redirects.get(arg) {
                    keep_id(&mut effective_keep, &mut queue, target);
                }
            }
        }

        // Punch holes for dropped ids; `TypeId`s are never renumbered, so
        // surviving entries keep their indices.
        self.types.retain(|id, _| effective_keep.contains(&id));
        // A redirect entry is meaningful only when both endpoints survive.
        self.redirects
            .retain(|id, &target| effective_keep.contains(&id) && effective_keep.contains(&target));
        // Retain symbol indices to surviving TypeIds only.
        self.symbol_by_type
            .retain(|id, _| effective_keep.contains(&id));
        self.type_by_symbol
            .retain(|_, id| effective_keep.contains(id));
        // Rebuild intern map from the surviving entries.
        self.intern_map.clear();
        self.struct_name_index.clear();
        self.decl_name_index.clear();
        for (id, ty) in self.types.iter() {
            self.intern_map.insert(ty.clone(), id);
            if let Some(def) = Self::nominal_key(ty) {
                let key = (self.def_name(def).to_string(), self.def_module(def).clone());
                self.decl_name_index.insert(key, id);
            }
        }
        // Structs index under the spelling they render to, the way `intern`
        // enters them — `Box` for the declaration, `Box<i32>` for that
        // instantiation. Keying the rebuild on `decl_name` alone would put
        // every instantiation of `Box` on one entry, and whichever survived
        // last would answer for the declaration and for its siblings.
        // Rendered up front because deriving one reads the arguments' types.
        let rendered: Vec<((String, ModuleSource), TypeId)> = self
            .types
            .iter()
            .filter_map(|(id, ty)| match ty {
                ResolvedType::Struct { def, type_args } => Some((
                    (
                        self.struct_rendered_name(*def, type_args),
                        self.struct_head_module(*def).clone(),
                    ),
                    id,
                )),
                _ => None,
            })
            .collect();
        self.struct_name_index.extend(rendered);
    }

    /// The declaration a non-struct nominal type interns under.
    fn nominal_key(ty: &ResolvedType) -> Option<crate::defs::DefId> {
        match ty {
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Flags { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Newtype { def, .. } => Some(*def),
            _ => None,
        }
    }

    /// Create a raw GC array type (`Array<T>`)
    pub fn make_builtin_array(&mut self, element: TypeId) -> TypeId {
        self.intern(ResolvedType::BuiltinArray(element))
    }

    /// Access the registry of compiler-recognised stdlib items.
    pub fn compiler_items(&self) -> &crate::compiler_item::CompilerItems {
        &self.compiler_items
    }

    /// Mutable handle on the registry. Used by the elaborator during the
    /// annotate pass to register each `#[compiler_item("...")]`
    /// declaration.
    pub fn compiler_items_mut(&mut self) -> &mut crate::compiler_item::CompilerItems {
        &mut self.compiler_items
    }

    /// Record that the receiver `head` (declared in `module_source`) satisfied a
    /// `T: <trait>` bound structurally (bound-driven synthesis). A no-op if
    /// already recorded for this triple — the same type is typically
    /// rediscovered from many call sites, so the pre-check avoids cloning the
    /// key each time.
    ///
    /// The head is the identity synthesis compares: a declaration by its
    /// [`crate::defs::DefId`], a shape no declaration names by its rendering.
    pub fn record_bound_driven_synth_request(
        &mut self,
        head: &crate::name::TypeHead,
        module_source: &ModuleSource,
        trait_key: &crate::defs::DefId,
    ) {
        let already_recorded = self
            .bound_driven_synth_requests
            .iter()
            .any(|(h, m, t)| h == head && m == module_source && t == trait_key);
        if !already_recorded {
            self.bound_driven_synth_requests.insert((
                head.clone(),
                module_source.clone(),
                *trait_key,
            ));
        }
    }

    /// [`Self::record_bound_driven_synth_request`] for a receiver held as a
    /// type: the head comes off the type itself, so it is the same head
    /// synthesis builds for that receiver.
    pub fn record_bound_driven_synth_request_for(
        &mut self,
        receiver: TypeId,
        module_source: &ModuleSource,
        trait_key: &crate::defs::DefId,
    ) {
        let head = self.fq_base_type_name(receiver).head().clone();
        self.record_bound_driven_synth_request(&head, module_source, trait_key);
    }

    /// Requests recorded by [`Self::record_bound_driven_synth_request`] so
    /// far whose trait name satisfies `matches`. A snapshot, not a drain:
    /// `synthesize_serde` and `synthesize_traits` each read this same
    /// shared set and filter for the trait names they own, so consuming it
    /// here would drop whichever runs second. Filtering before cloning
    /// means each caller only clones the entries it keeps.
    pub fn bound_driven_synth_requests(
        &self,
        mut matches: impl FnMut(&crate::defs::DefId) -> bool,
    ) -> Vec<(crate::name::TypeHead, ModuleSource, crate::defs::DefId)> {
        self.bound_driven_synth_requests
            .iter()
            .filter(|(_, _, trait_key)| matches(trait_key))
            .cloned()
            .collect()
    }

    /// Canonical name of a registered struct / trait / variant / enum
    /// [`CompilerItem`](crate::compiler_item::CompilerItem), forwarded from the
    /// registry so call sites read `tt.compiler_struct_name(item)` instead of
    /// chaining through `compiler_items()`.
    pub fn compiler_struct_name(&self, item: crate::compiler_item::CompilerItem) -> &str {
        self.compiler_items.struct_name(item)
    }

    /// The fq name of a compiler-item struct: its declaring module plus its
    /// name, the form any name that embeds a receiver expects.
    pub fn compiler_struct_fq_name(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> crate::name::FqTypeName {
        let decl = self
            .compiler_items
            .struct_decl(item)
            .expect("a registered struct item records its declaring node");
        let def = self
            .defs
            .of_ast_id(decl)
            .expect("a compiler item's declaring node is a declaration");
        crate::name::FqTypeName::declared(&self.defs, def)
    }

    pub fn compiler_trait_name(&self, item: crate::compiler_item::CompilerItem) -> &str {
        self.compiler_items.trait_name(item)
    }

    /// The compiler trait item as a mangled method name embeds it — named by
    /// the module that declares it.
    #[must_use]
    pub fn compiler_trait_fq(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> crate::name::FqTraitName {
        self.compiler_items.trait_fq(item)
    }

    pub fn compiler_variant_name(&self, item: crate::compiler_item::CompilerItem) -> &str {
        self.compiler_items.variant_name(item)
    }

    /// Whether `id` resolves to an instance of the compiler `Result` variant.
    ///
    /// Compares declarations. The spelling alone answered yes for any module's
    /// `Result`, which is the mis-identification this table exists to prevent.
    pub fn is_result(&self, id: TypeId) -> bool {
        let Some(decl) = self
            .compiler_items
            .variant_decl(crate::compiler_item::CompilerItem::Result)
        else {
            return false;
        };
        self.decl_of_type(id) == Some(decl)
    }

    /// Whether `id` is an instance of the compiler's `List` struct.
    pub fn is_list(&self, id: TypeId) -> bool {
        let Some(decl) = self
            .compiler_items
            .struct_decl(crate::compiler_item::CompilerItem::List)
        else {
            return false;
        };
        self.decl_of_type(id) == Some(decl)
    }

    /// Whether `id` is the compiler's `String` struct.
    ///
    /// Compares declarations; `name == "String"` answered yes for any module's
    /// own `String`.
    pub fn is_string(&self, id: TypeId) -> bool {
        let Some(decl) = self
            .compiler_items
            .struct_decl(crate::compiler_item::CompilerItem::String)
        else {
            return false;
        };
        self.decl_of_type(id) == Some(decl)
    }

    pub fn compiler_enum_name(&self, item: crate::compiler_item::CompilerItem) -> &str {
        self.compiler_items.enum_name(item)
    }

    /// Owned `(module, name)` for a registered struct / enum item — forwards
    /// the registry's `CompilerItems::struct_owned` so single-expression
    /// callers query the table directly instead of through `compiler_items()`.
    pub fn compiler_struct_owned(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> (ModuleSource, String) {
        self.compiler_items.struct_owned(item)
    }

    pub fn compiler_enum_owned(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> (ModuleSource, String) {
        self.compiler_items.enum_owned(item)
    }

    /// Module source of a registered struct item, if present.
    pub fn compiler_struct_module(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> Option<&ModuleSource> {
        self.compiler_items.struct_module(item)
    }

    /// Case name of a registered variant-case item (e.g. `Option::Some`).
    pub fn compiler_variant_case_name(&self, item: crate::compiler_item::CompilerItem) -> &str {
        self.compiler_items.variant_case_name(item)
    }

    /// Module + variant name + case name + discriminant of a registered
    /// variant-case item.
    pub fn compiler_variant_case(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> (&ModuleSource, &str, &str, u32) {
        self.compiler_items.require_variant_case(item)
    }

    /// Module + owner-type name + method name of a registered method item.
    pub fn compiler_method(
        &self,
        item: crate::compiler_item::CompilerItem,
    ) -> (&ModuleSource, &str, &str) {
        self.compiler_items.require_method(item)
    }

    /// Get the module source where the `Default` trait is defined, if
    /// the stdlib has registered it. Thin wrapper around
    /// `CompilerItems::trait_module`.
    pub fn default_trait_module_source(&self) -> Option<&ModuleSource> {
        self.compiler_items
            .trait_module(crate::compiler_item::CompilerItem::Default)
    }

    /// Make the struct type for a registered `CompilerItem` variant
    /// of kind `CompilerItemKind::Struct`. Reads both the module
    /// source and the struct name from the registry so the call site
    /// does not hard-code either. Panics with a clear ICE message when
    /// the item is not registered or has the wrong kind.
    pub fn make_compiler_struct(&mut self, item: crate::compiler_item::CompilerItem) -> TypeId {
        let decl = self
            .compiler_items
            .struct_decl(item)
            .and_then(|ast| self.defs.of_ast_id(ast))
            .unwrap_or_else(|| panic!("compiler item {item:?} is not a registered struct"));
        self.make_struct(StructDef::Decl(decl))
    }

    /// Make the enum type for a registered `CompilerItem` variant
    /// of kind `CompilerItemKind::Enum` (currently `Ordering`).
    /// Same shape as [`Self::make_compiler_struct`]: routes both name
    /// and module through the registry.
    pub fn make_compiler_enum(&mut self, item: crate::compiler_item::CompilerItem) -> TypeId {
        let def = self.require_compiler_item_def(item);
        self.make_enum(def)
    }

    /// Create an `Option<T>` type using the module source registered
    /// via `#[compiler_item("option")]`.
    pub fn make_option(&mut self, inner: TypeId) -> TypeId {
        let def = self
            .compiler_item_def(crate::compiler_item::CompilerItem::Option)
            .expect("the Option declaration is a registered compiler item");
        self.make_generic_instance(def, vec![inner])
    }

    /// Create a `Result<T, E>` type using the module source registered
    /// via `#[compiler_item("result")]`.
    pub fn make_result(&mut self, ok: TypeId, err: TypeId) -> TypeId {
        let def = self
            .compiler_item_def(crate::compiler_item::CompilerItem::Result)
            .expect("the Result declaration is a registered compiler item");
        self.make_generic_instance(def, vec![ok, err])
    }

    /// Create a `Future<T>` generic resource type.
    pub fn make_future(&mut self, inner: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(crate::compiler_item::CompilerItem::Future);
        self.intern(ResolvedType::GenericResource {
            def,
            type_args: vec![inner],
        })
    }

    /// Create a `FutureWritable<T>` generic resource type.
    pub fn make_future_writable(&mut self, inner: TypeId) -> TypeId {
        let def =
            self.require_compiler_item_def(crate::compiler_item::CompilerItem::FutureWritable);
        self.intern(ResolvedType::GenericResource {
            def,
            type_args: vec![inner],
        })
    }

    /// Create a `Stream<T>` generic resource type.
    pub fn make_stream(&mut self, inner: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(crate::compiler_item::CompilerItem::Stream);
        self.intern(ResolvedType::GenericResource {
            def,
            type_args: vec![inner],
        })
    }

    /// Create a `StreamWritable<T>` generic resource type.
    pub fn make_stream_writable(&mut self, inner: TypeId) -> TypeId {
        let def =
            self.require_compiler_item_def(crate::compiler_item::CompilerItem::StreamWritable);
        self.intern(ResolvedType::GenericResource {
            def,
            type_args: vec![inner],
        })
    }

    /// Create a `AsyncCall<T>` generic struct instance type.
    ///
    /// Unlike `Future<T>` and `Stream<T>` (which are CM handle resources),
    /// `AsyncCall<T>` is a Wado-level struct carrying both the raw subtask
    /// handle and the result buffer, so it is represented as a
    /// `GenericInstance`, not a `GenericResource`.
    pub fn make_async_call(&mut self, inner: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(crate::compiler_item::CompilerItem::AsyncCall);
        self.make_generic_instance(def, vec![inner])
    }

    /// If `type_id` is a `AsyncCall<T>` `GenericInstance`, return `T`.
    pub fn as_async_call(&self, type_id: TypeId) -> Option<TypeId> {
        if let ResolvedType::GenericInstance { def, type_args } = self.get(type_id)
            && self.def_name(*def) == "AsyncCall"
            && type_args.len() == 1
        {
            return Some(type_args[0]);
        }
        None
    }

    /// If `type_id` is a `GenericResource`, return `(name, module_source, type_args)`.
    pub fn as_generic_resource(&self, type_id: TypeId) -> Option<(&str, &ModuleSource, &[TypeId])> {
        if let ResolvedType::GenericResource { def, type_args } = self.get(type_id) {
            Some((
                self.def_name(*def),
                self.def_module(*def),
                type_args.as_slice(),
            ))
        } else {
            None
        }
    }

    /// Check if a type is `Option<T>`, returning the inner type if so.
    ///
    /// The instantiation is identified by the declaration it was interned
    /// against, not by the spelling: `name == "Option"` answered yes for any
    /// module's `Option`, and for a user type that merely shares the name.
    pub fn as_option(&self, type_id: TypeId) -> Option<TypeId> {
        let ResolvedType::GenericInstance { type_args, .. } = self.get(type_id) else {
            return None;
        };
        if type_args.len() != 1 {
            return None;
        }
        let inner = type_args[0];
        let decl = self
            .compiler_items
            .variant_decl(crate::compiler_item::CompilerItem::Option)?;
        (self.decl_of_type(type_id) == Some(decl)).then_some(inner)
    }

    pub fn make_tuple(&mut self, elements: Vec<TypeId>) -> TypeId {
        let def = self.require_compiler_item_def(crate::compiler_item::CompilerItem::Tuple);
        self.intern(ResolvedType::GenericInstance {
            def,
            type_args: elements,
        })
    }

    /// Whether a type is a built-in tuple.
    pub fn is_tuple(&self, id: TypeId) -> bool {
        matches!(
            self.get(id),
            ResolvedType::GenericInstance { def, .. }
                if Self::is_tuple_type(self.def_name(*def))
        )
    }

    /// Like [`Self::as_tuple`], but also looks through `&`/`&mut` wrappers
    /// (any nesting depth, via [`Self::peel_refs`]). Returns the element types
    /// together with a `by_ref` flag that is `true` when the tuple was reached
    /// through at least one reference. Used by for-of to iterate `&[..T]`
    /// element-by-reference (`&T_k`), mirroring the `for v of &list` refiter
    /// semantics. Peels to the same depth as tuple `.len()` / `.zip()`
    /// (`peel_refs`) so a `&&tuple` is recognised consistently across both.
    pub fn as_tuple_through_ref(&self, id: TypeId) -> Option<(Vec<TypeId>, bool)> {
        if let Some(elems) = self.as_tuple(id) {
            return Some((elems, false));
        }
        let peeled = self.peel_refs(id);
        if peeled != id
            && let Some(elems) = self.as_tuple(peeled)
        {
            return Some((elems, true));
        }
        None
    }

    /// If the type is a built-in tuple, return its element types.
    pub fn as_tuple(&self, id: TypeId) -> Option<Vec<TypeId>> {
        if let ResolvedType::GenericInstance { def, type_args } = self.get(id)
            && Self::is_tuple_type(self.def_name(*def))
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
        self.make_function_with_mut(false, params, return_type, effects, stores)
    }

    pub fn make_function_with_mut(
        &mut self,
        is_mut: bool,
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<EffectRef>,
        stores: Vec<u32>,
    ) -> TypeId {
        self.intern(ResolvedType::Function {
            is_mut,
            params,
            return_type,
            effects,
            stores,
        })
    }

    pub fn make_struct(&mut self, def: StructDef) -> TypeId {
        self.intern(ResolvedType::Struct {
            def,
            type_args: Vec::new(),
        })
    }

    /// A struct type's rendered spelling: the declaration alone, or the
    /// declaration with its arguments applied. Derived rather than stored, so
    /// there is no fused name for a declaration lookup to mistake for one.
    #[must_use]
    pub fn struct_rendered_name(&self, head: StructDef, type_args: &[TypeId]) -> String {
        let decl_name = self.struct_head_name(head);
        if type_args.is_empty() {
            return decl_name;
        }
        let args: Vec<String> = type_args
            .iter()
            .map(|&a| self.mangle_type_arg_for_generic(a))
            .collect();
        crate::name::mangle_generic_name(&decl_name, &args)
    }

    /// Intern the instantiation of `def` with `type_args`, deriving its
    /// rendered spelling rather than taking one from the caller. An empty
    /// `type_args` interns the *declaration* — a different type.
    pub fn make_monomorphized_struct_from_args(
        &mut self,
        def: StructDef,
        type_args: Vec<TypeId>,
    ) -> TypeId {
        self.intern(ResolvedType::Struct { def, type_args })
    }

    pub fn make_variant(&mut self, def: crate::defs::DefId) -> TypeId {
        self.intern(ResolvedType::Variant { def })
    }

    /// The `TypeId` of the declaration itself — not of any instantiation of it.
    pub fn find_struct_type(&self, def: StructDef) -> Option<TypeId> {
        let key = ResolvedType::Struct {
            def,
            type_args: Vec::new(),
        };
        self.intern_map.get(&key).copied()
    }

    /// Register a variant declaration's case templates for
    /// [`Self::variant_template_cases`].
    pub fn register_variant_cases(
        &mut self,
        def: crate::defs::DefId,
        cases: Vec<(String, u32, TypeId)>,
    ) {
        self.variant_case_index.insert(def, cases);
    }

    /// Case templates of a variant declaration (see `variant_case_index`).
    pub fn variant_template_cases(
        &self,
        def: crate::defs::DefId,
    ) -> Option<&[(String, u32, TypeId)]> {
        self.variant_case_index.get(&def).map(Vec::as_slice)
    }

    /// Find a variant type by (name, `module_source`) pair via `intern_map` (O(1)).
    /// Collision-safe across modules when two variant types share a name.
    pub fn find_variant_type(&self, def: crate::defs::DefId) -> Option<TypeId> {
        self.intern_map.get(&ResolvedType::Variant { def }).copied()
    }

    /// Find a resource type by (name, `module_source`) pair via `intern_map` (O(1)).
    /// Collision-safe across modules when two resource types share a name.
    pub fn find_resource_type(&self, def: crate::defs::DefId) -> Option<TypeId> {
        self.intern_map
            .get(&ResolvedType::Resource { def })
            .copied()
    }

    /// Find an enum type by (name, `module_source`) pair via `intern_map` (O(1)).
    /// Collision-safe across modules when two enum types share a name.
    pub fn find_enum_type(&self, def: crate::defs::DefId) -> Option<TypeId> {
        self.intern_map.get(&ResolvedType::Enum { def }).copied()
    }

    /// Find a flags type by (name, `module_source`) pair via `intern_map` (O(1)).
    /// Collision-safe across modules when two flags types share a name.
    pub fn find_flags_type(&self, def: crate::defs::DefId) -> Option<TypeId> {
        self.intern_map.get(&ResolvedType::Flags { def }).copied()
    }

    /// The declaration `module` declares under the WIT name `name`.
    ///
    /// The one place a name still reaches an identity, and it is the Component
    /// Model boundary. Two things make it unavoidable rather than unfinished:
    /// a WIT name is written in a namespace no Wado resolver walked, so there
    /// is no reference site to ask; and `CmInterfaceRegistry` parses its own
    /// copy of the WASI modules once per process, independent of any
    /// compilation, so the declaring node it could record is not a node this
    /// program's `DefTable` ever saw.
    ///
    /// It cannot mis-identify: `wado-from-idl` generates exactly one module per
    /// interface and each declares a WIT name once, so `module` picks the
    /// generated module and `name` the single declaration in it. It cannot
    /// invent one either — a name that declares nothing answers `None`.
    /// Nothing outside `synthesis::cm_binding` may call it; a Wado name
    /// resolves through [`crate::resolve::Resolutions`] and a stdlib type
    /// through [`Self::compiler_item_def`].
    #[must_use]
    pub(crate) fn cm_decl_in(
        &self,
        name: &str,
        module: &ModuleSource,
    ) -> Option<crate::defs::DefId> {
        self.decl_index
            .get(&(name.to_string(), module.clone()))
            .copied()
    }

    /// The interned type the CM declaration `module` names `name` was
    /// registered under, whichever nominal shape it is.
    pub fn find_named_type_by_source(
        &self,
        name: &str,
        module_source: &ModuleSource,
    ) -> Option<TypeId> {
        let def = self.cm_decl_in(name, module_source)?;
        self.find_struct_type(StructDef::Decl(def))
            .or_else(|| self.find_variant_type(def))
            .or_else(|| self.find_enum_type(def))
            .or_else(|| self.find_flags_type(def))
            .or_else(|| self.find_resource_type(def))
    }

    /// Find any decl-backed named type scoped to a single CM *interface*,
    /// addressed by the module it maps to (e.g. `sockets/ip_name_lookup.wado`).
    ///
    /// [`Self::find_named_type_by_cm_package`] scopes to the package, which
    /// holds several interfaces — two can declare the same name, and that scan
    /// returns whichever registered first.
    #[must_use]
    pub fn find_named_type_by_module_name(&self, name: &str, module_name: &str) -> Option<TypeId> {
        for (type_id, _) in self.all_types() {
            let Some((n, ms)) = self.nominal_head(type_id) else {
                continue;
            };
            if n != name {
                continue;
            }
            let matches = match ms {
                ModuleSource::Wasi { interface } => interface.as_str() == module_name,
                ModuleSource::Core { name: cm_name } => cm_name.as_str() == module_name,
                _ => false,
            };
            if matches {
                return Some(type_id);
            }
        }
        None
    }

    /// Find a decl-backed named type scoped to a CM package, matching any
    /// `module_source` under the `{cm_package}/` prefix — `Wasi` and `Core`
    /// alike. `cm_package` is the bare segment (`"http"`, `"kiln"`), not a
    /// fully-qualified source. Same-named types in distinct packages stay
    /// distinct because `module_source` is part of the intern key.
    pub fn find_named_type_by_cm_package(&self, name: &str, cm_package: &str) -> Option<TypeId> {
        let prefix = format!("{cm_package}/");
        for (type_id, _) in self.all_types() {
            let Some((n, ms)) = self.nominal_head(type_id) else {
                continue;
            };
            if n != name {
                continue;
            }
            match ms {
                ModuleSource::Wasi { interface } if interface.starts_with(&prefix) => {
                    return Some(type_id);
                }
                // Core-packaged CM types (e.g. `core:kiln/types.wado`) are
                // registered under `ModuleSource::Core`; match them by the
                // same package-prefix contract so generator bindings reach
                // their stdlib types through this lookup.
                ModuleSource::Core { name: cm_name } if cm_name.starts_with(&prefix) => {
                    return Some(type_id);
                }
                _ => {}
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
        for (type_id, resolved) in self.all_types() {
            if let ResolvedType::GenericInstance {
                def,
                type_args: gargs,
            } = resolved
                && self.def_name(*def) == name
                && gargs == type_args
            {
                return Some(type_id);
            }
        }
        None
    }

    pub fn make_enum(&mut self, def: crate::defs::DefId) -> TypeId {
        self.intern(ResolvedType::Enum { def })
    }

    pub fn make_resource(&mut self, def: crate::defs::DefId) -> TypeId {
        self.intern(ResolvedType::Resource { def })
    }

    /// Replace the type at an existing `TypeId` with a new type.
    /// Used by the boxing lowering pass to rewrite `Ref(primitive)` → `Struct(Box<T>)`.
    /// Removes the old type from the intern map so it won't be found by future `intern()` calls.
    pub fn replace_type(&mut self, id: TypeId, new_ty: ResolvedType) {
        if let Some(old_ty) = self.types.get(id).cloned() {
            // Only remove from intern_map if this TypeId was the canonical one
            if self.intern_map.get(&old_ty) == Some(&id) {
                self.intern_map.shift_remove(&old_ty);
            }
        }
        self.types.replace(id, new_ty);
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

    /// Peel reference layers and any `Box<T>` the boxing pass introduced,
    /// returning the underlying value type. Post-boxing IR should use this over
    /// [`Self::peel_refs`] so a `&fn(…)` parameter — by then `Box<fn(…)>` — and
    /// an unwrapped `fn(…)` look the same. Matches `peel_refs` when unboxed.
    pub fn peel_refs_and_box(&self, type_id: TypeId) -> TypeId {
        let peeled = self.peel_refs(type_id);
        self.box_payload_types
            .get(peeled)
            .copied()
            .unwrap_or(peeled)
    }

    /// Register a `Box<T>` wrapper's `TypeId` → payload `T`'s `TypeId`
    /// mapping. Called by the boxing pass for every wrapper it creates;
    /// downstream phases consume the mapping via [`Self::peel_refs_and_box`].
    pub fn register_box_payload(&mut self, wrapper: TypeId, payload: TypeId) {
        self.box_payload_types.set_growing(wrapper, payload);
    }

    /// Direct lookup for the payload of a single `Box<T>` wrapper, or
    /// `None` if the given `TypeId` is not a registered wrapper.
    pub fn box_payload_of(&self, wrapper: TypeId) -> Option<TypeId> {
        self.box_payload_types.get(wrapper).copied()
    }

    /// Whether `type_id` may name storage rather than hold a value: a `&T` /
    /// `&mut T`, or the `Box<T>` the boxing pass redefines one into. Both
    /// spellings answer yes, since after `prepare_types` no signature test
    /// tells a boxed borrow from a by-value parameter of the same shape.
    ///
    /// [`RefKind::from_resolved`] alone sees only what is still spelled as a
    /// borrow, so a caller reasoning about values asks this instead.
    pub fn is_reference_shaped(&self, type_id: TypeId) -> bool {
        RefKind::from_resolved(self.get(type_id)).is_some()
            || self.box_payload_of(type_id).is_some()
    }

    /// Record that `wrapper` was redefined from a shared `&T`, so it cannot be
    /// written through. Called by the boxing pass; see [`Self::is_mut_box`].
    pub fn register_shared_box(&mut self, wrapper: TypeId) {
        self.shared_box_type_ids.insert(wrapper);
    }

    /// Whether `wrapper` is a boxed reference that can be written through: a
    /// `&mut T` collapsed onto `Box<T>`, where `*q = v` writes the box the
    /// caller still holds. Only ids known to come from a shared `&T` answer
    /// `false`, so an unclassified wrapper stays writable.
    pub fn is_mut_box(&self, wrapper: TypeId) -> bool {
        self.box_payload_types.get(wrapper).is_some() && !self.shared_box_type_ids.contains(wrapper)
    }

    pub fn make_ref(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::Ref(inner))
    }

    pub fn make_mut_ref(&mut self, inner: TypeId) -> TypeId {
        self.intern(ResolvedType::MutRef(inner))
    }

    /// Build the `(binding_type, value)` for one unrolled tuple-for-of element.
    ///
    /// By value (`by_ref == false`), the element is the field access itself,
    /// typed `T_k`. By reference (`for v of &tuple`), the field access is
    /// wrapped in `&` so the binding is `&T_k` — a reference to a fresh copy of
    /// the element, the same semantics as `for v of &list` (refiter). Shared by
    /// the annotate (`resolve_tuple_for_of`), reify (`reify_tuple_for_of`), and
    /// monomorphize (`expand_variadic_for_of`) paths so the three stay in step.
    pub fn tuple_element_binding(
        &mut self,
        field_access: TirExpr,
        elem_type: TypeId,
        by_ref: bool,
        span: Span,
    ) -> (TypeId, TirExpr) {
        if by_ref {
            let ref_type = self.make_ref(elem_type);
            let value = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(field_access),
                },
                ref_type,
                span,
            );
            (ref_type, value)
        } else {
            (elem_type, field_access)
        }
    }

    /// Create a type parameter (e.g., `T` in `struct Box<T>`)
    pub fn make_type_param(&mut self, name: String, index: u32) -> TypeId {
        self.intern(ResolvedType::TypeParam { name, index })
    }

    /// Create an inference variable (see [`ResolvedType::InferVar`]).
    pub fn make_infer_var(&mut self, id: InferVarId) -> TypeId {
        self.intern(ResolvedType::InferVar(id))
    }

    /// Create a type pack parameter (e.g., `..T` in `fn foo<..T>(x: [..T])`)
    pub fn make_type_pack(&mut self, name: String, index: u32) -> TypeId {
        self.intern(ResolvedType::TypePack {
            name,
            index,
            mapped_elem: None,
        })
    }

    /// Create a mapped type pack: element `i` is `elem[F := F_i]`, where the
    /// pack param may recur in `elem` as a scalar `TypeParam` placeholder.
    /// Drives its arity from pack `(name, index)`. See
    /// [`ResolvedType::TypePack`].
    pub fn make_mapped_type_pack(&mut self, name: String, index: u32, elem: TypeId) -> TypeId {
        self.intern(ResolvedType::TypePack {
            name,
            index,
            mapped_elem: Some(elem),
        })
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
            owning_trait: None,
            bounds: vec![],
            assoc_type_bindings: vec![],
        })
    }

    /// Create an associated type projection: `T::X` where T is a type parameter.
    pub fn make_assoc_type_projection(
        &mut self,
        param_id: TypeId,
        assoc_name: String,
        bounds: Vec<crate::name::FqTraitName>,
        assoc_type_bindings: Vec<(String, TypeId)>,
    ) -> TypeId {
        self.make_assoc_type_projection_of_trait(
            param_id,
            None,
            assoc_name,
            bounds,
            assoc_type_bindings,
        )
    }

    /// [`Self::make_assoc_type_projection`] for a builder that knows which
    /// trait declares the associated type.
    pub fn make_assoc_type_projection_of_trait(
        &mut self,
        param_id: TypeId,
        owning_trait: Option<crate::defs::DefId>,
        assoc_name: String,
        bounds: Vec<crate::name::FqTraitName>,
        assoc_type_bindings: Vec<(String, TypeId)>,
    ) -> TypeId {
        self.intern(ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            owning_trait,
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
        trait_key: crate::defs::DefId,
        assoc_name: String,
        resolved_id: TypeId,
    ) {
        self.assoc_type_resolutions
            .insert((concrete_id, trait_key, assoc_name), resolved_id);
    }

    /// Resolve `<concrete_id as trait_name>::assoc_name` — the exact form,
    /// for callers that know which trait the projection came from.
    pub fn resolve_assoc_type_of_trait(
        &self,
        concrete_id: TypeId,
        trait_key: &crate::defs::DefId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        self.assoc_type_resolutions
            .get(&(concrete_id, *trait_key, assoc_name.to_string()))
            .copied()
    }

    /// Resolve `assoc_name` on `concrete_id`, qualified by `owning_trait`
    /// when the caller has one. Falls back to the unqualified rule when it
    /// does not, or when the named trait registered nothing for this type —
    /// a projection built under a bound can name the trait that *declared*
    /// the associated type while the impl registered it under a subtrait.
    pub fn resolve_assoc_type_qualified(
        &self,
        concrete_id: TypeId,
        owning_trait: &Option<crate::defs::DefId>,
        assoc_name: &str,
    ) -> Option<TypeId> {
        if let Some(trait_key) = owning_trait
            && let Some(resolved) =
                self.resolve_assoc_type_of_trait(concrete_id, trait_key, assoc_name)
        {
            return Some(resolved);
        }
        self.resolve_assoc_type(concrete_id, assoc_name)
    }

    /// Resolve an associated type named `assoc_name` on `concrete_id`
    /// without naming a trait.
    ///
    /// Answers only when exactly one implemented trait declares the name;
    /// two make it a coin flip, so the caller must qualify with
    /// [`Self::resolve_assoc_type_of_trait`] instead.
    pub fn resolve_assoc_type(&self, concrete_id: TypeId, assoc_name: &str) -> Option<TypeId> {
        let mut found = None;
        for ((type_id, _, name), &resolved) in &self.assoc_type_resolutions {
            if *type_id != concrete_id || name != assoc_name {
                continue;
            }
            if found.is_some_and(|prior| prior != resolved) {
                return None;
            }
            found = Some(resolved);
        }
        found
    }

    /// Register a generic associated type definition.
    /// E.g., for `impl Iterator for ListIter<T> { type Item = T; }`,
    /// register `(ListIter's ``AstId``, "Item") → TypeParam(0, "T")`.
    ///
    /// Keyed by the declaring [`AstId`](crate::ast::AstId): two modules may
    /// each declare a `Node<T>`, and their definitions must not overwrite one
    /// another.
    pub fn register_generic_assoc_type_def(
        &mut self,
        base_decl: crate::ast::AstId,
        trait_key: crate::defs::DefId,
        assoc_name: String,
        type_param_id: TypeId,
    ) {
        self.generic_assoc_type_defs
            .insert((base_decl, trait_key, assoc_name), type_param_id);
    }

    /// The generic definition of `assoc_name` on `base_decl`, together with
    /// the trait that declares it. `None` when no trait declares the name, or
    /// when two disagree — the same unambiguity rule
    /// [`Self::resolve_assoc_type`] applies to resolved types.
    fn generic_assoc_type_def(
        &self,
        base_decl: crate::ast::AstId,
        assoc_name: &str,
    ) -> Option<(crate::defs::DefId, TypeId)> {
        let mut found: Option<(crate::defs::DefId, TypeId)> = None;
        for ((decl, trait_key, name), &def_id) in &self.generic_assoc_type_defs {
            if *decl != base_decl || name != assoc_name {
                continue;
            }
            if found.as_ref().is_some_and(|(_, prior)| *prior != def_id) {
                return None;
            }
            found = Some((*trait_key, def_id));
        }
        found
    }

    /// Register associated-type resolutions for a freshly monomorphized struct.
    /// A [`ResolvedType::Struct`] carries no type args, so `Foo<…>::Item` can no
    /// longer go through [`Self::resolve_generic_assoc_type`]; each definition on
    /// `base_decl` is instead resolved eagerly against `substitution` and
    /// recorded under `concrete_id` for later [`Self::resolve_assoc_type`] hits.
    pub fn register_monomorphized_assoc_types(
        &mut self,
        concrete_id: TypeId,
        base_decl: crate::ast::AstId,
        substitution: &IndexMap<u32, TypeId>,
    ) {
        let defs: Vec<(crate::defs::DefId, String, TypeId)> = self
            .generic_assoc_type_defs
            .iter()
            .filter(|((decl, _, _), _)| *decl == base_decl)
            .map(|((_, trait_key, assoc_name), &def_id)| (*trait_key, assoc_name.clone(), def_id))
            .collect();
        for (trait_key, assoc_name, def_id) in defs {
            let resolved = self.substitute_type_params(def_id, substitution);
            if !self.contains_type_param(resolved) {
                self.register_assoc_type_resolution(concrete_id, trait_key, assoc_name, resolved);
            }
        }
    }

    /// Resolve an associated type for a `GenericInstance` type using generic definitions.
    /// For `ListIter<i32>::Item`: looks up `("ListIter", "Item")` → `TypeParam(0)`,
    /// then substitutes using the instance's `type_args` to get `i32`.
    pub fn resolve_generic_assoc_type(
        &self,
        concrete_id: TypeId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let type_args = match self.get(concrete_id).clone() {
            ResolvedType::GenericInstance { type_args, .. } => type_args,
            _ => return None,
        };
        let (_, def_type_id) =
            self.generic_assoc_type_def(self.decl_of_type(concrete_id)?, assoc_name)?;
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
            // A composite def (`&T`, `List<T>`, …) still carrying the base
            // struct's type params cannot be substituted here without interning
            // (this is a `&self` fast path). Return `None` so the caller keeps
            // the projection unresolved; the `&mut`
            // `resolve_generic_assoc_type_mono` resolves it with the instance's
            // type args. A param-free composite def is returned as-is.
            _ => {
                if self.contains_type_param(def_type_id) {
                    None
                } else {
                    Some(def_type_id)
                }
            }
        }
    }

    /// Monomorphization-time associated-type resolution for a `GenericInstance`.
    /// Where [`Self::resolve_generic_assoc_type`] is a `&self` fast path limited
    /// to a bare-`TypeParam` def, this substitutes positionally through
    /// [`Self::substitute_type_params`], so a composite or nested definition
    /// (`&T`, `I::Item`) becomes fully concrete one level per recursion.
    pub fn resolve_generic_assoc_type_mono(
        &mut self,
        concrete_id: TypeId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let type_args = match self.get(concrete_id).clone() {
            ResolvedType::GenericInstance { type_args, .. } => type_args,
            _ => return None,
        };
        let (_, def_type_id) =
            self.generic_assoc_type_def(self.decl_of_type(concrete_id)?, assoc_name)?;
        let subst: IndexMap<u32, TypeId> = type_args
            .iter()
            .enumerate()
            .map(|(i, &a)| (i as u32, a))
            .collect();
        Some(self.substitute_type_params(def_type_id, &subst))
    }

    /// Whether the declaration behind `type_id` can be reflected — every
    /// declaration but a sealed member handle.
    ///
    /// The bound check and reflect synthesis both read this, so synthesis
    /// covers exactly what the bound accepts without a demand channel between
    /// them.
    pub fn is_reflect_eligible(&self, type_id: TypeId) -> bool {
        !self
            .decl_of_type(type_id)
            .is_some_and(|decl| self.is_sealed_reflect_member(decl))
    }

    /// Whether a generic definition of `assoc_name` is registered for the
    /// declaration behind `type_id` — i.e. the generic type carries a
    /// synthesized impl binding that associated type.
    pub fn has_generic_assoc_type_def(&self, type_id: TypeId, assoc_name: &str) -> bool {
        self.decl_of_type(type_id)
            .is_some_and(|decl| self.has_generic_assoc_type_def_for_decl(decl, assoc_name))
    }

    /// [`Self::has_generic_assoc_type_def`] for a caller that already holds the
    /// declaring [`AstId`](crate::ast::AstId).
    pub fn has_generic_assoc_type_def_for_decl(
        &self,
        decl: crate::ast::AstId,
        assoc_name: &str,
    ) -> bool {
        self.generic_assoc_type_def(decl, assoc_name).is_some()
    }

    /// Resolve an associated type for whatever form the subject currently has:
    /// a registered resolution for a plain or monomorphized type, substitution
    /// of the generic definition for a `GenericInstance`.
    ///
    /// Reflection projections hit both forms — the same receiver reads as an
    /// instance before monomorphization and as a struct after.
    pub fn resolve_assoc_type_of_instance(
        &mut self,
        concrete_id: TypeId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        if let Some(resolved) = self.resolve_assoc_type(concrete_id, assoc_name) {
            return Some(resolved);
        }
        self.resolve_generic_assoc_type_mono(concrete_id, assoc_name)
    }

    /// The registered resolution of `trait_name::assoc_name` on `concrete_id`.
    /// Unlike [`Self::resolve_assoc_type`] this cannot be confused by a name
    /// several traits share; unlike
    /// [`Self::resolve_trait_assoc_type_of_instance`] it does not substitute a
    /// generic definition, so it needs no interning.
    pub fn resolve_trait_assoc_type(
        &self,
        concrete_id: TypeId,
        trait_key: &crate::defs::DefId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        self.assoc_type_resolutions
            .get(&(concrete_id, *trait_key, assoc_name.to_string()))
            .copied()
    }

    /// [`Self::resolve_assoc_type_of_instance`] for a caller that knows which
    /// trait declares the associated type. The untyped form scans every trait
    /// and gives up when two disagree, so a name several traits share — the
    /// reflection kinds all spell their member channel `Members` — is only
    /// unambiguous here.
    pub fn resolve_trait_assoc_type_of_instance(
        &mut self,
        concrete_id: TypeId,
        trait_key: &crate::defs::DefId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        let key = (concrete_id, *trait_key, assoc_name.to_string());
        if let Some(&resolved) = self.assoc_type_resolutions.get(&key) {
            return Some(resolved);
        }
        let type_args = match self.get(concrete_id).clone() {
            ResolvedType::GenericInstance { type_args, .. } => type_args,
            _ => return None,
        };
        let def_key = (
            self.decl_of_type(concrete_id)?,
            *trait_key,
            assoc_name.to_string(),
        );
        let def_type_id = *self.generic_assoc_type_defs.get(&def_key)?;
        let subst: IndexMap<u32, TypeId> = type_args
            .iter()
            .enumerate()
            .map(|(i, &a)| (i as u32, a))
            .collect();
        Some(self.substitute_type_params(def_type_id, &subst))
    }

    /// Substitute `TypeParam` and `TypePack` indices in `type_id`, descending
    /// through every container form, expanding a `TypePack` inside a tuple, and
    /// resolving an `AssocTypeProjection` once its parameter turns concrete.
    /// Missing indices are permissive — an unmatched `TypeParam` stays put, so
    /// callers can substitute partially during inference.
    pub fn substitute_type_params(
        &mut self,
        type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
    ) -> TypeId {
        self.substitute_type_params_with(type_id, substitution, &SlotProjections::default())
    }

    /// [`Self::substitute_type_params`], additionally answering the
    /// projections rooted at a slot.
    ///
    /// A declaration frame is abstract over its slots *and* over what
    /// `Self::X` means. Only the use site knows the second — it is written at
    /// the caller — so it supplies the answers here.
    pub fn substitute_type_params_with(
        &mut self,
        type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
        projections: &SlotProjections,
    ) -> TypeId {
        self.subst_rec(type_id, substitution, &IndexMap::default(), projections)
    }

    /// Substitute solved inference variables into `type_id`.
    ///
    /// The flexible counterpart of [`Self::substitute_type_params`]: that one
    /// fills a declaration's slots positionally, this one answers variables the
    /// solver has determined. They share one traversal, differing only in which
    /// leaf they replace.
    pub fn substitute_infer_vars(
        &mut self,
        type_id: TypeId,
        solutions: &IndexMap<InferVarId, TypeId>,
    ) -> TypeId {
        self.subst_rec(
            type_id,
            &IndexMap::default(),
            solutions,
            &SlotProjections::default(),
        )
    }

    /// The shared traversal behind [`Self::substitute_type_params_with`] and
    /// [`Self::substitute_infer_vars`].
    fn subst_rec(
        &mut self,
        type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
        vars: &IndexMap<InferVarId, TypeId>,
        projections: &SlotProjections,
    ) -> TypeId {
        if substitution.is_empty() && vars.is_empty() && projections.is_empty() {
            return type_id;
        }
        match self.get(type_id).clone() {
            ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                substitution.get(&index).copied().unwrap_or(type_id)
            }
            ResolvedType::InferVar(var) => vars.get(&var).copied().unwrap_or(type_id),
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.subst_rec(elem, substitution, vars, projections);
                if new_elem == elem {
                    type_id
                } else {
                    self.intern(ResolvedType::BuiltinArray(new_elem))
                }
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.subst_rec(inner, substitution, vars, projections);
                if new_inner == inner {
                    type_id
                } else {
                    self.make_ref(new_inner)
                }
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.subst_rec(inner, substitution, vars, projections);
                if new_inner == inner {
                    type_id
                } else {
                    self.make_mut_ref(new_inner)
                }
            }
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                effects,
                stores,
            } => {
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.subst_rec(p, substitution, vars, projections))
                    .collect();
                let new_return_type = self.subst_rec(return_type, substitution, vars, projections);
                if new_params == params && new_return_type == return_type {
                    type_id
                } else {
                    self.make_function_with_mut(
                        is_mut,
                        new_params,
                        new_return_type,
                        effects,
                        stores,
                    )
                }
            }
            ResolvedType::GenericResource { def, type_args } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&a| self.subst_rec(a, substitution, vars, projections))
                    .collect();
                if new_args == type_args {
                    type_id
                } else {
                    self.intern(ResolvedType::GenericResource {
                        def,
                        type_args: new_args,
                    })
                }
            }
            ResolvedType::GenericInstance { def, type_args } => {
                let name = self.def_name(def).to_string();
                if Self::is_tuple_type(&name) {
                    // Tuples need TypePack expansion: splice pack elements
                    // into the tuple's type-arg list.
                    let mut new_elems: Vec<TypeId> = Vec::new();
                    for &e in &type_args {
                        match self.get(e).clone() {
                            ResolvedType::TypePack {
                                index, mapped_elem, ..
                            } => {
                                if let Some(&pack_type) = substitution.get(&index) {
                                    match mapped_elem {
                                        // Mapped pack: substitute the element
                                        // once per source pack element, binding
                                        // the pack param to that element — a
                                        // constructor map `[..Case<T, P>]`
                                        // yields `Case<T, P_k>` at position k;
                                        // a pack-independent `..F::method()`
                                        // repeats its return type `|F|` times.
                                        Some(elem) => {
                                            let pack_elems = self
                                                .as_tuple(pack_type)
                                                .unwrap_or_else(|| vec![pack_type]);
                                            for pe in pack_elems {
                                                let mut elem_substitution = substitution.clone();
                                                elem_substitution.insert(index, pe);
                                                new_elems.push(self.subst_rec(
                                                    elem,
                                                    &elem_substitution,
                                                    vars,
                                                    projections,
                                                ));
                                            }
                                        }
                                        None => {
                                            if let Some(pack_elems) = self.as_tuple(pack_type) {
                                                new_elems.extend_from_slice(&pack_elems);
                                            } else {
                                                new_elems.push(pack_type);
                                            }
                                        }
                                    }
                                } else {
                                    new_elems.push(e);
                                }
                            }
                            _ => {
                                new_elems.push(self.subst_rec(e, substitution, vars, projections));
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
                        .map(|&a| self.subst_rec(a, substitution, vars, projections))
                        .collect();
                    if new_args == type_args {
                        type_id
                    } else {
                        self.make_generic_instance(def, new_args)
                    }
                }
            }
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                owning_trait,
                bounds,
                assoc_type_bindings,
            } => {
                // The use site's answer wins: a rebuilt projection cannot
                // re-derive what `Self::X` means there.
                let base_slot = match self.get(param_id) {
                    ResolvedType::TypeParam { index, .. }
                    | ResolvedType::TypePack { index, .. } => Some(*index),
                    _ => None,
                };
                if let Some(slot) = base_slot
                    && let Some(answer) = projections.get(&slot).and_then(|answers| {
                        answers
                            .iter()
                            .find(|(name, _)| *name == assoc_name)
                            .map(|(_, type_id)| *type_id)
                    })
                {
                    return answer;
                }
                // Substitute the parameter first; only attempt projection
                // resolution once the underlying type is fully concrete.
                let substituted_base = self.subst_rec(param_id, substitution, vars, projections);
                if !self.contains_type_param(substituted_base) {
                    // Associated types are inherited through references (mirrors
                    // method-call auto-deref), so peel `&`/`&mut` before
                    // projecting: a `D` inferred as `&mut MyDe` still projects
                    // `D::Acc` to `MyDe`'s associated type.
                    let concrete = self.peel_refs(substituted_base);
                    if let Some(resolved) =
                        self.resolve_assoc_type_qualified(concrete, &owning_trait, &assoc_name)
                    {
                        return resolved;
                    }
                    if let Some(resolved) =
                        self.resolve_generic_assoc_type_mono(concrete, &assoc_name)
                    {
                        return resolved;
                    }
                }
                // Bindings are resolved in the same frame as the rest of the
                // signature, so they carry its slots too.
                let mut new_bindings: Vec<(String, TypeId)> =
                    Vec::with_capacity(assoc_type_bindings.len());
                for (name, bound) in &assoc_type_bindings {
                    let substituted = self.subst_rec(*bound, substitution, vars, projections);
                    new_bindings.push((name.clone(), substituted));
                }
                if substituted_base == param_id && new_bindings == assoc_type_bindings {
                    type_id
                } else {
                    self.make_assoc_type_projection_of_trait(
                        substituted_base,
                        owning_trait,
                        assoc_name,
                        bounds,
                        new_bindings,
                    )
                }
            }
            // `Reactive` wraps an inner type, so substitute it recursively.
            // Defensive: reactive bindings are typed with the underlying value
            // type today, so the wrapper never reaches monomorphize — but the
            // contract is "rewrite every embedded parameter", and it embeds one.
            ResolvedType::Reactive(inner) => {
                let new_inner = self.subst_rec(inner, substitution, vars, projections);
                if new_inner == inner {
                    type_id
                } else {
                    self.intern(ResolvedType::Reactive(new_inner))
                }
            }
            // Primitives, Unit, Never, Unknown, Error, Struct, Enum, Variant,
            // Resource, Newtype, Flags — name-only or already-erased; no
            // embedded type params.
            _ => type_id,
        }
    }

    /// Create a generic instance (e.g., `Box<i32>`)
    pub fn make_generic_instance(
        &mut self,
        def: crate::defs::DefId,
        type_args: Vec<TypeId>,
    ) -> TypeId {
        // The instantiation carries the declaration it came from, so nothing
        // has to be registered beside it and nothing re-derives the answer
        // from a spelling whose base `prune` may already have dropped.
        self.intern(ResolvedType::GenericInstance { def, type_args })
    }

    /// Create a `List<T>` type (`GenericInstance` { name: "List", ... })
    pub fn make_list(&mut self, element: TypeId) -> TypeId {
        let def = self
            .compiler_item_def(crate::compiler_item::CompilerItem::List)
            .expect("the List declaration is a registered compiler item");
        self.make_generic_instance(def, vec![element])
    }

    /// Create the `ByteList` newtype (`type ByteList = List<u8>`).
    pub fn make_byte_list(&mut self) -> TypeId {
        let base = self.make_list(TypeTable::U8);
        let def = self.require_compiler_item_def(crate::compiler_item::CompilerItem::ByteList);
        self.make_newtype(def, base)
    }

    /// Create a newtype wrapping a base type
    pub fn make_newtype(&mut self, def: crate::defs::DefId, base_type: TypeId) -> TypeId {
        self.intern(ResolvedType::Newtype {
            def,
            type_args: Vec::new(),
            base_type,
        })
    }

    /// A generic newtype's instantiation: the declaration with what it was
    /// applied to, so the head stays the one an `impl` header writes.
    pub fn make_newtype_instance(
        &mut self,
        def: crate::defs::DefId,
        type_args: Vec<TypeId>,
        base_type: TypeId,
    ) -> TypeId {
        self.intern(ResolvedType::Newtype {
            def,
            type_args,
            base_type,
        })
    }

    /// Create a flags type (bitmask over u32)
    pub fn make_flags(&mut self, def: crate::defs::DefId) -> TypeId {
        self.intern(ResolvedType::Flags { def })
    }

    /// Erase all `Newtype` and `Flags` entries from the type table by populating
    /// the redirect map. After this call, `get(id)` for any erased `TypeId` returns
    /// its ultimate base type (`Newtype` chains) or `u32` (`Flags`).
    ///
    /// Must be called after resolve and synthesis, before monomorphize.
    pub fn erase_newtypes_and_flags(&mut self) {
        let ids: Vec<TypeId> = self.iter_type_ids().collect();
        for id in ids {
            let redirect = match self.types.get(id).unwrap() {
                ResolvedType::Newtype { .. } => Some(self.get_ultimate_base_type(id)),
                ResolvedType::Flags { .. } => Some(TypeTable::U32),
                _ => None,
            };
            if let Some(target) = redirect {
                self.redirects.set_growing(id, target);
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
        if let Some(&redirect) = self.redirects.get(id) {
            return redirect;
        }
        let mut current = id;
        loop {
            match self
                .types
                .get(current)
                .unwrap_or_else(|| panic!("TypeId {current:?} not found in TypeTable"))
            {
                ResolvedType::Newtype { base_type, .. } => {
                    // Use redirect if already computed; otherwise follow the raw chain.
                    current = self
                        .redirects
                        .get(*base_type)
                        .copied()
                        .unwrap_or(*base_type);
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

    /// Check if a type is `List<T>` and return the element type if so.
    /// Also unwraps Ref/MutRef types to check the inner type.
    pub fn as_list(&self, id: TypeId) -> Option<TypeId> {
        match self.get(id) {
            ResolvedType::GenericInstance { def, type_args }
                if self.def_name(*def) == "List" && type_args.len() == 1 =>
            {
                Some(type_args[0])
            }
            // Unwrap references and check the inner type
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => self.as_list(*inner),
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

    /// Whether `id` (recursively) mentions anything a type check cannot decide
    /// yet: an inference variable awaiting its solver, a type pack awaiting
    /// expansion, an associated-type projection awaiting its impl, or an
    /// unresolved / error type.
    ///
    /// Deliberately *not* the same question as [`Self::contains_type_param`].
    /// A rigid type parameter is decided — it is opaque, and stands only for
    /// itself — so it does not belong here.
    pub fn contains_undecided(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::InferVar(_)
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => true,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Reactive(inner) => self.contains_undecided(*inner),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params.iter().any(|p| self.contains_undecided(*p))
                    || self.contains_undecided(*return_type)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => {
                type_args.iter().any(|t| self.contains_undecided(*t))
            }
            _ => false,
        }
    }

    /// Whether `id` (recursively) mentions a *rigid* type parameter — a slot
    /// of some declaration's own frame, as opposed to an inference variable a
    /// solver still owns.
    pub fn contains_rigid_param(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. } => true,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Reactive(inner) => self.contains_rigid_param(*inner),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params.iter().any(|p| self.contains_rigid_param(*p))
                    || self.contains_rigid_param(*return_type)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => {
                type_args.iter().any(|t| self.contains_rigid_param(*t))
            }
            _ => false,
        }
    }

    /// Whether `id` is fully determined: no type parameter, no inference
    /// variable, no projection awaiting a bound's impl, nothing unresolved —
    /// anywhere inside it. A type that can be named, monomorphized, and
    /// emitted. The negation of [`Self::contains_type_param`], spelled
    /// positively so a caller filtering for real types need not reinvent the
    /// recursion.
    pub fn is_concrete(&self, id: TypeId) -> bool {
        !self.contains_type_param(id)
    }

    /// Check if a type is or contains type parameters or unresolved types (Unknown/Error)
    pub fn contains_type_param(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::InferVar(_)
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

    /// Whether `id` (recursively) mentions an associated-type projection
    /// (`I::Item`), i.e. still needs a bound's impl to become concrete.
    pub fn contains_assoc_type_projection(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::AssocTypeProjection { .. } => true,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Reactive(inner) => self.contains_assoc_type_projection(*inner),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params
                    .iter()
                    .any(|p| self.contains_assoc_type_projection(*p))
                    || self.contains_assoc_type_projection(*return_type)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => type_args
                .iter()
                .any(|t| self.contains_assoc_type_projection(*t)),
            _ => false,
        }
    }

    /// Whether `id` (recursively) mentions a `TypeParam` / `TypePack` whose
    /// `index` equals `index`. Used to tell whether a method type parameter is
    /// inferable from an argument position (it appears in a value-parameter's
    /// type) versus only from the return type.
    pub fn contains_type_param_index(&self, id: TypeId, index: u32) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { index: i, .. } | ResolvedType::TypePack { index: i, .. } => {
                *i == index
            }
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Reactive(inner) => self.contains_type_param_index(*inner, index),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params
                    .iter()
                    .any(|p| self.contains_type_param_index(*p, index))
                    || self.contains_type_param_index(*return_type, index)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => type_args
                .iter()
                .any(|t| self.contains_type_param_index(*t, index)),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_type_bindings,
                ..
            } => {
                self.contains_type_param_index(*param_id, index)
                    || assoc_type_bindings
                        .iter()
                        .any(|(_, t)| self.contains_type_param_index(*t, index))
            }
            _ => false,
        }
    }

    /// Whether every `TypeParam` / `TypePack` `id` (recursively) mentions is in
    /// `allowed` (by `TypeId`). A type with no type parameters trivially holds.
    /// Used to decide whether an inference hole may be solved against an
    /// expected type: only when that type's parameters are outer-scope generics
    /// (not a callee's own, still-being-inferred method parameters).
    pub fn type_params_all_in(&self, id: TypeId, allowed: &[TypeId]) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. } => allowed.contains(&id),
            ResolvedType::InferVar(_) => false,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Reactive(inner) => self.type_params_all_in(*inner, allowed),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params.iter().all(|p| self.type_params_all_in(*p, allowed))
                    && self.type_params_all_in(*return_type, allowed)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => type_args
                .iter()
                .all(|t| self.type_params_all_in(*t, allowed)),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_type_bindings,
                ..
            } => {
                self.type_params_all_in(*param_id, allowed)
                    && assoc_type_bindings
                        .iter()
                        .all(|(_, t)| self.type_params_all_in(*t, allowed))
            }
            _ => true,
        }
    }

    /// Whether `id` (recursively) mentions an inference variable.
    pub fn contains_infer_var(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::InferVar(_) => true,
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Reactive(inner) => self.contains_infer_var(*inner),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                params.iter().any(|p| self.contains_infer_var(*p))
                    || self.contains_infer_var(*return_type)
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => {
                type_args.iter().any(|t| self.contains_infer_var(*t))
            }
            _ => false,
        }
    }

    /// Get a human-readable name for a type
    pub fn type_name(&self, id: TypeId) -> String {
        self.render_type_name(id, false)
    }

    /// [`Self::type_name`] with every declared head written in the spec's
    /// `MODULE#SYMBOL` notation.
    ///
    /// Ask [`Self::type_names_for_mismatch`] rather than this directly:
    /// qualifying a name that was already unambiguous only makes the message
    /// longer.
    #[must_use]
    pub fn type_name_qualified(&self, id: TypeId) -> String {
        self.render_type_name(id, true)
    }

    /// The two spellings a mismatch message prints.
    ///
    /// Equal renderings mean two declarations of one name — the only case a
    /// reader cannot settle from the message — and only then is each qualified.
    /// Every other mismatch keeps the short form it prints today.
    #[must_use]
    pub fn type_names_for_mismatch(&self, expected: TypeId, found: TypeId) -> (String, String) {
        let (a, b) = (self.type_name(expected), self.type_name(found));
        if a != b {
            return (a, b);
        }
        let (qa, qb) = (
            self.type_name_qualified(expected),
            self.type_name_qualified(found),
        );
        // Two renderings that stay equal even qualified are the same type, or
        // two shapes no module names. Neither is helped by the longer form.
        if qa == qb { (a, b) } else { (qa, qb) }
    }

    /// The declared name of `def`, qualified by its module when `qualified`.
    fn head_name(&self, def: crate::defs::DefId, qualified: bool) -> String {
        let name = self.def_name(def);
        if qualified {
            crate::symbol_notation::render(&self.def_module(def).to_string(), name)
        } else {
            name.to_string()
        }
    }

    fn render_type_name(&self, id: TypeId, qualified: bool) -> String {
        let type_name = |t: TypeId| self.render_type_name(t, qualified);
        match self.get(id) {
            ResolvedType::Primitive(p) => p.as_str().to_string(),
            ResolvedType::Unit => "()".to_string(),
            ResolvedType::Never => "!".to_string(),
            ResolvedType::Unknown => "unknown".to_string(),
            ResolvedType::Error => "error".to_string(),
            ResolvedType::BuiltinArray(elem) => {
                format!("Array<{}>", type_name(*elem))
            }
            ResolvedType::Struct { def, type_args } => {
                // The declared head, not the rendered one: a message shows
                // `Box<i32>`, never the `Box@<local>` a local declaration is
                // stored under.
                let head = match (qualified, def.decl()) {
                    (true, Some(decl)) => self.head_name(decl, true),
                    _ => self.struct_head_decl_name(*def),
                };
                if type_args.is_empty() {
                    head
                } else {
                    let args: Vec<String> = type_args
                        .iter()
                        .map(|&a| self.mangle_type_arg_for_generic(a))
                        .collect();
                    crate::name::mangle_generic_name(&head, &args)
                }
            }
            ResolvedType::Enum { def } | ResolvedType::Resource { def } => {
                self.head_name(*def, qualified)
            }
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                ..
            } => {
                let param_names: Vec<String> = params.iter().map(|p| type_name(*p)).collect();
                let keyword = if *is_mut { "fn mut" } else { "fn" };
                format!(
                    "{}({}) -> {}",
                    keyword,
                    param_names.join(", "),
                    type_name(*return_type)
                )
            }
            ResolvedType::Ref(inner) => format!("&{}", type_name(*inner)),
            ResolvedType::MutRef(inner) => format!("&mut {}", type_name(*inner)),
            ResolvedType::Variant { def } => self.head_name(*def, qualified),
            ResolvedType::GenericResource { def, type_args } => {
                let arg_names: Vec<String> = type_args.iter().map(|t| type_name(*t)).collect();
                format!(
                    "{}<{}>",
                    self.head_name(*def, qualified),
                    arg_names.join(", ")
                )
            }
            ResolvedType::Reactive(inner) => format!("Reactive<{}>", type_name(*inner)),
            ResolvedType::TypeParam { name, .. } => name.clone(),
            ResolvedType::InferVar(var) => var.to_string(),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                ..
            } => {
                format!("{}::{}", type_name(*param_id), assoc_name)
            }
            ResolvedType::GenericInstance { def, type_args } => {
                let arg_names: Vec<String> = type_args.iter().map(|t| type_name(*t)).collect();
                // A tuple is module-independent, so it has nothing to qualify.
                if Self::is_tuple_type(self.def_name(*def)) {
                    format!("[{}]", arg_names.join(", "))
                } else {
                    format!(
                        "{}<{}>",
                        self.head_name(*def, qualified),
                        arg_names.join(", ")
                    )
                }
            }
            ResolvedType::Newtype { def, type_args, .. } => {
                let head = self.head_name(*def, qualified);
                if type_args.is_empty() {
                    head
                } else {
                    let args: Vec<String> = type_args.iter().map(|t| type_name(*t)).collect();
                    format!("{head}<{}>", args.join(", "))
                }
            }
            ResolvedType::Flags { def } => self.head_name(*def, qualified),
            ResolvedType::TypePack { name, .. } => format!("..{name}"),
        }
    }

    /// Resolve through newtypes/flags to find the base type.
    /// Returns the original `TypeId` if not a newtype or flags.
    ///
    /// Works both before and after `erase_newtypes_and_flags()`. After erasure,
    /// the redirect map is checked first so that `resolve_newtype_base(FieldName_id)` → `String_id`.
    #[must_use]
    pub fn resolve_newtype_base(&self, id: TypeId) -> TypeId {
        // Fast path: after erasure, redirects already point to the base.
        if let Some(&redirect) = self.redirects.get(id) {
            return redirect;
        }
        let mut current = id;
        loop {
            match self
                .types
                .get(current)
                .unwrap_or_else(|| panic!("TypeId {current:?} not found in TypeTable"))
            {
                ResolvedType::Newtype { base_type, .. } => {
                    current = self
                        .redirects
                        .get(*base_type)
                        .copied()
                        .unwrap_or(*base_type);
                }
                ResolvedType::Flags { .. } => return TypeTable::U32,
                _ => return current,
            }
        }
    }

    /// Mangle a type name, resolving all newtypes/flags to their base types recursively.
    /// E.g., `List<FieldName>` → `List<String>` when `FieldName = String`.
    pub fn mangle_type_name_resolving_newtypes(&self, id: TypeId) -> String {
        let base = self.resolve_newtype_base(id);
        match self.get(base) {
            ResolvedType::GenericInstance { def, type_args } => {
                let name = self.def_name(*def);
                let module_source = self.def_module(*def);
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_name_resolving_newtypes(*t))
                    .collect();
                // A tuple is module-independent; every other instance is named
                // by the module declaring its base.
                if Self::is_tuple_type(name) {
                    crate::name::mangle_tuple_type(&args)
                } else {
                    let unqualified =
                        crate::name::mangle_generic_name(&self.decl_render_name(*def), &args);
                    format!("{module_source}/{unqualified}")
                }
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
            ResolvedType::GenericInstance { def, type_args } => {
                let resolved: Vec<TypeId> = type_args
                    .iter()
                    .map(|t| self.resolve_newtypes_deep(*t))
                    .collect();
                if resolved == type_args {
                    base
                } else {
                    self.make_generic_instance(def, resolved)
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
            ResolvedType::GenericInstance { def, type_args } => {
                let resolved: Vec<TypeId> = type_args
                    .iter()
                    .map(|t| self.resolve_newtypes_deep_readonly(*t))
                    .collect();
                if resolved == *type_args {
                    base
                } else if let Some(existing) =
                    self.find_generic_instance(self.def_name(*def), &resolved)
                {
                    existing
                } else {
                    id // Can't create new type, return original
                }
            }
            _ => base,
        }
    }

    /// Mangle a type for use inside struct / function names — `Tuple<i32,String>`
    /// where [`Self::type_name`] would give the human-readable `[i32, String]`.
    /// A generic renders as `Name<T1,T2,…>`, a function as
    /// `Fn<paramCount,returnType>`, and references are stripped.
    #[must_use]
    pub fn mangle_type_name(&self, id: TypeId) -> String {
        let info = self.get_type_name_info(id);
        format_type_name(info)
    }

    /// Mangle `id` as a type argument inside a generic instance's or monomorph's
    /// identity name — the `T` in `Result<unit, T>`. Unlike
    /// [`Self::mangle_type_name`], every named user-defined head is qualified by
    /// its declaring `ModuleSource`, or two same-named types collapse onto one
    /// WIR identity and the second inherits the first's representation.
    pub fn mangle_type_arg_for_generic(&self, id: TypeId) -> String {
        match self.get(id) {
            ResolvedType::Variant { def }
            | ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::Flags { def } => {
                format!("{}/{}", self.def_module(*def), self.def_name(*def))
            }
            // A struct mangles as a type argument by its rendered spelling:
            // `TreeMap<String,i32>` and `TreeMap<String,String>` are distinct
            // arguments, and naming both `TreeMap` collides the functions
            // instantiated over them.
            ResolvedType::Struct { def, type_args } => format!(
                "{}/{}",
                self.struct_head_module(*def),
                self.struct_rendered_name(*def, type_args)
            ),
            ResolvedType::GenericInstance { def, type_args } => {
                let name = self.def_name(*def);
                let module_source = self.def_module(*def);
                // Qualify the BASE name of the instance and recursively
                // qualify each type argument so the result is identical
                // to the qualified name produced once the instance is
                // substituted to a `Struct` by the monomorphizer.
                // Without this, the mangled name flips at the
                // substitution boundary and downstream registries see
                // two distinct mangled names for the same logical type.
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_arg_for_generic(*t))
                    .collect();
                // A tuple is module-independent, so it carries no module prefix
                // (its elements stay qualified).
                if Self::is_tuple_type(name) {
                    return crate::name::mangle_tuple_type(&args);
                }
                let unqualified =
                    crate::name::mangle_generic_name(&self.decl_render_name(*def), &args);
                format!("{module_source}/{unqualified}")
            }
            // Ref / MutRef are preserved in the mangled output so that
            // `Box<T>` and `Box<&T>` (semantically distinct instantiations)
            // map to distinct mangled names. Stripping refs here used to
            // collapse two `InstantiationKey`s like `[List<char>]` and
            // `[&List<char>]` to the same mangled function name, breaking
            // `function_id_for` injectivity in `project.functions`
            // (issue #1093). Sites that want the "base type name" use
            // `mangle_type_name` (or `base_type_name`), which peels refs by
            // delegating through `TypeNameInfo::Ref`.
            ResolvedType::Ref(inner) => {
                format!("&{}", self.mangle_type_arg_for_generic(*inner))
            }
            ResolvedType::MutRef(inner) => {
                format!("&mut {}", self.mangle_type_arg_for_generic(*inner))
            }
            // A raw GC array must carry its element's *qualified* mangle.
            // Delegating to `mangle_type_name` (the `_` arm) mangles the
            // element unqualified, so `Array<Foo>` built from two modules'
            // same-named structs collapses to one mangle — merging their
            // otherwise-distinct `$value_copy$` helpers into a single helper
            // whose one concrete signature then mismatches the other array's
            // ref type (invalid Wasm). Structs / variants / generic instances
            // are already module-qualified above; arrays must match.
            ResolvedType::BuiltinArray(elem) => {
                crate::name::mangle_builtin_array_type(&self.mangle_type_arg_for_generic(*elem))
            }
            // Primitives / functions delegate to `mangle_type_name`.
            _ => self.mangle_type_name(id),
        }
    }

    /// Module-qualifying analogue of
    /// [`Self::mangle_type_name_resolving_newtypes`]. Used by the few WIR
    /// fq lookup sites that consult the newtype-resolved form
    /// (`wir_build/context.rs`).
    pub fn mangle_type_arg_for_generic_resolving_newtypes(&self, id: TypeId) -> String {
        let resolved = self.resolve_newtype_base(id);
        self.mangle_type_arg_for_generic(resolved)
    }

    /// [`Self::mangle_type_arg_for_generic`] as it reads *after*
    /// [`Self::erase_newtypes_and_flags`]: every `Newtype` collapses to its
    /// ultimate base and every `Flags` to `u32`, recursively through composite
    /// types. Lets pre-erasure synthesis (the `StructField::get` bridge helpers) mint a
    /// name that matches the post-erasure call site, whose `field_ty` reads
    /// through the erasure redirect map.
    pub fn mangle_type_arg_erased(&self, id: TypeId) -> String {
        match self.get(id) {
            ResolvedType::Newtype { .. } | ResolvedType::Flags { .. } => {
                self.mangle_type_arg_erased(self.get_ultimate_base_type(id))
            }
            ResolvedType::GenericInstance { def, type_args } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_arg_erased(*t))
                    .collect();
                let name = self.def_name(*def);
                if Self::is_tuple_type(name) {
                    return crate::name::mangle_tuple_type(&args);
                }
                let unqualified =
                    crate::name::mangle_generic_name(&self.decl_render_name(*def), &args);
                format!("{}/{unqualified}", self.def_module(*def))
            }
            ResolvedType::Ref(inner) => format!("&{}", self.mangle_type_arg_erased(*inner)),
            ResolvedType::MutRef(inner) => format!("&mut {}", self.mangle_type_arg_erased(*inner)),
            ResolvedType::BuiltinArray(elem) => {
                crate::name::mangle_builtin_array_type(&self.mangle_type_arg_erased(*elem))
            }
            _ => self.mangle_type_arg_for_generic(id),
        }
    }

    /// The base type name without type arguments: the head's own name, so
    /// `Option<String>` and a monomorphized `Option<String>` both answer
    /// `"Option"`. Everything else falls back to `mangle_type_name`.
    #[must_use]
    pub fn base_type_name(&self, id: TypeId) -> String {
        match self.get(id) {
            ResolvedType::GenericInstance { def, .. } => self.def_name(*def).to_string(),
            ResolvedType::Struct { def, .. } => self.struct_head_name(*def),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => self.base_type_name(*inner),
            _ => self.mangle_type_name(id),
        }
    }

    /// The name a struct is *stored* under in the package's struct list: the
    /// head left bare, with module disambiguation carried alongside as a
    /// `ModuleSource` rather than folded in — unlike every mangler, because that
    /// is how the list is keyed. It holds one entry per instantiation, so an
    /// instantiation is spelled with its arguments. `None` if not struct-shaped.
    #[must_use]
    pub fn struct_list_name(&self, id: TypeId) -> Option<String> {
        match self.get(id) {
            ResolvedType::Struct { def, type_args } => {
                Some(self.struct_rendered_name(*def, type_args))
            }
            ResolvedType::GenericInstance { def, type_args } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_arg_for_generic(*t))
                    .collect();
                let name = self.def_name(*def);
                Some(if Self::is_tuple_type(name) {
                    crate::name::mangle_tuple_type(&args)
                } else {
                    crate::name::mangle_generic_name(&self.decl_render_name(*def), &args)
                })
            }
            _ => None,
        }
    }

    /// The receiver `id`'s impl blocks are indexed under, with the reference
    /// kind lifted out.
    ///
    /// The head carries its declaring module, so two modules declaring the same
    /// simple name index apart. Consumers pick the namespace they need:
    /// [`crate::name::Receiver::decl_key`] for the name an `impl` header
    /// writes, [`crate::name::Receiver::head_key`] for the mangled identity.
    #[must_use]
    pub fn impl_receiver_key(&self, id: TypeId) -> crate::name::Receiver {
        use crate::name::{FqTypeName, Receiver};
        let declared =
            |def: crate::defs::DefId| Receiver::Type(FqTypeName::declared(&self.defs, def));
        let builtin = |name: &str| Receiver::Type(FqTypeName::builtin(name));
        // Unerased: which impls a type has is a fact about its identity, and
        // erasure rewrites a newtype / flags id to the representation it is
        // stored as, whose impls are a different set.
        match self.get_unerased(id) {
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) => {
                crate::name::RefKind::from_resolved(self.get(id))
                    .map_or_else(|| builtin(""), Receiver::Ref)
            }
            ResolvedType::Struct { def, .. } => Receiver::Type(self.fq_struct_head(*def)),
            // A newtype's head is its declaration, arguments never spelled into
            // it — an `impl` header writes `MyArray`, not `MyArray<i32>`.
            ResolvedType::Enum { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Flags { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::GenericInstance { def, .. } => declared(*def),
            // A generic resource and a binder name no declaration of their own.
            ResolvedType::GenericResource { def, .. } => builtin(self.def_name(*def)),
            ResolvedType::TypeParam { name, .. } => Receiver::Type(FqTypeName::binder(name)),
            ResolvedType::BuiltinArray(_) => builtin(Self::ARRAY_TYPE_NAME),
            ResolvedType::Unit => builtin(Self::UNIT_TYPE_NAME),
            ResolvedType::Primitive(prim) => builtin(prim.as_str()),
            ResolvedType::Function { .. } => builtin(crate::name::CLOSURE_FN_TRAIT),
            _ => builtin(&self.base_type_name(id)),
        }
    }

    /// The declaration a type's head names, with any arguments dropped.
    #[must_use]
    pub fn fq_base_type_name(&self, id: TypeId) -> crate::name::FqTypeName {
        use crate::name::FqTypeName;
        match self.get(id) {
            ResolvedType::Struct { def, .. } => self.fq_struct_head(*def),
            ResolvedType::Enum { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::Flags { def }
            | ResolvedType::Resource { def }
            | ResolvedType::GenericInstance { def, .. } => FqTypeName::declared(&self.defs, *def),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.fq_base_type_name(*inner)
            }
            ResolvedType::BuiltinArray(_) => FqTypeName::builtin(Self::ARRAY_TYPE_NAME),
            ResolvedType::Unit => FqTypeName::builtin(Self::UNIT_TYPE_NAME),
            ResolvedType::Function { .. } => FqTypeName::builtin(crate::name::CLOSURE_FN_TRAIT),
            // Tuples, primitives and function types are builtin shapes: no
            // module declares them and every mangler spells them bare.
            _ => FqTypeName::builtin(&self.base_type_name(id)),
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
            // No recovery: a struct carries its arguments, so an empty list
            // means it is a declaration rather than an instantiation.
            ResolvedType::Struct { type_args, .. } if !type_args.is_empty() => {
                Some(type_args.clone())
            }
            _ => None,
        }
    }

    /// The structured fq name of `id`. The type table is the only thing that
    /// knows a declaration's module, so it hands back structure and lets the
    /// caller render or inspect. Rendering here would be one-way — a
    /// `ModuleSource` cannot be rebuilt from a string without the interner — so
    /// the name stays structured all the way to its consumers.
    #[must_use]
    pub fn fq_type_name(&self, id: TypeId) -> crate::name::FqTypeName {
        use crate::name::FqTypeName;
        let args_of = |type_args: &[TypeId]| -> Vec<FqTypeName> {
            type_args.iter().map(|t| self.fq_type_name(*t)).collect()
        };
        match self.get(id) {
            ResolvedType::Primitive(prim) => FqTypeName::builtin(prim.as_str()),
            ResolvedType::Unit => FqTypeName::builtin(Self::UNIT_TYPE_NAME),
            ResolvedType::Never => FqTypeName::builtin("!"),
            // Head and arguments come straight off the type — the same shape
            // every other instantiated type has. No recovery step, because
            // there is no fused spelling left to recover them from.
            ResolvedType::Struct { def, type_args } => {
                self.fq_struct_head(*def).with_args(args_of(type_args))
            }
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::Flags { def } => FqTypeName::declared(&self.defs, *def),
            ResolvedType::TypeParam { name, .. } => FqTypeName::binder(name),
            ResolvedType::GenericInstance { def, type_args } => {
                let args = args_of(type_args);
                if Self::is_tuple_type(self.def_name(*def)) {
                    FqTypeName::tuple(args)
                } else {
                    FqTypeName::declared(&self.defs, *def).with_args(args)
                }
            }
            ResolvedType::GenericResource { def, type_args } => {
                FqTypeName::builtin(self.def_name(*def)).with_args(args_of(type_args))
            }
            ResolvedType::BuiltinArray(elem) => {
                FqTypeName::builtin(Self::ARRAY_TYPE_NAME).with_args(vec![self.fq_type_name(*elem)])
            }
            ResolvedType::Ref(inner) => self
                .fq_type_name(*inner)
                .with_reference(crate::name::RefKind::Shared),
            ResolvedType::MutRef(inner) => self
                .fq_type_name(*inner)
                .with_reference(crate::name::RefKind::Mut),
            // Shapes that name no declaration — assoc-type projections, packs,
            // `Unknown`. They carry no module, so the rendered spelling is
            // already their whole identity.
            _ => FqTypeName::builtin(&self.mangle_type_name(id)),
        }
    }

    fn get_type_name_info(&self, id: TypeId) -> TypeNameInfo {
        match self.get(id) {
            ResolvedType::Primitive(prim) => TypeNameInfo::Primitive(prim.as_str().to_string()),
            ResolvedType::Unit => TypeNameInfo::Unit,
            // A declared type is named by its declaring module too: two modules
            // may declare the same simple name, and a mangled name that omits
            // the module collapses them onto one identity — the hazard
            // `mangle_type_arg_for_generic` documents, reached from here as
            // well.
            // The identity every mangled name embeds, so the rendered
            // spelling: each instantiation is its own type.
            ResolvedType::Struct { def, type_args } => TypeNameInfo::Named(format!(
                "{}/{}",
                self.struct_head_module(*def),
                self.struct_rendered_name(*def, type_args)
            )),
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::Flags { def } => {
                TypeNameInfo::Named(format!("{}/{}", self.def_module(*def), self.def_name(*def)))
            }
            // A type parameter is a template's own binder, not a declaration.
            ResolvedType::TypeParam { name, .. } => TypeNameInfo::Named(name.clone()),
            ResolvedType::InferVar(var) => TypeNameInfo::Named(var.to_string()),
            ResolvedType::GenericInstance { def, type_args } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_arg_for_generic(*t))
                    .collect();
                let name = self.def_name(*def);
                let module_source = self.def_module(*def);
                // A tuple is module-independent; its elements stay qualified.
                if Self::is_tuple_type(name) {
                    return TypeNameInfo::Tuple(args);
                }
                TypeNameInfo::Generic {
                    name: format!("{module_source}/{}", self.decl_render_name(*def)),
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
            ResolvedType::GenericResource { def, type_args } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_arg_for_generic(*t))
                    .collect();
                TypeNameInfo::Generic {
                    name: self.def_name(*def).to_string(),
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
            ResolvedType::Never => TypeNameInfo::Named("!".to_string()),
            ResolvedType::Unknown | ResolvedType::Error => TypeNameInfo::Unknown,
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
        } else {
            let path = self.module_source.to_path();
            format!("{}/{}", path.join("/"), self.name)
        }
    }

    /// Get the builtin function name if this is a builtin call.
    /// Returns the qualified name (e.g., "`builtin::array_len`").
    ///
    /// Functions declared in `core:builtin` and functions synthesised
    /// from wasm-asset exports (`ModuleSource::Wasm`) both go through
    /// the import-style builtin lowering — they share `#[canonical(...)]`
    /// metadata in `BuiltinRegistry` and resolve to the same wasm
    /// import call shape.
    pub fn builtin_name(&self) -> Option<String> {
        if self.monomorph_info.is_some() {
            return None;
        }
        if self.module_source.is_core_builtin() || self.module_source.is_wasm_asset() {
            Some(format!("builtin::{}", self.name))
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
            "array_get" | "array_get_ref" | "array_get_mut_ref" | "array_set" | "array_new"
            | "array_len" | "array_copy" | "array_fill" | "array_clone" | "array_clone_prefix"
            | "select" | "copy_value" | "is_uninitialized" | "black_box" => {
                Some(format!("builtin::{generic_name}"))
            }
            _ => None,
        }
    }

    /// Check if this function is monomorphized (instantiated from a generic)
    pub fn is_monomorphized(&self) -> bool {
        self.monomorph_info.is_some()
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

impl TirExprKind {
    /// Build `recv.m(args)`: a [`TirExprKind::Call`] whose receiver heads the
    /// argument list, so `args[i]` maps to the callee's `params[i]`.
    ///
    /// Callers are expected to have typechecked `args` against the callee's
    /// declared parameter types before reaching here.
    ///
    /// The receiver's `is_mut` is left `false`; `lower` fills the real value in
    /// from the callee's `self` parameter, which is where every consumer of it
    /// lives.
    pub(crate) fn method_call(
        receiver: Box<TirExpr>,
        func: FunctionRef,
        type_args: Vec<TypeId>,
        args: Vec<CallArg>,
    ) -> Self {
        let mut all = Vec::with_capacity(args.len() + 1);
        all.push(CallArg::new(*receiver, false));
        all.extend(args);
        Self::Call {
            func: Box::new(func),
            type_args,
            args: all,
            has_receiver: true,
        }
    }

    /// An instance-method call viewed as receiver plus the arguments after it.
    /// `None` for a free call.
    ///
    /// The split is a view, not storage: the node keeps one argument list in the
    /// callee's parameter order, so a pass that treats every argument alike
    /// (traversal, substitution, type-arg rewriting) matches `Call` directly and
    /// never needs this.
    pub fn as_method_call(&self) -> Option<(&TirExpr, &FunctionRef, &[CallArg])> {
        let TirExprKind::Call {
            func,
            args,
            has_receiver: true,
            ..
        } = self
        else {
            return None;
        };
        let (receiver, rest) = args.split_first()?;
        Some((&receiver.expr, func, rest))
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
    /// Byte array literal from `#include_bytes`. Lowered to `List<u8>` via data segment.
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
        /// Type arguments pinned for a generic function reference (either via
        /// turbofish `identity::<i32>` or inferred from an expected `fn(...)`
        /// type at the use site). Empty for non-generic function references.
        /// Consumed by the monomorphizer, which queues the corresponding
        /// instantiation and rewrites `name` to the mangled form.
        type_args: Vec<TypeId>,
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

    /// Every call: free function (`foo(args)`), static method
    /// (`Type::method(args)`), and instance method (`recv.method(args)`), which
    /// all map to the same `WirInstr::Call` and share identical semantics.
    Call {
        /// Boxed: unboxed, it dominates the enum by ~290 bytes (clippy's
        /// `large_enum_variant`).
        func: Box<FunctionRef>,
        /// Explicit type arguments for generic functions: `identity::<i32>(x)`
        type_args: Vec<TypeId>,
        /// Arguments in the callee's parameter order — a method's receiver is
        /// `args[0]`, so `args[i]` maps to `params[i]` for every call shape.
        args: Vec<CallArg>,
        /// Whether `args[0]` is the receiver of an instance method, set by
        /// `TirExprKind::method_call` alone — so it marks dot syntax. A
        /// trait-qualified (UFCS) call carries its receiver in `args[0]` too but
        /// leaves this `false`: it spells the receiver's mode itself
        /// (`Trait::m(&mut x, …)`), so the receiver is already reference-typed
        /// and needs none of the treatment this flag gates, notably `lower`'s
        /// never-value-copy-a-receiver rule.
        has_receiver: bool,
    },
    /// Raw Component Model call to a lowered WASI import or a canonical built-in.
    ///
    /// Used inside synthesized CM binding functions to call the flat-ABI function
    /// directly, bypassing the normal effect call mechanism. Args are already lowered
    /// to flat CM types (i32, i64, f32, f64).
    CmRawCall {
        /// Which function this calls, by identity rather than by rendered name.
        target: CmCallTarget,
        /// Flat ABI arguments (already lowered to core Wasm types)
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

    /// Spread a tuple expression into an enclosing `TupleLiteral`.
    /// Created by the elaborator for `[..expr]` syntax. Expanded by monomorphization
    /// into individual `FieldAccess` elements once the concrete tuple arity is known.
    TupleSpread {
        expr: Box<TirExpr>,
    },

    /// Transpose a tuple-of-tuples: `[a, b].zip()` → `[[a.0, b.0], [a.1, b.1], ...]`.
    /// Created by the elaborator for the `.zip()` pseudo-method on tuples.
    /// Expanded during monomorphization once concrete tuple arities are known.
    TupleZip {
        expr: Box<TirExpr>,
    },

    /// Compile-time arity of a tuple whose type still contains a type pack
    /// (`[..T].len()`). For a fully concrete tuple the elaborator folds `.len()`
    /// to an integer literal immediately; when a `..T` pack is present the arity
    /// is unknown until monomorphization, so it is deferred here and expanded to
    /// an `IntLiteral` once the concrete arity is known. The `expr` is evaluated
    /// only for its type (tuples are value types with no side effects in `len`).
    TupleLen {
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

    /// Deferred `[for let v of tuple { expr }]` over a pack-typed tuple.
    ///
    /// The body is resolved once with the binding typed as the pack element;
    /// the monomorphizer unrolls it into a tuple literal once the pack is
    /// concrete. See `docs/wep-2026-03-14-variadic-type-parameters.md`.
    VariadicTupleComprehension {
        /// The source tuple (type contains `TypePack` before substitution)
        iterable: Box<TirExpr>,
        /// The element binding's name and local slot
        binding_name: String,
        binding_local: u32,
        /// Sub-bindings of a destructured binding (`[i, v]`), as
        /// field reads off the binding local
        destructure: Vec<TirStmt>,
        /// The per-element expression
        body: Box<TirExpr>,
        /// Unique ID for generating labels
        unique_id: u32,
        /// Whether the binding is the `[index, value]` pair of `.enumerate()`
        is_enumerate: bool,
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
        /// Closure-scope address-taken locals, captured from the
        /// closure's resolution `FunctionContext`. The boxing pass uses
        /// this when descending into the closure body — the body's
        /// `Local { index: N }` references closure locals, not the
        /// parent function's, so the parent's set would mis-box. Empty
        /// for synthesised closures (e.g. effect-handler dispatch),
        /// which never take addresses.
        address_taken_locals: crate::hashmap::IndexSet<u32>,
        /// Body-level let-bindings inside the closure, in declaration order,
        /// occupying `params.len()..` in its local-index namespace (the params
        /// themselves live in `params`). Captured at resolve time so pattern
        /// lowering can seed a closure-scoped allocator without re-walking the
        /// body. Empty for the synthetic closures `synthesis/` creates.
        body_locals: Vec<TirLocal>,
        /// Effects the closure type was annotated with at the use site (let
        /// annotation, function-typed parameter, etc.). `Some` only when the
        /// annotation provides a concrete effect set; the effect checker swaps
        /// to these when entering the body so e.g. `let f: fn() = ||{println}`
        /// rejects the Stdout leak. `None` means "unannotated, inherit outer
        /// effects" (preserves the original behaviour for free closures).
        declared_effects: Option<Vec<EffectRef>>,
    },

    /// Indirect call through a callable value (closure or funcref)
    IndirectCall {
        /// The callee expression (closure struct or funcref)
        callee: Box<TirExpr>,
        /// Arguments to pass to the callee
        args: Vec<TirExpr>,
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

    /// Unresolved template string expression.
    ///
    /// Created by the elaborator with resolved sub-expressions but without
    /// expanding to formatting code. The synthesis phase (pre-monomorphize)
    /// expands this into the `__tmpl` labeled block with `String::with_capacity`,
    /// `push_str`, `Formatter`, and `Display`/inspect calls.
    TemplateString {
        parts: Vec<TirTemplatePart>,
    },

    /// Effect handler installation: `with E1 => h1, ... do { body }`.
    /// See `docs/wep-2026-04-11-effect-handler.md`.
    ///
    /// Each binding installs a handler for one effect for the duration of `body`.
    /// The block evaluates to `body`'s value; in MVP `result_type` is always Unit
    /// because do-block bodies are statement blocks, not expression blocks.
    WithHandler {
        bindings: Vec<TirHandlerBinding>,
        body: TirBlock,
        result_type: TypeId,
    },

    /// `resume value` — control-flow expression valid only inside an effect
    /// handler method body.
    ///
    /// In the MVP (no post-resume code), `resume` lowers to `Return { value }`.
    /// The expression itself is typed as `Unit` because it does not produce a
    /// value to its enclosing expression — it transfers control out.
    Resume {
        value: Box<TirExpr>,
    },
}

/// One `Effect => handler` binding inside a `with ... do` block.
#[derive(Debug, Clone)]
pub struct TirHandlerBinding {
    /// The effect being handled. The elaborator always fills this in with a
    /// concrete effect reference — the bundled `with &mut h do` form is
    /// expanded to one binding per implemented effect, each carrying a
    /// concrete `EffectRef`. `None` only appears transiently when an
    /// upstream diagnostic prevented resolution.
    pub effect: Option<EffectRef>,
    /// Concrete `TypeId`s of the trait / resource type arguments at this
    /// installation site (e.g. `[u8]` for `with Stream<u8> => &mut s do`,
    /// or as derived from the impl block in a bundled `with &mut s do`
    /// where `MockCM` implements `Stream<u8>`). Empty for non-generic
    /// effects / resources. The dispatch synthesis projects this together
    /// with `effect.module_source` and `effect.name` into the
    /// `InstantiationKey` it uses to look up the per-monomorphisation
    /// dispatch infrastructure.
    pub trait_type_args: Vec<TypeId>,
    /// Handler value expression (e.g., `&mut mock`).
    pub handler: TirExpr,
    /// The concrete struct type (after deref) implementing the effect.
    /// Used by codegen to pick the correct `impl E for T` methods.
    pub handler_type: TypeId,
    pub span: Span,
    /// `Some(id)` marks a binding from a bundled `with &mut h do` expansion; all
    /// bindings from one clause share the id, so dispatch synthesis can allocate
    /// a single `__h_<bundle>` local that every per-effect closure captures —
    /// the handler is evaluated once and one effect's mutations are seen by the
    /// rest. `None` for an explicit `Effect => handler`. Unique per `WithHandler`.
    pub bundle_group: Option<u32>,
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

/// Value-yielding type of a block: the last statement decides, except that a
/// two-branch `If` needs its branches to agree (or one to be `Never`)
/// and falls back to `Unit`, and a diverging `Return` / `Break` / `Continue`
/// yields `Never`. The elaborator enforces the agreement rule while typing the
/// surrounding expression, so a mismatch here is already reported.
pub fn block_result_type(block: &TirBlock) -> TypeId {
    block
        .stmts
        .last()
        .and_then(|s| match &s.kind {
            TirStmtKind::Expr(e) => Some(e.type_id),
            TirStmtKind::If {
                then_block,
                else_block: Some(else_block),
                ..
            } => agree_branch_types(block_result_type(then_block), block_result_type(else_block)),
            TirStmtKind::Return { .. } | TirStmtKind::Break { .. } | TirStmtKind::Continue => {
                Some(TypeTable::NEVER)
            }
            _ => None,
        })
        .unwrap_or(TypeTable::UNIT)
}

/// Combine two branch result types under the elaborator's rule:
/// equal types agree; a `Never` branch defers to the other; an
/// outright mismatch yields `None` so the caller falls back to
/// `Unit`.
pub(crate) fn agree_branch_types(t: TypeId, e: TypeId) -> Option<TypeId> {
    if t == e {
        Some(t)
    } else if t == TypeTable::NEVER {
        Some(e)
    } else if e == TypeTable::NEVER {
        Some(t)
    } else {
        None
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
        /// When the iterable is `&[..T]` (a reference to a variadic tuple),
        /// each element is bound by reference (`&T_k`), matching the
        /// `for v of &list` refiter semantics. The binding is resolved with
        /// type `&TypePack`; expansion wraps each element field in `&`.
        by_ref: bool,
        /// When the iterable is `tuple.enumerate()`, the binding is the pair
        /// `[i32, T_k]`; expansion pairs each element with its index literal.
        is_enumerate: bool,
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
    /// For a pack param bound by projection — `impl<T: ReflectStruct<FieldTypes = [..F]>,
    /// ..F: Trait>` — records `(source param index, assoc type name)`. The pack
    /// is not supplied by the caller; monomorphization derives it by resolving
    /// the source param's associated type (e.g. `T::Fields`) to its tuple.
    pub projected_from: Option<(u32, String)>,
}

/// What a use site knows about the associated types projected from a slot,
/// beyond the type filling it: slot index → `[(associated-type name, what it
/// means here)]`.
///
/// The companion of the slot substitution in
/// [`TypeTable::substitute_type_params_with`]. A declaration resolves
/// `Self::Item` in its own frame, where it can only be a projection; what it
/// stands for is written at the use site (`I: IntoIterator<Item = u8>`).
pub type SlotProjections = IndexMap<u32, Vec<(String, TypeId)>>;

/// Substitution-key base for method-level type params: past the highest
/// impl-param *index*, not the count. A concrete type in a receiver slot
/// (`String` in `impl<V> ... for TreeMap<String, V>`) is not a param, so a later
/// param keeps a sparse index the count would undershoot, colliding a method
/// param onto an impl slot. Elaboration and monomorphization both derive it here.
#[must_use]
pub fn method_param_offset(impl_type_params: &[TirTypeParam]) -> u32 {
    impl_type_params
        .iter()
        .map(|p| p.index + 1)
        .max()
        .unwrap_or(0)
}

/// Information about monomorphization origin for instantiated items
#[derive(Debug, Clone)]
pub struct MonomorphInfo {
    /// Original generic name: `"Box"` for `"Box<i32>"`, or
    /// `"BTreeNode<K,V>::insert"` for methods.
    pub generic_name: String,
    /// Impl-level type arguments (from the struct/type, e.g. `[i32]` for `List<i32>`)
    pub impl_type_args: Vec<TypeId>,
    /// Method-level type arguments (from the method's own generics, e.g. `[String]` for `.transform::<String>()`)
    pub method_type_args: Vec<TypeId>,
    /// Whether this originates from a blanket impl (e.g., `impl<I: Iterator> IntoIterator for I`)
    pub is_blanket: bool,
}

/// Whether a function identifies as the core builtin `builtin`, matching both
/// the plain generic form (`name`) and a monomorphized instance whose `name` is
/// mangled but whose `monomorph_info.generic_name` is the base name. A name
/// check that only compares `name` silently misses monomorphized builtins.
pub fn matches_builtin(name: &str, monomorph_info: Option<&MonomorphInfo>, builtin: &str) -> bool {
    name == builtin || monomorph_info.is_some_and(|m| m.generic_name == builtin)
}

/// A `#[param]` compile-time parameter declared on a `global`.
///
/// Carried from reify (which validates the attribute shape) to the
/// param-resolution pass (which resolves an override and rewrites the
/// initializer). See `wep-2026-04-26-compile-time-params.md`.
#[derive(Debug, Clone)]
pub struct ParamSpec {
    /// Parameter name matched against `-D NAME=value`. Defaults to the
    /// global's identifier; overridden by `#[param(name = "...")]`.
    pub name: String,
    /// Environment variable read at compile time, from
    /// `#[param(from_env = "...")]`. Independent of `name`.
    pub from_env: Option<String>,
}

/// How a global's storage gets its value.
///
/// One choice rather than a flag beside an initializer field, so a placeholder
/// can never be read as the declared value — it is a plausible constant, and
/// mistaking it folds every read of `global A: i32 = 1 + 2` to zero.
///
/// See [Global Variables](../../docs/wep-2026-01-27-global-variables.md).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalInit<E> {
    /// The storage holds the declared value, as a constant expression.
    Direct(E),
    /// The storage starts at this placeholder; the module's initialization
    /// function assigns the declared value before anything else runs.
    Deferred(E),
}

impl<E> GlobalInit<E> {
    /// What the storage is initialized with: the declared value, or the
    /// placeholder standing in for it.
    pub fn slot_expr(&self) -> &E {
        match self {
            Self::Direct(e) | Self::Deferred(e) => e,
        }
    }

    pub fn slot_expr_mut(&mut self) -> &mut E {
        match self {
            Self::Direct(e) | Self::Deferred(e) => e,
        }
    }

    /// The declared value, or `None` when it is assigned elsewhere. Anything
    /// asking what a global holds goes through this.
    pub fn declared(&self) -> Option<&E> {
        match self {
            Self::Direct(e) => Some(e),
            Self::Deferred(_) => None,
        }
    }

    pub fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred(_))
    }
}

/// Global variable declaration in TIR
#[derive(Debug, Clone)]
pub struct TirGlobal {
    pub name: String,
    pub ty: TypeId,
    pub init: GlobalInit<TirExpr>,
    /// `Some` when the global carries a `#[param]` attribute. Drives the
    /// param-resolution pass; `None` for ordinary globals.
    pub param: Option<ParamSpec>,
    /// Whether the program may assign to this global — `global mut`. The Wasm
    /// slot's mutability is wider and derived when the module is built.
    pub wado_mutable: bool,
    pub visibility: crate::ast::Visibility,
    /// Module where this global is defined
    pub module_source: ModuleSource,
    pub span: Span,
    /// Per-local metadata for the initializer expression. Populated when
    /// the initializer is non-trivial (e.g., `SequenceLiteralBuilder`
    /// coercion). Indexed by local index, like `TirFunction::locals`.
    pub locals: Vec<TirLocal>,
}

#[derive(Debug, Clone)]
pub struct TirFunction {
    pub name: String,
    /// Module this function belongs to. Set by the link phase when flattening
    /// per-module TIR into flat lists; before link, the `module_source` is
    /// carried implicitly by the parent `TirModule`.
    pub module_source: ModuleSource,
    pub visibility: crate::ast::Visibility,
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
    /// Declared return type for `async fn` (where `return_type` is erased to unit
    /// because the result is delivered via `task return`). `None` for non-async fns
    /// and for CM-binding / synthesized wrappers. Preserved so the effect checker
    /// can infer signature resources from the user-visible return type.
    pub task_return_type: Option<TypeId>,
    pub effects: Vec<EffectRef>,
    /// Parameter names declared in `stores[...]` — the function may store these references.
    pub stores: Vec<String>,
    pub body: Option<TirBlock>,
    pub span: Span,
    pub local_count: u32,
    /// Per-local metadata — `name`, `type_id`, `is_mut` — indexed by Wasm
    /// local index. Entries `0..params.len()` shadow the corresponding
    /// `params[i]` (for uniform absolute indexing); body let-bindings and
    /// elaborator/optimizer-allocated temporaries occupy `params.len()..`.
    /// `local_count == locals.len()` post-resolve; passes that grow the
    /// local set must keep the two in sync.
    pub locals: Vec<TirLocal>,
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

    /// Whether this function is a synthesised effect-dispatch wrapper
    /// (generated by `synthesis::effect_dispatch`). Effect-operation
    /// call-site rewriting must skip these — their fallback path
    /// directly calls `__cm_binding__<E>_<op>`, which would loop back
    /// through the wrapper if rewritten.
    pub is_dispatch_wrapper: bool,

    /// Whether this function is a synthesized CM *export* binding (world export wrapper).
    /// When true, the global initializer (`__initialize_modules`) is injected at the start
    /// of this function's body during lowering.
    pub is_cm_export: bool,

    /// Whether this function is marked `#[ambient]`. Ambient functions are implicitly
    /// available to callers without requiring matching `with` clauses — they still carry
    /// interface declarations for documentation / implementation purposes, but the effect
    /// checker does not propagate those requirements to callers.
    pub is_ambient: bool,

    /// Effects from `#[benign(E)]`. The checker admits each one in the body
    /// without a `with E` clause and never propagates it to callers. Unlike
    /// `is_ambient`, only the listed effects are suppressed; the world import
    /// for `E` is still required since the body references the operation.
    pub benign_effects: Vec<EffectRef>,

    /// Inline hint from `#[inline]`, `#[inline(always)]`, or `#[inline(never)]` attributes.
    pub inline_hint: InlineHint,

    /// The compiler-recognized stdlib role this function fills, if any.
    /// Set from `#[compiler_item("...")]` on the source declaration; see
    /// [`crate::compiler_item::CompilerItem`].
    pub compiler_item: Option<crate::compiler_item::CompilerItem>,

    /// Custom wasm export name from `#[export_name("...")]` attribute.
    pub export_name: Option<String>,

    /// Allocator tag from `#[allocator("...")]` attribute (e.g., `"bump"`, `"debug"`).
    pub allocator_tag: Option<String>,

    /// Categorizes the function for kind-specific optimizations. Most functions
    /// are `Regular`; synthesis passes set specialized kinds so the TIR
    /// optimizer can apply targeted transformations (e.g. freshness-based
    /// elision for `ValueCopy`).
    pub kind: FunctionKind,

    /// ABI for delivering the function's return value at WIR / Wasm level.
    /// Defaults to [`ReturnAbi::Single`]; an analysis pass sets
    /// [`ReturnAbi::MultiValue`] for tuple- or user-struct-returning
    /// functions whose every call site destructures the result via
    /// `FieldAccess` and whose body's returns produce a fresh
    /// `TupleLiteral` / `StructLiteral`. WIR build then emits a
    /// multi-value Wasm result signature (no heap struct round-trip).
    pub return_abi: ReturnAbi,
}

/// How a function delivers its return value at the Wasm level.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReturnAbi {
    /// Single Wasm return value. The function's TIR `return_type` is taken
    /// as-is; tuple / user-struct types lower to a heap struct ref.
    #[default]
    Single,
    /// Multi-value Wasm return: each tuple element / struct field becomes a
    /// separate Wasm result. Carries the per-element TIR type ids and field
    /// names for WIR-build's signature emission and call-site split-local
    /// generation. The function's TIR `return_type` is unchanged (it remains
    /// the tuple / struct type) — only the WIR-level ABI shifts.
    ///
    /// For tuple returns, `field_names` is `["0", "1", ...]` (matching the
    /// numeric field names tuple structs carry). For user-struct returns,
    /// `field_names` is the struct's fields in declaration order.
    MultiValue {
        /// TIR types of each result, in declaration order.
        result_types: Vec<TypeId>,
        /// Field names matching the source aggregate's declaration order.
        /// Used by WIR build to look up the right split local from a
        /// `FieldAccess` access on a multi-value-bound temp.
        field_names: Vec<String>,
    },
}

/// Semantic category of a `TirFunction`. Carries the type operand so the
/// optimizer can reason about the call without re-deriving it from the
/// signature.
/// Identifies which `Fn<N, Ret>` trait method an auto-derived
/// dispatch stub implements. Recovered from
/// [`FunctionKind::FnCanonicalDispatch`] so WIR build can choose
/// the right vtable slot (`inspect` vs `inspect_alt`) without
/// re-parsing mangled names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnDispatchTrait {
    Inspect,
    InspectAlt,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FunctionKind {
    /// Ordinary user-defined or synthesized function.
    #[default]
    Regular,
    /// Synthesized `copy_value` function that deep-copies a value of
    /// `type_id`. Calls to such functions may be elided when the argument is
    /// provably fresh.
    ValueCopy { type_id: TypeId },
    /// Auto-derived `Fn<arity, return_type>^Inspect::inspect` dispatch stub (or
    /// its `^InspectAlt` twin). The TIR body is `unreachable()` — enough for the
    /// function to be registered and the call resolvable — and WIR build
    /// supplies the real one, a `call_ref` through `CanonicalClosure_K`'s vtable
    /// slot. `(arity, return_type)` are structured so nobody parses the mangle.
    FnCanonicalDispatch {
        trait_kind: FnDispatchTrait,
        arity: usize,
        return_type: TypeId,
    },
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

    /// Returns true if this is the synthesized `__call` method on a
    /// `__Closure_N` functor struct. See
    /// [`LocalMethodName::is_closure_call`] for the rationale.
    #[inline]
    pub fn is_closure_call(&self) -> bool {
        self.method_info
            .as_ref()
            .is_some_and(super::name::LocalMethodName::is_closure_call)
    }

    /// Returns true if this function has type params that need monomorphization
    /// (excludes effect params, which are erased at compile time).
    #[inline]
    pub fn has_real_type_params(&self) -> bool {
        self.type_params.iter().any(|p| !p.is_effect)
    }

    /// Returns the copied type if this is a synthesized value-copy function.
    #[inline]
    pub fn value_copy_type(&self) -> Option<TypeId> {
        match self.kind {
            FunctionKind::ValueCopy { type_id } => Some(type_id),
            _ => None,
        }
    }

    /// Returns the dispatch coordinates if this is an auto-derived
    /// `Fn<arity, return_type>^Inspect` / `^InspectAlt` stub.
    /// WIR build uses the result to supply the indirect-call body
    /// without scanning mangled function names.
    #[inline]
    pub fn fn_canonical_dispatch(&self) -> Option<(FnDispatchTrait, usize, TypeId)> {
        match self.kind {
            FunctionKind::FnCanonicalDispatch {
                trait_kind,
                arity,
                return_type,
            } => Some((trait_kind, arity, return_type)),
            _ => None,
        }
    }

    /// Returns true if this function was synthesized as a value-copy helper.
    #[inline]
    pub fn is_value_copy(&self) -> bool {
        matches!(self.kind, FunctionKind::ValueCopy { .. })
    }
}

/// A resolved local slot in a function, global initializer, or closure scope.
/// `FunctionContext::locals` is the single source of truth — every parameter,
/// `let`, destructure binding and elaborator temporary — and is projected onto
/// `TirFunction::locals` / `TirGlobal::locals` (keyed by Wasm local index) and
/// onto `Closure { body_locals }`, whose params stay in `params` instead.
#[derive(Debug, Clone)]
pub struct TirLocal {
    /// Source-level name of the binding (or a synthesised `__name` for
    /// elaborator-generated temporaries that have no surface syntax).
    pub name: String,
    pub type_id: TypeId,
    pub is_mut: bool,
}

impl TirLocal {
    /// Build a `TirLocal` for a synthesised slot whose name follows the
    /// `__local_N` convention used by `wir_build` when no source-level
    /// name is available.
    pub fn synth(index: u32, type_id: TypeId, is_mut: bool) -> Self {
        Self {
            name: format!("__local_{index}"),
            type_id,
            is_mut,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TirParam {
    pub name: String,
    pub type_id: TypeId,
    pub local_index: u32,
    pub is_mut: bool,
    /// The parameter is a `&mut T` borrow (captured here before the boxing
    /// plan rewrites `&mut T` and `&T` to the same `Box<T>` type, erasing the
    /// distinction). A `&T` cannot be written through, so only a `&mut`
    /// parameter can mutate the caller's argument storage.
    pub is_mut_ref: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirStruct {
    /// The struct type this reifies, as its head plus what it was
    /// instantiated with — the same pair `ResolvedType::Struct` carries, so a
    /// consumer keys on it instead of re-rendering a spelling to match one
    /// built elsewhere.
    pub def: StructDef,
    pub type_args: Vec<TypeId>,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: crate::ast::Visibility,
    /// Generic type parameters (empty for non-generic structs)
    pub type_params: Vec<TirTypeParam>,
    /// If this struct was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub fields: Vec<TirField>,
    pub span: Span,
    /// `#[wire(name_policy = "...")]` — naming strategy for all fields.
    pub wire_name_policy: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TirField {
    pub name: String,
    pub visibility: crate::ast::Visibility,
    pub type_id: TypeId,
    pub index: u32,
    pub span: Span,
    /// `#[secret]` — field not shown in debug inspect output.
    pub is_secret: bool,
    /// `#[wire(name = "name")]` — custom serialization name for this field.
    pub wire_name_override: Option<String>,
    /// `#[wire(default)]` — use default value when field is missing during deserialization.
    pub serde_default: bool,
    /// `#[wire(positional)]` — field is resolved by position, not by name.
    /// Format-agnostic ordinal hint: synthesized `FieldSchema::lookup` omits it
    /// (never matched by name) and `positional_at` enumerates it. Name-only and
    /// sequence-only formats ignore it; `core:args` binds it to a bare token.
    pub serde_positional: bool,
    /// Resolved default expression for `struct S { x: T = expr }`.
    /// Inserted by the elaborator when the field is omitted in a struct literal.
    pub default_expr: Option<Box<TirExpr>>,
}

#[derive(Debug, Clone)]
pub struct TirEnum {
    /// The declaration this was reified from.
    pub def: crate::defs::DefId,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: crate::ast::Visibility,
    /// Generic type parameters (empty for non-generic enums)
    pub type_params: Vec<TirTypeParam>,
    /// If this enum was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub cases: Vec<TirEnumCase>,
    pub span: Span,
    /// `#[wire(name_policy = "...")]` — naming strategy for all cases.
    pub wire_name_policy: Option<String>,
}

/// A case in a TIR enum.
/// Unlike `TirVariantCase`, enum cases have no payload.
#[derive(Debug, Clone)]
pub struct TirEnumCase {
    pub name: String,
    pub index: u32,
    pub span: Span,
    /// `#[wire(name = "...")]` — custom serialized name for this case.
    pub wire_name_override: Option<String>,
}

/// A flags type declaration (bitmask type, like WIT flags)
/// e.g., `flags PathFlags { SymlinkFollow }`
/// Represented as `ResolvedType::Flags`; each member is a bitmask value (1 << index).
#[derive(Debug, Clone)]
pub struct TirFlags {
    /// The declaration this was reified from.
    pub def: crate::defs::DefId,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: crate::ast::Visibility,
    /// The newtype `TypeId` (base type is u32)
    pub type_id: TypeId,
    pub members: Vec<TirFlagsMember>,
    pub span: Span,
    pub wire_name_policy: Option<String>,
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
    /// The declaration this was reified from. Case indices are looked up
    /// through it, so a same-named variant in another module cannot answer.
    pub def: crate::defs::DefId,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: crate::ast::Visibility,
    /// Generic type parameters (e.g., `T` in `variant Option<T>`)
    pub type_params: Vec<TirTypeParam>,
    /// Cases of the variant (e.g., Some, None for Option)
    pub cases: Vec<TirVariantCase>,
    pub span: Span,
    /// `#[wire(name_policy = "...")]` — naming strategy for all cases.
    pub wire_name_policy: Option<String>,
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
    /// `#[wire(name = "...")]` — custom serialized name for this case.
    pub wire_name_override: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TirNewtype {
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: crate::ast::Visibility,
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
    pub is_synopsis: bool,
}

#[derive(Debug, Clone)]
pub struct TirEffect {
    pub name: String,
    pub visibility: crate::ast::Visibility,
    pub operations: Vec<TirEffectOp>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TirEffectOp {
    pub name: String,
    pub params: Vec<TirParam>,
    pub return_type: TypeId,
    pub span: Span,
    /// CM canonical name from `#[cm("...")]` on the resource method
    /// declaration (e.g. `"stream-write"`, `"future-read"`). `None` for
    /// effect operations and for resource methods that don't carry a
    /// CM attribute. The dispatch synthesis uses this to map raw
    /// resource call sites — which carry `cm_name` on their
    /// `MethodInfo` — back to the right per-monomorphisation wrapper.
    pub cm_name: Option<String>,
    pub is_async: bool,
}

/// Resource declaration captured in TIR for effect propagation.
///
/// Resources are effects in Wado's effect system: every operation on a
/// resource type requires the resource to be in scope. The `operations`
/// list mirrors `TirEffect` so the propagation closure builder can treat
/// effects and resources uniformly.
#[derive(Debug, Clone)]
pub struct TirResource {
    /// The declaration this was reified from.
    pub def: crate::defs::DefId,
    pub name: String,
    pub visibility: crate::ast::Visibility,
    pub operations: Vec<TirEffectOp>,
    pub is_generic: bool,
    pub span: Span,
}

/// Trait declaration
#[derive(Debug, Clone)]
pub struct TirTrait {
    pub name: String,
    pub visibility: crate::ast::Visibility,
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

/// Which compiler-synthesizable trait an `impl Trait for Type;` request names.
///
/// The set is closed: the elaborator classifies the requested trait at the
/// syntax boundary and rejects anything else with a diagnostic, so downstream
/// synthesis never needs to re-parse a trait-name string. `From` carries its
/// source type as a resolved [`TypeId`] rather than a mangled `From<…>` name.
#[derive(Debug, Clone)]
pub enum SynthTrait {
    From { source: TypeId },
    Serialize,
    Deserialize,
}

/// An `impl` block as declared — its identity, not its methods, which live in
/// [`TirModule::functions`] linked back by [`TirFunction::method_info`]. The
/// record exists for a block whose only content is a rest clause
/// (`impl Log for Passthrough { ..forward }`), which produces no methods at
/// all. Consumed by the effect-dispatch synthesis; nothing past it sees an
/// impl block.
#[derive(Debug, Clone)]
pub struct TirImpl {
    /// Canonical `(declaring_module, base_trait_name)`; `None` for an inherent
    /// impl. Matches `LocalMethodName::{base_trait_module, base_trait_name}`,
    /// so a block and its methods agree on which declaration they target.
    pub trait_canonical: Option<(ModuleSource, String)>,
    /// Trait / resource type arguments at the impl site (`impl Stream<u8>` →
    /// `[u8]`). Matches `LocalMethodName::trait_type_args`.
    pub trait_type_args: Vec<TypeId>,
    /// The target type's name, derived exactly as `reify_method` derives it
    /// for this block's methods, so a block and its methods produce the same
    /// key in the effect-dispatch handler index.
    pub struct_name: String,
    /// `..trap` / `..forward`, when the block ends with a rest clause.
    pub rest: Option<crate::ast::RestClause>,
    pub span: Span,
}

/// `impl Trait for Type;` — request the compiler to synthesize the trait implementation.
#[derive(Debug, Clone)]
pub struct SynthesisRequest {
    pub trait_ref: SynthTrait,
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
    /// Canonical user-declared `(name, type)` pairs of the closure literal,
    /// captured at functor creation and never mutated. `register_closure_wrappers`
    /// derives the wrapper's external signature
    /// (`fn(env, ..canonical_user_params) -> canonical_return`) from this
    /// snapshot, so a later DAE shrink of `__call` cannot desynchronise it.
    pub canonical_user_params: Vec<(String, TypeId)>,
    /// Canonical return type of the closure literal. Same role as
    /// `canonical_user_params` — drives the wrapper external signature.
    pub canonical_return: TypeId,
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
    /// The generic declaration being instantiated, where the site holds one.
    ///
    /// `name` cannot stand in for it: two sibling functions may each declare a
    /// `struct Box<T>` in one module, and a `(name, module)` lookup answers
    /// with whichever was declared first — collapsing two distinct types onto
    /// one. `None` for a function or enum instantiation, which key by name.
    pub def: Option<crate::defs::DefId>,
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
        self.def == other.def
            && self.name == other.name
            && self.module_source == other.module_source
            && self.impl_type_args == other.impl_type_args
            && self.method_type_args == other.method_type_args
    }
}

impl Eq for InstantiationKey {}

impl std::hash::Hash for InstantiationKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.def.hash(state);
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
    pub resources: Vec<TirResource>,
    pub traits: Vec<TirTrait>,
    /// `impl` blocks as declared (identity + rest clause); their methods are
    /// in `functions`.
    pub impls: Vec<TirImpl>,
    /// `impl Trait for Type;` — synthesis requests (populated by elaborator, consumed by synthesis)
    pub synthesis_requests: Vec<SynthesisRequest>,
    /// Test declarations with their metadata
    pub tests: Vec<TirTest>,
    /// Global variable declarations
    pub globals: Vec<TirGlobal>,
    pub data_section: Option<String>,
    /// `#![wasm_module("name")]` — items in this module compile to a separate Wasm core module.
    pub wasm_module: Option<String>,
    /// Generic struct definitions (before monomorphization)
    /// Key: (struct name, module source)
    pub generic_structs: IndexMap<(String, ModuleSource), TirStruct>,
    /// Generic function definitions (before monomorphization)
    /// Key: (module source, function name). `module_source` is the function
    /// body's home module; two generics that share a mangled name in
    /// different modules are kept distinct by this pair.
    pub generic_functions: IndexMap<(ModuleSource, String), Rc<RefCell<TirFunction>>>,
    /// Requested instantiations (populated during resolution, processed in lower)
    pub instantiation_requests: IndexSet<InstantiationKey>,
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
            resources: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            wasm_module: None,
            generic_structs: IndexMap::default(),
            generic_functions: IndexMap::default(),
            instantiation_requests: IndexSet::default(),
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
            resources: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            wasm_module: None,
            generic_structs: IndexMap::default(),
            generic_functions: IndexMap::default(),
            instantiation_requests: IndexSet::default(),
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

    pub fn add_resource(&mut self, resource: TirResource) {
        self.resources.push(resource);
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
    #[should_panic(expected = "type_id_of_decl")]
    fn type_id_of_decl_panics_when_unregistered() {
        let table = TypeTable::new();
        let unregistered = crate::ast::AstId::new(crate::ast::AstIdSpace::next(), 0);
        let _ = table.type_id_of_decl(unregistered);
    }

    /// Substituting a projection's base rewrites the projection even when the
    /// replacement is itself a parameter. `Self::Item` under `Self := I` is
    /// `I::Item`, not `Self::Item` — a trait signature instantiated for an
    /// impl's receiver slot is the case that needs it.
    #[test]
    fn substitute_rewrites_projection_base_to_another_param() {
        let mut table = TypeTable::new();
        let self_param = table.make_type_param("Self".to_string(), 0);
        let projection = table.make_assoc_type_projection_simple(self_param, "Item".to_string());

        let receiver = table.make_type_param("I".to_string(), 1);
        let substitution = IndexMap::from_iter([(0, receiver)]);
        let substituted = table.substitute_type_params(projection, &substitution);

        let ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            ..
        } = table.get(substituted).clone()
        else {
            panic!("expected a projection, got {:?}", table.get(substituted));
        };
        assert_eq!(param_id, receiver);
        assert_eq!(assoc_name, "Item");
    }

    /// A frame is abstract over what `Self::X` means as well as over its
    /// slots, and only the use site can say. Given the answer, the projection
    /// is replaced by it rather than rebuilt over the substituted base.
    #[test]
    fn a_projection_answer_replaces_the_projection() {
        let mut table = TypeTable::new();
        let self_param = table.make_type_param("Self".to_string(), 0);
        let projection = table.make_assoc_type_projection_simple(self_param, "Item".to_string());

        let receiver = table.make_type_param("I".to_string(), 1);
        let projections =
            SlotProjections::from_iter([(0, vec![("Item".to_string(), TypeTable::U8)])]);
        let substituted = table.substitute_type_params_with(
            projection,
            &IndexMap::from_iter([(0, receiver)]),
            &projections,
        );

        assert_eq!(substituted, TypeTable::U8);
    }

    /// An unanswered name leaves the projection abstract over the substituted
    /// base — the frame simply does not know, and inventing an answer would
    /// be worse than deferring to monomorphization.
    #[test]
    fn an_unanswered_projection_stays_abstract() {
        let mut table = TypeTable::new();
        let self_param = table.make_type_param("Self".to_string(), 0);
        let projection = table.make_assoc_type_projection_simple(self_param, "Iter".to_string());

        let receiver = table.make_type_param("I".to_string(), 1);
        let substituted = table.substitute_type_params_with(
            projection,
            &IndexMap::from_iter([(0, receiver)]),
            &SlotProjections::from_iter([(0, vec![("Item".to_string(), TypeTable::U8)])]),
        );

        let ResolvedType::AssocTypeProjection { param_id, .. } = table.get(substituted).clone()
        else {
            panic!("expected a projection");
        };
        assert_eq!(param_id, receiver);
    }

    /// A substitution that misses the base leaves the projection interned as
    /// it was, so callers keeping `TypeId` identity are unaffected.
    #[test]
    fn substitute_leaves_unrelated_projection_untouched() {
        let mut table = TypeTable::new();
        let self_param = table.make_type_param("Self".to_string(), 0);
        let projection = table.make_assoc_type_projection_simple(self_param, "Item".to_string());

        let substitution = IndexMap::from_iter([(7, TypeTable::I32)]);
        assert_eq!(
            table.substitute_type_params(projection, &substitution),
            projection
        );
    }
}
