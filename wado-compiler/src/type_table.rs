//! The type table: every `TypeId` the compiler interns, and the questions
//! asked of them.

use std::fmt::Write;
use std::sync::Arc;

use rustc_hash::FxBuildHasher;
use sha2::Digest;

use crate::compiler_item::CompilerItem;
use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{AstId, HandleClasses, RangeKind};
use crate::compiler_item::CompilerItems;
use crate::defs::{DefId, DefKind, DefTable};
use crate::module_source::{CmNamespace, ModuleSource};
use crate::name;
use crate::name::{
    ARRAY_TYPE_NAME, FqTraitName, FqTypeName, INTERNAL_PREFIX, NEVER_TYPE_NAME, Receiver, RefKind,
    TEMPLATE_SHAPE_PREFIX, TypeHead, TypeNameInfo, UNIT_TYPE_NAME, assoc_type_projection_name,
    display_assoc_type_projection, display_generic_name, display_tuple_type, format_type_name,
    mangle_builtin_array_type, mangle_declared, mangle_generic_name, mangle_local_item_name,
    mangle_tuple_type, ref_type_name, type_pack_name,
};
use crate::primitive::PrimitiveType;
use crate::symbol_notation::render;
use crate::token::Span;

use crate::tir::{
    AnonShape, AssocTypeSig, EffectRef, EffectRows, InferVarId, REFLECT_NEWTYPE_BASE, ResolvedType,
    RowFlow, ShapeSubstitution, SlotProjections, StructDef, TemplateHole, TemplateShape, TirExpr,
    TirExprKind, TirUnaryOp, TraitFrame, TraitRef, TupleSlot, TypeId, positional_substitution,
    projection_arguments, range_item, solve_row,
};

#[derive(Debug, Clone)]
pub struct TypeTable {
    /// `TypeId` → `ResolvedType`. See [`TypeMap`]; `get` reads this on
    /// essentially every type query, so it is a hash-free `Vec` index.
    types: TypeMap<ResolvedType>,
    intern_map: IndexMap<ResolvedType, TypeId>,
    /// The slot each inference variable stands for, read only by diagnostics.
    /// Beside the variant, not inside it, so the interning key stays the id.
    infer_var_names: IndexMap<InferVarId, String>,
    /// The id the next [`EffectRef::Var`] takes.
    next_effect_var: u32,
    /// Whether the effect check has solved the variables, after which a use
    /// mints none: no check is left to solve it.
    effect_vars_solved: bool,
    /// Where a function value whose row holds a variable moved into a slot,
    /// which the type check admits unbounded and the effect check bounds.
    row_flows: IndexSet<RowFlow>,
    /// Registry of stdlib items the compiler is allowed to reference
    /// (Box, Option, Default, `push_str`, …). Populated during the
    /// annotate pass from `#[compiler_item("...")]` attributes; see
    /// [`crate::compiler_item`].
    compiler_items: CompilerItems,
    /// Associated type resolutions:
    /// `(concrete_type_id, declaring trait, assoc_name)` → `resolved_type_id`.
    /// Populated when impl blocks with associated type bindings are processed.
    ///
    /// Keyed by trait because one type may implement several declaring the
    /// same name — `f32` has both `FromStr::Err` and `LenientFromStr::Err`.
    /// `<type as trait[args]>::name` → the type it resolves to, the arguments
    /// being what the impl wrote beyond the declared defaults — so a bare bound
    /// and `impl Add<Cm> for Cm` meet at the empty list.
    assoc_type_resolutions: IndexMap<AssocTypeKey, AssocAnswers>,
    /// Generic associated type definitions:
    /// `(base decl, declaring trait, assoc_name)` → `TypeId`.
    /// The `TypeId` is typically a `TypeParam` that can be substituted using the
    /// `GenericInstance`'s `type_args`. Populated when processing generic impl blocks
    /// (e.g., `impl Iterator for ListIter<T> { type Item = T; }`).
    /// Used by the monomorphizer to resolve associated types for `GenericInstance` types.
    /// A generic impl's `type X = …` before substitution, keyed like
    /// [`Self::assoc_type_resolutions`] but by the target's declaration: two
    /// generic impls of one trait at different instantiations are two answers.
    generic_assoc_type_defs: IndexMap<GenericAssocTypeKey, AssocAnswers>,
    /// What each trait declares of its associated types, by declaring trait and
    /// name. See [`AssocTypeSig`].
    assoc_type_sigs: IndexMap<DefId, IndexMap<String, Arc<AssocTypeSig>>>,
    /// Each trait's own frame, which its signatures and references read. See
    /// [`TraitFrame`].
    trait_frames: IndexMap<DefId, Arc<TraitFrame>>,
    /// What [`Self::projection_bounds`] answered for each projection, which
    /// depends on nothing but the interned projection.
    projection_bounds_cache: IndexMap<TypeKey, Arc<[TraitRef]>>,
    /// Erasure redirects: set by `erase_newtypes_and_flags()`.
    /// After erasure, `get(id)` for any erased `TypeId` returns the base type.
    /// Newtype → ultimate base type; Flags → u32.
    ///
    /// A sparse [`TypeMap`] (most types are not erased): `Some(target)` is a
    /// live redirect, an absent slot means "no redirect". `get` consults it
    /// on every call, so the hash-free index matters here too.
    redirects: TypeMap<TypeId>,
    /// Each type whose spelling names an instance as its `GenericInstance`
    /// where monomorphization made a `Struct` of it, mapped to the spelling
    /// naming the struct: two spellings of one type, which [`Self::type_key`]
    /// answers alike. Sparse, as `redirects` is.
    settled_spellings: TypeMap<TypeId>,
    /// Whether monomorphization is done, so a type interned from here on is
    /// settled as it arrives.
    instances_settled: bool,
    /// The settled spellings minted as keys that nothing has interned as a
    /// type yet, which the name indexes leave out until something does.
    unindexed: IndexSet<TypeId>,
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
    /// The `Box<T>` a slot holds a value in, for the types
    /// [`Self::boxes_slot`] names, keyed by the type.
    slot_boxes: IndexMap<TypeKey, TypeId>,
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
    type_by_symbol: IndexMap<AstId, TypeId>,
    /// Inverse of `type_by_symbol` plus monomorphization tracking: every
    /// decl-backed `TypeId` — including monomorphized instances —
    /// maps to the [`AstId`](crate::ast::AstId) of its declaring AST node.
    ///
    /// A sparse [`TypeMap`] keyed by the decl-backed `TypeId`.
    symbol_by_type: TypeMap<AstId>,
    /// `(receiver head, module, trait)` triples that satisfied a `Serialize` /
    /// `Deserialize` / `Eq` / `Ord` bound structurally during elaboration
    /// (bound-driven synthesis, WEP 2026-06-25). Keyed by the receiver's head
    /// rather than by `TypeId`, so a generic records once against its
    /// declaration. Lives on the shared `TypeTable` because elaboration runs one
    /// `Elaborator` per module.
    bound_driven_synth_requests: IndexSet<(TypeHead, ModuleSource, DefId)>,
    /// What each impl block's target writes: what decides which instances of
    /// the head the block reaches.
    impl_targets: IndexMap<DefId, ImplTarget>,
    /// Variant case templates: `(variant name, module)` → `(case name, case
    /// index, payload TypeId)`. Payload ids are in the declaring template's
    /// terms; unit cases use `TypeTable::UNIT`.
    variant_case_index: IndexMap<DefId, Vec<(String, u32, TypeId)>>,
    /// Struct field types by declaration and instance arguments, a template's
    /// (no arguments) in its own terms: what [`Self::holds_affine_resource`]
    /// reads.
    struct_field_index: IndexMap<(DefId, Vec<TypeKey>), Vec<TypeId>>,
    /// Every struct shape the compiler minted, by [`AnonStructId`].
    anon_structs: Vec<AnonEntry>,
    /// Dedup for the above: the same shape in the same module is one id.
    anon_struct_index: IndexMap<(ModuleSource, AnonShape), AnonStructId>,
    /// Every mangle handed out, so a template shape's hash cannot name two.
    anon_struct_mangles: IndexSet<(ModuleSource, String)>,
    /// Each shape substitution recorded, in order, by the shape and its index
    /// among the shape's substitutions: what monomorphization instantiates.
    shape_substitution_log: Vec<(AnonStructId, usize)>,
    /// `(WIT name, generated module)` of each type declaration, for
    /// [`Self::cm_decl_in`]. Built with [`Self::attach_defs`], so it answers at
    /// any point in the pipeline rather than only after a declaration's type is
    /// interned.
    decl_index: IndexMap<(String, ModuleSource), DefId>,
    /// [`Self::decl_index`] addressed by the module's CM coordinate instead, for
    /// [`Self::cm_decl_in_module_named`]: a caller holding an interface FQ can
    /// spell that without an interner.
    cm_decl_index: IndexMap<(String, Option<CmNamespace>, String), DefId>,
    /// Resources declared `#[cm(..., linearity = "unrestricted")]`, with the
    /// classes their handles carry where they declare them.
    unrestricted_resources: IndexMap<DefId, Option<HandleClasses>>,
    /// Structs and variants declared `affine`.
    affine_decls: IndexSet<DefId>,
    /// `resource Child extends Parent`, child → parent.
    resource_parents: IndexMap<DefId, DefId>,
    /// Every declaration in the program, for rendering a nominal type's head.
    ///
    /// A name comes out of an identity and never goes back in. Attached where
    /// [`crate::resolve::Resolutions`] is built, and again on the snapshot
    /// restore path with the seeded table — which is what keeps a `DefId` a
    /// cached type carries pointing at the same declaration.
    defs: std::sync::Arc<DefTable>,
}

impl Default for TypeTable {
    fn default() -> Self {
        Self::new()
    }
}

/// What [`TypeTable::declared_members`] finds for a declaration.
struct DeclaredMembers {
    types: Vec<TypeId>,
    /// Whether `types` are the template's, written in terms of its parameters,
    /// rather than the instance's own.
    of_template: bool,
}

impl TypeTable {
    pub const I8: TypeId = TypeId(0);
    pub const I16: TypeId = TypeId(1);
    pub const I32: TypeId = TypeId(2);
    pub const I64: TypeId = TypeId(3);
    pub const U8: TypeId = TypeId(4);
    pub const U16: TypeId = TypeId(5);
    pub const U32: TypeId = TypeId(6);
    pub const U64: TypeId = TypeId(7);
    pub const F32: TypeId = TypeId(8);
    pub const F64: TypeId = TypeId(9);
    pub const F16: TypeId = TypeId(10);
    pub const BF16: TypeId = TypeId(11);
    pub const BOOL: TypeId = TypeId(12);
    pub const CHAR: TypeId = TypeId(13);
    pub const V128: TypeId = TypeId(14);
    pub const UNIT: TypeId = TypeId(15);
    pub const NEVER: TypeId = TypeId(16);
    pub const UNKNOWN: TypeId = TypeId(17);
    pub const ERROR: TypeId = TypeId(18);

    /// The well-known id `prim` interns to. [`Self::new`] asserts each one
    /// against what it actually interns, so neither can drift.
    #[must_use]
    pub fn primitive_type_id(prim: PrimitiveType) -> TypeId {
        match prim {
            PrimitiveType::I8 => Self::I8,
            PrimitiveType::I16 => Self::I16,
            PrimitiveType::I32 => Self::I32,
            PrimitiveType::I64 => Self::I64,
            PrimitiveType::U8 => Self::U8,
            PrimitiveType::U16 => Self::U16,
            PrimitiveType::U32 => Self::U32,
            PrimitiveType::U64 => Self::U64,
            PrimitiveType::F32 => Self::F32,
            PrimitiveType::F64 => Self::F64,
            PrimitiveType::F16 => Self::F16,
            PrimitiveType::Bf16 => Self::BF16,
            PrimitiveType::Bool => Self::BOOL,
            PrimitiveType::Char => Self::CHAR,
            PrimitiveType::V128 => Self::V128,
        }
    }

    /// The type spelling → well-known `TypeId` mapping, the single source for
    /// every by-name primitive resolution. `i128` / `u128` are struct-backed.
    pub fn primitive_by_name(name: &str) -> Option<TypeId> {
        match name {
            UNIT_TYPE_NAME => Some(Self::UNIT),
            NEVER_TYPE_NAME => Some(Self::NEVER),
            _ => PrimitiveType::from_name(name).map(Self::primitive_type_id),
        }
    }

    /// The type `def` declares where it is a primitive, `()` or `!`.
    pub fn primitive_of_decl(defs: &DefTable, def: DefId) -> Option<TypeId> {
        if defs.kind(def) != DefKind::BuiltinType {
            return None;
        }
        Self::primitive_by_name(defs.name(def))
    }

    /// The primitive a `flags` type lowers to, whose methods it inherits.
    pub const FLAGS_BASE_NAME: &'static str = "u32";

    /// The `(base name, struct type args)` a generic container (`GenericInstance`
    /// or the raw GC array `Array<T>`, whose methods live in `impl Array<T>`)
    /// dispatches under. A trait-method call site
    /// (`synthesis::template::method_name_for_type`) and the monomorphizer's
    /// `struct_info_for_method` must agree on this, or a call mangles to a
    /// name no impl was registered under. `None` for every other type (their
    /// dispatch name is derived differently).
    pub fn generic_dispatch_components(&self, type_id: TypeId) -> Option<(String, Vec<TypeId>)> {
        match self.get(type_id) {
            ResolvedType::GenericInstance { def, type_args }
            | ResolvedType::GenericResource { def, type_args } => {
                Some((self.def_name(*def).to_string(), type_args.clone()))
            }
            ResolvedType::BuiltinArray(elem) => Some((ARRAY_TYPE_NAME.to_string(), vec![*elem])),
            _ => None,
        }
    }

    pub fn new() -> Self {
        let mut table = Self {
            types: TypeMap::default(),
            intern_map: IndexMap::default(),
            infer_var_names: IndexMap::default(),
            next_effect_var: 0,
            effect_vars_solved: false,
            row_flows: IndexSet::default(),
            compiler_items: CompilerItems::new(),
            assoc_type_resolutions: IndexMap::default(),
            generic_assoc_type_defs: IndexMap::default(),
            assoc_type_sigs: IndexMap::default(),
            trait_frames: IndexMap::default(),
            projection_bounds_cache: IndexMap::default(),
            redirects: TypeMap::default(),
            settled_spellings: TypeMap::default(),
            instances_settled: false,
            unindexed: IndexSet::default(),
            box_payload_types: TypeMap::default(),
            slot_boxes: IndexMap::default(),
            struct_name_index: IndexMap::default(),
            decl_name_index: IndexMap::default(),
            type_by_symbol: IndexMap::default(),
            symbol_by_type: TypeMap::default(),
            bound_driven_synth_requests: IndexSet::default(),
            impl_targets: IndexMap::default(),
            variant_case_index: IndexMap::default(),
            struct_field_index: IndexMap::default(),
            anon_structs: Vec::new(),
            anon_struct_index: IndexMap::default(),
            anon_struct_mangles: IndexSet::default(),
            shape_substitution_log: Vec::new(),
            decl_index: IndexMap::default(),
            cm_decl_index: IndexMap::default(),
            unrestricted_resources: IndexMap::default(),
            affine_decls: IndexSet::default(),
            resource_parents: IndexMap::default(),
            defs: std::sync::Arc::default(),
        };

        for &prim in PrimitiveType::ALL {
            let id = table.intern(ResolvedType::Primitive(prim));
            assert_eq!(id, Self::primitive_type_id(prim));
        }
        // Landing here says `ALL` holds every variant once: a missing or
        // repeated entry shifts everything after the primitives.
        assert_eq!(table.intern(ResolvedType::Unit), Self::UNIT);
        table.intern(ResolvedType::Never);
        table.intern(ResolvedType::Unknown);
        table.intern(ResolvedType::Error);

        table
    }

    pub fn intern(&mut self, ty: ResolvedType) -> TypeId {
        if let Some(&id) = self.intern_map.get(&ty) {
            if self.unindexed.swap_remove(&id) {
                self.index_type(id, &ty);
            }
            return id;
        }
        let id = self.types.next_id();
        self.index_type(id, &ty);
        self.types.push(ty.clone());
        self.intern_map.insert(ty, id);
        if self.instances_settled {
            self.settle_spelling(id);
        }
        id
    }

    fn index_type(&mut self, id: TypeId, ty: &ResolvedType) {
        // Update struct name index for O(1) lookups by (name, module_source).
        // Keyed on the *rendered* spelling: every instantiation of a generic
        // struct is a distinct type, and keying on the declaration would
        // collapse them all onto one entry.
        if let ResolvedType::Struct { def, type_args } = ty {
            let rendered = self.struct_rendered_name(*def, type_args);
            let module_source = self.struct_head_module(*def).clone();
            self.struct_name_index.insert((rendered, module_source), id);
        }
        if let Some(def) = Self::nominal_key(ty) {
            let key = (self.def_name(def).to_string(), self.def_module(def).clone());
            self.decl_name_index.insert(key, id);
        }
    }

    /// The spelling of `id` with each instance of a struct named as the
    /// `Struct` monomorphization makes of it, whether or not it has yet.
    fn settle_spelling(&mut self, id: TypeId) -> TypeId {
        if let Some(&settled) = self.settled_spellings.get(id) {
            return settled;
        }
        let spelling = self.types.get(id).expect("an interned type").clone();
        let rebuilt = match &spelling {
            ResolvedType::GenericInstance { def, type_args }
                if self.defs.kind(*def) == DefKind::Struct && !self.is_tuple_def(*def) =>
            {
                ResolvedType::Struct {
                    def: StructDef::Decl(*def),
                    type_args: self.settle_struct_args(type_args),
                }
            }
            ResolvedType::GenericInstance { def, type_args } => ResolvedType::GenericInstance {
                def: *def,
                type_args: self.settle_spellings(type_args),
            },
            ResolvedType::Struct { def, type_args } => ResolvedType::Struct {
                def: *def,
                type_args: self.settle_struct_args(type_args),
            },
            ResolvedType::GenericResource { def, type_args } => ResolvedType::GenericResource {
                def: *def,
                type_args: self.settle_spellings(type_args),
            },
            ResolvedType::Ref(inner) => ResolvedType::Ref(self.settle_spelling(*inner)),
            ResolvedType::MutRef(inner) => ResolvedType::MutRef(self.settle_spelling(*inner)),
            ResolvedType::BuiltinArray(elem) => {
                ResolvedType::BuiltinArray(self.settle_spelling(*elem))
            }
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                effects,
            } => ResolvedType::Function {
                is_mut: *is_mut,
                params: self.settle_spellings(params),
                return_type: self.settle_spelling(*return_type),
                effects: effects.clone(),
            },
            // A newtype keeps its identity until erasure redirects it, and the
            // rest name no other type.
            ResolvedType::Newtype { .. }
            | ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::TypeParam { .. }
            | ResolvedType::InferVar(_)
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocParam { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => spelling.clone(),
        };
        let settled = if rebuilt == spelling {
            id
        } else {
            self.intern_settled(rebuilt)
        };
        self.settled_spellings.set_growing(id, settled);
        settled
    }

    /// The id `spelling` interns to, minted where no type spells it yet. A
    /// minted one enters the name indexes only once something interns it as a
    /// type: until then the type it settles is there under its own spelling.
    fn intern_settled(&mut self, spelling: ResolvedType) -> TypeId {
        if let Some(&id) = self.intern_map.get(&spelling) {
            return id;
        }
        let id = self.types.next_id();
        self.types.push(spelling.clone());
        self.intern_map.insert(spelling, id);
        self.unindexed.insert(id);
        self.settled_spellings.set_growing(id, id);
        id
    }

    fn settle_spellings(&mut self, ids: &[TypeId]) -> Vec<TypeId> {
        ids.iter().map(|&id| self.settle_spelling(id)).collect()
    }

    /// A struct instance's arguments as monomorphization keys it: settled, and
    /// rowless as [`Self::make_monomorphized_struct_from_args`] makes them.
    fn settle_struct_args(&mut self, ids: &[TypeId]) -> Vec<TypeId> {
        ids.iter()
            .map(|&id| {
                let rowless = self.without_rows(id);
                self.settle_spelling(rowless)
            })
            .collect()
    }

    /// The slot holding `id`'s value: itself, or what a redirect points it at.
    /// One hop — [`Self::redefine_to`] refuses a target that is redirected.
    fn resolved_id(&self, id: TypeId) -> TypeId {
        self.redirects.get(id).copied().unwrap_or(id)
    }

    /// [`Self::get`] for a caller holding an id from another table. One this
    /// table does not carry answers `None` rather than panicking.
    pub fn try_get(&self, id: TypeId) -> Option<&ResolvedType> {
        self.types.get(self.resolved_id(id))
    }

    pub fn get(&self, id: TypeId) -> &ResolvedType {
        let id = self.resolved_id(id);
        self.types
            .get(id)
            .unwrap_or_else(|| panic!("TypeId {id:?} not found in TypeTable"))
    }

    /// Where [`Self::redefine_to`] pointed `id`, when `id` is a reference it
    /// redefined. Erasure's redirect is not one, and the spelling tells them
    /// apart: only a reference is ever redefined.
    fn box_redefinition(&self, id: TypeId) -> Option<TypeId> {
        let target = self.redirects.get(id).copied()?;
        self.spelled_ref(id).map(|_| target)
    }

    /// [`Self::get`] before the newtype / flags erasure, which runs once
    /// monomorphize is done. A boxing redefinition is no part of that, so a
    /// redefined reference reads as the `Box<T>` here too.
    ///
    /// Erasure is a representation choice — a `flags` value is a `u32` at
    /// runtime — but `impl Trait for Perms` is still keyed under `Perms`. A
    /// name that has to match an impl must read the identity; only code that
    /// cares how the value is stored should read [`Self::get`].
    #[must_use]
    pub fn get_unerased(&self, id: TypeId) -> &ResolvedType {
        let id = self.box_redefinition(id).unwrap_or(id);
        self.types
            .get(id)
            .unwrap_or_else(|| panic!("TypeId {id:?} not found in TypeTable"))
    }

    /// Whether `id` names a newtype, erased or not.
    pub fn is_newtype(&self, id: TypeId) -> bool {
        matches!(self.get_unerased(id), ResolvedType::Newtype { .. })
    }

    /// [`Self::get`] returning `None` for ids pruned by DCE's `retain`.
    pub fn get_pruned(&self, id: TypeId) -> Option<&ResolvedType> {
        self.types.get(self.resolved_id(id))
    }

    /// The [`TypeKey`] for `id`.
    ///
    /// Newtype erasure and the boxing rewrite both leave many ids resolving to
    /// one type, so an id is a slot and not a type identity.
    #[must_use]
    pub fn type_key(&self, id: TypeId) -> TypeKey {
        let resolved = self.resolved_id(id);
        let interned = self
            .types
            .get(resolved)
            .and_then(|ty| self.intern_map.get(ty).copied())
            .unwrap_or(resolved);
        TypeKey(
            self.settled_spellings
                .get(interned)
                .copied()
                .unwrap_or(interned),
        )
    }

    /// [`Self::type_key`] of each id, in order.
    #[must_use]
    pub fn type_keys(&self, ids: &[TypeId]) -> Vec<TypeKey> {
        ids.iter().map(|&id| self.type_key(id)).collect()
    }

    /// `ty` without its references and newtypes: the object a handle names.
    #[must_use]
    pub fn strip_handles(&self, mut ty: TypeId) -> TypeId {
        loop {
            if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = self.get(ty) {
                ty = *inner;
            } else if let ResolvedType::Newtype { .. } = self.get(ty) {
                ty = self.representation_head(ty);
            } else {
                return ty;
            }
        }
    }

    /// The heap object a value of `ty` is, or reaches first through its
    /// references: the key an access to one of its fields is made under.
    #[must_use]
    pub fn heap_object_key(&self, ty: TypeId) -> Option<TypeKey> {
        let ty = self.strip_handles(ty);
        match self.get(ty) {
            ResolvedType::Struct { .. }
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::Variant { .. }
            | ResolvedType::Function { .. }
            | ResolvedType::GenericInstance { .. } => Some(self.type_key(ty)),
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::TypeParam { .. }
            | ResolvedType::AssocParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => None,
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) | ResolvedType::Newtype { .. } => {
                unreachable!("strip_handles removes every reference and newtype")
            }
            ResolvedType::InferVar(var) => panic!("{var} reached heap-effect analysis"),
        }
    }

    /// Whether `a` and `b` name one list of types, position by position.
    #[must_use]
    pub fn same_types(&self, a: &[TypeId], b: &[TypeId]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(&x, &y)| self.type_key(x) == self.type_key(y))
    }

    /// Whether two projections' trait arguments name one instantiation of the
    /// trait. A side reached by a bare bound, `None`, names none other.
    fn trait_args_agree(&self, a: Option<&[TypeId]>, b: Option<&[TypeId]>) -> bool {
        match (a, b) {
            (Some(a), Some(b)) => self.same_types(a, b),
            _ => true,
        }
    }

    /// True when `id` resolves to the never type `!`, through any newtype. An
    /// expression of this type diverges and never yields a value.
    pub fn is_never(&self, id: TypeId) -> bool {
        matches!(self.get(self.representation_head(id)), ResolvedType::Never)
    }

    /// Iterate over all live types in the type table. Erased slots (`None`,
    /// produced by [`Self::retain`]) are skipped.
    pub fn all_types(&self) -> impl Iterator<Item = (TypeId, &ResolvedType)> {
        self.types.iter()
    }

    /// The primitive `id` bottoms out in, through any newtype chain.
    pub fn primitive_head(&self, id: TypeId) -> Option<PrimitiveType> {
        match self.get(self.representation_head(id)) {
            ResolvedType::Primitive(p) => Some(*p),
            _ => None,
        }
    }

    /// Whether `id` resolves to exactly `want`, through any newtype chain.
    pub fn is_primitive(&self, id: TypeId, want: PrimitiveType) -> bool {
        self.primitive_head(id) == Some(want)
    }

    /// Whether `id` is one of the scalar integers. `i128` / `u128` answer
    /// `false`: no Wasm integer instruction takes them.
    pub fn is_integer(&self, id: TypeId) -> bool {
        matches!(
            self.primitive_head(id),
            Some(
                PrimitiveType::I8
                    | PrimitiveType::I16
                    | PrimitiveType::I32
                    | PrimitiveType::I64
                    | PrimitiveType::U8
                    | PrimitiveType::U16
                    | PrimitiveType::U32
                    | PrimitiveType::U64
            )
        )
    }

    pub fn is_float(&self, id: TypeId) -> bool {
        matches!(
            self.primitive_head(id),
            Some(PrimitiveType::F32 | PrimitiveType::F64)
        )
    }

    pub fn is_half(&self, id: TypeId) -> bool {
        self.primitive_head(id).is_some_and(PrimitiveType::is_half)
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

    pub fn mark_unrestricted_resource(&mut self, def: DefId, classes: Option<HandleClasses>) {
        self.unrestricted_resources.insert(def, classes);
    }

    /// Whether `def` declares an unrestricted resource, which no affine check
    /// and no cleanup pass owns.
    #[must_use]
    pub fn is_unrestricted_resource(&self, def: DefId) -> bool {
        self.unrestricted_resources.contains_key(&def)
    }

    pub fn mark_affine_decl(&mut self, def: DefId) {
        self.affine_decls.insert(def);
    }

    /// Whether `def` is a struct or variant declared `affine`: move-only in
    /// every instantiation, whatever its fields hold.
    #[must_use]
    pub fn is_affine_decl(&self, def: DefId) -> bool {
        self.affine_decls.contains(&def)
    }

    /// The classes an unrestricted resource's handles carry, where it declares them.
    #[must_use]
    pub fn handle_classes(&self, def: DefId) -> Option<HandleClasses> {
        self.unrestricted_resources.get(&def).copied().flatten()
    }

    /// The classes a narrowing to the resource `target` tests.
    #[must_use]
    pub fn narrowing_classes(&self, target: TypeId) -> Option<HandleClasses> {
        let ResolvedType::Resource { def } = self.get(target) else {
            unreachable!("only a resource is narrowed to");
        };
        self.handle_classes(*def)
    }

    /// Each class the resource tree holding `def` numbers, with the resource
    /// whose own class it is.
    pub fn handle_class_owners(&self, def: DefId) -> impl Iterator<Item = (u16, DefId)> {
        let root = self
            .resource_chain(def)
            .last()
            .expect("a chain starts at `def`");
        self.unrestricted_resources
            .iter()
            .filter_map(move |(&member, classes)| {
                let lo = classes.as_ref()?.lo;
                self.is_resource_subtype(member, root)
                    .then_some((lo, member))
            })
    }

    /// The scalar a resource handle is in the guest: the `u64` bits of the `f64`
    /// an unrestricted handle is outside it, or an `i32`. `None` for a non-resource.
    #[must_use]
    pub fn handle_scalar(&self, ty: TypeId) -> Option<TypeId> {
        match self.get(ty) {
            ResolvedType::Resource { def } if self.is_unrestricted_resource(*def) => {
                Some(Self::U64)
            }
            ResolvedType::Resource { .. } | ResolvedType::GenericResource { .. } => Some(Self::I32),
            _ => None,
        }
    }

    /// Whether `ty` is an affine resource, which the guest holds as a GC object
    /// boxing its `i32` handle.
    #[must_use]
    pub fn is_resource_object(&self, ty: TypeId) -> bool {
        self.handle_scalar(ty) == Some(Self::I32)
    }

    /// Whether `ty` is an unrestricted resource, whose handle is an `f64` outside the guest.
    #[must_use]
    pub fn is_unrestricted_handle(&self, ty: TypeId) -> bool {
        self.handle_scalar(ty) == Some(Self::U64)
    }

    /// Record `child extends parent`, already validated by the caller.
    pub fn set_resource_parent(&mut self, child: DefId, parent: DefId) {
        assert_ne!(child, parent, "a resource cannot extend itself");
        assert!(
            !self.is_resource_subtype(parent, child),
            "a resource inheritance cycle would make every chain walk unbounded"
        );
        self.resource_parents.insert(child, parent);
    }

    #[must_use]
    pub fn resource_parent(&self, def: DefId) -> Option<DefId> {
        self.resource_parents.get(&def).copied()
    }

    /// The ancestor two branches agree on when one resource extends the other,
    /// under the shared reference they were written with. `&mut` is invariant.
    /// The answer is always one of `a` and `b`, so nothing new is interned.
    #[must_use]
    pub fn resource_join(&self, a: TypeId, b: TypeId) -> Option<TypeId> {
        if let (ResolvedType::Ref(a_inner), ResolvedType::Ref(b_inner)) = (self.get(a), self.get(b))
        {
            let (a_inner, b_inner) = (*a_inner, *b_inner);
            let joined = self.resource_join(a_inner, b_inner)?;
            return Some(if joined == a_inner { a } else { b });
        }
        let (ResolvedType::Resource { def: a_def }, ResolvedType::Resource { def: b_def }) =
            (self.get(a), self.get(b))
        else {
            return None;
        };
        if self.is_resource_subtype(*a_def, *b_def) {
            return Some(b);
        }
        self.is_resource_subtype(*b_def, *a_def).then_some(a)
    }

    /// `def` and every resource it extends, nearest first.
    pub fn resource_chain(&self, def: DefId) -> impl Iterator<Item = DefId> {
        std::iter::successors(Some(def), |&current| self.resource_parent(current))
    }

    /// Whether `sub` is `sup` or extends it, directly or transitively.
    #[must_use]
    pub fn is_resource_subtype(&self, sub: DefId, sup: DefId) -> bool {
        self.resource_chain(sub).any(|current| current == sup)
    }

    /// Whether only the host can tell a `value` is a `target`: `target` is a
    /// resource strictly extending `value`'s, which makes both unrestricted.
    #[must_use]
    pub fn is_resource_narrowing(&self, value: TypeId, target: TypeId) -> bool {
        let (ResolvedType::Resource { def: value }, ResolvedType::Resource { def: target }) =
            (self.get(value), self.get(target))
        else {
            return false;
        };
        value != target && self.is_resource_subtype(*target, *value)
    }

    /// Whether `==` compares `a` and `b` as handles: both unrestricted, one
    /// extending the other. The host interns handles, so equal means same object.
    #[must_use]
    pub fn handles_compare(&self, a: TypeId, b: TypeId) -> bool {
        self.resource_join(a, b)
            .is_some_and(|joined| self.is_unrestricted_handle(joined))
    }

    /// Attach the program's declarations, so a nominal type can render its
    /// head once it carries one instead of a spelling.
    pub fn attach_defs(&mut self, defs: std::sync::Arc<DefTable>) {
        // A module-level declaration is entered first and kept: a
        // function-local item shares its module, and a spelling that reaches
        // both means the module-level one everywhere this index is consulted.
        self.decl_index = IndexMap::default();
        self.cm_decl_index = IndexMap::default();
        for def in defs.iter() {
            if !defs.kind(def).is_type() {
                continue;
            }
            let name = defs.name(def).to_string();
            let module = defs.module(def);
            if let Some(key) = cm_module_key(&name, module) {
                self.cm_decl_index.entry(key).or_insert(def);
            }
            self.decl_index.entry((name, module.clone())).or_insert(def);
        }
        self.defs = defs;
    }

    /// Every declaration in the program.
    #[must_use]
    pub fn defs(&self) -> &DefTable {
        &self.defs
    }

    /// The name `def` writes — a rendering, for a diagnostic or a mangle.
    #[must_use]
    pub fn def_name(&self, def: DefId) -> &str {
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
    pub fn decl_render_name(&self, def: DefId) -> String {
        let name = self.defs.name(def);
        if self.defs.is_function_local(def) {
            return mangle_local_item_name(name, self.defs.ast_id(def));
        }
        name.to_string()
    }

    /// The module that declares `def`.
    #[must_use]
    pub fn def_module(&self, def: DefId) -> &ModuleSource {
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

    /// Intern a tagged template literal's shape. The shape is the identity, so
    /// two sites writing one template in one module reach one id.
    pub fn intern_template_shape(
        &mut self,
        module_source: ModuleSource,
        shape: TemplateShape,
    ) -> AnonStructId {
        self.intern_shape(module_source, AnonShape::Template(shape))
    }

    /// The template shape behind an anonymous struct, or `None` for a struct
    /// literal's or a synthetic one.
    #[must_use]
    pub fn template_shape(&self, id: AnonStructId) -> Option<&TemplateShape> {
        match &self.anon_structs[id.0 as usize].shape {
            AnonShape::Template(shape) => Some(shape),
            AnonShape::Fields(_) | AnonShape::Synthetic(_) => None,
        }
    }

    /// The template shape a type denotes, or `None` where it is not one.
    #[must_use]
    pub fn template_shape_of_type(&self, id: TypeId) -> Option<&TemplateShape> {
        match self.get(id) {
            ResolvedType::Struct {
                def: StructDef::Anon(shape),
                ..
            } => self.template_shape(*shape),
            _ => None,
        }
    }

    /// The field type holding a hole of type `ty`: the handle where a
    /// reference is one, the value where a reference would be a box — a
    /// scalar copy is free and a box is an allocation (WEP 2026-01-10).
    pub fn hole_field_type(&mut self, ty: TypeId) -> TypeId {
        if self.hole_held_by_ref(ty) {
            self.make_ref(ty)
        } else {
            ty
        }
    }

    /// Whether [`Self::hole_field_type`] holds a hole of type `ty` by `&`.
    #[must_use]
    pub fn hole_held_by_ref(&self, ty: TypeId) -> bool {
        !self.is_boxed_reference_target(ty)
    }

    /// Whether `ty` is an instance of the compiler struct `item`, as a generic
    /// instance or as the monomorphized struct its declaration heads.
    #[must_use]
    pub fn is_compiler_struct_instance(&self, ty: TypeId, item: CompilerItem) -> bool {
        match self.get(ty) {
            ResolvedType::GenericInstance { def, .. } => self.is_compiler_item(*def, item),
            ResolvedType::Struct { def, type_args } => {
                !type_args.is_empty() && self.is_compiler_struct(*def, item)
            }
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Ref(_)
            | ResolvedType::MutRef(_)
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::Function { .. }
            | ResolvedType::TypeParam { .. }
            | ResolvedType::AssocParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::InferVar(_)
            | ResolvedType::Unknown
            | ResolvedType::Error => false,
        }
    }

    /// Whether `&T` / `&mut T` is represented as a `Box<T>` cell rather than
    /// `T`'s own GC handle (WEP 2026-06-13, Reference Representation). The
    /// boxing pass and every consumer deciding by representation read this one
    /// predicate.
    #[must_use]
    pub fn is_boxed_reference_target(&self, ty: TypeId) -> bool {
        match self.get(ty) {
            ResolvedType::Primitive(_) => true,
            ResolvedType::Enum { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::Function { .. } => true,
            ResolvedType::GenericInstance { def, .. } => self.find_variant_type(*def).is_some(),
            _ => false,
        }
    }

    /// Whether a value of `ty` is mutated in place through a reference to it, so
    /// a reference to it is its own GC handle: a `struct` (`String` among them),
    /// a `List` or a tuple. Assigning such a place writes into its storage
    /// wherever a reference to it may be read (WEP 2026-10-13). `i128` / `u128`
    /// are struct-backed but immutable, so their storage may be shared and is
    /// never written. A reference names storage rather than holding a value,
    /// whether spelled `&T` or as the `Box<T>` boxing makes of one.
    pub fn is_in_place_aggregate(&self, ty: TypeId) -> bool {
        if self.is_reference_shaped(ty) {
            return false;
        }
        match self.get(ty) {
            ResolvedType::Struct { .. } => !self.is_wide_int(ty),
            _ => self.list_element(ty).is_some() || self.as_tuple(ty).is_some(),
        }
    }

    /// Intern a shape a literal writes.
    fn intern_shape(&mut self, module_source: ModuleSource, shape: AnonShape) -> AnonStructId {
        let id = self.intern_shape_entry(module_source, shape);
        self.anon_structs[id.0 as usize].written = true;
        id
    }

    fn intern_shape_entry(
        &mut self,
        module_source: ModuleSource,
        shape: AnonShape,
    ) -> AnonStructId {
        let key = (module_source, shape);
        if let Some(&id) = self.anon_struct_index.get(&key) {
            return id;
        }
        let id = AnonStructId(u32::try_from(self.anon_structs.len()).expect("anon struct space"));
        let mut mangle = self.render_shape(&key.1, &|tt, ty| tt.mangle_type_arg_rowless(ty));
        // A template shape renders as a hash of its text, so two shapes can
        // collide; the mangle is the whole identity of a shape, and sharing
        // one would give both the other's bridges. Widen the loser instead.
        while !self
            .anon_struct_mangles
            .insert((key.0.clone(), mangle.clone()))
        {
            mangle.push('_');
        }
        let open = Self::any_shape_type(&key.1, &mut |ty| self.mentions_rigid_param(ty));
        self.anon_structs.push(AnonEntry {
            module: key.0.clone(),
            shape: key.1.clone(),
            mangle,
            open,
            written: false,
            substitutions: Vec::new(),
        });
        self.anon_struct_index.insert(key, id);
        id
    }

    /// Record that `substitution` makes `shape`, unless its origin already
    /// reached it: an origin's methods are instantiated at a shape once.
    fn record_shape_substitution(&mut self, shape: AnonStructId, substitution: ShapeSubstitution) {
        let entry = &mut self.anon_structs[shape.0 as usize];
        if entry
            .substitutions
            .iter()
            .any(|s| s.origin == substitution.origin)
        {
            return;
        }
        entry.substitutions.push(substitution);
        self.shape_substitution_log
            .push((shape, entry.substitutions.len() - 1));
    }

    /// The substitution recorded at `index` among those making `shape`.
    #[must_use]
    pub fn shape_substitution(&self, shape: AnonStructId, index: usize) -> &ShapeSubstitution {
        &self.anon_structs[shape.0 as usize].substitutions[index]
    }

    /// Whether a literal wrote shape `id`, rather than only a substitution.
    #[must_use]
    pub fn anon_struct_is_written(&self, id: AnonStructId) -> bool {
        self.anon_structs[id.0 as usize].written
    }

    /// The substitutions into a closed shape recorded at log index `from` or
    /// later, as the shape and the substitution's index among its own, and the
    /// index the log ends at, to ask from next time.
    #[must_use]
    pub fn closed_shape_substitutions_since(
        &self,
        from: usize,
    ) -> (Vec<(AnonStructId, usize)>, usize) {
        let log = &self.shape_substitution_log[from..];
        let closed = log
            .iter()
            .copied()
            .filter(|&(shape, _)| !self.anon_shape_is_open(shape))
            .collect();
        (closed, self.shape_substitution_log.len())
    }

    /// The fields a struct of shape `id` holds: a literal's as written, a
    /// template's one per hole typed by [`Self::hole_field_type`], then each
    /// segment as a `String`, cooked and raw.
    pub fn anon_shape_struct_fields(&mut self, id: AnonStructId) -> Vec<(String, TypeId)> {
        match self.anon_structs[id.0 as usize].shape.clone() {
            AnonShape::Fields(fields) => fields,
            AnonShape::Template(shape) => {
                let string_type = self.make_compiler_struct(CompilerItem::String);
                let holes =
                    shape.holes.iter().enumerate().map(|(k, hole)| {
                        (TemplateShape::field_name(k), self.hole_field_type(hole.ty))
                    });
                let segments = shape
                    .segment_keys()
                    .map(|key| (shape.segment_field(key).0, string_type));
                holes.chain(segments).collect()
            }
            AnonShape::Synthetic(name) => {
                unreachable!("synthetic shape `{name}` carries its fields itself")
            }
        }
    }

    /// Whether shape `id` mentions a type parameter, so that only a
    /// substitution into it reaches a struct codegen can lay out.
    #[must_use]
    pub fn anon_shape_is_open(&self, id: AnonStructId) -> bool {
        self.anon_structs[id.0 as usize].open
    }

    /// Whether `id` mentions a type parameter or pack: what a substitution
    /// answers, unlike an inference variable, which solving answers in place.
    fn mentions_rigid_param(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. } => true,
            _ => self.any_constituent(id, &mut |t| self.mentions_rigid_param(t)),
        }
    }

    /// The type parameters shape `id` mentions, by name: the frame its
    /// synthesized methods are instantiated in at a substituted shape.
    #[must_use]
    pub fn anon_shape_type_params(&self, id: AnonStructId) -> IndexMap<String, u32> {
        self.anon_shape_params(id)
            .into_iter()
            .map(|(index, ty)| match self.get(ty) {
                ResolvedType::TypeParam { name, .. } | ResolvedType::TypePack { name, .. } => {
                    (name.clone(), index)
                }
                other => unreachable!("collected a parameter that is not one: {other:?}"),
            })
            .collect()
    }

    /// The type parameters and packs shape `id` mentions, by index.
    fn anon_shape_params(&self, id: AnonStructId) -> IndexMap<u32, TypeId> {
        let mut found = IndexMap::default();
        self.any_anon_shape_type(id, &mut |ty| {
            self.collect_type_params(ty, &mut found);
            false
        });
        found
    }

    fn collect_type_params(&self, id: TypeId, found: &mut IndexMap<u32, TypeId>) {
        match self.get(id) {
            ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                found.insert(*index, id);
            }
            _ => {
                self.any_constituent(id, &mut |t| {
                    self.collect_type_params(t, found);
                    false
                });
            }
        }
    }

    /// Whether `f` holds of a type shape `id` is built over.
    fn any_anon_shape_type(&self, id: AnonStructId, f: &mut dyn FnMut(TypeId) -> bool) -> bool {
        Self::any_shape_type(&self.anon_structs[id.0 as usize].shape, f)
    }

    /// Whether `f` holds of a type `shape` is built over.
    fn any_shape_type(shape: &AnonShape, f: &mut dyn FnMut(TypeId) -> bool) -> bool {
        match shape {
            AnonShape::Fields(fields) => fields.iter().any(|&(_, ty)| f(ty)),
            AnonShape::Template(shape) => shape.holes.iter().any(|hole| f(hole.ty)),
            AnonShape::Synthetic(_) => false,
        }
    }

    /// `type_id`, a struct of shape `id`, with `leaves` substituted into the
    /// shape's types. A shape's identity is its types, so the result is the
    /// struct of the substituted shape (WEP 2026-01-10, open shapes).
    fn subst_anon_shape(
        &mut self,
        type_id: TypeId,
        id: AnonStructId,
        leaves: &Leaves<'_>,
    ) -> TypeId {
        // A row erased or an effect parameter answered changes no layout, and
        // re-interning a closed shape over one would part a value from its type.
        if !self.anon_shape_is_open(id) {
            return type_id;
        }
        let entry = &self.anon_structs[id.0 as usize];
        let module = entry.module.clone();
        let shape = match entry.shape.clone() {
            AnonShape::Synthetic(name) => unreachable!("synthetic shape `{name}` is never open"),
            AnonShape::Fields(fields) => AnonShape::Fields(
                fields
                    .into_iter()
                    .map(|(name, ty)| (name, self.subst_rec(ty, leaves)))
                    .collect(),
            ),
            AnonShape::Template(shape) => AnonShape::Template(TemplateShape {
                holes: shape
                    .holes
                    .into_iter()
                    .map(|hole| TemplateHole {
                        ty: self.subst_rec(hole.ty, leaves),
                        ..hole
                    })
                    .collect(),
            }),
        };
        if shape == self.anon_structs[id.0 as usize].shape {
            return type_id;
        }
        // Each origin of this shape reaches the result, its parameters mapped
        // to what this substitution makes of their answers here. A literal
        // makes the shape its own origin, every parameter answering itself.
        let entry = &self.anon_structs[id.0 as usize];
        let mut origins = entry.substitutions.clone();
        if entry.written {
            origins.push(ShapeSubstitution {
                origin: id,
                params: self.anon_shape_params(id),
            });
        }
        assert!(
            !origins.is_empty(),
            "an open shape is written or substituted"
        );
        let substituted = self.intern_shape_entry(module, shape);
        for earlier in origins {
            let params = earlier
                .params
                .into_iter()
                .map(|(slot, ty)| (slot, self.subst_rec(ty, leaves)))
                .collect();
            self.record_shape_substitution(
                substituted,
                ShapeSubstitution {
                    origin: earlier.origin,
                    params,
                },
            );
        }
        self.make_struct(StructDef::Anon(substituted))
    }

    /// The fields of an anonymous struct shape; empty for a synthetic one.
    #[must_use]
    pub fn anon_struct_fields(&self, id: AnonStructId) -> &[(String, TypeId)] {
        match &self.anon_structs[id.0 as usize].shape {
            AnonShape::Fields(fields) => fields,
            AnonShape::Synthetic(_) | AnonShape::Template(_) => &[],
        }
    }

    /// Whether the shape is one the compiler minted under a name: no literal
    /// wrote it and no field walk reads it.
    #[must_use]
    pub fn anon_struct_is_synthetic(&self, id: AnonStructId) -> bool {
        matches!(
            self.anon_structs[id.0 as usize].shape,
            AnonShape::Synthetic(_)
        )
    }

    /// The module the shape was written in.
    #[must_use]
    pub fn anon_struct_module(&self, id: AnonStructId) -> &ModuleSource {
        &self.anon_structs[id.0 as usize].module
    }

    /// The spelling an anonymous struct shows a reader — `{ x: i32, y: i32 }`
    /// as a literal writes it, or a template's text. The declaration namespace;
    /// [`Self::anon_struct_mangle`] is what a key is built from.
    #[must_use]
    pub fn anon_struct_name(&self, id: AnonStructId) -> String {
        self.render_anon_struct(id, &|t| self.type_name(t))
    }

    /// [`Self::anon_struct_name`] with each field or hole type spelled by
    /// `field_type`, so a qualified rendering stays qualified inside the shape.
    fn render_anon_struct(
        &self,
        id: AnonStructId,
        field_type: &dyn Fn(TypeId) -> String,
    ) -> String {
        match &self.anon_structs[id.0 as usize].shape {
            AnonShape::Template(shape) => self.template_shape_name(shape, field_type),
            AnonShape::Fields(fields) if fields.is_empty() => "{}".to_string(),
            AnonShape::Fields(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(n, ty)| format!("{n}: {}", field_type(*ty)))
                    .collect();
                format!("{{ {} }}", parts.join(", "))
            }
            AnonShape::Synthetic(name) => name.clone(),
        }
    }

    /// A template shape as its holes, each spelled as its type and specifier,
    /// cut to [`TEMPLATE_NAME_MAX_CHARS`] characters.
    fn template_shape_name(
        &self,
        shape: &TemplateShape,
        hole_type: &dyn Fn(TypeId) -> String,
    ) -> String {
        let mut name = String::from("`");
        let mut room = TEMPLATE_NAME_MAX_CHARS;
        let mut push = |text: &str| {
            for c in text.chars() {
                let shown: String = if c.is_control() {
                    c.escape_default().collect()
                } else {
                    c.into()
                };
                let width = shown.chars().count();
                if width > room {
                    return false;
                }
                room -= width;
                name.push_str(&shown);
            }
            true
        };
        let complete = shape.holes.iter().all(|hole| {
            let spec = hole
                .spec
                .as_ref()
                .map_or(String::new(), |s| format!(":{s}"));
            push(&format!("${{{}{spec}}}", hole_type(hole.ty)))
        });
        if !complete {
            name.push_str("...");
        }
        name.push('`');
        name
    }

    /// [`Self::anon_struct_name`] in the mangled namespace: every field type is
    /// module-qualified. A shape has no declaration, so its rendering *is* its
    /// identity, and an unqualified one collapses two shapes onto one helper.
    /// Fixed when the shape is interned (see [`AnonEntry`]).
    #[must_use]
    pub fn anon_struct_mangle(&self, id: AnonStructId) -> String {
        self.anon_structs[id.0 as usize].mangle.clone()
    }

    fn render_shape(
        &self,
        shape: &AnonShape,
        field_type: &dyn Fn(&Self, TypeId) -> String,
    ) -> String {
        fn digest_len(h: &mut sha2::Sha256, n: usize) {
            h.update((n as u64).to_le_bytes());
        }
        fn digest_str(h: &mut sha2::Sha256, s: &str) {
            digest_len(h, s.len());
            h.update(s.as_bytes());
        }

        match shape {
            AnonShape::Synthetic(name) => name.clone(),
            AnonShape::Fields(fields) => {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(n, ty)| format!("{n}:{}", field_type(self, *ty)))
                    .collect();
                format!("$anon_{{{}}}", parts.join(","))
            }
            // A specifier is arbitrary text, so the name is a digest of the
            // shape, its hole types rendered through `field_type`. It names a
            // compiled artifact, so no Rust release may move it, and every
            // piece goes in length-prefixed so no two shapes encode alike.
            AnonShape::Template(shape) => {
                let mut h = sha2::Sha256::new();
                digest_len(&mut h, shape.holes.len());
                for hole in &shape.holes {
                    digest_str(&mut h, &field_type(self, hole.ty));
                    match &hole.spec {
                        Some(spec) => {
                            h.update([1u8]);
                            digest_str(&mut h, spec);
                        }
                        None => h.update([0u8]),
                    }
                }
                let bytes: [u8; 32] = h.finalize().into();
                let mut mangle = TEMPLATE_SHAPE_PREFIX.to_string();
                for b in &bytes[..8] {
                    let _ = write!(mangle, "{b:02x}");
                }
                mangle
            }
        }
    }

    /// The name a struct head renders to: the declaration's, or the shape's.
    #[must_use]
    pub fn struct_head_name(&self, head: StructDef) -> String {
        match head {
            StructDef::Decl(def) => self.decl_render_name(def),
            StructDef::Anon(id) => self.anon_struct_mangle(id),
        }
    }

    /// Whether `id` is stored as `i128` or `u128`, newtypes of them included.
    #[must_use]
    pub fn is_wide_int(&self, id: TypeId) -> bool {
        self.wide_int_item(id).is_some()
    }

    /// A struct head as a mangled name embeds it: the declaration when it names
    /// one, the interned shape otherwise.
    #[must_use]
    pub fn fq_struct_head(&self, head: StructDef) -> FqTypeName {
        match head {
            StructDef::Decl(def) => FqTypeName::declared(&self.defs, def),
            StructDef::Anon(id) => {
                FqTypeName::shape(self.anon_struct_module(id), &self.anon_struct_mangle(id))
            }
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

    /// What `Reflect::type_name()` answers for a struct: its declaration's name.
    /// A struct literal's anonymous struct names no declaration, so it answers
    /// `""`, and its `Inspect` writes `{ x: 1 }` as the literal does.
    #[must_use]
    pub fn struct_reflect_name(&self, head: StructDef) -> String {
        match head {
            StructDef::Anon(id)
                if matches!(self.anon_structs[id.0 as usize].shape, AnonShape::Fields(_)) =>
            {
                String::new()
            }
            StructDef::Decl(_) | StructDef::Anon(_) => self.struct_head_decl_name(head),
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
    pub fn compiler_item_def(&self, item: CompilerItem) -> Option<DefId> {
        self.compiler_items
            .decl(item)
            .and_then(|ast| self.defs.of_ast_id(ast))
    }

    /// Whether `def` is the declaration `item` names.
    #[must_use]
    pub fn is_compiler_item(&self, def: DefId, item: CompilerItem) -> bool {
        self.compiler_item_def(item) == Some(def)
    }

    /// Whether `head` is the struct `item` names.
    #[must_use]
    pub fn is_compiler_struct(&self, head: StructDef, item: CompilerItem) -> bool {
        head.decl()
            .is_some_and(|def| self.is_compiler_item(def, item))
    }

    /// Like [`Self::compiler_item_def`], but ICEs rather than answering `None`
    /// — for the items the compiler requires to be registered.
    #[must_use]
    pub fn require_compiler_item_def(&self, item: CompilerItem) -> DefId {
        self.compiler_item_def(item)
            .unwrap_or_else(|| panic!("compiler item `{item}` names no declaration"))
    }

    /// The declaration a nominal type was written from, if it names one.
    ///
    /// This is identity: compare these, never the names below.
    #[must_use]
    pub fn nominal_def(&self, id: TypeId) -> Option<DefId> {
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

    pub fn register_decl_type(&mut self, key: AstId, type_id: TypeId) {
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
    pub fn register_mono_type(&mut self, base_key: AstId, type_id: TypeId) {
        self.symbol_by_type.set_growing(type_id, base_key);
    }

    /// Canonical `TypeId` for a declared-type [`AstId`](crate::ast::AstId).
    ///
    /// Returns `None` if the symbol is not a decl-backed type, or if the
    /// elaborator has not yet created a `TypeId` for it.
    pub fn type_of_symbol(&self, key: &AstId) -> Option<TypeId> {
        self.type_by_symbol.get(key).copied()
    }

    /// Canonical `TypeId` for a declared-type [`AstId`](crate::ast::AstId).
    /// Prefer this over re-deriving one from `(name, module_source)`: the
    /// `AstId` is a cheap `Copy` key, and it stays unique even where name+module
    /// does not — a local type can share another's name.
    ///
    /// Panics if `collect_types` has not run for this declaration; both that and
    /// a non-decl-backed `AstId` are compiler bugs, not recoverable conditions.
    pub fn type_id_of_decl(&self, key: AstId) -> TypeId {
        self.type_of_symbol(&key).unwrap_or_else(|| {
            panic!(
                "type_id_of_decl: no TypeId registered for {key:?} — \
                 collect_types must run before type resolution queries it"
            )
        })
    }

    /// Walk a decl-backed `TypeId` (including monomorphizations) back to the
    /// declaring [`AstId`](crate::ast::AstId).
    pub fn symbol_of_type(&self, type_id: TypeId) -> Option<&AstId> {
        self.symbol_by_type.get(type_id)
    }

    /// The declaring [`AstId`](crate::ast::AstId) behind `type_id`, whether it
    /// is a plain declaration, a monomorphization, or a `GenericInstance`.
    ///
    /// `Node<i32>` and `Node<String>` answer with the one `Node` they were
    /// spelled from; a `Node` in another module answers with a different id.
    pub fn decl_of_type(&self, type_id: TypeId) -> Option<AstId> {
        let type_id = self.peel_refs(type_id);
        if let Some(key) = self.symbol_by_type.get(type_id) {
            return Some(*key);
        }
        // `Array<T>` is declared definitionless (`type Array<T>;`), so an
        // instantiation is a `BuiltinArray` that carries no `def` and that
        // `symbol_by_type` does not key. The `array` compiler item names its
        // declaration by identity — a primitive needs no such step because its
        // declaration and its instantiation are the same type.
        if matches!(self.get(type_id), ResolvedType::BuiltinArray(_)) {
            return self.compiler_items.decl(CompilerItem::Array);
        }
        // An instantiation records the declaration it came from, so the
        // answer is read off the type rather than re-derived from a spelling
        // whose base may already have been pruned.
        let ResolvedType::GenericInstance { def, .. } = self.get(type_id) else {
            return None;
        };
        Some(self.defs.ast_id(*def))
    }

    /// Whether `decl` is one of the four reflection member handles, whose own
    /// `Members` would mention `StructField<Self, …>` and grow `Self` without
    /// bound (WEP 2026-06-13).
    ///
    /// Matched by declaration, so a user type spelled `StructField` stays
    /// reflectable.
    pub fn is_sealed_reflect_member(&self, decl: AstId) -> bool {
        [
            CompilerItem::ReflectStructField,
            CompilerItem::ReflectVariantCase,
            CompilerItem::ReflectEnumCase,
            CompilerItem::ReflectFlagsBit,
            CompilerItem::ReflectTemplateHole,
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

    /// Every type the registries answer each associated type with, and the
    /// trait arguments each answer is filed under, by declaring trait and name.
    fn registered_answers(&self) -> IndexMap<(DefId, String), Vec<TypeId>> {
        let concrete = self
            .assoc_type_resolutions
            .iter()
            .map(|(key, answers)| ((key.trait_decl, key.assoc_name.clone()), answers));
        let generic = self
            .generic_assoc_type_defs
            .iter()
            .map(|(key, answers)| ((key.trait_decl, key.assoc_name.clone()), answers));
        let mut out: IndexMap<(DefId, String), Vec<TypeId>> = IndexMap::default();
        for (key, answers) in concrete.chain(generic) {
            out.entry(key).or_default().extend(answers.types());
        }
        out
    }

    /// Remove all type entries whose `TypeId` is not in `keep`. Erased entries
    /// become `None` holes rather than being renumbered away, so surviving ids
    /// keep their indices, and the intern map and secondary indices are rebuilt.
    /// `get(id)` must not panic for a surviving id, so `keep` is implicitly
    /// closed under `redirects` and stale redirect entries are dropped.
    pub fn retain(&mut self, keep: &IndexSet<TypeId>) {
        // Built where a family's projection first needs it.
        let mut registered: Option<IndexMap<(DefId, String), Vec<TypeId>>> = None;
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
        // A surviving type spells itself with the ids it is built from, and the
        // reachability walk reaches it through its erased view alone. Whatever
        // reads one back — `contains_type_param` over a `Function`'s params,
        // `peel_refs_and_box` over a `Box` wrapper — needs them to resolve.
        while let Some(id) = queue.pop() {
            let mut components: Vec<TypeId> = Vec::new();
            if let Some(&payload) = self.box_payload_types.get(id) {
                components.push(payload);
            }
            if let Some(wrapper) = self.slot_box(id) {
                components.push(wrapper);
            }
            if let Some(&settled) = self.settled_spellings.get(id) {
                components.push(settled);
            }
            if let Some(ty) = self.types.get(id) {
                match ty {
                    ResolvedType::Struct {
                        def: StructDef::Anon(shape),
                        ..
                    } => {
                        self.any_anon_shape_type(*shape, &mut |t| {
                            components.push(t);
                            false
                        });
                    }
                    ResolvedType::Struct { type_args, .. }
                    | ResolvedType::GenericInstance { type_args, .. }
                    | ResolvedType::GenericResource { type_args, .. } => {
                        components.extend(type_args.iter().copied());
                    }
                    ResolvedType::Newtype {
                        type_args,
                        base_type,
                        ..
                    } => {
                        components.extend(type_args.iter().copied());
                        components.push(*base_type);
                    }
                    ResolvedType::Ref(inner)
                    | ResolvedType::MutRef(inner)
                    | ResolvedType::BuiltinArray(inner) => components.push(*inner),
                    ResolvedType::Function {
                        params,
                        return_type,
                        ..
                    } => {
                        components.extend(params.iter().copied());
                        components.push(*return_type);
                    }
                    ResolvedType::TypePack { mapped_elem, .. } => {
                        components.extend(mapped_elem.iter().copied());
                    }
                    ResolvedType::AssocTypeProjection {
                        param_id,
                        args,
                        owning_trait,
                        trait_args,
                        assoc_name,
                        assoc_type_bindings,
                    } => {
                        components.push(*param_id);
                        components.extend(projection_arguments(args, trait_args));
                        components.extend(assoc_type_bindings.iter().map(|(_, t)| *t));
                        // What a substitution will answer a family's
                        // projection with: the binding over the family's own
                        // parameters, which no instantiated type reaches.
                        if !args.is_empty()
                            && let Some(answers) = registered
                                .get_or_insert_with(|| self.registered_answers())
                                .get(&(*owning_trait, assoc_name.clone()))
                        {
                            components.extend(answers.iter().copied());
                        }
                    }
                    ResolvedType::Primitive(_)
                    | ResolvedType::Unit
                    | ResolvedType::Never
                    | ResolvedType::Enum { .. }
                    | ResolvedType::Resource { .. }
                    | ResolvedType::Variant { .. }
                    | ResolvedType::Flags { .. }
                    | ResolvedType::TypeParam { .. }
                    | ResolvedType::AssocParam { .. }
                    | ResolvedType::InferVar(_)
                    | ResolvedType::Unknown
                    | ResolvedType::Error => {}
                }
            }
            for component in components {
                keep_id(&mut effective_keep, &mut queue, component);
                if let Some(&target) = self.redirects.get(component) {
                    keep_id(&mut effective_keep, &mut queue, target);
                }
            }
        }

        // Punch holes for dropped ids; `TypeId`s are never renumbered, so
        // surviving entries keep their indices.
        self.types.retain(|id, _| effective_keep.contains(&id));
        // Read off the types just pruned, so asked again where needed.
        self.projection_bounds_cache.clear();
        // A redirect entry is meaningful only when both endpoints survive.
        self.redirects
            .retain(|id, &target| effective_keep.contains(&id) && effective_keep.contains(&target));
        self.box_payload_types.retain(|id, &payload| {
            effective_keep.contains(&id) && effective_keep.contains(&payload)
        });
        self.slot_boxes
            .retain(|_, wrapper| effective_keep.contains(wrapper));
        self.settled_spellings
            .retain(|id, _| effective_keep.contains(&id));
        self.unindexed.retain(|id| effective_keep.contains(id));
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
            // A redefined reference keeps its spelling in the slot alone.
            // Re-interning it would hand a later `intern(&T)` an id that no
            // longer resolves as a reference.
            if self.box_redefinition(id).is_none() {
                self.intern_map.insert(ty.clone(), id);
            }
        }
        let indexed: Vec<(TypeId, ResolvedType)> = self
            .types
            .iter()
            .filter(|(id, _)| !self.unindexed.contains(id))
            .map(|(id, ty)| (id, ty.clone()))
            .collect();
        for (id, ty) in indexed {
            self.index_type(id, &ty);
        }
    }

    /// The declaration a non-struct nominal type interns under.
    fn nominal_key(ty: &ResolvedType) -> Option<DefId> {
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
    pub fn compiler_items(&self) -> &CompilerItems {
        &self.compiler_items
    }

    /// Mutable handle on the registry. Used by the elaborator during the
    /// annotate pass to register each `#[compiler_item("...")]`
    /// declaration.
    pub fn compiler_items_mut(&mut self) -> &mut CompilerItems {
        &mut self.compiler_items
    }

    /// Record that `head` satisfied a `T: <trait>` bound structurally. A no-op if
    /// already recorded — the same type is rediscovered from many call sites.
    ///
    /// The head is the identity synthesis compares: a declaration by its
    /// [`crate::defs::DefId`], a shape by its rendering.
    pub fn record_bound_driven_synth_request(
        &mut self,
        head: &TypeHead,
        module_source: &ModuleSource,
        trait_key: &DefId,
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
        trait_key: &DefId,
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
        mut matches: impl FnMut(&DefId) -> bool,
    ) -> Vec<(TypeHead, ModuleSource, DefId)> {
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
    pub fn compiler_struct_name(&self, item: CompilerItem) -> &str {
        self.compiler_items.struct_name(item)
    }

    /// The fq name of a compiler-item struct: its declaring module plus its
    /// name, the form any name that embeds a receiver expects.
    pub fn compiler_struct_fq_name(&self, item: CompilerItem) -> FqTypeName {
        let decl = self
            .compiler_items
            .struct_decl(item)
            .expect("a registered struct item records its declaring node");
        let def = self.defs.def_at(decl);
        FqTypeName::declared(&self.defs, def)
    }

    pub fn compiler_trait_name(&self, item: CompilerItem) -> &str {
        self.compiler_items.trait_name(item)
    }

    /// Whether `type_id` is an unsigned integer, `u128` included. An integer
    /// literal pattern asks this to pick the `u128` over the `i128` comparison.
    #[must_use]
    pub fn is_unsigned_int(&self, type_id: TypeId) -> bool {
        // Through the newtype chain: a newtype over `u32` compares unsigned, or
        // a bound past `i32::MAX` never matches.
        matches!(
            self.primitive_head(type_id),
            Some(PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64)
        ) || self.wide_int_item(type_id) == Some(CompilerItem::U128)
    }

    /// Which wide-integer prelude struct `type_id` is stored as, through any
    /// newtype chain; `None` for anything else. By declaration identity: a
    /// name match also answers for a user type.
    #[must_use]
    pub fn wide_int_item(&self, type_id: TypeId) -> Option<CompilerItem> {
        let ResolvedType::Struct {
            def: StructDef::Decl(def),
            ..
        } = self.get(self.representation_head(type_id))
        else {
            return None;
        };
        self.compiler_type_item(*def)
            .filter(|item| matches!(item, CompilerItem::I128 | CompilerItem::U128))
    }

    /// Which compiler item declares the type `def`; `None` for a trait.
    #[must_use]
    pub fn compiler_type_item(&self, def: DefId) -> Option<CompilerItem> {
        self.compiler_items.type_item_of_decl(self.defs.ast_id(def))
    }

    /// How the compiler builds an application of `def`, for the generic heads
    /// it builds a type of its own for.
    #[must_use]
    pub fn compiler_generic_builder(&self, def: DefId) -> Option<fn(&mut Self, TypeId) -> TypeId> {
        match self.compiler_type_item(def)? {
            CompilerItem::Option => Some(Self::make_option),
            CompilerItem::Stream => Some(Self::make_stream),
            CompilerItem::StreamWritable => Some(Self::make_stream_writable),
            CompilerItem::Future => Some(Self::make_future),
            CompilerItem::FutureWritable => Some(Self::make_future_writable),
            CompilerItem::Array => Some(Self::make_builtin_array),
            _ => None,
        }
    }

    /// The compiler trait item as a mangled method name embeds it — named by
    /// the module that declares it.
    #[must_use]
    pub fn compiler_trait_fq(&self, item: CompilerItem) -> FqTraitName {
        self.compiler_items.trait_fq(item)
    }

    pub fn compiler_variant_name(&self, item: CompilerItem) -> &str {
        self.compiler_items.variant_name(item)
    }

    /// Whether `id` resolves to an instance of the compiler `Result` variant.
    pub fn is_result(&self, id: TypeId) -> bool {
        self.is_compiler_item_type(id, CompilerItem::Result)
    }

    /// Whether `id` is an instance of the compiler's `List` struct.
    pub fn is_list(&self, id: TypeId) -> bool {
        self.is_compiler_item_type(id, CompilerItem::List)
    }

    /// Whether `id` is the compiler's `String` struct.
    pub fn is_string(&self, id: TypeId) -> bool {
        self.is_compiler_item_type(id, CompilerItem::String)
    }

    /// Whether `id`, refs peeled, is the type a compiler item declares, compared
    /// by the `def` it carries (WEP 2026-08-12).
    pub fn is_compiler_item_type(&self, id: TypeId, item: CompilerItem) -> bool {
        // A dead declaration's body is cleared in place and its signature types
        // go with the prune, so an optimizer pass reading one off a function
        // record holds an id this table no longer carries.
        let Some(id) = self.try_peel_refs(id) else {
            return false;
        };
        self.nominal_def(id)
            .is_some_and(|def| self.is_compiler_item(def, item))
    }

    pub fn compiler_enum_name(&self, item: CompilerItem) -> &str {
        self.compiler_items.enum_name(item)
    }

    /// Owned `(module, name)` for a registered struct / enum item — forwards
    /// the registry's `CompilerItems::struct_owned` so single-expression
    /// callers query the table directly instead of through `compiler_items()`.
    pub fn compiler_struct_owned(&self, item: CompilerItem) -> (ModuleSource, String) {
        self.compiler_items.struct_owned(item)
    }

    pub fn compiler_enum_owned(&self, item: CompilerItem) -> (ModuleSource, String) {
        self.compiler_items.enum_owned(item)
    }

    /// Module source of a registered struct item, if present.
    pub fn compiler_struct_module(&self, item: CompilerItem) -> Option<&ModuleSource> {
        self.compiler_items.struct_module(item)
    }

    /// Case name of a registered variant-case item (e.g. `Option::Some`).
    pub fn compiler_variant_case_name(&self, item: CompilerItem) -> &str {
        self.compiler_items.variant_case_name(item)
    }

    /// Module + variant name + case name + discriminant of a registered
    /// variant-case item.
    pub fn compiler_variant_case(&self, item: CompilerItem) -> (&ModuleSource, &str, &str, u32) {
        self.compiler_items.require_variant_case(item)
    }

    /// Module + owner-type name + method name of a registered method item.
    pub fn compiler_method(&self, item: CompilerItem) -> (&ModuleSource, &str, &str) {
        self.compiler_items.require_method(item)
    }

    /// Get the module source where the `Default` trait is defined, if
    /// the stdlib has registered it. Thin wrapper around
    /// `CompilerItems::trait_module`.
    pub fn default_trait_module_source(&self) -> Option<&ModuleSource> {
        self.compiler_items.trait_module(CompilerItem::Default)
    }

    /// Make the struct type for a registered `CompilerItem` variant
    /// of kind `CompilerItemKind::Struct`. Reads both the module
    /// source and the struct name from the registry so the call site
    /// does not hard-code either. Panics with a clear ICE message when
    /// the item is not registered or has the wrong kind.
    pub fn make_compiler_struct(&mut self, item: CompilerItem) -> TypeId {
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
    pub fn make_compiler_enum(&mut self, item: CompilerItem) -> TypeId {
        let def = self.require_compiler_item_def(item);
        self.make_enum(def)
    }

    /// Create an `Option<T>` type using the module source registered
    /// via `#[compiler_item("option")]`.
    pub fn make_option(&mut self, inner: TypeId) -> TypeId {
        let def = self
            .compiler_item_def(CompilerItem::Option)
            .expect("the Option declaration is a registered compiler item");
        self.make_generic_instance(def, vec![inner])
    }

    /// Create a `Result<T, E>` type using the module source registered
    /// via `#[compiler_item("result")]`.
    pub fn make_result(&mut self, ok: TypeId, err: TypeId) -> TypeId {
        let def = self
            .compiler_item_def(CompilerItem::Result)
            .expect("the Result declaration is a registered compiler item");
        self.make_generic_instance(def, vec![ok, err])
    }

    /// Create a `Future<T>` generic resource type.
    pub fn make_future(&mut self, inner: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::Future);
        self.intern(ResolvedType::GenericResource {
            def,
            type_args: vec![inner],
        })
    }

    /// Create a `FutureWritable<T>` generic resource type.
    pub fn make_future_writable(&mut self, inner: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::FutureWritable);
        self.intern(ResolvedType::GenericResource {
            def,
            type_args: vec![inner],
        })
    }

    /// Create a `Stream<T>` generic resource type.
    pub fn make_stream(&mut self, inner: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::Stream);
        self.intern(ResolvedType::GenericResource {
            def,
            type_args: vec![inner],
        })
    }

    /// Create a `StreamWritable<T>` generic resource type.
    pub fn make_stream_writable(&mut self, inner: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::StreamWritable);
        self.intern(ResolvedType::GenericResource {
            def,
            type_args: vec![inner],
        })
    }

    pub fn make_started_read(&mut self, elem: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::StartedRead);
        self.make_generic_instance(def, vec![elem])
    }

    pub fn make_started_write(&mut self, elem: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::StartedWrite);
        self.make_generic_instance(def, vec![elem])
    }

    pub fn make_future_write(&mut self, payload: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::FutureWrite);
        self.make_generic_instance(def, vec![payload])
    }

    pub fn make_subtask(&mut self) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::Subtask);
        self.make_resource(def)
    }

    /// Create a `AsyncCall<T>` generic struct instance type.
    ///
    /// Unlike `Future<T>` and `Stream<T>` (which are CM handle resources),
    /// `AsyncCall<T>` is a Wado-level struct carrying both the raw subtask
    /// handle and the result buffer, so it is represented as a
    /// `GenericInstance`, not a `GenericResource`.
    pub fn make_async_call(&mut self, inner: TypeId) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::AsyncCall);
        self.make_generic_instance(def, vec![inner])
    }

    pub fn make_result_buffer(&mut self) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::ResultBuffer);
        self.make_struct(StructDef::Decl(def))
    }

    pub fn make_copy_buffer(&mut self) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::CopyBuffer);
        self.make_struct(StructDef::Decl(def))
    }

    /// If `type_id` is a `AsyncCall<T>` `GenericInstance`, return `T`.
    pub fn as_async_call(&self, type_id: TypeId) -> Option<TypeId> {
        self.single_arg_of(type_id, CompilerItem::AsyncCall)
    }

    /// The one argument of an instance of the compiler generic `item`.
    fn single_arg_of(&self, type_id: TypeId, item: CompilerItem) -> Option<TypeId> {
        match self.get(type_id) {
            ResolvedType::GenericInstance { type_args, .. }
                if type_args.len() == 1 && self.is_compiler_item_type(type_id, item) =>
            {
                Some(type_args[0])
            }
            _ => None,
        }
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

    /// The `T` of a compiler `Box<T>` instance.
    pub fn as_box(&self, type_id: TypeId) -> Option<TypeId> {
        self.single_arg_of(type_id, CompilerItem::Box)
    }

    /// Check if a type is `Option<T>`, returning the inner type if so.
    pub fn as_option(&self, type_id: TypeId) -> Option<TypeId> {
        self.single_arg_of(type_id, CompilerItem::Option)
    }

    /// `TreeMap<K, V>`'s key and value types, keyed by the declaration as
    /// [`Self::as_option`] is.
    pub fn as_tree_map(&self, type_id: TypeId) -> Option<(TypeId, TypeId)> {
        let ResolvedType::GenericInstance { type_args, .. } = self.get(type_id) else {
            return None;
        };
        let [key, value] = type_args[..] else {
            return None;
        };
        self.is_compiler_item_type(type_id, CompilerItem::TreeMap)
            .then_some((key, value))
    }

    /// `Result<T, E>`'s two arguments, keyed by the declaration the registry
    /// records rather than the spelling `Result` (WEP 2026-08-12). A newtype
    /// over one answers through its representation.
    pub fn as_result(&self, type_id: TypeId) -> Option<(TypeId, TypeId)> {
        let head = self.representation_head(type_id);
        let ResolvedType::GenericInstance { type_args, .. } = self.get(head) else {
            return None;
        };
        let [ok, err] = type_args[..] else {
            return None;
        };
        self.is_compiler_item_type(head, CompilerItem::Result)
            .then_some((ok, err))
    }

    pub fn make_tuple(&mut self, elements: Vec<TypeId>) -> TypeId {
        let def = self.require_compiler_item_def(CompilerItem::Tuple);
        self.intern(ResolvedType::GenericInstance {
            def,
            type_args: elements,
        })
    }

    /// Whether `def` declares the built-in tuple family.
    pub fn is_tuple_def(&self, def: DefId) -> bool {
        self.is_compiler_item(def, CompilerItem::Tuple)
    }

    /// Whether a type is a built-in tuple.
    pub fn is_tuple(&self, id: TypeId) -> bool {
        matches!(
            self.get(id),
            ResolvedType::GenericInstance { def, .. } if self.is_tuple_def(*def)
        )
    }

    /// Whether a type is a declared type pack, as opposed to one of the
    /// tuples a pack stands for.
    pub fn is_type_pack(&self, id: TypeId) -> bool {
        matches!(self.get(id), ResolvedType::TypePack { .. })
    }

    /// How a tuple's positions divide into packs and single slots, or `None`
    /// where `id` is no tuple. Equal layouts are the same shape for a value.
    pub fn tuple_layout(&self, id: TypeId) -> Option<Vec<TupleSlot>> {
        Some(
            self.as_tuple(id)?
                .into_iter()
                .map(|e| match self.get(e) {
                    ResolvedType::TypePack { name, .. } => TupleSlot::Pack(name.clone()),
                    _ => TupleSlot::Fixed,
                })
                .collect(),
        )
    }

    /// The type of `t.zip()`: the tuple-of-tuples `t` transposed column by
    /// column, or `None` where its rows share no layout to transpose.
    pub fn transposed_tuple(&mut self, id: TypeId) -> Option<TypeId> {
        let row_ids = self.as_tuple(id)?;
        let layout = self.tuple_layout(*row_ids.first()?)?;
        let mut rows = Vec::with_capacity(row_ids.len());
        for &row in &row_ids {
            if self.tuple_layout(row).as_ref() != Some(&layout) {
                return None;
            }
            rows.push(self.as_tuple(row).expect("a row with a layout is a tuple"));
        }
        let columns: Vec<TypeId> = layout
            .iter()
            .enumerate()
            .map(|(col, slot)| {
                let cells: Vec<TypeId> = rows.iter().map(|row| row[col]).collect();
                match slot {
                    TupleSlot::Fixed => self.make_tuple(cells),
                    // A pack slot stands for a run, so its column is a pack too:
                    // element `k` is the cells with the pack bound to its `k`-th.
                    TupleSlot::Pack(name) => {
                        let parts: Vec<(u32, TypeId)> =
                            cells.iter().map(|&c| self.pack_element(c)).collect();
                        let (index, _) = parts[0];
                        assert!(
                            parts.iter().all(|&(i, _)| i == index),
                            "one scope gives a pack name one index"
                        );
                        let elem = self.make_tuple(parts.iter().map(|&(_, e)| e).collect());
                        self.make_mapped_type_pack(name.clone(), index, elem)
                    }
                }
            })
            .collect();
        Some(self.make_tuple(columns))
    }

    /// What one row contributes at a pack slot: the pack's index, and its
    /// mapped element or the scalar placeholder an identity pack stands for.
    fn pack_element(&mut self, cell: TypeId) -> (u32, TypeId) {
        let ResolvedType::TypePack {
            name,
            index,
            mapped_elem,
        } = self.get(cell).clone()
        else {
            unreachable!("`tuple_layout` marks a slot a pack only for a `TypePack`")
        };
        (
            index,
            mapped_elem.unwrap_or_else(|| self.make_type_param(name, index)),
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
            && self.is_tuple_def(*def)
        {
            Some(type_args.clone())
        } else {
            None
        }
    }

    /// The element types `id` stands for: a tuple's own, and otherwise the one
    /// type itself. This is what a spread splices into the tuple holding it.
    pub fn elem_types_or_self(&self, id: TypeId) -> Vec<TypeId> {
        self.as_tuple(id).unwrap_or_else(|| vec![id])
    }

    pub fn make_function(
        &mut self,
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<EffectRef>,
    ) -> TypeId {
        self.make_function_with_mut(false, params, return_type, effects)
    }

    pub fn make_function_with_mut(
        &mut self,
        is_mut: bool,
        params: Vec<TypeId>,
        return_type: TypeId,
        effects: Vec<EffectRef>,
    ) -> TypeId {
        self.intern(ResolvedType::Function {
            is_mut,
            params,
            return_type,
            effects,
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
        self.rendered_name(&self.struct_head_name(head), type_args)
    }

    /// The rendered spelling of the struct type `id`, which a struct literal
    /// of it carries as its name.
    #[must_use]
    pub fn struct_type_rendered_name(&self, id: TypeId) -> String {
        match self.get(id) {
            ResolvedType::Struct { def, type_args } => self.struct_rendered_name(*def, type_args),
            ResolvedType::GenericInstance { def, type_args } => {
                self.generic_rendered_name(*def, type_args)
            }
            other => panic!("`{other:?}` is not a struct type"),
        }
    }

    /// The rendered spelling of an instantiation of `def`: the sibling of
    /// [`Self::struct_rendered_name`] for a declaration named by `DefId`.
    #[must_use]
    pub fn generic_rendered_name(&self, def: DefId, type_args: &[TypeId]) -> String {
        self.rendered_name(&self.decl_render_name(def), type_args)
    }

    fn rendered_name(&self, decl_name: &str, type_args: &[TypeId]) -> String {
        let args: Vec<String> = type_args
            .iter()
            .map(|&a| self.mangle_type_arg_rowless(a))
            .collect();
        mangle_generic_name(decl_name, &args)
    }

    /// Intern the instantiation of `def` with `type_args`, deriving its
    /// rendered spelling rather than taking one from the caller. An empty
    /// `type_args` interns the *declaration* — a different type.
    pub fn make_monomorphized_struct_from_args(
        &mut self,
        def: StructDef,
        type_args: Vec<TypeId>,
    ) -> TypeId {
        // An argument's row selects no representation, so instantiations
        // differing only there are one struct, as their names already are.
        let type_args = type_args
            .into_iter()
            .map(|arg| self.without_rows(arg))
            .collect();
        self.intern(ResolvedType::Struct { def, type_args })
    }

    /// `id` with every function type's row emptied: what it is as a type's
    /// argument, where no row tells two instances apart.
    pub fn without_rows(&mut self, id: TypeId) -> TypeId {
        self.subst_rec(
            id,
            &Leaves {
                erase_rows: true,
                ..Leaves::NONE
            },
        )
    }

    pub fn make_variant(&mut self, def: DefId) -> TypeId {
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
    pub fn register_variant_cases(&mut self, def: DefId, cases: Vec<(String, u32, TypeId)>) {
        self.variant_case_index.insert(def, cases);
    }

    /// Case templates of a variant declaration (see `variant_case_index`).
    pub fn variant_template_cases(&self, def: DefId) -> Option<&[(String, u32, TypeId)]> {
        self.variant_case_index.get(&def).map(Vec::as_slice)
    }

    /// Register the field types of a struct declaration instantiated with
    /// `args` (none for the template) for [`Self::holds_affine_resource`].
    pub fn register_struct_fields(&mut self, def: DefId, args: Vec<TypeId>, fields: Vec<TypeId>) {
        let key = (def, self.type_keys(&args));
        self.struct_field_index.insert(key, fields);
    }

    /// Whether a value of `ty` is, or holds at any depth, an affine resource:
    /// what makes each place holding one a nullable reference to its object
    /// (WEP: Affine Types). A reference, a function and an unrestricted
    /// handle hold none.
    #[must_use]
    pub fn holds_affine_resource(&self, ty: TypeId) -> bool {
        self.holds_affine_in(ty, &[], &mut Vec::new())
    }

    /// [`Self::holds_affine_resource`] inside a template whose type parameters
    /// hold one where `params` says. `open` is the declarations the walk is
    /// inside, each with its `params`: one reached again with the same answers
    /// adds nothing, which ends polymorphic recursion.
    fn holds_affine_in(
        &self,
        ty: TypeId,
        params: &[bool],
        open: &mut Vec<(DefId, Vec<bool>)>,
    ) -> bool {
        let ty = self.representation_head(ty);
        // `List` is the compiler's, so no struct registers its fields.
        if let Some(elem) = self.list_element(ty) {
            return self.holds_affine_in(elem, params, open);
        }
        let (def, args, members): (Option<DefId>, &[TypeId], Vec<TypeId>) = match self.get(ty) {
            ResolvedType::TypeParam { index, .. } => {
                return params.get(*index as usize).copied().unwrap_or(false);
            }
            ResolvedType::Resource { .. } | ResolvedType::GenericResource { .. } => {
                return self.is_resource_object(ty);
            }
            ResolvedType::BuiltinArray(elem) => {
                return self.holds_affine_in(*elem, params, open);
            }
            ResolvedType::Struct {
                def: StructDef::Anon(shape),
                ..
            } => (
                None,
                &[],
                self.anon_struct_fields(*shape)
                    .iter()
                    .map(|(_, ty)| *ty)
                    .collect(),
            ),
            ResolvedType::Struct {
                def: StructDef::Decl(def),
                type_args,
            }
            | ResolvedType::GenericInstance { def, type_args } => {
                if self.is_affine_decl(*def) {
                    return true;
                }
                let Some(members) = self.declared_members(*def, type_args) else {
                    return false;
                };
                // An instance's own fields are concrete, so the arguments its
                // template would read are only the recursion guard's.
                let guarded = (!self.is_tuple_def(*def)).then_some(*def);
                (guarded, type_args.as_slice(), members.types)
            }
            ResolvedType::Variant { def } => match self.variant_case_index.get(def) {
                Some(cases) => (
                    Some(*def),
                    &[],
                    cases.iter().map(|(_, _, payload)| *payload).collect(),
                ),
                None => return false,
            },
            _ => return false,
        };
        let Some(def) = def else {
            return members
                .into_iter()
                .any(|member| self.holds_affine_in(member, params, open));
        };
        if self.is_affine_decl(def) {
            return true;
        }
        let inner: Vec<bool> = args
            .iter()
            .map(|&arg| self.holds_affine_in(arg, params, open))
            .collect();
        if open.iter().any(|(d, p)| *d == def && *p == inner) {
            return false;
        }
        open.push((def, inner.clone()));
        let held = members
            .into_iter()
            .any(|member| self.holds_affine_in(member, &inner, open));
        open.pop();
        held
    }

    /// The member types of declaration `def` instantiated with `type_args`: a
    /// tuple's elements, a struct's fields, a variant's payloads. `None` where
    /// nothing registered them.
    fn declared_members(&self, def: DefId, type_args: &[TypeId]) -> Option<DeclaredMembers> {
        if self.is_tuple_def(def) {
            return Some(DeclaredMembers {
                types: type_args.to_vec(),
                of_template: false,
            });
        }
        if let Some(fields) = self
            .struct_field_index
            .get(&(def, self.type_keys(type_args)))
        {
            return Some(DeclaredMembers {
                types: fields.clone(),
                of_template: false,
            });
        }
        let types = match self.struct_field_index.get(&(def, Vec::new())) {
            Some(fields) => fields.clone(),
            None => self
                .variant_case_index
                .get(&def)?
                .iter()
                .map(|(_, _, payload)| *payload)
                .collect(),
        };
        Some(DeclaredMembers {
            types,
            of_template: true,
        })
    }

    /// Whether a value of `ty` reaches a `&mut`, through which a holder of a
    /// `&` to it may still write: one it is or holds at any depth, or one
    /// behind a function or resource, whose captures and state no type shows.
    /// A type whose members are not registered may hold anything, so it does.
    #[must_use]
    pub fn reaches_mut_ref(&self, ty: TypeId) -> bool {
        self.reaches_mut_ref_in(ty, &[], &mut IndexSet::default())
    }

    /// [`Self::reaches_mut_ref`] of `ty` as written inside a template
    /// instantiated with `args`. `walked` holds what the walk is already
    /// inside, which adds nothing.
    fn reaches_mut_ref_in(
        &self,
        ty: TypeId,
        args: &[TypeId],
        walked: &mut IndexSet<(TypeKey, Vec<TypeKey>)>,
    ) -> bool {
        let bind = |ty: TypeId| match self.get(self.representation_head(ty)) {
            ResolvedType::TypeParam { index, .. } => args.get(*index as usize).copied(),
            _ => Some(ty),
        };
        let Some(ty) = bind(ty) else {
            return true;
        };
        let ty = self.representation_head(ty);
        if !walked.insert((self.type_key(ty), self.type_keys(args))) {
            return false;
        }
        if let Some(elem) = self.list_element(ty) {
            return self.reaches_mut_ref_in(elem, args, walked);
        }
        let (template_args, members): (Vec<TypeId>, Vec<TypeId>) = match self.get(ty) {
            ResolvedType::Primitive(_)
            | ResolvedType::Unit
            | ResolvedType::Never
            | ResolvedType::Enum { .. }
            | ResolvedType::Flags { .. } => return false,
            ResolvedType::Ref(inner) | ResolvedType::BuiltinArray(inner) => {
                return self.reaches_mut_ref_in(*inner, args, walked);
            }
            ResolvedType::Struct {
                def: StructDef::Anon(shape),
                ..
            } => (
                args.to_vec(),
                self.anon_struct_fields(*shape)
                    .iter()
                    .map(|(_, ty)| *ty)
                    .collect(),
            ),
            ResolvedType::Struct {
                def: StructDef::Decl(def),
                type_args,
            }
            | ResolvedType::GenericInstance { def, type_args } => {
                let Some(bound) = type_args
                    .iter()
                    .map(|&arg| bind(arg))
                    .collect::<Option<Vec<_>>>()
                else {
                    return true;
                };
                let Some(members) = self.declared_members(*def, &bound) else {
                    return true;
                };
                let template_args = if members.of_template {
                    bound
                } else {
                    Vec::new()
                };
                (template_args, members.types)
            }
            ResolvedType::Variant { def } => match self.variant_case_index.get(def) {
                Some(cases) => (
                    Vec::new(),
                    cases.iter().map(|(_, _, payload)| *payload).collect(),
                ),
                None => return true,
            },
            ResolvedType::Newtype { base_type, .. } => {
                return self.reaches_mut_ref_in(*base_type, args, walked);
            }
            ResolvedType::MutRef(_)
            | ResolvedType::Function { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::TypeParam { .. }
            | ResolvedType::InferVar(_)
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocParam { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => return true,
        };
        members
            .into_iter()
            .any(|member| self.reaches_mut_ref_in(member, &template_args, walked))
    }

    /// Find a variant type by (name, `module_source`) pair via `intern_map` (O(1)).
    /// Collision-safe across modules when two variant types share a name.
    pub fn find_variant_type(&self, def: DefId) -> Option<TypeId> {
        self.intern_map.get(&ResolvedType::Variant { def }).copied()
    }

    /// Find a resource type by (name, `module_source`) pair via `intern_map` (O(1)).
    /// Collision-safe across modules when two resource types share a name.
    pub fn find_resource_type(&self, def: DefId) -> Option<TypeId> {
        self.intern_map
            .get(&ResolvedType::Resource { def })
            .copied()
    }

    /// Find an enum type by (name, `module_source`) pair via `intern_map` (O(1)).
    /// Collision-safe across modules when two enum types share a name.
    pub fn find_enum_type(&self, def: DefId) -> Option<TypeId> {
        self.intern_map.get(&ResolvedType::Enum { def }).copied()
    }

    /// Find a flags type by (name, `module_source`) pair via `intern_map` (O(1)).
    /// Collision-safe across modules when two flags types share a name.
    pub fn find_flags_type(&self, def: DefId) -> Option<TypeId> {
        self.intern_map.get(&ResolvedType::Flags { def }).copied()
    }

    /// The declaration `module` declares under the WIT name `name`.
    ///
    /// The one place a name still reaches an identity. §9 of the
    /// declaration-identity WEP says why that is unavoidable here alone.
    ///
    /// It cannot mis-identify: `wado-from-idl` generates one module per
    /// interface and each declares a WIT name once, so `module` picks the
    /// generated module and `name` the single declaration in it. A name that
    /// declares nothing answers `None`.
    ///
    /// Only the Component Model boundary may ask it: `synthesis::cm_binding`
    /// and [`crate::component_model::cm_decl_in_interface`]. A Wado name
    /// resolves through [`crate::resolve::Resolutions`] and a stdlib type
    /// through [`Self::compiler_item_def`].
    #[must_use]
    pub(crate) fn cm_decl_in(&self, name: &str, module: &ModuleSource) -> Option<DefId> {
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
        self.find_named_type_by_def(self.cm_decl_in(name, module_source)?)
    }

    /// The type `def` declares, of any kind a Component Model interface names.
    pub fn find_named_type_by_def(&self, def: DefId) -> Option<TypeId> {
        self.find_struct_type(StructDef::Decl(def))
            .or_else(|| self.find_variant_type(def))
            .or_else(|| self.find_enum_type(def))
            .or_else(|| self.find_flags_type(def))
            .or_else(|| self.find_resource_type(def))
    }

    /// [`Self::cm_decl_in`] addressed by the module's path rather than by a
    /// `ModuleSource` value, for a caller holding an interface FQ and no
    /// interner. Answers for a declaration whose type was never interned.
    #[must_use]
    pub fn cm_decl_in_module_named(
        &self,
        name: &str,
        module_name: &str,
        namespace: Option<CmNamespace>,
    ) -> Option<DefId> {
        self.cm_decl_index
            .get(&(name.to_string(), namespace, module_name.to_string()))
            .copied()
    }

    /// Find any decl-backed named type scoped to a single CM *interface*,
    /// addressed by the namespace that owns it and the module it maps to (e.g.
    /// `(Wasi, sockets/ip_name_lookup.wado)`). `namespace` is `None` for a
    /// `core:` module, which carries none.
    ///
    /// [`Self::find_named_type_by_cm_package`] scopes to the package, which
    /// holds several interfaces — two can declare the same name, and that scan
    /// returns whichever registered first.
    #[must_use]
    pub fn find_named_type_by_module_name(
        &self,
        name: &str,
        module_name: &str,
        namespace: Option<CmNamespace>,
    ) -> Option<TypeId> {
        for (type_id, _) in self.all_types() {
            let Some((n, ms)) = self.nominal_head(type_id) else {
                continue;
            };
            if n != name {
                continue;
            }
            let matches = match ms {
                ModuleSource::Binding {
                    namespace: ns,
                    interface,
                } => namespace == Some(ns) && interface.as_str() == module_name,
                ModuleSource::Core { name: cm_name } => {
                    namespace.is_none() && cm_name.as_str() == module_name
                }
                _ => false,
            };
            if matches {
                return Some(type_id);
            }
        }
        None
    }

    /// Find a decl-backed named type scoped to a CM package: any
    /// `module_source` under the `{cm_package}/` prefix, restricted to
    /// `namespace` when the caller knows it and searching every bundled one
    /// when it does not. `cm_package` is the bare segment (`"http"`, `"kiln"`),
    /// not a fully-qualified source; same-named types in distinct packages stay
    /// distinct because `module_source` is part of the intern key.
    pub fn find_named_type_by_cm_package(
        &self,
        name: &str,
        cm_package: &str,
        namespace: Option<CmNamespace>,
    ) -> Option<TypeId> {
        let prefix = format!("{cm_package}/");
        for (type_id, _) in self.all_types() {
            let Some((n, ms)) = self.nominal_head(type_id) else {
                continue;
            };
            if n != name {
                continue;
            }
            match ms {
                ModuleSource::Binding {
                    namespace: ns,
                    interface,
                } if namespace.is_none_or(|want| want == ns) && interface.starts_with(&prefix) => {
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

    /// The interned instance of `def` at `type_args`, if one exists.
    fn find_generic_instance(&self, def: DefId, type_args: &[TypeId]) -> Option<TypeId> {
        let spelling = ResolvedType::GenericInstance {
            def,
            type_args: type_args.to_vec(),
        };
        self.intern_map.get(&spelling).copied()
    }

    pub fn make_enum(&mut self, def: DefId) -> TypeId {
        self.intern(ResolvedType::Enum { def })
    }

    pub fn make_resource(&mut self, def: DefId) -> TypeId {
        self.intern(ResolvedType::Resource { def })
    }

    /// Redefine `id` to resolve as `target`, retiring its own spelling so a
    /// later [`Self::intern`] of that spelling mints a fresh id instead.
    ///
    /// The boxing pass's `&T` → `Box<T>`. The slot keeps the reference, which is
    /// how [`Self::is_mut_box`] still tells `&T` from `&mut T`.
    pub fn redefine_to(&mut self, id: TypeId, target: TypeId) {
        assert!(
            self.redirects.get(target).is_none(),
            "a redirect target is never itself redirected: `get` takes one hop"
        );
        let spelling = self
            .types
            .get(id)
            .cloned()
            .unwrap_or_else(|| panic!("TypeId {id:?} not found in TypeTable"));
        assert!(
            matches!(spelling, ResolvedType::Ref(_) | ResolvedType::MutRef(_)),
            "only a reference is redefined: `box_redefinition` tells one by that shape"
        );
        if self.intern_map.get(&spelling) == Some(&id) {
            self.intern_map.shift_remove(&spelling);
        }
        self.redirects.set_growing(id, target);
    }

    /// Whether `id` bottoms out in a primitive, through any newtype chain.
    pub fn is_primitive_like(&self, id: TypeId) -> bool {
        self.primitive_head(id).is_some()
    }

    /// Whether `id` bottoms out in a primitive that carries Wasm arithmetic.
    /// `v128`'s belongs to the lane type's own impl, and the half types carry
    /// none at all.
    pub fn is_scalar_primitive_like(&self, id: TypeId) -> bool {
        matches!(self.primitive_head(id), Some(p) if p != PrimitiveType::V128 && !p.is_half())
    }

    /// Whether a value of this type leaves nothing on the Wasm stack: unit or
    /// never, or a reference to either — `&x` is transparent at the WIR level.
    /// `type_id_to_wir_type` asserts it answers `WirType::Unit` for exactly these.
    pub fn is_stackless(&self, type_id: TypeId) -> bool {
        let head = self.representation_head(self.peel_refs(type_id));
        matches!(self.get(head), ResolvedType::Unit | ResolvedType::Never)
    }

    /// The referents a cast of `source` to `target` reads through, outermost
    /// first: a cast to anything but a reference converts what its operand's
    /// references point at, and the last of these is the type it converts.
    #[must_use]
    pub fn cast_read_through(&self, source: TypeId, target: TypeId) -> Vec<TypeId> {
        let mut referents = Vec::new();
        if RefKind::from_resolved(self.get(self.representation_head(target))).is_some() {
            return referents;
        }
        let mut operand = source;
        while let ResolvedType::Ref(referent) | ResolvedType::MutRef(referent) =
            self.get(self.representation_head(operand))
        {
            operand = *referent;
            referents.push(operand);
        }
        referents
    }

    /// The type a cast of `source` to `target` converts: the last referent it
    /// reads through, or `source` itself.
    #[must_use]
    pub fn cast_operand_type(&self, source: TypeId, target: TypeId) -> TypeId {
        self.cast_read_through(source, target)
            .last()
            .copied()
            .unwrap_or(source)
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

    /// The reference layer holding the pointee: `&&mut X` → `&mut X`.
    pub fn innermost_ref(&self, mut type_id: TypeId) -> TypeId {
        while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = self.get(type_id)
            && matches!(
                self.get(*inner),
                ResolvedType::Ref(_) | ResolvedType::MutRef(_)
            )
        {
            type_id = *inner;
        }
        type_id
    }

    /// [`Self::peel_refs`] as [`Self::try_get`] is to [`Self::get`]: an id this
    /// table does not carry answers `None` rather than panicking.
    pub fn try_peel_refs(&self, mut type_id: TypeId) -> Option<TypeId> {
        loop {
            match self.try_get(type_id)? {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => type_id = *inner,
                _ => return Some(type_id),
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

    /// Whether a slot (a field, a tuple element, a payload or an array element)
    /// holds a `ty` in a `Box<T>`: an affine variant. A variant value is
    /// immutable, so only a box makes the slot a place that `&mut` writes
    /// (WEP: Affine Types).
    pub fn boxes_slot(&self, ty: TypeId) -> bool {
        self.is_variant_value(ty) && self.holds_affine_resource(ty)
    }

    /// Whether a value of `ty` is a variant's, generic or not, through any
    /// newtype.
    pub fn is_variant_value(&self, ty: TypeId) -> bool {
        match self.get(self.representation_head(ty)) {
            ResolvedType::Variant { .. } => true,
            ResolvedType::GenericInstance { def, .. } => {
                self.variant_template_cases(*def).is_some()
            }
            _ => false,
        }
    }

    /// Record the `Box<T>` a slot holds each `ty` in.
    pub fn register_slot_box(&mut self, ty: TypeId, wrapper: TypeId) {
        let key = self.type_key(ty);
        self.slot_boxes.insert(key, wrapper);
    }

    /// The `Box<T>` a slot holds each `ty` in, where [`Self::boxes_slot`] says
    /// it does.
    pub fn slot_box(&self, ty: TypeId) -> Option<TypeId> {
        self.slot_boxes.get(&self.type_key(ty)).copied()
    }

    /// Whether `type_id` may name storage rather than hold a value: a `&T` /
    /// `&mut T`, or the `Box<T>` the boxing pass redefines one into. Both
    /// spellings answer yes, since after `prepare_types` no signature test
    /// tells a boxed reference from a by-value parameter of the same shape.
    ///
    /// [`RefKind::from_resolved`] alone sees only what is still spelled as a
    /// reference, so a caller reasoning about values asks this instead.
    pub fn is_reference_shaped(&self, type_id: TypeId) -> bool {
        RefKind::from_resolved(self.get(type_id)).is_some()
            || self.box_payload_of(type_id).is_some()
    }

    /// Whether `wrapper` is a boxed reference that can be written through: a
    /// `&mut T` collapsed onto `Box<T>`, where `*q = v` writes the box the
    /// caller still holds. Only an id still spelled as a shared `&T` answers
    /// `false`, so an unclassified wrapper stays writable.
    pub fn is_mut_box(&self, wrapper: TypeId) -> bool {
        self.box_payload_types.get(wrapper).is_some()
            && !matches!(self.spelled_ref(wrapper), Some((_, RefKind::Shared)))
    }

    /// How `id`'s own slot spells a reference, or `None` when it is not one.
    /// The sole reader of a redefined reference; every other view answers `Box<T>`.
    fn spelled_ref(&self, id: TypeId) -> Option<(TypeId, RefKind)> {
        match *self.types.get(id)? {
            ResolvedType::Ref(payload) => Some((payload, RefKind::Shared)),
            ResolvedType::MutRef(payload) => Some((payload, RefKind::Mut)),
            _ => None,
        }
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

    /// The parameters of `owning_trait`'s generic associated type
    /// `assoc_name`, named as the declaration names them: the family's own
    /// arguments, which a binding is written over.
    pub fn family_params(
        &mut self,
        owning_trait: DefId,
        assoc_name: &str,
        names: &[String],
    ) -> Vec<TypeId> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                self.intern(ResolvedType::AssocParam {
                    owning_trait,
                    assoc_name: assoc_name.to_string(),
                    index: index as u32,
                    name: name.clone(),
                })
            })
            .collect()
    }

    /// The id a declaration's parameter at `index` interns to. A pack holds a
    /// tuple in its one slot, so it is a `TypePack` and not a `TypeParam`.
    pub fn make_declared_param(&mut self, name: String, index: u32, is_pack: bool) -> TypeId {
        if is_pack {
            self.make_type_pack(name, index)
        } else {
            self.make_type_param(name, index)
        }
    }

    /// Create an inference variable (see [`ResolvedType::InferVar`]).
    pub fn make_infer_var(&mut self, id: InferVarId) -> TypeId {
        self.intern(ResolvedType::InferVar(id))
    }

    pub fn record_row_flow(&mut self, flow: RowFlow) {
        self.row_flows.insert(flow);
    }

    pub fn row_flows(&self) -> &IndexSet<RowFlow> {
        &self.row_flows
    }

    /// A fresh effect row for the effect check to solve. Ids are unique across
    /// the program, which the check solves as one.
    pub fn mint_effect_var(&mut self) -> EffectRef {
        let id = self.next_effect_var;
        self.next_effect_var += 1;
        EffectRef::Var {
            id,
            name: format!("{INTERNAL_PREFIX}e{id}"),
        }
    }

    /// Record the slot `id` stands for, for diagnostics. Called on every mint,
    /// `None` included: ids restart per module, so a reused id must not read
    /// its last holder's name.
    pub fn set_infer_var_name(&mut self, id: InferVarId, name: Option<String>) {
        match name {
            Some(name) => self.infer_var_names.insert(id, name),
            None => self.infer_var_names.swap_remove(&id),
        };
    }

    /// How `id` reads in a message: its slot's name, else its own.
    fn infer_var_name(&self, id: InferVarId) -> String {
        self.infer_var_names
            .get(&id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
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

    /// Create the projection `<param_id as owning_trait<trait_args>>::assoc_name<args>`,
    /// carrying `assoc_type_bindings` as its builder read them. The declaring
    /// trait is required: without it the projection has no identity to compare.
    pub fn make_assoc_type_projection(
        &mut self,
        param_id: TypeId,
        owning_trait: DefId,
        trait_args: Option<Vec<TypeId>>,
        assoc_name: String,
        args: Vec<TypeId>,
        assoc_type_bindings: Vec<(String, TypeId)>,
    ) -> TypeId {
        self.intern(ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            args,
            owning_trait,
            trait_args,
            assoc_type_bindings,
        })
    }

    /// Record what `owning_trait` declares of its associated type `assoc_name`.
    pub fn register_assoc_type_sig(
        &mut self,
        owning_trait: DefId,
        assoc_name: String,
        sig: Arc<AssocTypeSig>,
    ) {
        self.assoc_type_sigs
            .entry(owning_trait)
            .or_default()
            .insert(assoc_name, sig);
    }

    /// What `owning_trait` declares of its associated type `assoc_name`.
    pub fn assoc_type_sig(
        &self,
        owning_trait: DefId,
        assoc_name: &str,
    ) -> Option<&Arc<AssocTypeSig>> {
        self.assoc_type_sigs.get(&owning_trait)?.get(assoc_name)
    }

    /// `refs`, written in `sig`'s trait frame, at a site: `Self` standing for
    /// `base`, each trait parameter for its argument in `trait_args`, the
    /// associated type's own parameters for `args`. Where the site does not
    /// know the trait's arguments, a ref that reads one says nothing there and
    /// is left out: kept bare, it would claim the trait's defaults.
    pub fn instantiate_trait_refs(
        &mut self,
        sig: &AssocTypeSig,
        refs: &[TraitRef],
        base: TypeId,
        trait_args: Option<&[TypeId]>,
        args: &[TypeId],
    ) -> Vec<TraitRef> {
        let frame = self.frame_slots(&sig.frame, base, trait_args.unwrap_or_default());
        let said: Vec<&TraitRef> = refs
            .iter()
            .filter(|written| self.says_at(sig, written, trait_args.is_some()))
            .collect();
        said.into_iter()
            .map(|written| {
                let at_site = written
                    .args
                    .iter()
                    .map(|&arg| {
                        let in_frame = self.substitute_type_params(arg, &frame);
                        self.instantiate_family(sig.owning_trait, &sig.assoc_name, in_frame, args)
                    })
                    .collect();
                TraitRef::new(written.decl, at_site)
            })
            .collect()
    }

    /// Whether `written`, one of `sig`'s refs, says anything at a site that
    /// knows the trait's arguments where `trait_args_known`: one reading them
    /// says nothing where they are not known. The one rule both readings of a
    /// projection's bounds apply.
    pub fn says_at(&self, sig: &AssocTypeSig, written: &TraitRef, trait_args_known: bool) -> bool {
        trait_args_known || !self.reads_trait_params(sig, written)
    }

    /// Whether `written`, one of `sig`'s refs, reads a parameter of the trait
    /// declaring it, and so means nothing where its arguments are unknown.
    pub fn reads_trait_params(&self, sig: &AssocTypeSig, written: &TraitRef) -> bool {
        written.args.iter().any(|&arg| {
            sig.frame
                .params
                .iter()
                .flatten()
                .any(|&param| self.mentions(arg, param))
        })
    }

    /// The substitution reading `frame` at a reference: `Self` as `base`, each
    /// parameter as its argument in `args`, which name a prefix of them.
    fn frame_slots(
        &self,
        frame: &TraitFrame,
        base: TypeId,
        args: &[TypeId],
    ) -> IndexMap<u32, TypeId> {
        assert!(
            args.len() <= frame.params.len(),
            "a trait is reached at no more arguments than it has parameters"
        );
        let slot_of = |param: TypeId| {
            self.param_slot(param)
                .expect("a trait frame's parameters are slots")
        };
        let mut slots = IndexMap::from_iter([(slot_of(frame.self_param), base)]);
        for (param, &arg) in frame.params.iter().zip(args) {
            if let Some(param) = param {
                slots.insert(slot_of(*param), arg);
            }
        }
        slots
    }

    /// `written`, a reference's arguments to the trait of `frame` reached on
    /// `receiver`, with the defaults filling the rest, each read with `Self`
    /// the receiver and the arguments before it. `None` where a parameter left
    /// out has no default.
    pub fn fill_trait_args(
        &mut self,
        frame: &TraitFrame,
        receiver: TypeId,
        written: &[TypeId],
    ) -> Option<Vec<TypeId>> {
        let mut filled = written.to_vec();
        for &default in &frame.defaults[written.len().min(frame.defaults.len())..] {
            let slots = self.frame_slots(frame, receiver, &filled);
            filled.push(self.substitute_type_params(default?, &slots));
        }
        Some(filled)
    }

    /// Whether `id` is or is built over `part`.
    fn mentions(&self, id: TypeId, part: TypeId) -> bool {
        id == part || self.any_constituent(id, &mut |t| self.mentions(t, part))
    }

    /// `trait_ref` as a name, its arguments spelled.
    pub fn trait_ref_name(&self, trait_ref: &TraitRef) -> FqTraitName {
        FqTraitName::declared(&self.defs, trait_ref.decl).with_args(
            trait_ref
                .args
                .iter()
                .map(|&arg| self.fq_type_name(arg))
                .collect(),
        )
    }

    /// Whether `a` and `b` project one associated type off one base at one
    /// list of arguments: the same type, whatever bindings each carries from
    /// the frame that built it.
    pub fn projects_alike(&self, a: TypeId, b: TypeId) -> bool {
        match (self.get(a), self.get(b)) {
            (
                ResolvedType::AssocTypeProjection {
                    param_id: base_a,
                    assoc_name: name_a,
                    args: args_a,
                    owning_trait: trait_a,
                    trait_args: trait_args_a,
                    ..
                },
                ResolvedType::AssocTypeProjection {
                    param_id: base_b,
                    assoc_name: name_b,
                    args: args_b,
                    owning_trait: trait_b,
                    trait_args: trait_args_b,
                    ..
                },
            ) => {
                self.type_key(*base_a) == self.type_key(*base_b)
                    && name_a == name_b
                    && self.same_types(args_a, args_b)
                    && trait_a == trait_b
                    && self.trait_args_agree(trait_args_a.as_deref(), trait_args_b.as_deref())
            }
            _ => self.type_key(a) == self.type_key(b),
        }
    }

    /// The bounds the declaration puts on projection `id` (`type A: Bound`),
    /// read at it: `Self` its base, the trait's parameters its trait arguments,
    /// the family's its arguments. `None` while its signature is still being
    /// built, which a reader inside that build meets.
    pub fn projection_bounds(&mut self, id: TypeId) -> Option<Arc<[TraitRef]>> {
        let key = self.type_key(id);
        if let Some(bounds) = self.projection_bounds_cache.get(&key) {
            return Some(Arc::clone(bounds));
        }
        let ResolvedType::AssocTypeProjection {
            param_id,
            assoc_name,
            args,
            owning_trait,
            trait_args,
            ..
        } = self.get(id).clone()
        else {
            panic!(
                "projection_bounds asked of {:?}, not a projection",
                self.get(id)
            );
        };
        let sig = Arc::clone(self.assoc_type_sig(owning_trait, &assoc_name)?);
        let bounds: Arc<[TraitRef]> = self
            .instantiate_trait_refs(&sig, &sig.bounds, param_id, trait_args.as_deref(), &args)
            .into();
        self.projection_bounds_cache
            .insert(key, Arc::clone(&bounds));
        Some(bounds)
    }

    /// What `<base as owning_trait>::assoc_name<args>` is, or `None` where
    /// nothing binds it.
    pub fn answer_projection(
        &mut self,
        base: TypeId,
        owning_trait: &DefId,
        trait_args: Option<&[TypeId]>,
        assoc_name: &str,
        args: &[TypeId],
    ) -> Option<TypeId> {
        if !self.projection_base_is_rigid(base) {
            return None;
        }
        let family = self.projection_family(base, owning_trait, trait_args, assoc_name)?;
        Some(self.instantiate_family(*owning_trait, assoc_name, family, args))
    }

    /// The binding [`Self::answer_projection`] instantiates, its own
    /// parameters still open.
    fn projection_family(
        &mut self,
        base: TypeId,
        owning_trait: &DefId,
        trait_args: Option<&[TypeId]>,
        assoc_name: &str,
    ) -> Option<TypeId> {
        // An impl on the reference answers first, then the referent's, as
        // method-call auto-deref does: `&mut MyDe` projects `MyDe`'s.
        let concrete = self.peel_refs(base);
        if concrete != base
            && let Some(resolved) =
                self.resolve_assoc_type_of_trait_at(base, owning_trait, trait_args, assoc_name)
        {
            return Some(resolved);
        }
        if let Some(resolved) =
            self.resolve_assoc_type_of_trait_at(concrete, owning_trait, trait_args, assoc_name)
        {
            return Some(resolved);
        }
        // Identity before spelling: a projection that names its trait is
        // answered exactly, so two traits declaring the same associated-type
        // name on one implementor stay apart (WEP-2026-08-12). The name-keyed
        // forms below give up on that case rather than choosing.
        if let Some(resolved) = self.resolve_trait_assoc_type_of_instance(
            concrete,
            owning_trait,
            trait_args,
            assoc_name,
        ) {
            return Some(resolved);
        }
        if let Some(resolved) =
            self.resolve_assoc_type_qualified(concrete, owning_trait, trait_args, assoc_name)
        {
            return Some(resolved);
        }
        if let Some(resolved) = self.resolve_generic_assoc_type_mono(concrete, assoc_name) {
            return Some(resolved);
        }
        // A scalar primitive's arithmetic is compiler-supplied, so nothing
        // registered its `Output`: it is the receiver itself, a newtype over
        // one included. `v128` is not one — only a lane type's own impl names
        // its output.
        (assoc_name == "Output" && self.is_scalar_primitive_like(concrete)).then_some(concrete)
    }

    /// Register `<concrete_id as trait_ref>::assoc_name` → `resolved_id`: for
    /// `impl Serializer for JsonSerializer`, `"StructSerializer"` →
    /// `JsonStructSerializer`.
    pub fn register_assoc_type_resolution(
        &mut self,
        concrete_id: TypeId,
        trait_ref: TraitRef,
        assoc_name: String,
        resolved_id: TypeId,
    ) {
        let filled = self.filled_trait_args(&trait_ref, concrete_id);
        self.assoc_type_resolutions
            .entry(AssocTypeKey {
                receiver: self.instance_key(concrete_id),
                trait_decl: trait_ref.decl,
                assoc_name,
            })
            .or_default()
            .insert(trait_ref.args, filled, resolved_id, None);
    }

    /// Each associated type `receiver` answers: what synthesis registered for
    /// an open shape, for [`Self::instantiate_assoc_types`] to instantiate at
    /// every shape substituted from it.
    #[must_use]
    pub fn assoc_types_of(&self, receiver: TypeId) -> ReceiverAssocTypes {
        let from = self.instance_key(receiver);
        ReceiverAssocTypes(
            self.assoc_type_resolutions
                .iter()
                .filter(|(key, _)| key.receiver == from)
                .map(|(key, answers)| (key.clone(), answers.clone()))
                .collect(),
        )
    }

    /// Register for `instance` each of `origin`'s associated types under
    /// `substitution`, where `instance` answers none of its own: a literal
    /// writing `instance` gave it its own.
    pub fn instantiate_assoc_types(
        &mut self,
        origin: &ReceiverAssocTypes,
        instance: TypeId,
        substitution: &IndexMap<u32, TypeId>,
    ) {
        let to = self.instance_key(instance);
        for (key, answers) in &origin.0 {
            let key = AssocTypeKey {
                receiver: to.clone(),
                ..key.clone()
            };
            if self.assoc_type_resolutions.contains_key(&key) {
                continue;
            }
            let mut subst = |ty: TypeId| self.substitute_type_params(ty, substitution);
            let mut instantiated = AssocAnswers::default();
            for entry in answers.0.iter().cloned() {
                let written = entry.written.into_iter().map(&mut subst).collect();
                let filled = entry
                    .filled
                    .map(|args| args.into_iter().map(&mut subst).collect());
                instantiated.insert(written, filled, subst(entry.answer), entry.impl_def);
            }
            self.assoc_type_resolutions.insert(key, instantiated);
        }
    }

    /// `trait_ref`'s arguments with the trait's defaults filled where it
    /// leaves them out, read with `Self` the `receiver`. `None` where the
    /// trait's frame is not built, or a parameter it leaves out has no
    /// default.
    fn filled_trait_args(&mut self, trait_ref: &TraitRef, receiver: TypeId) -> Option<Vec<TypeId>> {
        let frame = Arc::clone(self.trait_frames.get(&trait_ref.decl)?);
        self.fill_trait_args(&frame, receiver, &trait_ref.args)
    }

    /// Record `trait_`'s frame.
    pub fn register_trait_frame(&mut self, trait_: DefId, frame: Arc<TraitFrame>) {
        self.trait_frames.insert(trait_, frame);
    }

    /// `trait_`'s frame, where it is built.
    pub fn trait_frame(&self, trait_: DefId) -> Option<&Arc<TraitFrame>> {
        self.trait_frames.get(&trait_)
    }

    /// The [`InstanceKey`] of `id`.
    pub fn instance_key(&self, id: TypeId) -> InstanceKey {
        let args = |type_args: &[TypeId]| type_args.iter().map(|a| self.instance_key(*a)).collect();
        match self.get_unerased(id) {
            ResolvedType::GenericInstance { def, type_args }
            | ResolvedType::Struct {
                def: StructDef::Decl(def),
                type_args,
            } if !type_args.is_empty() => InstanceKey::Nominal(*def, args(type_args)),
            ResolvedType::Ref(inner) => InstanceKey::Ref(Box::new(self.instance_key(*inner))),
            ResolvedType::MutRef(inner) => InstanceKey::MutRef(Box::new(self.instance_key(*inner))),
            unerased => InstanceKey::Other(self.intern_map.get(unerased).copied().unwrap_or(id)),
        }
    }

    /// Resolve `<concrete_id as trait_key>::assoc_name` for a caller that knows
    /// which trait the projection came from. Writing no trait arguments, it
    /// names the defaulted instantiation, else what the rest agree on.
    pub fn resolve_assoc_type_of_trait(
        &self,
        concrete_id: TypeId,
        trait_key: &DefId,
        assoc_name: &str,
    ) -> Option<TypeId> {
        self.resolve_assoc_type_of_trait_at(concrete_id, trait_key, None, assoc_name)
    }

    /// [`Self::resolve_assoc_type_of_trait`] for a projection that knows the
    /// arguments its trait is reached at, which pick the impl among several.
    pub fn resolve_assoc_type_of_trait_at(
        &self,
        concrete_id: TypeId,
        trait_key: &DefId,
        trait_args: Option<&[TypeId]>,
        assoc_name: &str,
    ) -> Option<TypeId> {
        // `ReflectNewtype::Base` is the type itself: every newtype carries what
        // it wraps, instantiation included, so reading it there answers for one
        // written `type N = T` and for `N<i32>` alike.
        if assoc_name == REFLECT_NEWTYPE_BASE
            && let ResolvedType::Newtype { base_type, .. } = self.get(concrete_id)
            && self
                .compiler_items()
                .trait_def(CompilerItem::ReflectNewtype)
                == Some(*trait_key)
        {
            return Some(*base_type);
        }
        self.inheriting(concrete_id, |receiver| {
            self.assoc_type_resolutions
                .get(&AssocTypeKey {
                    receiver: self.instance_key(receiver),
                    trait_decl: *trait_key,
                    assoc_name: assoc_name.to_string(),
                })?
                .at(self, trait_args)
        })
    }

    /// Answer a receiver-keyed lookup, falling back to what a newtype inherits.
    /// A newtype inherits its *immediate* base's impls (WEP 2026-01-29), so the
    /// chain answers a link at a time: a middle link carrying its own impl is
    /// what a jump to the base would step over.
    fn inheriting<T>(&self, receiver: TypeId, lookup: impl Fn(TypeId) -> Option<T>) -> Option<T> {
        let mut current = receiver;
        loop {
            if let Some(found) = lookup(current) {
                return Some(found);
            }
            let ResolvedType::Newtype { base_type, .. } = self.get_unerased(current) else {
                return None;
            };
            current = *base_type;
        }
    }

    /// Resolve `assoc_name` on `concrete_id`, qualified by `owning_trait`,
    /// falling back to the unqualified rule where that trait registered nothing.
    // A projection built under a bound can name the trait that *declared* the
    // associated type while the impl registered it under a subtrait.
    pub fn resolve_assoc_type_qualified(
        &self,
        concrete_id: TypeId,
        owning_trait: &DefId,
        trait_args: Option<&[TypeId]>,
        assoc_name: &str,
    ) -> Option<TypeId> {
        if let Some(resolved) =
            self.resolve_assoc_type_of_trait_at(concrete_id, owning_trait, trait_args, assoc_name)
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
        self.inheriting(concrete_id, |receiver| {
            let receiver = self.instance_key(receiver);
            one_assoc_answer(
                self.assoc_type_resolutions
                    .iter()
                    .filter(move |(key, _)| {
                        key.receiver == receiver && key.assoc_name == assoc_name
                    })
                    .flat_map(|(_, answers)| answers.tagged()),
            )
        })
    }

    /// Register a generic associated type definition.
    /// E.g., for `impl Iterator for ListIter<T> { type Item = T; }`,
    /// register `(ListIter's ``AstId``, "Item") → TypeParam(0, "T")`, written
    /// over the parameters of `impl_def`, or of `ListIter` itself where `None`.
    ///
    /// Keyed by the declaring [`AstId`](crate::ast::AstId): two modules may
    /// each declare a `Node<T>`, and their definitions must not overwrite one
    /// another.
    pub fn register_generic_assoc_type_def(
        &mut self,
        base_decl: AstId,
        trait_ref: TraitRef,
        assoc_name: String,
        type_param_id: TypeId,
        impl_def: Option<DefId>,
    ) {
        self.generic_assoc_type_defs
            .entry(GenericAssocTypeKey {
                target_decl: base_decl,
                trait_decl: trait_ref.decl,
                assoc_name,
            })
            .or_default()
            .insert(trait_ref.args, None, type_param_id, impl_def);
    }

    /// The generic definition of `assoc_name` on `base_decl`, together with
    /// the trait that declares it. `None` when no trait declares the name, or
    /// when two disagree — the same unambiguity rule
    /// [`Self::resolve_assoc_type`] applies to resolved types. With
    /// `type_args`, only an impl whose target binds at them takes part.
    fn generic_assoc_type_def(
        &self,
        base_decl: AstId,
        assoc_name: &str,
        type_args: Option<&[TypeId]>,
    ) -> Option<(DefId, AssocAnswer)> {
        let mut candidates: Vec<(DefId, &AssocAnswer)> = self
            .generic_assoc_type_defs
            .iter()
            .filter(|(key, _)| key.target_decl == base_decl && key.assoc_name == assoc_name)
            .flat_map(|(key, answers)| answers.0.iter().map(move |entry| (key.trait_decl, entry)))
            .filter(|(_, entry)| type_args.is_none_or(|args| self.entry_reaches(entry, args)))
            .collect();
        if candidates.iter().any(|(_, entry)| entry.written.is_empty()) {
            candidates.retain(|(_, entry)| entry.written.is_empty());
        }
        // Several traits may declare one name over a single definition —
        // `IterFilter` answers `Item` as both `Iterator` and `IntoIterator` —
        // so the definitions decide agreement. One answer means one type only
        // where both are written over the same parameters.
        let &(trait_decl, last) = candidates.last()?;
        candidates
            .iter()
            .all(|(_, entry)| entry.answer == last.answer && self.same_frame(entry, last))
            .then(|| (trait_decl, last.clone()))
    }

    /// Whether two entries are written over the same parameters: the
    /// declaration's own, or impl targets that place them alike.
    fn same_frame(&self, a: &AssocAnswer, b: &AssocAnswer) -> bool {
        match (a.impl_def, b.impl_def) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b || self.impl_target_args(a) == self.impl_target_args(b),
            (Some(_), None) | (None, Some(_)) => false,
        }
    }

    /// The parameters a generic `entry` is written over, at an instance whose
    /// declaration takes `type_args`: what its impl's target binds there. An
    /// impl's parameters need not stand at the declaration's positions: one is
    /// nested in an argument (`impl<T> Tr for W<List<T>>`), or written only in
    /// an `<F: fn(...)>` signature.
    fn entry_slots(
        &self,
        entry: &AssocAnswer,
        type_args: &[TypeId],
    ) -> Option<IndexMap<u32, TypeId>> {
        match entry.impl_def {
            Some(def) => self.impl_target_binding(self.impl_target_args(def), type_args),
            None => Some(positional_substitution(type_args)),
        }
    }

    /// Whether `entry`'s impl's target binds at `type_args`:
    /// `impl<T> Tr for W<List<T>>` answers nothing at `W<i32>`.
    fn entry_reaches(&self, entry: &AssocAnswer, type_args: &[TypeId]) -> bool {
        self.entry_slots(entry, type_args).is_some()
    }

    fn answers_reaching(&self, answers: &AssocAnswers, type_args: &[TypeId]) -> AssocAnswers {
        AssocAnswers(
            answers
                .0
                .iter()
                .filter(|entry| self.entry_reaches(entry, type_args))
                .cloned()
                .collect(),
        )
    }

    /// [`Self::entry_slots`] with the slots the impl's bounds project from
    /// those, which interning reaches.
    fn entry_slots_mut(
        &mut self,
        entry: &AssocAnswer,
        type_args: &[TypeId],
    ) -> IndexMap<u32, TypeId> {
        match entry.impl_def {
            Some(def) => self.impl_slots(def, type_args),
            None => positional_substitution(type_args),
        }
    }

    /// Register associated-type resolutions for a freshly monomorphized struct.
    /// A [`ResolvedType::Struct`] carries no type args, so `Foo<…>::Item` can no
    /// longer go through [`Self::resolve_generic_assoc_type`]; each definition on
    /// `base_decl` is instead resolved eagerly against `substitution` and
    /// recorded under `concrete_id` for later [`Self::resolve_assoc_type`] hits.
    pub fn register_monomorphized_assoc_types(
        &mut self,
        concrete_id: TypeId,
        base_decl: AstId,
        substitution: &IndexMap<u32, TypeId>,
    ) {
        let mut type_args: Vec<(u32, TypeId)> =
            substitution.iter().map(|(&i, &t)| (i, t)).collect();
        type_args.sort_by_key(|&(index, _)| index);
        let type_args: Vec<TypeId> = type_args.into_iter().map(|(_, t)| t).collect();
        let defs: Vec<(DefId, String, AssocAnswer)> = self
            .generic_assoc_type_defs
            .iter()
            .filter(|(key, _)| key.target_decl == base_decl)
            .flat_map(|(key, answers)| {
                self.answers_reaching(answers, &type_args)
                    .0
                    .into_iter()
                    .map(move |entry| (key.trait_decl, key.assoc_name.clone(), entry))
            })
            .collect();
        for (trait_decl, assoc_name, entry) in defs {
            let slots = self.entry_slots_mut(&entry, &type_args);
            // The header's arguments are written over the impl's parameters too.
            let args = entry
                .written
                .iter()
                .map(|&arg| self.substitute_type_params(arg, &slots))
                .collect();
            let trait_ref = TraitRef::new(trait_decl, args);
            let resolved = self.substitute_type_params(entry.answer, &slots);
            if self.binding_is_concrete(resolved) {
                self.register_assoc_type_resolution(concrete_id, trait_ref, assoc_name, resolved);
            }
        }
    }

    /// The type arguments a nominal instantiation binds to its declaration's
    /// parameters — the companion to [`Self::decl_of_type`], which names the
    /// declaration. Every query that reasons "declaration plus arguments" asks
    /// here, so a shape that is not a `GenericInstance` is handled once.
    ///
    /// `Array<T>` is the shape that is not: it is declared definitionless, so
    /// it interns as a `BuiltinArray` carrying its element type alone.
    pub fn nominal_type_args(&self, type_id: TypeId) -> Option<Vec<TypeId>> {
        match self.get(type_id) {
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => Some(type_args.clone()),
            ResolvedType::BuiltinArray(elem) => Some(vec![*elem]),
            // A monomorphized `Struct` is deliberately absent: it keeps no type
            // args, and its associated types are registered against the
            // instance itself by `register_monomorphized_assoc_types`.
            _ => None,
        }
    }

    /// The arguments `type_id`'s own declaration is instantiated with, a
    /// newtype's included: what an `impl` header naming that declaration binds.
    /// [`Self::nominal_type_args`] leaves a newtype out, since a newtype's
    /// representation is its base's.
    pub fn declared_type_args(&self, type_id: TypeId) -> Option<Vec<TypeId>> {
        match self.get_unerased(type_id) {
            ResolvedType::Newtype { type_args, .. } => Some(type_args.clone()),
            _ => self.nominal_type_args(type_id),
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
        let type_args = self.nominal_type_args(concrete_id)?;
        let (_, entry) = self.generic_assoc_type_def(
            self.decl_of_type(concrete_id)?,
            assoc_name,
            Some(&type_args),
        )?;
        let slots = self.entry_slots(&entry, &type_args)?;
        let def_type_id = entry.answer;
        match self.get(def_type_id).clone() {
            ResolvedType::TypeParam { index, .. } => slots.get(&index).copied(),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name: inner_assoc_name,
                ..
            } => {
                // The def is `I::InnerName`. Substitute I with the concrete type arg.
                let inner_concrete_id =
                    if let ResolvedType::TypeParam { index, .. } = self.get(param_id).clone() {
                        slots.get(&index).copied()?
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
        self.resolve_generic_assoc_type_of_owner(concrete_id, assoc_name)
            .map(|(_, resolved)| resolved)
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
    pub fn has_generic_assoc_type_def_for_decl(&self, decl: AstId, assoc_name: &str) -> bool {
        self.generic_assoc_type_def(decl, assoc_name, None)
            .is_some()
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
        self.resolve_assoc_type_of_owner(concrete_id, assoc_name)
            .map(|(_, resolved)| resolved)
    }

    /// [`Self::resolve_assoc_type_of_instance`] with the trait whose impl binds
    /// it, which says how many arguments its own parameters take.
    pub fn resolve_assoc_type_of_owner(
        &mut self,
        concrete_id: TypeId,
        assoc_name: &str,
    ) -> Option<(DefId, TypeId)> {
        // Traits agreeing on the answer agree on the type, so any of them names
        // its owner: the answer mentions the parameters of one family at most.
        let resolved = self.inheriting(concrete_id, |receiver| {
            let receiver = self.instance_key(receiver);
            let candidates = self
                .assoc_type_resolutions
                .iter()
                .filter(move |(key, _)| key.receiver == receiver && key.assoc_name == assoc_name)
                .flat_map(|(key, answers)| {
                    answers
                        .tagged()
                        .map(|(bare, answer)| (bare, (key.trait_decl, answer)))
                });
            let answer = one_assoc_answer(candidates.clone().map(|(bare, (_, a))| (bare, a)))?;
            candidates
                .map(|(_, candidate)| candidate)
                .find(|&(_, a)| a == answer)
        });
        resolved.or_else(|| self.resolve_generic_assoc_type_of_owner(concrete_id, assoc_name))
    }

    /// [`Self::resolve_generic_assoc_type_mono`] with the trait whose generic
    /// impl binds it.
    pub fn resolve_generic_assoc_type_of_owner(
        &mut self,
        concrete_id: TypeId,
        assoc_name: &str,
    ) -> Option<(DefId, TypeId)> {
        let type_args = self.nominal_type_args(concrete_id)?;
        let (owner, entry) = self.generic_assoc_type_def(
            self.decl_of_type(concrete_id)?,
            assoc_name,
            Some(&type_args),
        )?;
        let slots = self.entry_slots_mut(&entry, &type_args);
        Some((owner, self.substitute_type_params(entry.answer, &slots)))
    }

    /// [`Self::resolve_assoc_type_of_instance`] for a caller that knows which
    /// trait declares the associated type. The untyped form scans every trait
    /// and gives up when two disagree, so a name several traits share — the
    /// reflection kinds all spell their member channel `Members` — is only
    /// unambiguous here. `trait_args`, where known, pick the impl among the
    /// trait's instantiations, as [`Self::resolve_assoc_type_of_trait_at`]
    /// does.
    pub fn resolve_trait_assoc_type_of_instance(
        &mut self,
        concrete_id: TypeId,
        trait_key: &DefId,
        trait_args: Option<&[TypeId]>,
        assoc_name: &str,
    ) -> Option<TypeId> {
        if let Some(resolved) =
            self.resolve_assoc_type_of_trait_at(concrete_id, trait_key, trait_args, assoc_name)
        {
            return Some(resolved);
        }
        let type_args = self.nominal_type_args(concrete_id)?;
        let decl = self.decl_of_type(concrete_id)?;
        let answers = self.answers_reaching(
            self.generic_assoc_type_defs.get(&GenericAssocTypeKey {
                target_decl: decl,
                trait_decl: *trait_key,
                assoc_name: assoc_name.to_string(),
            })?,
            &type_args,
        );
        // A generic impl records the arguments it writes, which may read its
        // own parameters (`impl<T> Mix<T> for G<T>`), so each is compared at
        // the instance's arguments, defaults filled as `wanted`'s are.
        let reached = trait_args.and_then(|wanted| {
            answers.0.iter().find(|entry| {
                let slots = self.entry_slots_mut(entry, &type_args);
                let at = TraitRef::new(
                    *trait_key,
                    entry
                        .written
                        .iter()
                        .map(|&arg| self.substitute_type_params(arg, &slots))
                        .collect(),
                );
                self.filled_trait_args(&at, concrete_id)
                    .is_some_and(|filled| self.same_types(&filled, wanted))
            })
        });
        let entry = match reached {
            Some(entry) => entry,
            None => answers.bare_entry()?,
        }
        .clone();
        let slots = self.entry_slots_mut(&entry, &type_args);
        Some(self.substitute_type_params(entry.answer, &slots))
    }

    /// [`Self::substitute_type_params`] with `args` read positionally, which a
    /// declaration's own parameters are, since it numbers them densely from zero.
    pub fn substitute_positional(&mut self, type_id: TypeId, args: &[TypeId]) -> TypeId {
        self.substitute_type_params(type_id, &positional_substitution(args))
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
        self.subst_rec(
            type_id,
            &Leaves {
                params: substitution,
                projections,
                ..Leaves::NONE
            },
        )
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
            &Leaves {
                vars: solutions,
                ..Leaves::NONE
            },
        )
    }

    /// `type_id` with each effect parameter or variable `rows` names replaced
    /// by its row, wherever a function type in it carries one.
    pub fn substitute_effect_rows(&mut self, type_id: TypeId, rows: &EffectRows) -> TypeId {
        self.subst_rec(
            type_id,
            &Leaves {
                rows,
                ..Leaves::NONE
            },
        )
    }

    /// A declared type with each effect parameter its rows name replaced by a
    /// fresh variable, for the effect check to solve at one use. Applied in the
    /// declaration's frame, before its type arguments are, so a parameter the
    /// use site declares under the same name stays its own.
    pub fn instantiate_effect_params(&mut self, type_id: TypeId) -> TypeId {
        let mut named = IndexSet::default();
        self.effect_params_in(type_id, &mut named);
        let rows = self.fresh_effect_vars(named.iter());
        self.substitute_effect_rows(type_id, &rows)
    }

    /// A fresh variable for each of the effect parameters `names`, or none
    /// once the effect check has solved the variables: no check is left to
    /// solve a new one.
    pub fn fresh_effect_vars<'n>(
        &mut self,
        names: impl IntoIterator<Item = &'n String>,
    ) -> EffectRows {
        if self.effect_vars_solved {
            return EffectRows::default();
        }
        names
            .into_iter()
            .map(|name| (name.clone(), vec![self.mint_effect_var()]))
            .collect()
    }

    /// What the effect parameters an impl's target writes in its function
    /// types' rows stand for at a receiver with `receiver_args`: the row the
    /// receiver's argument has at the same place, less what the target names
    /// outright there.
    pub fn impl_effect_binding(&self, def: DefId, receiver_args: &[TypeId]) -> EffectRows {
        let mut rows = EffectRows::default();
        for (&written, &arg) in self.impl_target_args(def).iter().zip(receiver_args) {
            self.bind_rows(written, arg, &mut rows);
        }
        rows
    }

    /// What each of the effect parameters `names` stands for where `declared`
    /// meets `actual`: the row `actual` has at the same place, less what
    /// `declared` names outright there. A parameter no row reaches is empty.
    /// `actual` is written in another frame, whose own parameter of the same
    /// name is a different one, so each of `names` is told apart from it by a
    /// fresh variable while the two are matched.
    pub fn effect_binding(
        &mut self,
        declared: TypeId,
        actual: TypeId,
        names: &[String],
    ) -> EffectRows {
        let marks: Vec<(String, EffectRef)> = names
            .iter()
            .map(|name| (name.clone(), self.mint_effect_var()))
            .collect();
        let marked_rows: EffectRows = marks
            .iter()
            .map(|(name, mark)| (name.clone(), vec![mark.clone()]))
            .collect();
        let marked = self.substitute_effect_rows(declared, &marked_rows);
        let mut rows: EffectRows = names
            .iter()
            .map(|name| (name.clone(), Vec::new()))
            .collect();
        self.zip_rows(marked, actual, true, &mut |we, ae| {
            for (name, _) in marks.iter().filter(|(_, mark)| we.contains(mark)) {
                let row = rows.get_mut(name).expect("every name has a row");
                for e in ae.iter().filter(|e| !we.contains(e)) {
                    if !row.contains(e) {
                        row.push(e.clone());
                    }
                }
            }
        });
        rows
    }

    /// [`Self::impl_effect_binding`] for one written type beside the type
    /// that meets it.
    fn bind_rows(&self, written: TypeId, actual: TypeId, rows: &mut EffectRows) {
        self.zip_rows(written, actual, true, &mut |we, ae| {
            let met: Vec<EffectRef> = ae.iter().filter(|e| !we.contains(e)).cloned().collect();
            for param in we.iter().filter(|e| e.is_param()) {
                let row = rows.entry(param.name().to_string()).or_default();
                for e in &met {
                    if !row.contains(e) {
                        row.push(e.clone());
                    }
                }
            }
        });
    }

    /// Walk a declared type beside a type that meets it, calling `each` with
    /// every function type's row in `declared` and the same row in `actual`.
    /// A function's parameters are walked only `through_params`: a value
    /// hands its row on everywhere else, and there the row flows the other way.
    pub fn zip_rows(
        &self,
        declared: TypeId,
        actual: TypeId,
        through_params: bool,
        each: &mut impl FnMut(&[EffectRef], &[EffectRef]),
    ) {
        let all = |d: &[TypeId], a: &[TypeId], each: &mut _| {
            for (&d, &a) in d.iter().zip(a) {
                self.zip_rows(d, a, through_params, each);
            }
        };
        // Two instances of one newtype meet at their own arguments, which a
        // row may sit in that the base never mentions.
        if let (
            ResolvedType::Newtype {
                def: dd,
                type_args: da,
                ..
            },
            ResolvedType::Newtype {
                def: ad,
                type_args: aa,
                ..
            },
        ) = (self.get(declared), self.get(actual))
            && dd == ad
            && !da.is_empty()
        {
            all(da, aa, each);
            return;
        }
        let declared = self.representation_head(declared);
        let actual = self.representation_head(actual);
        match (self.get(declared), self.get(actual)) {
            (
                ResolvedType::Function {
                    params: dp,
                    return_type: dr,
                    effects: de,
                    ..
                },
                ResolvedType::Function {
                    params: ap,
                    return_type: ar,
                    effects: ae,
                    ..
                },
            ) => {
                each(de, ae);
                if through_params {
                    all(dp, ap, each);
                }
                all(&[*dr], &[*ar], each);
            }
            (
                ResolvedType::Ref(d) | ResolvedType::MutRef(d) | ResolvedType::BuiltinArray(d),
                ResolvedType::Ref(a) | ResolvedType::MutRef(a) | ResolvedType::BuiltinArray(a),
            ) => all(&[*d], &[*a], each),
            _ => {
                if let (Some(d), Some(a)) = (
                    self.generic_type_args(declared),
                    self.generic_type_args(actual),
                ) {
                    all(&d, &a, each);
                }
            }
        }
    }

    /// Every effect parameter named in a row of `type_id` that
    /// [`Self::zip_rows`] reaches: those an argument's type can determine, and
    /// no other.
    pub fn effect_params_in(&self, type_id: TypeId, out: &mut IndexSet<String>) {
        self.zip_rows(type_id, type_id, true, &mut |row, _| {
            out.extend(
                row.iter()
                    .filter(|e| e.is_param())
                    .map(|e| e.name().to_string()),
            );
        });
    }

    /// `owning_trait`'s generic associated type `assoc_name`, bound to
    /// `family`, at a projection's `args`: each of that family's
    /// [`ResolvedType::AssocParam`]s replaced by the argument at its index.
    /// Another family's parameters, which `family` may mention, stay.
    pub fn instantiate_family(
        &mut self,
        owning_trait: DefId,
        assoc_name: &str,
        family: TypeId,
        args: &[TypeId],
    ) -> TypeId {
        self.subst_rec(
            family,
            &Leaves {
                family: Some(FamilyAt {
                    owning_trait,
                    assoc_name,
                    args,
                }),
                ..Leaves::NONE
            },
        )
    }

    /// [`Self::instantiate_family`] at arguments given as names, for a reader
    /// that names types without interning them, as mangling does: each
    /// parameter replaced by its argument, all at once.
    pub fn family_name_at(
        &self,
        owning_trait: DefId,
        assoc_name: &str,
        family: TypeId,
        args: &[FqTypeName],
    ) -> FqTypeName {
        let mut params = Vec::new();
        self.collect_assoc_params(family, &mut params);
        let at_args: IndexMap<FqTypeName, FqTypeName> = params
            .into_iter()
            .filter_map(|param| {
                let index = self.family_param_index(param, owning_trait, assoc_name)?;
                Some((self.fq_type_name(param), args[index as usize].clone()))
            })
            .collect();
        self.fq_type_name(family)
            .rewrite(&|node| at_args.get(node).cloned())
    }

    /// The position of `id` among `owning_trait`'s `assoc_name` parameters, or
    /// `None` where it is no parameter of that family.
    fn family_param_index(&self, id: TypeId, owning_trait: DefId, assoc_name: &str) -> Option<u32> {
        match self.get(id) {
            ResolvedType::AssocParam {
                owning_trait: owner,
                assoc_name: name,
                index,
                ..
            } if *owner == owning_trait && name == assoc_name => Some(*index),
            _ => None,
        }
    }

    fn collect_assoc_params(&self, id: TypeId, out: &mut Vec<TypeId>) {
        if let ResolvedType::AssocParam { .. } = self.get(id) {
            if !out.contains(&id) {
                out.push(id);
            }
            return;
        }
        self.any_constituent(id, &mut |t| {
            self.collect_assoc_params(t, out);
            false
        });
    }

    /// The shared traversal behind [`Self::substitute_type_params_with`],
    /// [`Self::substitute_infer_vars`] and [`Self::instantiate_family`].
    fn subst_rec(&mut self, type_id: TypeId, leaves: &Leaves<'_>) -> TypeId {
        if leaves.is_empty() {
            return type_id;
        }
        match self.get(type_id).clone() {
            ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                leaves.params.get(&index).copied().unwrap_or(type_id)
            }
            ResolvedType::AssocParam {
                owning_trait,
                assoc_name,
                index,
                ..
            } => match leaves.family {
                Some(at) if at.owning_trait == owning_trait && at.assoc_name == assoc_name => {
                    at.args[index as usize]
                }
                _ => type_id,
            },
            ResolvedType::InferVar(var) => leaves.vars.get(&var).copied().unwrap_or(type_id),
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.subst_rec(elem, leaves);
                if new_elem == elem {
                    type_id
                } else {
                    self.intern(ResolvedType::BuiltinArray(new_elem))
                }
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.subst_rec(inner, leaves);
                if new_inner == inner {
                    type_id
                } else {
                    self.make_ref(new_inner)
                }
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.subst_rec(inner, leaves);
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
            } => {
                let new_params: Vec<TypeId> =
                    params.iter().map(|&p| self.subst_rec(p, leaves)).collect();
                let new_return_type = self.subst_rec(return_type, leaves);
                let new_effects = leaves.row(&effects);
                if new_params == params && new_return_type == return_type && new_effects == effects
                {
                    type_id
                } else {
                    self.make_function_with_mut(is_mut, new_params, new_return_type, new_effects)
                }
            }
            ResolvedType::GenericResource { def, type_args } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&a| self.subst_rec(a, leaves))
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
                if self.is_tuple_def(def) {
                    // Tuples need TypePack expansion: splice pack elements
                    // into the tuple's type-arg list.
                    let mut new_elems: Vec<TypeId> = Vec::new();
                    for &e in &type_args {
                        match self.get(e).clone() {
                            ResolvedType::TypePack {
                                index, mapped_elem, ..
                            } => {
                                if let Some(&pack_type) = leaves.params.get(&index) {
                                    match mapped_elem {
                                        // Mapped pack: substitute the element
                                        // once per source pack element, binding
                                        // the pack param to that element — a
                                        // constructor map `[..Case<T, P>]`
                                        // yields `Case<T, P_k>` at position k;
                                        // a pack-independent `..F::method()`
                                        // repeats its return type `|F|` times.
                                        Some(elem) => {
                                            let pack_elems = self.elem_types_or_self(pack_type);
                                            for pe in pack_elems {
                                                let mut elem_substitution = leaves.params.clone();
                                                elem_substitution.insert(index, pe);
                                                new_elems.push(self.subst_rec(
                                                    elem,
                                                    &Leaves {
                                                        params: &elem_substitution,
                                                        ..*leaves
                                                    },
                                                ));
                                            }
                                        }
                                        None => {
                                            new_elems.extend(self.elem_types_or_self(pack_type));
                                        }
                                    }
                                } else {
                                    new_elems.push(e);
                                }
                            }
                            _ => {
                                new_elems.push(self.subst_rec(e, leaves));
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
                        .map(|&a| self.subst_rec(a, leaves))
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
                args,
                owning_trait,
                trait_args,
                assoc_type_bindings,
            } => {
                let new_args: Vec<TypeId> =
                    args.iter().map(|&a| self.subst_rec(a, leaves)).collect();
                let new_trait_args: Option<Vec<TypeId>> = trait_args
                    .as_ref()
                    .map(|given| given.iter().map(|&a| self.subst_rec(a, leaves)).collect());
                // The use site's answer wins: a rebuilt projection cannot
                // re-derive what `Self::X` means there.
                if let Some(slot) = self.param_slot(param_id)
                    && let Some(answer) = leaves.projections.get(&slot).and_then(|answers| {
                        answers
                            .iter()
                            .find(|answer| {
                                answer.owning_trait == owning_trait
                                    && answer.assoc_name == assoc_name
                                    && self.trait_args_agree(
                                        answer.trait_args.as_deref(),
                                        new_trait_args.as_deref(),
                                    )
                            })
                            .map(|answer| answer.answer)
                    })
                {
                    return self.instantiate_family(owning_trait, &assoc_name, answer, &new_args);
                }
                let substituted_base = self.subst_rec(param_id, leaves);
                // A projection over a projection is answered by what the base
                // carries: `IntoIterator::Iter: Iterator<Item = Self::Item>`
                // makes `C::Iter::Item` the `C::Item` the frame already named.
                if let ResolvedType::AssocTypeProjection {
                    assoc_type_bindings: base_bindings,
                    ..
                } = self.get(substituted_base)
                    && let Some((_, answer)) =
                        base_bindings.iter().find(|(name, _)| *name == assoc_name)
                {
                    let answer = *answer;
                    return self.instantiate_family(owning_trait, &assoc_name, answer, &new_args);
                }
                if let Some(resolved) = self.answer_projection(
                    substituted_base,
                    &owning_trait,
                    new_trait_args.as_deref(),
                    &assoc_name,
                    &new_args,
                ) {
                    return resolved;
                }
                // Bindings are resolved in the same frame as the rest of the
                // signature, so they carry its slots too.
                let mut new_bindings: Vec<(String, TypeId)> =
                    Vec::with_capacity(assoc_type_bindings.len());
                for (name, bound) in &assoc_type_bindings {
                    let substituted = self.subst_rec(*bound, leaves);
                    new_bindings.push((name.clone(), substituted));
                }
                if substituted_base == param_id
                    && new_args == args
                    && new_trait_args == trait_args
                    && new_bindings == assoc_type_bindings
                {
                    type_id
                } else {
                    self.make_assoc_type_projection(
                        substituted_base,
                        owning_trait,
                        new_trait_args,
                        assoc_name,
                        new_args,
                        new_bindings,
                    )
                }
            }
            ResolvedType::Newtype {
                def,
                type_args,
                base_type,
            } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&a| self.subst_rec(a, leaves))
                    .collect();
                if new_args == type_args {
                    type_id
                } else {
                    let new_base = self.subst_rec(base_type, leaves);
                    self.make_newtype_instance(def, new_args, new_base)
                }
            }
            ResolvedType::Struct {
                def: StructDef::Anon(id),
                ..
            } => self.subst_anon_shape(type_id, id, leaves),
            // Primitives, Unit, Never, Unknown, Error, Enum, Variant, Resource,
            // Flags name no parameter; a declared `Struct` is a monomorphized
            // instance.
            _ => type_id,
        }
    }

    /// Create a generic instance (e.g., `Box<i32>`)
    pub fn make_generic_instance(&mut self, def: DefId, type_args: Vec<TypeId>) -> TypeId {
        // The instantiation carries the declaration it came from, so nothing
        // has to be registered beside it and nothing re-derives the answer
        // from a spelling whose base `prune` may already have dropped.
        self.intern(ResolvedType::GenericInstance { def, type_args })
    }

    /// Create a `List<T>` type (`GenericInstance` { name: "List", ... })
    pub fn make_list(&mut self, element: TypeId) -> TypeId {
        let def = self
            .compiler_item_def(CompilerItem::List)
            .expect("the List declaration is a registered compiler item");
        self.make_generic_instance(def, vec![element])
    }

    /// Create a `TreeMap<K, V>` type — the Wado spelling of CM `map<K, V>`.
    pub fn make_tree_map(&mut self, key: TypeId, value: TypeId) -> TypeId {
        let def = self
            .compiler_item_def(CompilerItem::TreeMap)
            .expect("the TreeMap declaration is a registered compiler item");
        self.make_generic_instance(def, vec![key, value])
    }

    /// Create the `ByteList` newtype (`type ByteList = List<u8>`).
    pub fn make_byte_list(&mut self) -> TypeId {
        let base = self.make_list(TypeTable::U8);
        let def = self.require_compiler_item_def(CompilerItem::ByteList);
        self.make_newtype(def, base)
    }

    /// Create a newtype wrapping a base type
    pub fn make_newtype(&mut self, def: DefId, base_type: TypeId) -> TypeId {
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
        def: DefId,
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
    pub fn make_flags(&mut self, def: DefId) -> TypeId {
        self.intern(ResolvedType::Flags { def })
    }

    /// Erase all `Newtype` and `Flags` entries from the type table by populating
    /// the redirect map. After this call, `get(id)` for any erased `TypeId` returns
    /// its ultimate base type (`Newtype` chains) or `u32` (`Flags`).
    ///
    /// Must be called after monomorphize, whose dispatch reads the identities
    /// this replaces, and settles the instance spellings it leaves.
    pub fn erase_newtypes_and_flags(&mut self) {
        let ids: Vec<TypeId> = self.iter_type_ids().collect();
        for id in ids {
            let redirect = match self.types.get(id).unwrap() {
                ResolvedType::Newtype { .. } => {
                    Some(self.monomorphized_or_self(self.representation_head(id)))
                }
                ResolvedType::Flags { .. } => Some(TypeTable::U32),
                _ => None,
            };
            if let Some(target) = redirect {
                self.redirects.set_growing(id, target);
            }
        }
        self.settle_instance_spellings();
    }

    /// Settle every spelling, and each one interned from here on, now that
    /// monomorphization is done. A site it never reached still spells an
    /// instance as its `GenericInstance`, and a type argument holding that
    /// spelling would otherwise key apart from the same type holding the struct.
    fn settle_instance_spellings(&mut self) {
        let ids: Vec<TypeId> = self.iter_type_ids().collect();
        self.instances_settled = true;
        for id in ids {
            self.settle_spelling(id);
        }
    }

    /// Redirect each type holding an effect variable to the type with each
    /// variable replaced by what `solved` says it stands for, and one nothing
    /// solved by the empty row, once the effect check has solved them.
    pub fn redirect_effect_vars(&mut self, solved: &EffectRows) {
        self.effect_vars_solved = true;
        let ids: Vec<TypeId> = self.iter_type_ids().collect();
        let mut rows = solved.clone();
        for &id in &ids {
            self.unsolved_vars_in(id, &mut rows);
        }
        if rows.is_empty() {
            return;
        }
        // A solution may name another variable, which stands for what that one
        // is solved to, or for nothing where it is solved nowhere.
        let rows: EffectRows = rows
            .iter()
            .map(|(var, solution)| {
                let mut solved = solve_row(solution, &rows, &mut |_| {});
                solved.retain(|effect| !effect.is_var());
                (var.clone(), solved)
            })
            .collect();
        for id in ids {
            if self.redirects.get(id).is_some() {
                continue;
            }
            let solved_type = self.substitute_effect_rows(id, &rows);
            if solved_type != id {
                self.redirects.set_growing(id, solved_type);
            }
        }
    }

    /// Note each effect variable in `id`'s rows that `rows` holds no solution
    /// for, as standing for the empty row.
    fn unsolved_vars_in(&self, id: TypeId, rows: &mut EffectRows) {
        if let ResolvedType::Function { effects, .. } = self.types.get(id).unwrap() {
            for effect in effects.iter().filter(|e| e.is_var()) {
                rows.entry(effect.name().to_string()).or_default();
            }
        }
    }

    /// The monomorphized `Struct` an instantiation of `def` became, when the run
    /// made one. Monomorphization registers each one under this spelling.
    pub fn monomorphized_struct_of(&self, def: DefId, type_args: &[TypeId]) -> Option<TypeId> {
        let name = self.generic_rendered_name(def, type_args);
        self.find_struct_by_name(&name, self.def_module(def))
    }

    /// The monomorphized `Struct` a `GenericInstance` became. Monomorphization
    /// rewrites the sites its walk reaches; a newtype base it never reached
    /// still spells the instance, and a value of the newtype carries the struct.
    pub fn monomorphized_struct(&self, id: TypeId) -> Option<TypeId> {
        let ResolvedType::GenericInstance { def, type_args } = self.types.get(id)? else {
            return None;
        };
        self.monomorphized_struct_of(*def, type_args)
    }

    /// [`Self::monomorphized_struct`] where there is one, else `id` itself.
    pub fn monomorphized_or_self(&self, id: TypeId) -> TypeId {
        self.monomorphized_struct(id).unwrap_or(id)
    }

    /// Get the base type if this is a newtype, or None otherwise
    pub fn get_newtype_base(&self, id: TypeId) -> Option<TypeId> {
        if let ResolvedType::Newtype { base_type, .. } = self.get(id) {
            Some(*base_type)
        } else {
            None
        }
    }

    /// Whether `target::from(value: source)` converts with no impl body behind
    /// it: `source` is `target`, or one lies down the other's newtype chain
    /// (WEP: Conversion Traits).
    #[must_use]
    pub fn converts_without_impl(&self, target: TypeId, source: TypeId) -> bool {
        self.newtype_chain_holds(target, source) || self.newtype_chain_holds(source, target)
    }

    /// Whether `link` is `id` or lies down its newtype chain. A reference is a
    /// type of its own here, never stepped over: `&Meters` does not wrap `i64`
    /// the way `Meters` does.
    #[must_use]
    pub fn newtype_chain_holds(&self, id: TypeId, link: TypeId) -> bool {
        let link = self.type_key(link);
        self.newtype_chain(id).any(|t| self.type_key(t) == link)
    }

    /// A newtype's representation head, or `None` for anything else. The head
    /// rather than one peel, which on a chain lands on another newtype.
    #[must_use]
    pub fn newtype_representation(&self, id: TypeId) -> Option<TypeId> {
        matches!(self.get(id), ResolvedType::Newtype { .. }).then(|| self.representation_head(id))
    }

    /// The first link of `id`'s newtype chain that `owns` accepts, outermost
    /// first: a link's impl serves every level above it, so the chain — not the
    /// receiver alone — says which type a call dispatches to (WEP 2026-01-29).
    /// References are stepped over. `None` when no link owns.
    ///
    /// Reads the unerased view: erasure redirects a link to its base, and the
    /// erased view would report no chain at all.
    pub fn newtype_link_owning(&self, id: TypeId, owns: impl Fn(TypeId) -> bool) -> Option<TypeId> {
        let mut current = id;
        loop {
            match self.get_unerased(current) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => current = *inner,
                ResolvedType::Newtype { base_type, .. } => {
                    if owns(current) {
                        return Some(current);
                    }
                    current = *base_type;
                }
                _ => return None,
            }
        }
    }

    /// Whether `link` is a base anywhere in `id`'s newtype chain, rather than
    /// one peel down, which a longer chain steps past.
    #[must_use]
    pub fn newtype_chain_reaches(&self, id: TypeId, link: TypeId) -> bool {
        self.newtype_link_owning(id, |tid| self.get_newtype_base(tid) == Some(link))
            .is_some()
    }

    /// The declaration a newtype inherits from: its chain peeled to what it
    /// wraps, any other type unchanged (WEP 2026-01-29). That is where its
    /// impls live, and so where a `Reflect*` kind reads its members.
    ///
    /// Unlike [`Self::representation_head`] the walk stops at a `flags`
    /// type, which is a declaration carrying its own impls rather than a
    /// stand-in for `u32`, and it reads the identity ([`Self::get_unerased`]):
    /// erasure redirects a link to its base, and the erased view shows no chain
    /// to walk. Identity is *not* inherited — a newtype names itself through
    /// `Reflect` — so this never answers for a type's name.
    pub fn reflect_structure_head(&self, id: TypeId) -> TypeId {
        self.newtype_chain(id)
            .last()
            .expect("a chain holds the type it starts at")
    }

    /// The type whose declaration answers `Reflect::wire_name_policy` for `id`.
    /// The policy names members, and a newtype inherits its members, so it
    /// answers with the declaration it wraps. A newtype over a type with no
    /// `Reflect` kind (a primitive) answers for itself.
    pub fn wire_policy_subject(&self, id: TypeId) -> TypeId {
        let head = self.reflect_structure_head(id);
        if self.reflect_kind(head).is_some() {
            head
        } else {
            id
        }
    }

    /// `id` and every type its newtype chain wraps, nearest first.
    fn newtype_chain(&self, id: TypeId) -> impl Iterator<Item = TypeId> {
        std::iter::successors(Some(id), |&current| match self.get_unerased(current) {
            ResolvedType::Newtype { base_type, .. } => Some(*base_type),
            _ => None,
        })
    }

    /// The structure a match takes its cases from: references peeled for match
    /// ergonomics, then newtypes, a newtype's cases being its base's.
    pub fn scrutinee_structure_head(&self, id: TypeId) -> TypeId {
        self.reflect_structure_head(self.peel_refs(id))
    }

    /// Every declaration on `id`'s newtype chain, its head's included: the names
    /// that stand for one structure, so any of them names `id`'s cases.
    pub fn structure_chain_defs(&self, id: TypeId) -> Vec<DefId> {
        self.newtype_chain(self.peel_refs(id))
            .filter_map(|t| self.nominal_def(t))
            .collect()
    }

    /// The reflection kind `id` is, or `None` where reflection does not cover
    /// it: the one answer to which `Reflect*` the compiler synthesizes for a
    /// type, and so to which kind bound can hold for it. A `GenericInstance`
    /// takes its declaration's kind — `Pair<i32>` is a struct because `Pair` is.
    ///
    /// The sealed member handles are the one declared struct it withholds.
    pub fn reflect_kind(&self, id: TypeId) -> Option<CompilerItem> {
        if self
            .decl_of_type(id)
            .is_some_and(|decl| self.is_sealed_reflect_member(decl))
        {
            return None;
        }
        match self.get(id) {
            ResolvedType::Struct {
                def: StructDef::Anon(shape),
                ..
            } if self.template_shape(*shape).is_some() => Some(CompilerItem::ReflectTemplate),
            ResolvedType::Struct { .. } => Some(CompilerItem::ReflectStruct),
            ResolvedType::Variant { .. } => Some(CompilerItem::ReflectVariant),
            ResolvedType::Enum { .. } => Some(CompilerItem::ReflectEnum),
            ResolvedType::Flags { .. } => Some(CompilerItem::ReflectFlags),
            ResolvedType::Newtype { .. } => Some(CompilerItem::ReflectNewtype),
            ResolvedType::GenericInstance { def, .. } => {
                let def = *def;
                // A variant is asked first: a variant declaration also registers
                // a struct-shaped payload layout under its own name, so the
                // struct lookup answers for both kinds.
                if self.find_variant_type(def).is_some() {
                    Some(CompilerItem::ReflectVariant)
                } else if self
                    .find_struct_by_name(self.def_name(def), self.def_module(def))
                    .is_some()
                {
                    Some(CompilerItem::ReflectStruct)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// The type `id` is represented by: its newtype chain followed to the end
    /// and a `flags` type answered as `u32`, anything else unchanged.
    /// [`Self::reflect_structure_head`] answers the other question — where a
    /// declaration's impls live — and so stops at `flags`, which writes its own.
    ///
    /// Works before and after `erase_newtypes_and_flags()`: the redirect map is
    /// consulted first, so `FieldName` answers `String` either way.
    pub fn representation_head(&self, id: TypeId) -> TypeId {
        // Fast path: after erasure, redirects always point directly to the base.
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
        self.type_key(self.representation_head(a)) == self.type_key(self.representation_head(b))
    }

    /// Whether `id` is `List<u8>` or a newtype chain over it (`ByteList`): what
    /// a byte-string literal coerces to.
    pub fn is_byte_list_representation(&self, id: TypeId) -> bool {
        self.list_element(self.representation_head(id)) == Some(TypeTable::U8)
    }

    /// Whether `id` is a `List` whose element is still open, which a
    /// byte-string literal settles as `u8`.
    pub fn is_list_of_open_element(&self, id: TypeId) -> bool {
        self.list_element(id).is_some_and(|element| {
            matches!(
                self.get(element),
                ResolvedType::TypeParam { .. } | ResolvedType::InferVar(_)
            )
        })
    }

    /// The fixed-width primitive a sequence type (`Array<T>`, `List<T>`, or a
    /// newtype over either) reads from little-endian data; `None` for the rest.
    pub fn packed_element(&self, seq: TypeId) -> Option<PrimitiveType> {
        self.primitive_head(self.seq_element(seq)?)
            .filter(|p| p.data_width().is_some())
    }

    /// The element type of `Array<T>`, `List<T>`, or a newtype over either.
    pub fn seq_element(&self, seq: TypeId) -> Option<TypeId> {
        let head = self.representation_head(seq);
        match self.get(head) {
            ResolvedType::BuiltinArray(elem) => Some(*elem),
            _ => self.as_list(head),
        }
    }

    /// Check if a type is `List<T>` and return the element type if so.
    /// Also unwraps Ref/MutRef types to check the inner type.
    pub fn as_list(&self, id: TypeId) -> Option<TypeId> {
        match self.get(id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => self.as_list(*inner),
            _ => self.list_element(id),
        }
    }

    /// The element type of `id` when it is a `List` itself, not a reference to one.
    pub fn list_element(&self, id: TypeId) -> Option<TypeId> {
        self.single_arg_of(id, CompilerItem::List)
    }

    /// The element type of `id` when it is the range struct a `kind` range
    /// literal builds.
    pub fn range_element(&self, id: TypeId, kind: RangeKind) -> Option<TypeId> {
        self.single_arg_of(id, range_item(kind))
    }

    /// Check if a type contains UNKNOWN (undefined type that was not resolved).
    pub fn contains_unknown(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::Unknown => true,
            _ => self.any_constituent(id, &mut |t| self.contains_unknown(t)),
        }
    }

    /// Whether `id` carries `!` in an argument position — `Option<!>`, a bare
    /// `null`'s type, names a value of every `Option` and so decides nothing.
    /// `!` itself is not an argument position and answers `false`; callers that
    /// mean "diverges" compare against [`Self::NEVER`].
    pub fn contains_never_arg(&self, id: TypeId) -> bool {
        fn mentions(tt: &TypeTable, id: TypeId) -> bool {
            id == TypeTable::NEVER || tt.any_constituent(id, &mut |t| mentions(tt, t))
        }
        id != TypeTable::NEVER && mentions(self, id)
    }

    /// Whether `id` names no type of its own: it still holds UNKNOWN, or it is
    /// a bare `null`'s `Option<!>`. Such a type never decides a branch
    /// construct's result — a sibling with a definite type does.
    pub fn is_indefinite(&self, id: TypeId) -> bool {
        self.contains_unknown(id) || self.contains_never_arg(id)
    }

    /// Whether `id` (recursively) mentions anything a type check cannot decide
    /// yet: an inference variable, a type pack, or an unresolved / error type.
    /// A rigid type parameter is decided, and so is a projection over one.
    pub fn contains_undecided(&self, id: TypeId) -> bool {
        self.contains_hole(id, true)
    }

    /// [`Self::contains_undecided`] without the packs. A pack is decided
    /// wherever its own declaration is in scope, so a rule about shape must not
    /// read it as the hole an `InferVar` is.
    pub fn awaits_inference(&self, id: TypeId) -> bool {
        self.contains_hole(id, false)
    }

    /// The walk both of the above are, differing only in whether a declared
    /// pack counts as a hole.
    fn contains_hole(&self, id: TypeId, packs_count: bool) -> bool {
        match self.get(id) {
            ResolvedType::TypePack { .. } => packs_count,
            ResolvedType::InferVar(_) | ResolvedType::Unknown | ResolvedType::Error => true,
            // Its arguments are holes of their own: `S::Buf<?0>` awaits `?0`.
            ResolvedType::AssocTypeProjection {
                param_id,
                args,
                trait_args,
                ..
            } => {
                !self.projects_from_param(*param_id)
                    || projection_arguments(args, trait_args)
                        .any(|arg| self.contains_hole(arg, packs_count))
            }
            _ => self.any_constituent(id, &mut |t| self.contains_hole(t, packs_count)),
        }
    }

    /// The name of every type pack `id` mentions, so a caller can ask whose
    /// declaration they belong to.
    pub fn pack_names(&self, id: TypeId) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_pack_names(id, &mut out);
        out
    }

    fn collect_pack_names(&self, id: TypeId, out: &mut Vec<String>) {
        match self.get(id) {
            ResolvedType::TypePack {
                name, mapped_elem, ..
            } => {
                out.push(name.clone());
                if let Some(elem) = mapped_elem {
                    self.collect_pack_names(*elem, out);
                }
            }
            _ => {
                self.any_constituent(id, &mut |t| {
                    self.collect_pack_names(t, out);
                    false
                });
            }
        }
    }

    /// Whether a projection over `base` bottoms out at a rigid type parameter,
    /// a family's own included, chaining through nested projections
    /// (`I::Iter::Item`).
    fn projects_from_param(&self, base: TypeId) -> bool {
        match self.get(base) {
            ResolvedType::TypeParam { .. } | ResolvedType::AssocParam { .. } => true,
            ResolvedType::AssocTypeProjection { param_id, .. } => {
                self.projects_from_param(*param_id)
            }
            _ => false,
        }
    }

    /// Whether `id` (recursively) mentions a *rigid* type parameter — a slot
    /// of some declaration's own frame, as opposed to an inference variable a
    /// solver still owns.
    pub fn contains_rigid_param(&self, id: TypeId) -> bool {
        self.mentions_slot(id, Through::Projection)
    }

    /// Whether `id` mentions a slot a value assigned to it could still fill.
    /// [`Self::contains_rigid_param`] stopping at a projection's base: `I::Item`
    /// mentions a slot without being one, and inference cannot invert it. Its
    /// arguments it can, since two projections of one family meet argument by
    /// argument.
    pub fn contains_fillable_slot(&self, id: TypeId) -> bool {
        self.mentions_slot(id, Through::ProjectionStops)
    }

    fn mentions_slot(&self, id: TypeId, through: Through) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. } => true,
            ResolvedType::AssocTypeProjection {
                param_id,
                args,
                trait_args,
                ..
            } => {
                let base = match through {
                    Through::Projection => self.mentions_slot(*param_id, through),
                    Through::ProjectionStops => false,
                };
                base || projection_arguments(args, trait_args)
                    .any(|arg| self.mentions_slot(arg, through))
            }
            _ => self.any_constituent(id, &mut |t| self.mentions_slot(t, through)),
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

    /// Whether `id` is a `TypePack` or a tuple whose elements transitively
    /// contain one.
    pub fn contains_type_pack(&self, id: TypeId) -> bool {
        self.any_type_pack(id, &|_| true)
    }

    /// Whether `id` mentions a `TypePack` anywhere, under any constructor.
    pub fn mentions_type_pack(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::TypePack { .. } => true,
            _ => self.any_constituent(id, &mut |t| self.mentions_type_pack(t)),
        }
    }

    /// Whether `id` is, or is a tuple spreading, a pack `pred` holds of.
    pub fn any_type_pack(&self, id: TypeId, pred: &impl Fn(TypeId) -> bool) -> bool {
        match self.get(id) {
            ResolvedType::TypePack { .. } => pred(id),
            ResolvedType::GenericInstance { def, type_args } if self.is_tuple_def(*def) => {
                type_args.iter().any(|e| self.any_type_pack(*e, pred))
            }
            _ => false,
        }
    }

    /// Check if a type is or contains type parameters or unresolved types (Unknown/Error)
    pub fn contains_type_param(&self, id: TypeId) -> bool {
        self.contains_open(id, true)
    }

    /// Whether an impl's associated-type binding `id` is concrete for every
    /// argument its own parameters take: open in nothing else.
    pub fn binding_is_concrete(&self, id: TypeId) -> bool {
        !self.contains_open(id, false)
    }

    /// [`Self::contains_type_param`], counting a generic associated type's own
    /// parameters as open only where `family_is_open`.
    fn contains_open(&self, id: TypeId, family_is_open: bool) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { .. }
            | ResolvedType::TypePack { .. }
            | ResolvedType::InferVar(_)
            | ResolvedType::Unknown
            | ResolvedType::Error => true,
            ResolvedType::AssocParam { .. } => family_is_open,
            // One off a family's parameter (`E::Inner`) is as settled as the
            // parameter: the projection's arguments fill it, as they fill `E`.
            ResolvedType::AssocTypeProjection {
                param_id,
                args,
                trait_args,
                ..
            } => {
                family_is_open
                    || !self.rooted_in_family(*param_id)
                    || projection_arguments(args, trait_args)
                        .any(|arg| self.contains_open(arg, false))
            }
            _ => self.any_constituent(id, &mut |t| self.contains_open(t, family_is_open)),
        }
    }

    /// Whether `id` is a family's parameter, or a projection chain off one.
    fn rooted_in_family(&self, id: TypeId) -> bool {
        match self.get(id) {
            ResolvedType::AssocParam { .. } => true,
            ResolvedType::AssocTypeProjection { param_id, .. } => self.rooted_in_family(*param_id),
            _ => false,
        }
    }

    /// Whether `id`, unfolded into a tree, nests more than `max_depth` types on
    /// one path or holds more than `max_size` in all. A shared constituent
    /// counts once per use, as naming the type spells it, and the walk stops
    /// at the first type past either bound.
    pub fn outgrows(&self, id: TypeId, max_depth: usize, max_size: usize) -> bool {
        let mut size_room = max_size;
        self.outgrows_from(id, max_depth, &mut size_room)
    }

    fn outgrows_from(&self, id: TypeId, depth_room: usize, size_room: &mut usize) -> bool {
        if depth_room == 0 || *size_room == 0 {
            return true;
        }
        *size_room -= 1;
        self.any_constituent(id, &mut |t| {
            self.outgrows_from(t, depth_room - 1, size_room)
        })
    }

    /// Whether `f` holds of any type `id` is built over, through the
    /// constructors a use site substitutes into (`Self::subst_rec`).
    fn any_constituent(&self, id: TypeId, f: &mut dyn FnMut(TypeId) -> bool) -> bool {
        match self.get(id) {
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner) => f(*inner),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => params.iter().any(|&p| f(p)) || f(*return_type),
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. }
            | ResolvedType::Newtype { type_args, .. } => type_args.iter().any(|&t| f(t)),
            ResolvedType::AssocTypeProjection {
                param_id,
                args,
                trait_args,
                assoc_type_bindings,
                ..
            } => {
                f(*param_id)
                    || projection_arguments(args, trait_args).any(&mut *f)
                    || assoc_type_bindings.iter().any(|(_, t)| f(*t))
            }
            ResolvedType::Struct {
                def: StructDef::Anon(id),
                ..
            } => self.any_anon_shape_type(*id, f),
            _ => false,
        }
    }

    /// Whether a receiver of type `id` names no dispatch head until
    /// substitution, which is what
    /// [`crate::name::LocalMethodName::is_type_param_receiver`] marks.
    pub fn receiver_head_awaits_substitution(&self, id: TypeId) -> bool {
        matches!(
            self.get(id),
            ResolvedType::TypeParam { .. }
                | ResolvedType::TypePack { .. }
                | ResolvedType::AssocTypeProjection { .. }
        )
    }

    /// Whether an impl answering a projection off `base` is chosen by `base`'s
    /// head alone: `SliceRefIter<T>::Item` is `&T` whatever `T` is.
    fn projection_base_is_rigid(&self, base: TypeId) -> bool {
        let head = self.peel_refs(base);
        !self.receiver_head_awaits_substitution(head)
            && !matches!(
                self.get(head),
                ResolvedType::InferVar(_) | ResolvedType::Unknown | ResolvedType::Error
            )
    }

    /// Whether `id`'s representation is known only once substitution settles
    /// it: a type parameter or a projection, read through newtypes.
    pub fn representation_awaits_substitution(&self, id: TypeId) -> bool {
        matches!(
            self.get(self.representation_head(id)),
            ResolvedType::TypeParam { .. } | ResolvedType::AssocTypeProjection { .. }
        )
    }

    /// Whether `id` (recursively) mentions an associated-type projection
    /// (`I::Item`), i.e. still needs a bound's impl to become concrete.
    pub fn contains_assoc_type_projection(&self, id: TypeId) -> bool {
        !self.assoc_type_projections(id).is_empty()
    }

    /// Every associated-type projection `id` mentions, outermost first.
    pub fn assoc_type_projections(&self, id: TypeId) -> Vec<TypeId> {
        let mut out = Vec::new();
        self.collect_assoc_type_projections(id, &mut out);
        out
    }

    fn collect_assoc_type_projections(&self, id: TypeId, out: &mut Vec<TypeId>) {
        if let ResolvedType::AssocTypeProjection {
            args, trait_args, ..
        } = self.get(id)
        {
            out.push(id);
            for arg in projection_arguments(args, trait_args) {
                self.collect_assoc_type_projections(arg, out);
            }
            return;
        }
        self.any_constituent(id, &mut |t| {
            self.collect_assoc_type_projections(t, out);
            false
        });
    }

    /// The frame slot `id` names where it is a type parameter or a pack.
    pub fn param_slot(&self, id: TypeId) -> Option<u32> {
        match self.get(id) {
            ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                Some(*index)
            }
            _ => None,
        }
    }

    /// Whether `id` (recursively) mentions the frame slot `index`, projections
    /// included; slot 0 of a trait's frame is `Self`. Tells whether a method
    /// type parameter is inferable from an argument or only from the return.
    pub fn contains_type_param_index(&self, id: TypeId, index: u32) -> bool {
        match self.get(id) {
            ResolvedType::TypeParam { index: i, .. } | ResolvedType::TypePack { index: i, .. } => {
                *i == index
            }
            _ => self.any_constituent(id, &mut |t| self.contains_type_param_index(t, index)),
        }
    }

    /// Whether `id` is one of `slots` or is built over one, matched by id as a
    /// substitution binds them.
    pub fn mentions_any(&self, id: TypeId, slots: &[TypeId]) -> bool {
        slots.contains(&id) || self.any_constituent(id, &mut |t| self.mentions_any(t, slots))
    }

    /// The frame slots `id` (recursively) mentions, projections included.
    pub fn type_param_indices(&self, id: TypeId) -> IndexSet<u32> {
        let mut indices = IndexSet::default();
        self.collect_type_param_indices(id, &mut indices);
        indices
    }

    fn collect_type_param_indices(&self, id: TypeId, indices: &mut IndexSet<u32>) {
        match self.get(id) {
            ResolvedType::TypeParam { index, .. } | ResolvedType::TypePack { index, .. } => {
                indices.insert(*index);
            }
            _ => {
                self.any_constituent(id, &mut |t| {
                    self.collect_type_param_indices(t, indices);
                    false
                });
            }
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
            _ => !self.any_constituent(id, &mut |t| !self.type_params_all_in(t, allowed)),
        }
    }

    /// Whether `id` is an inference variable or is built over one — through the
    /// constructors a use site instantiates and substitutes through.
    ///
    /// A projection's base is: `?I::Item` is answered once `?I` is. A pack's
    /// mapped element and the bindings a projection carries to be answered are
    /// not what a use site is still waiting on. Reading them as such left
    /// `Ok(v)` in `f32::from_str_lenient` with no resolved type.
    pub fn contains_infer_var(&self, id: TypeId) -> bool {
        self.any_infer_var(id, &mut |_| true)
    }

    /// Whether `id` is an inference variable itself, not a type naming one.
    pub fn is_infer_var(&self, id: TypeId) -> bool {
        matches!(self.get(id), ResolvedType::InferVar(_))
    }

    /// The inference variables [`Self::contains_infer_var`] finds in `id`,
    /// each once, in the order it meets them.
    pub fn infer_vars_in(&self, id: TypeId) -> Vec<TypeId> {
        let mut out = Vec::new();
        self.any_infer_var(id, &mut |var| {
            if !out.contains(&var) {
                out.push(var);
            }
            false
        });
        out
    }

    /// Whether `pred` holds of an inference variable
    /// [`Self::contains_infer_var`] reaches in `id`, visiting them in order.
    fn any_infer_var(&self, id: TypeId, pred: &mut impl FnMut(TypeId) -> bool) -> bool {
        match self.get(id) {
            ResolvedType::InferVar(_) => pred(id),
            ResolvedType::AssocTypeProjection {
                param_id,
                args,
                trait_args,
                ..
            } => {
                self.any_infer_var(*param_id, pred)
                    || projection_arguments(args, trait_args)
                        .any(|arg| self.any_infer_var(arg, pred))
            }
            _ => self.any_constituent(id, &mut |t| self.any_infer_var(t, pred)),
        }
    }

    /// Get a human-readable name for a type
    pub fn type_name(&self, id: TypeId) -> String {
        self.render_type_name(id, false)
    }

    /// [`Self::type_name`] as the type read before `boxing::prepare_types`
    /// redefined a reference into `Box<T>`: what the source wrote, `&i32`.
    #[must_use]
    pub fn type_name_unboxed(&self, id: TypeId) -> String {
        match self.spelled_ref(id) {
            Some((payload, kind)) => ref_type_name(kind, &self.type_name_unboxed(payload)),
            None => self.type_name(id),
        }
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
    fn head_name(&self, def: DefId, qualified: bool) -> String {
        let name = self.def_name(def);
        if qualified {
            render(&self.def_module(def).to_string(), name)
        } else {
            name.to_string()
        }
    }

    fn render_type_name(&self, id: TypeId, qualified: bool) -> String {
        let type_name = |t: TypeId| self.render_type_name(t, qualified);
        match self.get(id) {
            ResolvedType::Primitive(p) => p.as_str().to_string(),
            ResolvedType::Unit => UNIT_TYPE_NAME.to_string(),
            ResolvedType::Never => NEVER_TYPE_NAME.to_string(),
            ResolvedType::Unknown => "<unknown>".to_string(),
            ResolvedType::Error => "<error>".to_string(),
            ResolvedType::BuiltinArray(elem) => {
                display_generic_name(ARRAY_TYPE_NAME, &[type_name(*elem)])
            }
            ResolvedType::Struct {
                def: StructDef::Anon(id),
                ..
            } => self.render_anon_struct(*id, &type_name),
            ResolvedType::Struct { def, type_args } => {
                // The declared head, not the rendered one: a message shows
                // `Box<i32>`, never the `Box@<local>` a local declaration is
                // stored under.
                let head = match (qualified, def.decl()) {
                    (true, Some(decl)) => self.head_name(decl, true),
                    _ => self.struct_head_decl_name(*def),
                };
                let args: Vec<String> = type_args.iter().map(|t| type_name(*t)).collect();
                display_generic_name(&head, &args)
            }
            ResolvedType::Enum { def } | ResolvedType::Resource { def } => {
                self.head_name(*def, qualified)
            }
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                effects,
            } => {
                let param_names: Vec<String> = params.iter().map(|p| type_name(*p)).collect();
                // A variable is a row still to be worked out: nothing the
                // source wrote, so nothing to show.
                let effect_names: Vec<String> = effects
                    .iter()
                    .filter(|e| !e.is_var())
                    .map(|e| e.display_name(qualified))
                    .collect();
                name::display_fn_type(
                    *is_mut,
                    &param_names,
                    &type_name(*return_type),
                    matches!(self.get(*return_type), ResolvedType::Function { .. }),
                    &effect_names,
                )
            }
            ResolvedType::Ref(inner) => ref_type_name(RefKind::Shared, &type_name(*inner)),
            ResolvedType::MutRef(inner) => ref_type_name(RefKind::Mut, &type_name(*inner)),
            ResolvedType::Variant { def } => self.head_name(*def, qualified),
            ResolvedType::GenericResource { def, type_args } => {
                let arg_names: Vec<String> = type_args.iter().map(|t| type_name(*t)).collect();
                display_generic_name(&self.head_name(*def, qualified), &arg_names)
            }
            ResolvedType::TypeParam { name, .. } | ResolvedType::AssocParam { name, .. } => {
                name.clone()
            }
            ResolvedType::InferVar(var) => self.infer_var_name(*var),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                args,
                owning_trait,
                trait_args,
                ..
            } => {
                let names =
                    |ids: &[TypeId]| -> Vec<String> { ids.iter().map(|a| type_name(*a)).collect() };
                // The owning trait and its arguments are part of a
                // projection's identity, so two that differ only there render
                // the same without them.
                let owner = qualified.then(|| {
                    display_generic_name(
                        &self.head_name(*owning_trait, true),
                        &trait_args.as_deref().map(names).unwrap_or_default(),
                    )
                });
                display_assoc_type_projection(
                    &type_name(*param_id),
                    assoc_name,
                    &names(args),
                    owner.as_deref(),
                )
            }
            ResolvedType::GenericInstance { def, type_args } => {
                let arg_names: Vec<String> = type_args.iter().map(|t| type_name(*t)).collect();
                // A tuple is module-independent, so it has nothing to qualify.
                if self.is_tuple_def(*def) {
                    display_tuple_type(&arg_names)
                } else {
                    display_generic_name(&self.head_name(*def, qualified), &arg_names)
                }
            }
            ResolvedType::Newtype { def, type_args, .. } => {
                let args: Vec<String> = type_args.iter().map(|t| type_name(*t)).collect();
                display_generic_name(&self.head_name(*def, qualified), &args)
            }
            ResolvedType::Flags { def } => self.head_name(*def, qualified),
            ResolvedType::TypePack { name, .. } => type_pack_name(name),
        }
    }

    /// Mangle a type name, resolving all newtypes/flags to their base types recursively.
    /// E.g., `List<FieldName>` → `List<String>` when `FieldName = String`.
    pub fn mangle_type_name_resolving_newtypes(&self, id: TypeId) -> String {
        let base = self.representation_head(id);
        match self.get(base) {
            ResolvedType::GenericInstance { def, type_args } => {
                let module_source = self.def_module(*def);
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_name_resolving_newtypes(*t))
                    .collect();
                // A tuple is module-independent; every other instance is named
                // by the module declaring its base.
                if self.is_tuple_def(*def) {
                    mangle_tuple_type(&args)
                } else {
                    let unqualified = mangle_generic_name(&self.decl_render_name(*def), &args);
                    mangle_declared(module_source, &unqualified)
                }
            }
            ResolvedType::BuiltinArray(elem) => {
                let elem_name = self.mangle_type_name_resolving_newtypes(*elem);
                mangle_builtin_array_type(&elem_name)
            }
            _ => self.mangle_type_name(base),
        }
    }

    /// Recursively resolve newtypes inside compound types (tuples, generics, arrays).
    /// Returns the same `TypeId` if no newtypes are found, or a new `TypeId` with all
    /// newtypes replaced by their base types.
    pub fn resolve_newtypes_deep(&mut self, id: TypeId) -> TypeId {
        let base = self.representation_head(id);
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
        let base = self.representation_head(id);
        match self.get(base) {
            ResolvedType::GenericInstance { def, type_args } => {
                let resolved: Vec<TypeId> = type_args
                    .iter()
                    .map(|t| self.resolve_newtypes_deep_readonly(*t))
                    .collect();
                if resolved == *type_args {
                    base
                } else if let Some(existing) = self.find_generic_instance(*def, &resolved) {
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
    /// A generic renders as `Name<T1,T2,…>`, a function as [`crate::name::mangle_fn_type`]
    /// spells it, and references are stripped.
    #[must_use]
    pub fn mangle_type_name(&self, id: TypeId) -> String {
        let info = self.get_type_name_info(id);
        format_type_name(info)
    }

    /// Mangle `id` as a type argument inside a generic instance's identity — the
    /// `T` in `Result<unit, T>`. Unlike [`Self::mangle_type_name`], every named
    /// head is qualified by its declaring module, or two same-named types collapse.
    ///
    /// Delegates to [`crate::name::FqTypeName::to_mangled`], the one renderer, so
    /// a definition's name and a lookup's cannot disagree.
    #[must_use]
    pub fn mangle_type_arg_for_generic(&self, id: TypeId) -> String {
        self.fq_type_name(id).to_mangled()
    }

    /// [`Self::mangle_type_arg_for_generic`] as a type's own argument, where
    /// a function value's row selects no representation: instances whose
    /// arguments differ only in rows are one type.
    pub fn mangle_type_arg_rowless(&self, id: TypeId) -> String {
        self.fq_type_name_rowless(id).to_mangled()
    }

    /// [`Self::mangle_type_arg_rowless`] of the newtype-resolved form, which
    /// the WIR array and `List` wrapper registrations key on.
    pub fn mangle_type_arg_rowless_resolving_newtypes(&self, id: TypeId) -> String {
        let resolved = self.representation_head(id);
        self.mangle_type_arg_rowless(resolved)
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
                self.mangle_type_arg_erased(self.representation_head(id))
            }
            ResolvedType::GenericInstance { def, type_args } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_arg_erased(*t))
                    .collect();
                if self.is_tuple_def(*def) {
                    return mangle_tuple_type(&args);
                }
                let unqualified = mangle_generic_name(&self.decl_render_name(*def), &args);
                mangle_declared(self.def_module(*def), &unqualified)
            }
            ResolvedType::Ref(inner) => {
                ref_type_name(RefKind::Shared, &self.mangle_type_arg_erased(*inner))
            }
            ResolvedType::MutRef(inner) => {
                ref_type_name(RefKind::Mut, &self.mangle_type_arg_erased(*inner))
            }
            ResolvedType::BuiltinArray(elem) => {
                mangle_builtin_array_type(&self.mangle_type_arg_erased(*elem))
            }
            _ => self.mangle_type_arg_rowless(id),
        }
    }

    /// [`Self::mangle_type_arg_rowless`] as the type read *before*
    /// `boxing::prepare_types` redefined every reference `TypeId` into `Box<T>`,
    /// so a post-boxing call site names the bridge pre-boxing synthesis minted.
    /// A bridge is keyed by a type its struct takes as an argument, which no
    /// row tells apart.
    pub fn mangle_type_arg_unboxed(&self, id: TypeId) -> String {
        self.fq_type_name_spelled_in(id, true, RowSpelling::Nowhere)
            .to_mangled()
    }

    /// The base type name without type arguments: the head's own name, so
    /// `Option<String>` and a monomorphized `Option<String>` both answer
    /// `"Option"`. Everything else falls back to `mangle_type_name`.
    #[must_use]
    pub fn base_type_name(&self, id: TypeId) -> String {
        match self.get(id) {
            // A generic resource heads its methods bare, like a generic struct:
            // spelling the argument into the head applies it twice
            // (`StreamWritable<u8><u8>::write_all`) and resolves to nothing.
            ResolvedType::GenericInstance { def, .. }
            | ResolvedType::GenericResource { def, .. } => self.def_name(*def).to_string(),
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
                    .map(|t| self.mangle_type_arg_rowless(*t))
                    .collect();
                Some(if self.is_tuple_def(*def) {
                    mangle_tuple_type(&args)
                } else {
                    mangle_generic_name(&self.decl_render_name(*def), &args)
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
    /// [`Receiver::decl_key`] for the name an `impl` header writes,
    /// [`Receiver::head_key`] for the mangled identity.
    #[must_use]
    pub fn impl_receiver_key(&self, id: TypeId) -> Receiver {
        let declared = |def: DefId| Receiver::Type(FqTypeName::declared(&self.defs, def));
        let builtin = |name: &str| Receiver::Type(FqTypeName::builtin(name));
        // Unerased: which impls a type has is a fact about its identity, and
        // erasure rewrites a newtype / flags id to the representation it is
        // stored as, whose impls are a different set.
        match self.get_unerased(id) {
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) => {
                Receiver::Ref(RefKind::from_resolved(self.get(id)).expect(
                    "a reference unerased is a reference erased: `get_unerased` resolves the \
                     box redefinition, the one rewrite that makes the two views differ",
                ))
            }
            ResolvedType::Struct { def, .. } => Receiver::Type(self.fq_struct_head(*def)),
            // The head is the declaration, arguments never spelled into it — an
            // `impl` header writes `MyArray`, not `MyArray<i32>`, and
            // `impl Stream<T>`, not `Stream<u8>`. A call keyed the other way
            // mangles under a second head and never meets the impl's methods.
            ResolvedType::Enum { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Flags { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::GenericInstance { def, .. }
            | ResolvedType::GenericResource { def, .. } => declared(*def),
            ResolvedType::TypeParam { name, .. } => Receiver::Type(FqTypeName::binder(name)),
            ResolvedType::BuiltinArray(_) => builtin(ARRAY_TYPE_NAME),
            ResolvedType::Unit => builtin(UNIT_TYPE_NAME),
            ResolvedType::Primitive(prim) => builtin(prim.as_str()),
            ResolvedType::Function { .. } => Receiver::Type(self.fn_receiver_name(self.get(id))),
            _ => builtin(&self.base_type_name(id)),
        }
    }

    /// A `fn(..)` type's name, each component named by `name_of`.
    fn fq_fn_name(
        &self,
        resolved: &ResolvedType,
        name_of: impl Fn(TypeId) -> FqTypeName,
    ) -> FqTypeName {
        let ResolvedType::Function {
            is_mut,
            params,
            return_type,
            effects,
        } = resolved
        else {
            panic!("fq_fn_name expects a function type");
        };
        FqTypeName::function(
            *is_mut,
            params.iter().map(|&p| name_of(p)).collect(),
            name_of(*return_type),
            effects.clone(),
        )
    }

    /// The receiver a `fn(..)` value dispatches through: its own name.
    #[must_use]
    pub fn fn_receiver_name(&self, resolved: &ResolvedType) -> FqTypeName {
        self.fq_fn_name(resolved, |t| self.fq_type_name(t))
    }

    /// The declaration a type's head names, with any arguments dropped. Read
    /// off the identity ([`Self::get_unerased`]): this is the head an `impl`
    /// header writes, and erasure would answer `u32` for a `flags` type and the
    /// base's name for a newtype — templates no impl declares.
    #[must_use]
    pub fn fq_base_type_name(&self, id: TypeId) -> FqTypeName {
        match self.get_unerased(id) {
            ResolvedType::Struct { def, .. } => self.fq_struct_head(*def),
            ResolvedType::Enum { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::Flags { def }
            | ResolvedType::Resource { def }
            | ResolvedType::GenericInstance { def, .. }
            | ResolvedType::GenericResource { def, .. } => FqTypeName::declared(&self.defs, *def),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.fq_base_type_name(*inner)
            }
            ResolvedType::BuiltinArray(_) => FqTypeName::builtin(ARRAY_TYPE_NAME),
            ResolvedType::Unit => FqTypeName::builtin(UNIT_TYPE_NAME),
            ResolvedType::Function { .. } => self.fn_receiver_name(self.get(id)),
            // Tuples and primitives are builtin shapes: no module declares
            // them and every mangler spells them bare.
            _ => FqTypeName::builtin(&self.base_type_name(id)),
        }
    }

    /// The arguments an instantiation carries; `None` if `id` is not one.
    #[must_use]
    pub fn generic_type_args(&self, id: TypeId) -> Option<Vec<TypeId>> {
        match self.get(id) {
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => Some(type_args.clone()),
            // No fallback search: an empty list is a declaration, not an instantiation.
            ResolvedType::Struct { type_args, .. } | ResolvedType::Newtype { type_args, .. }
                if !type_args.is_empty() =>
            {
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
    pub fn fq_type_name(&self, id: TypeId) -> FqTypeName {
        self.fq_type_name_spelled(id, false)
    }

    /// [`Self::fq_type_name`] as the type read *before* `boxing::prepare_types`
    /// redefined every reference `TypeId` into `Box<T>`. Every shape recurses,
    /// so a reference nested anywhere in the spelling comes back as one.
    pub fn fq_type_name_unboxed(&self, id: TypeId) -> FqTypeName {
        self.fq_type_name_spelled(id, true)
    }

    fn fq_type_name_spelled(&self, id: TypeId, unboxed: bool) -> FqTypeName {
        self.fq_type_name_spelled_in(id, unboxed, RowSpelling::OutsideArgs)
    }

    /// [`Self::fq_type_name`] with every function type's row spelled, inside
    /// type arguments too: what the effect check reads a receiver's argument
    /// rows off.
    pub fn fq_type_name_with_rows(&self, id: TypeId) -> FqTypeName {
        self.fq_type_name_spelled_in(id, false, RowSpelling::Everywhere)
    }

    /// [`Self::fq_type_name`] with no row spelled: a type's own argument,
    /// where a row selects no representation.
    pub fn fq_type_name_rowless(&self, id: TypeId) -> FqTypeName {
        self.fq_type_name_spelled_in(id, false, RowSpelling::Nowhere)
    }

    /// [`Self::fq_type_name_spelled`], with function types' rows spelled where
    /// `rows` says.
    fn fq_type_name_spelled_in(&self, id: TypeId, unboxed: bool, rows: RowSpelling) -> FqTypeName {
        let rowless = rows == RowSpelling::Nowhere;
        let arg_rows = match rows {
            RowSpelling::Everywhere => RowSpelling::Everywhere,
            RowSpelling::OutsideArgs | RowSpelling::Nowhere => RowSpelling::Nowhere,
        };
        // Only a reference is read off the slot's own type. Every other shape
        // keeps the erased view below, where ids that erase together must
        // answer one name.
        if unboxed && let Some((payload, kind)) = self.spelled_ref(id) {
            return self
                .fq_type_name_spelled_in(payload, unboxed, rows)
                .with_reference(kind);
        }
        let args_of = |type_args: &[TypeId]| -> Vec<FqTypeName> {
            type_args
                .iter()
                .map(|t| self.fq_type_name_spelled_in(*t, unboxed, arg_rows))
                .collect()
        };
        // The erased view, unlike [`Self::fq_base_type_name`]: this spelling
        // also names representation-level functions minted after erasure — a
        // canonical `$case_extract$` among them — where two ids that erase
        // together must answer one name.
        match self.get(id) {
            ResolvedType::Primitive(prim) => FqTypeName::builtin(prim.as_str()),
            ResolvedType::Unit => FqTypeName::builtin(UNIT_TYPE_NAME),
            ResolvedType::Never => FqTypeName::builtin(NEVER_TYPE_NAME),
            // Head and arguments come straight off the type — the same shape
            // every other instantiated type has. No recovery step, because
            // there is no fused spelling left to recover them from.
            ResolvedType::Struct { def, type_args } => {
                self.fq_struct_head(*def).with_args(args_of(type_args))
            }
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Flags { def } => FqTypeName::declared(&self.defs, *def),
            // `MyArray<i32>` and `MyArray<String>` are two instantiations; the
            // head alone names them the same.
            ResolvedType::Newtype { def, type_args, .. } => {
                FqTypeName::declared(&self.defs, *def).with_args(args_of(type_args))
            }
            ResolvedType::TypeParam { name, .. } => FqTypeName::binder(name),
            ResolvedType::AssocParam {
                owning_trait,
                assoc_name,
                name,
                ..
            } => FqTypeName::binder_of_family(&self.defs, *owning_trait, assoc_name, name),
            ResolvedType::GenericInstance { def, type_args } => {
                let args = args_of(type_args);
                if self.is_tuple_def(*def) {
                    FqTypeName::tuple(args)
                } else {
                    FqTypeName::declared(&self.defs, *def).with_args(args)
                }
            }
            ResolvedType::GenericResource { def, type_args } => {
                FqTypeName::declared(&self.defs, *def).with_args(args_of(type_args))
            }
            ResolvedType::BuiltinArray(elem) => FqTypeName::builtin(ARRAY_TYPE_NAME)
                .with_args(vec![self.fq_type_name_spelled_in(*elem, unboxed, arg_rows)]),
            ResolvedType::Ref(inner) => self
                .fq_type_name_spelled_in(*inner, unboxed, rows)
                .with_reference(RefKind::Shared),
            ResolvedType::MutRef(inner) => self
                .fq_type_name_spelled_in(*inner, unboxed, rows)
                .with_reference(RefKind::Mut),
            // Structural, so substituting the base reaches it: the base is what
            // a monomorphized call answers, and the projection follows.
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                args,
                owning_trait,
                trait_args,
                ..
            } => FqTypeName::projection(
                self.fq_type_name_spelled(*param_id, unboxed),
                assoc_name,
                &self.defs,
                *owning_trait,
                trait_args.as_ref().map(|given| args_of(given)),
            )
            .with_args(args_of(args)),
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                effects,
            } => FqTypeName::function(
                *is_mut,
                params
                    .iter()
                    .map(|&p| self.fq_type_name_spelled_in(p, unboxed, rows))
                    .collect(),
                self.fq_type_name_spelled_in(*return_type, unboxed, rows),
                if rowless { Vec::new() } else { effects.clone() },
            ),
            // Shapes that name no declaration — packs, `Unknown`. They carry no
            // module, so the rendered spelling is already their whole identity.
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
            ResolvedType::Struct { def, type_args } => TypeNameInfo::Named(mangle_declared(
                self.struct_head_module(*def),
                &self.struct_rendered_name(*def, type_args),
            )),
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::Flags { def } => {
                TypeNameInfo::Named(mangle_declared(self.def_module(*def), self.def_name(*def)))
            }
            // A type parameter is a template's own binder, not a declaration.
            ResolvedType::TypeParam { name, .. } => TypeNameInfo::Named(name.clone()),
            ResolvedType::AssocParam { .. } => {
                TypeNameInfo::Named(self.fq_type_name(id).to_mangled())
            }
            // A mangled name is an identity, so two unsolved slots sharing a
            // spelling must not collapse. The slot's name is for reading.
            ResolvedType::InferVar(var) => TypeNameInfo::Named(var.to_string()),
            ResolvedType::GenericInstance { def, type_args } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_arg_rowless(*t))
                    .collect();
                // A tuple is module-independent; its elements stay qualified.
                if self.is_tuple_def(*def) {
                    return TypeNameInfo::Tuple(args);
                }
                TypeNameInfo::Generic {
                    name: mangle_declared(self.def_module(*def), &self.decl_render_name(*def)),
                    args,
                }
            }
            // One spelling with the receiver's, so a stub's name and a call
            // site's are one.
            resolved @ ResolvedType::Function { .. } => {
                TypeNameInfo::Named(self.fn_receiver_name(resolved).to_mangled())
            }
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
                    .map(|t| self.mangle_type_arg_rowless(*t))
                    .collect();
                TypeNameInfo::Generic {
                    name: self.def_name(*def).to_string(),
                    args,
                }
            }
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                args,
                ..
            } => {
                let base =
                    assoc_type_projection_name(&self.mangle_type_name(*param_id), assoc_name);
                if args.is_empty() {
                    TypeNameInfo::Named(base)
                } else {
                    TypeNameInfo::Generic {
                        name: base,
                        args: args
                            .iter()
                            .map(|t| self.mangle_type_arg_for_generic(*t))
                            .collect(),
                    }
                }
            }
            ResolvedType::TypePack { name, .. } => TypeNameInfo::Named(type_pack_name(name)),
            ResolvedType::Never => TypeNameInfo::Named(NEVER_TYPE_NAME.to_string()),
            ResolvedType::Unknown | ResolvedType::Error => TypeNameInfo::Unknown,
        }
    }
}

/// What an impl block's target writes.
#[derive(Debug, Clone)]
struct ImplTarget {
    /// The target as a whole, its reference included.
    whole: TypeId,
    /// The arguments of the head it names past any reference, a binder as its
    /// own `TypeParam`.
    args: Vec<TypeId>,
    projections: Vec<ImplProjection>,
}

/// Slots of an impl block that a bound determines rather than its target:
/// `B` in `impl<N: Tr<Assoc = B>, B> W<N>` is whatever `Assoc` is for the type
/// filling `N`.
#[derive(Debug, Clone)]
pub struct ImplProjection {
    /// The slot whose bound carries the constraint.
    pub source: u32,
    pub trait_: DefId,
    pub assoc: String,
    /// The constraint as written, its slots as `TypeParam`s.
    pub pattern: TypeId,
}

/// What an impl target binds at a receiver's arguments.
#[derive(Debug, Default)]
pub struct TargetBinding {
    /// The type each of the target's parameter slots takes.
    pub slots: IndexMap<u32, TypeId>,
    /// The type each open receiver variable must take for the target to match.
    pub answers: IndexMap<TypeId, TypeId>,
}

impl TargetBinding {
    /// `ty` read through the answers, where it is an answered variable.
    fn answered(&self, mut ty: TypeId) -> TypeId {
        while let Some(&answer) = self.answers.get(&ty) {
            ty = answer;
        }
        ty
    }
}

impl TypeTable {
    /// Record what impl block `def`'s target writes: `whole`, and `args` for
    /// the head it names, with the slots its bounds determine.
    pub fn record_impl_target(
        &mut self,
        def: DefId,
        whole: TypeId,
        args: Vec<TypeId>,
        projections: Vec<ImplProjection>,
    ) {
        self.impl_targets.insert(
            def,
            ImplTarget {
                whole,
                args,
                projections,
            },
        );
    }

    /// Impl block `def`'s target head at `head_args`; `None` where the target
    /// is no generic head of that arity.
    pub fn impl_target_at(&mut self, def: DefId, head_args: &[TypeId]) -> Option<TypeId> {
        match self.get_unerased(self.impl_target(def).whole) {
            ResolvedType::GenericInstance {
                def: head,
                type_args,
            } if type_args.len() == head_args.len() => {
                let head = *head;
                Some(self.make_generic_instance(head, head_args.to_vec()))
            }
            _ => None,
        }
    }

    /// Impl block `def`'s target as a whole, its reference included.
    pub fn impl_target_whole(&self, def: DefId) -> TypeId {
        self.impl_target(def).whole
    }

    /// The arguments of the head impl block `def`'s target names past any
    /// reference, a binder as its own `TypeParam`; empty for a non-generic one.
    pub fn impl_target_args(&self, def: DefId) -> &[TypeId] {
        &self.impl_target(def).args
    }

    /// The slots each of impl block `def`'s target positions holds, at any
    /// depth, filled from the receiver's arguments: `T` in `Pair<List<T>, i32>`
    /// from `Pair<List<String>, i32>`, then the slots its bounds project from
    /// those. The target is recorded as its module's decl pass reaches the
    /// block, so an earlier module's bound check can ask.
    pub fn impl_slots(&mut self, def: DefId, receiver_args: &[TypeId]) -> IndexMap<u32, TypeId> {
        let mut slots = IndexMap::default();
        for (&declared, &concrete) in self.impl_target_args(def).iter().zip(receiver_args) {
            if let Some(bound) = self.bind_type_params(&[declared], &[concrete]) {
                for (slot, ty) in bound {
                    slots.entry(slot).or_insert(ty);
                }
            }
        }
        self.project_impl_slots(def, &mut slots);
        slots
    }

    /// The declaration impl block `def` targets, past any reference.
    pub fn impl_target_decl(&self, def: DefId) -> Option<DefId> {
        self.nominal_def(self.peel_refs(self.impl_target(def).whole))
    }

    /// `receiver`'s arguments as declaration `decl` names them, read at the
    /// link of its newtype chain that `decl` declares: a newtype's own impls
    /// take the newtype's arguments, its base's impls the base's, and a
    /// re-shaping newtype (`type Pair<T> = List<[T, i32]>`) differs on the two.
    /// `None` where no link is `decl`.
    pub fn args_at_decl(&self, receiver: TypeId, decl: DefId) -> Option<Vec<TypeId>> {
        let mut link = self.peel_refs(receiver);
        loop {
            if self.nominal_def(link) == Some(decl) {
                return self.declared_type_args(link);
            }
            let ResolvedType::Newtype { base_type, .. } = self.get_unerased(link) else {
                return None;
            };
            link = *base_type;
        }
    }

    /// Fill the slots impl block `def`'s bounds project from those `slots`
    /// already holds: `X` in `impl<T: Tr<Assoc = List<X>>, X>` once `T` is
    /// settled, read through `Tr` itself, so another trait's `Assoc` on the
    /// same type cannot answer.
    pub fn project_impl_slots(&mut self, def: DefId, slots: &mut IndexMap<u32, TypeId>) {
        for projection in self.impl_target(def).projections.clone() {
            let Some(&source) = slots.get(&projection.source) else {
                continue;
            };
            let Some(concrete) = self.resolve_trait_assoc_type_of_instance(
                source,
                &projection.trait_,
                None,
                &projection.assoc,
            ) else {
                continue;
            };
            self.register_assoc_type_resolution(
                source,
                TraitRef::bare(projection.trait_),
                projection.assoc,
                concrete,
            );
            if let Some(bound) = self.bind_projection_pattern(projection.pattern, concrete) {
                for (slot, ty) in bound {
                    slots.entry(slot).or_insert(ty);
                }
            }
        }
    }

    /// [`Self::bind_type_params`] for one pattern, where a tuple pattern may
    /// spread a pack (`[A, ..F]`): the pack takes the elements its neighbours
    /// leave, as one tuple.
    fn bind_projection_pattern(
        &mut self,
        pattern: TypeId,
        concrete: TypeId,
    ) -> Option<IndexMap<u32, TypeId>> {
        let (Some(written), Some(elements)) = (self.as_tuple(pattern), self.as_tuple(concrete))
        else {
            return self.bind_type_params(&[pattern], &[concrete]);
        };
        let Some(at) = written.iter().position(|&w| self.is_type_pack(w)) else {
            return self.bind_type_params(&[pattern], &[concrete]);
        };
        let after = written.len() - at - 1;
        let pack_end = elements.len().checked_sub(after)?;
        if pack_end < at {
            return None;
        }
        let pack = self.make_tuple(elements[at..pack_end].to_vec());
        let mut rest_written = written.clone();
        rest_written.remove(at);
        let rest_concrete: Vec<TypeId> = elements[..at]
            .iter()
            .chain(&elements[pack_end..])
            .copied()
            .collect();
        let mut bound = self.bind_type_params(&rest_written, &rest_concrete)?;
        bound.insert(self.param_slot(written[at])?, pack);
        Some(bound)
    }

    fn impl_target(&self, def: DefId) -> &ImplTarget {
        self.impl_targets.get(&def).unwrap_or_else(|| {
            panic!("impl block {def:?} is asked about before its target is recorded")
        })
    }

    /// Whether impl block `def` reaches the receiver type `instance`. Reference
    /// targets share one `&` head, so their referents' heads must agree too.
    pub fn impl_reaches_instance(&self, def: DefId, instance: TypeId) -> bool {
        let target = self.impl_target(def);
        if let ResolvedType::Ref(written) | ResolvedType::MutRef(written) =
            self.get_unerased(target.whole)
            && let ResolvedType::Ref(asked) | ResolvedType::MutRef(asked) = self.get(instance)
            && self.param_slot(*written).is_none()
        {
            let head = self.impl_receiver_key(*written);
            let mut link = self.peel_refs(*asked);
            while self.impl_receiver_key(link) != head {
                let ResolvedType::Newtype { base_type, .. } = self.get_unerased(link) else {
                    return false;
                };
                link = *base_type;
            }
        }
        let written = &target.args;
        let instance = self.peel_refs(instance);
        let args = match self.get_unerased(instance) {
            ResolvedType::Struct { type_args, .. } => type_args.clone(),
            _ => self.declared_type_args(instance).unwrap_or_default(),
        };
        self.impl_target_binding(written, &args).is_some()
    }

    /// Whether impl block `def` reaches every instance of its head: its target
    /// names a head, not a shape, over distinct binders.
    pub fn impl_covers_every_instance(&self, def: DefId) -> bool {
        let target = self.impl_target(def);
        let names_a_head = match self.get_unerased(target.whole) {
            ResolvedType::Ref(_)
            | ResolvedType::MutRef(_)
            | ResolvedType::Unit
            | ResolvedType::Function { .. } => false,
            ResolvedType::GenericInstance { .. } => !self.is_tuple(target.whole),
            ResolvedType::Primitive(_)
            | ResolvedType::Never
            | ResolvedType::Struct { .. }
            | ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::GenericResource { .. }
            | ResolvedType::TypeParam { .. }
            | ResolvedType::AssocParam { .. }
            | ResolvedType::InferVar(_)
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => true,
        };
        let mut seen = IndexSet::default();
        names_a_head
            && target
                .args
                .iter()
                .all(|&arg| self.param_slot(arg).is_some_and(|slot| seen.insert(slot)))
    }

    /// Every recorded impl block reaching only some instances of its head.
    pub fn partial_impls(&self) -> IndexSet<DefId> {
        self.impl_targets
            .keys()
            .copied()
            .filter(|&def| !self.impl_covers_every_instance(def))
            .collect()
    }

    /// What a target writing `written` binds at a receiver with `receiver_args`,
    /// or `None` where it misses it. Unknown arguments and a pack's tail pin none.
    pub fn impl_target_binding(
        &self,
        written: &[TypeId],
        receiver_args: &[TypeId],
    ) -> Option<IndexMap<u32, TypeId>> {
        self.open_impl_target_binding(written, receiver_args, &|_| false)
            .map(|binding| binding.slots)
    }

    /// [`Self::impl_target_binding`] where the receiver's arguments hold
    /// variables still to be answered, which `open` tells. A variable takes the
    /// one closed type the target puts at its place; it cannot take two, nor a
    /// type naming another variable, nor a type naming the impl's parameters,
    /// where its answer would have to be found first.
    pub fn open_impl_target_binding(
        &self,
        written: &[TypeId],
        receiver_args: &[TypeId],
        open: &dyn Fn(TypeId) -> bool,
    ) -> Option<TargetBinding> {
        let mut binding = TargetBinding::default();
        if written.is_empty() || receiver_args.is_empty() {
            return Some(binding);
        }
        let fixed = written
            .iter()
            .position(|&w| self.is_type_pack(w))
            .unwrap_or(written.len());
        if receiver_args.len() < written.len() && fixed == written.len() {
            return None;
        }
        self.bind_all(
            &written[..fixed],
            receiver_args.get(..fixed)?,
            open,
            &mut binding,
        )
        .then_some(binding)
    }

    /// The type-parameter slots `concrete` fills in `written`, at any depth;
    /// `None` where they differ outside a slot or a slot would take two types.
    pub fn bind_type_params(
        &self,
        written: &[TypeId],
        concrete: &[TypeId],
    ) -> Option<IndexMap<u32, TypeId>> {
        let mut binding = TargetBinding::default();
        self.bind_all(written, concrete, &|_| false, &mut binding)
            .then_some(binding.slots)
    }

    fn bind_all(
        &self,
        written: &[TypeId],
        concrete: &[TypeId],
        open: &dyn Fn(TypeId) -> bool,
        binding: &mut TargetBinding,
    ) -> bool {
        written.len() == concrete.len()
            && written
                .iter()
                .zip(concrete)
                .all(|(&w, &c)| self.bind_one(w, c, open, binding))
    }

    fn bind_one(
        &self,
        written: TypeId,
        concrete: TypeId,
        open: &dyn Fn(TypeId) -> bool,
        binding: &mut TargetBinding,
    ) -> bool {
        if let Some(slot) = self.param_slot(written) {
            if let Some(&prior) = binding.slots.get(&slot) {
                return self.agree(prior, concrete, open, binding);
            }
            binding.slots.insert(slot, concrete);
            return true;
        }
        if open(concrete) {
            return !self.contains_type_param(written)
                && self.agree(concrete, written, open, binding);
        }
        // An `_` or an unresolvable name: nothing written to match against.
        // A projection, written inside an `<F: fn(...)>` signature, is its
        // base's binding's, which the solver checks once the base is bound.
        if matches!(
            self.get(written),
            ResolvedType::Unknown | ResolvedType::Error | ResolvedType::AssocTypeProjection { .. }
        ) {
            return true;
        }
        if let Some(agrees) =
            self.zip_shapes(written, concrete, |w, c| self.bind_one(w, c, open, binding))
        {
            return agrees;
        }
        // A head written bare (`impl Slot<Box>`) constrains the head alone.
        if self.peel_refs(written) == written
            && self.decl_of_type(written).is_some()
            && self.generic_type_args(written).is_none()
            && self.generic_type_args(concrete).is_some()
        {
            return self.fq_base_type_name(written).head()
                == self.fq_base_type_name(concrete).head();
        }
        self.type_key(written) == self.type_key(concrete)
    }

    /// Whether two receiver-side types can be one, answering an open variable
    /// on either side with the other where that settles it.
    fn agree(
        &self,
        a: TypeId,
        b: TypeId,
        open: &dyn Fn(TypeId) -> bool,
        binding: &mut TargetBinding,
    ) -> bool {
        let (a, b) = (binding.answered(a), binding.answered(b));
        if self.type_key(a) == self.type_key(b) {
            return true;
        }
        let closed = |ty: TypeId| !self.any_infer_var(ty, &mut |var| open(var));
        for (var, answer) in [(a, b), (b, a)] {
            if open(var) {
                if !closed(answer) {
                    return false;
                }
                binding.answers.insert(var, answer);
                return true;
            }
        }
        self.zip_shapes(a, b, |x, y| self.agree(x, y, open, binding))
            .unwrap_or(false)
    }

    /// Whether `each` holds of every pair of parts `a` and `b` line up, or
    /// `None` where they share no outer shape to line parts up by.
    fn zip_shapes(
        &self,
        a: TypeId,
        b: TypeId,
        mut each: impl FnMut(TypeId, TypeId) -> bool,
    ) -> Option<bool> {
        let mut all = |xs: &[TypeId], ys: &[TypeId]| {
            xs.len() == ys.len() && xs.iter().zip(ys).all(|(&x, &y)| each(x, y))
        };
        match (self.get(a), self.get(b)) {
            (ResolvedType::Ref(x), ResolvedType::Ref(y))
            | (ResolvedType::MutRef(x), ResolvedType::MutRef(y))
            | (ResolvedType::BuiltinArray(x), ResolvedType::BuiltinArray(y)) => {
                Some(all(&[*x], &[*y]))
            }
            (
                ResolvedType::Function {
                    is_mut: x_mut,
                    params: x_params,
                    return_type: x_ret,
                    effects: x_effects,
                },
                ResolvedType::Function {
                    is_mut: y_mut,
                    params: y_params,
                    return_type: y_ret,
                    effects: y_effects,
                },
            ) => Some(
                x_mut == y_mut
                    && rows_agree(x_effects, y_effects)
                    && all(x_params, y_params)
                    && all(&[*x_ret], &[*y_ret]),
            ),
            _ => match (self.generic_type_args(a), self.generic_type_args(b)) {
                (Some(x), Some(y)) => Some(
                    self.fq_base_type_name(a).head() == self.fq_base_type_name(b).head()
                        && all(&x, &y),
                ),
                _ => None,
            },
        }
    }

    /// Whether some type instantiates both targets `a` and `b`, their type
    /// parameters kept apart even where they share an id.
    pub fn targets_overlap(&self, a: TypeId, b: TypeId) -> bool {
        let mut subst = IndexMap::default();
        self.unify_apart((Side::Left, a), (Side::Right, b), &mut subst)
    }

    fn unify_apart(&self, a: Term, b: Term, subst: &mut IndexMap<(Side, u32), Term>) -> bool {
        let (a, b) = (self.walk_term(a, subst), self.walk_term(b, subst));
        let var = |(side, id): Term| self.param_slot(id).map(|slot| (side, slot));
        match (var(a), var(b)) {
            (Some(x), Some(y)) if x == y => return true,
            (Some(x), _) => return !self.occurs(x, b, subst) && subst.insert(x, b).is_none(),
            (_, Some(y)) => return !self.occurs(y, a, subst) && subst.insert(y, a).is_none(),
            (None, None) => {}
        }
        let unknown =
            |id: TypeId| matches!(self.get(id), ResolvedType::Unknown | ResolvedType::Error);
        if unknown(a.1) || unknown(b.1) {
            return true;
        }
        self.zip_shapes(a.1, b.1, |x, y| self.unify_apart((a.0, x), (b.0, y), subst))
            .unwrap_or_else(|| self.type_key(a.1) == self.type_key(b.1))
    }

    /// `term` past every parameter the substitution has bound.
    fn walk_term(&self, mut term: Term, subst: &IndexMap<(Side, u32), Term>) -> Term {
        while let Some(slot) = self.param_slot(term.1)
            && let Some(&next) = subst.get(&(term.0, slot))
        {
            term = next;
        }
        term
    }

    /// Whether the parameter `var` appears in `term`, bindings followed: binding
    /// it there would make a type contain itself.
    fn occurs(&self, var: (Side, u32), term: Term, subst: &IndexMap<(Side, u32), Term>) -> bool {
        let (side, id) = self.walk_term(term, subst);
        if let Some(slot) = self.param_slot(id) {
            return (side, slot) == var;
        }
        let inside = |ids: &[TypeId]| ids.iter().any(|&i| self.occurs(var, (side, i), subst));
        match self.get(id) {
            ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::BuiltinArray(inner) => inside(&[*inner]),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => inside(params) || inside(&[*return_type]),
            _ => self.generic_type_args(id).is_some_and(|args| inside(&args)),
        }
    }
}

/// Which of two impl targets a type parameter belongs to, in
/// [`TypeTable::targets_overlap`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Side {
    Left,
    Right,
}

type Term = (Side, TypeId);

/// How many characters of a template's text its type name shows.
const TEMPLATE_NAME_MAX_CHARS: usize = 50;

/// An interned shape with the mangle it was minted under. The mangle renders
/// the shape's field types, and erasure later redirects a newtype among them
/// to its base: rendering again would spell a different name from the one the
/// `TirStruct` was registered under, so the name is fixed at interning.
#[derive(Debug, Clone)]
struct AnonEntry {
    module: ModuleSource,
    shape: AnonShape,
    mangle: String,
    /// Whether the shape mentions a type parameter, which its types fix.
    open: bool,
    /// Whether a literal wrote the shape, so elaboration emitted its struct.
    written: bool,
    /// The substitutions that make this shape, one per written origin: a
    /// concrete shape is where any number of open ones, and a literal, meet.
    substitutions: Vec<ShapeSubstitution>,
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
}

/// [`AssocTypeKey`] for a generic impl, whose target is a declaration rather
/// than an instantiated type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GenericAssocTypeKey {
    target_decl: AstId,
    trait_decl: DefId,
    assoc_name: String,
}

/// What an associated type belongs to. The trait's arguments are not part of
/// it: a bound writes none, so every lookup names them all and
/// [`AssocAnswers`] picks between them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AssocTypeKey {
    receiver: InstanceKey,
    trait_decl: DefId,
    assoc_name: String,
}

/// A type keyed by its declaration and arguments: one key whether read as the
/// instance or as the monomorphized struct, which [`TypeKey`] keeps apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InstanceKey {
    Nominal(DefId, Vec<InstanceKey>),
    Ref(Box<InstanceKey>),
    MutRef(Box<InstanceKey>),
    /// The interned slot of the unerased type: a newtype keeps its own impls.
    Other(TypeId),
}

/// The answers registered under one key, by the arguments the impl wrote
/// beyond the declared defaults: `impl Add for Cm` and `impl Add<Inch> for Cm`
/// both give `Output`.
#[derive(Debug, Clone, Default)]
struct AssocAnswers(Vec<AssocAnswer>);

/// The associated types one receiver answers, from
/// [`TypeTable::assoc_types_of`].
#[derive(Debug, Clone)]
pub struct ReceiverAssocTypes(Vec<(AssocTypeKey, AssocAnswers)>);

/// One impl's answer: the trait arguments its header writes, which a bare
/// bound tells apart by their absence, and those with the trait's defaults
/// filled, which a projection reached at full arguments compares. A generic
/// impl's answer is written over the parameters of `impl_def`, whose target
/// binds them at an instance; one with no impl block, a synthesized impl's, is
/// written over the declaration's own parameters.
#[derive(Debug, Clone, Default)]
struct AssocAnswer {
    written: Vec<TypeId>,
    filled: Option<Vec<TypeId>>,
    answer: TypeId,
    impl_def: Option<DefId>,
}

impl AssocAnswers {
    fn insert(
        &mut self,
        written: Vec<TypeId>,
        filled: Option<Vec<TypeId>>,
        answer: TypeId,
        impl_def: Option<DefId>,
    ) {
        let entry = AssocAnswer {
            written,
            filled,
            answer,
            impl_def,
        };
        match self.0.iter_mut().find(|slot| slot.written == entry.written) {
            Some(slot) => *slot = entry,
            None => self.0.push(entry),
        }
    }

    /// The one answer a bound names: the bare instantiation where one is
    /// registered, else whatever the arguments-writing impls agree on. A bound
    /// writes no arguments, so it cannot pick between impls that disagree.
    fn bare(&self) -> Option<TypeId> {
        one_assoc_answer(self.tagged())
    }

    /// The entry [`Self::bare`] answers from, whose impl frame its answer is
    /// written in: a bare one where one is registered.
    fn bare_entry(&self) -> Option<&AssocAnswer> {
        let answer = self.bare()?;
        let any_bare = self.0.iter().any(|entry| entry.written.is_empty());
        self.0
            .iter()
            .find(|entry| (entry.written.is_empty() || !any_bare) && entry.answer == answer)
    }

    /// The answer of the impl reached at `trait_args`, where they are known
    /// and an impl's, defaults filled, are exactly them; else [`Self::bare`].
    fn at(&self, table: &TypeTable, trait_args: Option<&[TypeId]>) -> Option<TypeId> {
        trait_args
            .and_then(|wanted| {
                self.0
                    .iter()
                    .find(|entry| {
                        entry
                            .filled
                            .as_deref()
                            .is_some_and(|filled| table.same_types(filled, wanted))
                    })
                    .map(|entry| entry.answer)
            })
            .or_else(|| self.bare())
    }

    /// Each answer paired with whether a bare bound names it, for a caller
    /// weighing answers from several traits at once.
    fn tagged(&self) -> impl Iterator<Item = (bool, TypeId)> + Clone {
        self.0
            .iter()
            .map(|entry| (entry.written.is_empty(), entry.answer))
    }

    /// Every type the answers hold: the answers and the arguments filing them.
    fn types(&self) -> impl Iterator<Item = TypeId> + '_ {
        self.0.iter().flat_map(|entry| {
            entry
                .written
                .iter()
                .chain(entry.filled.iter().flatten())
                .copied()
                .chain([entry.answer])
        })
    }
}

/// The Component Model coordinate `module` is addressed by, or `None` where it
/// declares nothing at that boundary. A `core:` module carries no namespace.
fn cm_module_key(
    name: &str,
    module: &ModuleSource,
) -> Option<(String, Option<CmNamespace>, String)> {
    match module {
        ModuleSource::Binding {
            namespace,
            interface,
        } => Some((
            name.to_string(),
            Some(*namespace),
            interface.as_str().to_string(),
        )),
        ModuleSource::Core { name: cm_name } => {
            Some((name.to_string(), None, cm_name.as_str().to_string()))
        }
        _ => None,
    }
}

/// The one answer among `candidates`, preferring those a bare bound names when
/// any is registered: a bound writes no trait arguments, so an impl that does
/// is consulted only when nothing else answers. `None` when they disagree.
fn one_assoc_answer<T: Copy + PartialEq>(
    candidates: impl Iterator<Item = (bool, T)> + Clone,
) -> Option<T> {
    let bare = candidates.clone().any(|(bare, _)| bare);
    let mut answers = candidates
        .filter(|(is_bare, _)| *is_bare || !bare)
        .map(|(_, answer)| answer);
    let first = answers.next()?;
    answers.all(|answer| answer == first).then_some(first)
}

/// Whether a slot search descends into a projection's base or stops there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Through {
    Projection,
    ProjectionStops,
}

static NO_PARAMS: IndexMap<u32, TypeId> = IndexMap::with_hasher(FxBuildHasher);
static NO_VARS: IndexMap<InferVarId, TypeId> = IndexMap::with_hasher(FxBuildHasher);
static NO_PROJECTIONS: SlotProjections = IndexMap::with_hasher(FxBuildHasher);
static NO_ROWS: EffectRows = IndexMap::with_hasher(FxBuildHasher);

/// Where a type's spelling writes its function types' rows. A function value
/// is one whatever its row, so an instance whose arguments differ only in
/// rows is one instance and answers one name; a function type standing alone
/// keeps its row, which selects an impl written for it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowSpelling {
    /// Outside type arguments only: the name an instance answers to.
    OutsideArgs,
    /// Nowhere: inside a type argument.
    Nowhere,
    /// Everywhere: the rows themselves, for the effect check to read.
    Everywhere,
}

/// Whether two function types' rows can be one: equal, or one still open. An
/// effect parameter stands for whatever row meets it, and a variable for the
/// row the effect check solves it to.
fn rows_agree(a: &[EffectRef], b: &[EffectRef]) -> bool {
    let open = |row: &[EffectRef]| row.iter().any(|e| e.is_param() || e.is_var());
    a == b || open(a) || open(b)
}

/// What one substitution replaces, leaf by leaf: a declaration's slots, solved
/// inference variables, a generic associated type's own parameters, the
/// projections a use site answers, and the effect parameters and variables in
/// a function type's row.
#[derive(Clone, Copy)]
struct Leaves<'s> {
    params: &'s IndexMap<u32, TypeId>,
    vars: &'s IndexMap<InferVarId, TypeId>,
    family: Option<FamilyAt<'s>>,
    projections: &'s SlotProjections,
    rows: &'s EffectRows,
    /// Whether every row is emptied, not just the names `rows` holds.
    erase_rows: bool,
}

/// One generic associated type's parameters, and the arguments a projection
/// gives them.
#[derive(Clone, Copy)]
struct FamilyAt<'s> {
    owning_trait: DefId,
    assoc_name: &'s str,
    args: &'s [TypeId],
}

impl Leaves<'static> {
    const NONE: Self = Self {
        params: &NO_PARAMS,
        vars: &NO_VARS,
        family: None,
        projections: &NO_PROJECTIONS,
        rows: &NO_ROWS,
        erase_rows: false,
    };
}

impl Leaves<'_> {
    fn is_empty(&self) -> bool {
        self.params.is_empty()
            && self.vars.is_empty()
            && self.family.is_none_or(|at| at.args.is_empty())
            && self.projections.is_empty()
            && self.rows.is_empty()
            && !self.erase_rows
    }

    /// `effects` with each parameter or variable this substitution names
    /// replaced by its row, each effect once.
    fn row(&self, effects: &[EffectRef]) -> Vec<EffectRef> {
        if self.erase_rows {
            return Vec::new();
        }
        let mut out: Vec<EffectRef> = Vec::new();
        for effect in effects {
            let replaced = match effect {
                EffectRef::Param { name } | EffectRef::Var { name, .. } => self.rows.get(name),
                EffectRef::Concrete(_) | EffectRef::Undeclared { .. } => None,
            };
            for e in replaced.map_or(std::slice::from_ref(effect), Vec::as_slice) {
                if !out.contains(e) {
                    out.push(e.clone());
                }
            }
        }
        out
    }
}

/// What a [`TypeId`] resolves to, as a value to compare or key a map by.
/// An id addresses a slot; this answers identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeKey(TypeId);

/// An arbitrary but fixed order, for a key that has to sort.
impl Ord for TypeKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.0.cmp(&other.0.0)
    }
}

impl PartialOrd for TypeKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{AstId, AstIdSpace};
    use crate::intern::StringInterner;
    use crate::tir::ProjectionAnswer;
    use std::assert_matches;

    #[test]
    fn test_primitive_constants() {
        let table = TypeTable::new();
        assert_matches!(
            table.get(TypeTable::I32),
            ResolvedType::Primitive(PrimitiveType::I32)
        );
        assert_matches!(
            table.get(TypeTable::BOOL),
            ResolvedType::Primitive(PrimitiveType::Bool)
        );
        // Note: String is now a user-defined struct, not a builtin type
        assert_matches!(table.get(TypeTable::UNIT), ResolvedType::Unit);
    }

    /// The boxing rewrite redefines many slots onto one type, so two ids that
    /// resolve alike are the normal case and `==` is not the question to ask.
    #[test]
    fn a_key_unifies_ids_a_redefinition_left_resolving_alike() {
        let mut table = TypeTable::new();
        let shared = table.intern(ResolvedType::Ref(TypeTable::I32));
        let redefined = table.intern(ResolvedType::MutRef(TypeTable::I32));
        table.redefine_to(redefined, shared);

        assert_ne!(shared, redefined);
        assert_eq!(table.type_key(redefined), table.type_key(shared));
        assert_ne!(table.type_key(shared), table.type_key(TypeTable::I32));
    }

    /// A redefinition reaches every view of the type: only `is_mut_box` reads
    /// the reference left in the slot. The spelling also leaves `intern`, so a
    /// later ask for it mints a live reference.
    #[test]
    fn a_redefinition_reaches_every_view_of_the_type() {
        let mut table = TypeTable::new();
        let shared = table.intern(ResolvedType::Ref(TypeTable::I32));
        let redefined = table.intern(ResolvedType::MutRef(TypeTable::I32));
        table.redefine_to(redefined, shared);

        assert_matches!(table.get(redefined), ResolvedType::Ref(TypeTable::I32));
        assert_matches!(
            table.get_unerased(redefined),
            ResolvedType::Ref(TypeTable::I32)
        );
        let minted = table.intern(ResolvedType::MutRef(TypeTable::I32));
        assert_ne!(minted, redefined);
        assert_matches!(table.get(minted), ResolvedType::MutRef(TypeTable::I32));
    }

    /// `&T` and `&mut T` share one `Box<T>`, so the reference left in the slot
    /// is the only thing that still tells a writable box from a shared one.
    #[test]
    fn a_redefined_ref_still_says_whether_it_was_mut() {
        let mut table = TypeTable::new();
        let shared = table.intern(ResolvedType::Ref(TypeTable::I32));
        let mutable = table.intern(ResolvedType::MutRef(TypeTable::I32));
        for wrapper in [shared, mutable] {
            table.redefine_to(wrapper, TypeTable::I32);
            table.register_box_payload(wrapper, TypeTable::I32);
        }

        assert!(!table.is_mut_box(shared));
        assert!(table.is_mut_box(mutable));
    }

    /// `retain` rebuilds the intern map from the surviving slots, and a
    /// retired reference is still spelled as one there.
    #[test]
    fn retain_leaves_a_retired_spelling_retired() {
        let mut table = TypeTable::new();
        let redefined = table.intern(ResolvedType::Ref(TypeTable::I32));
        table.redefine_to(redefined, TypeTable::I32);
        let mut keep = IndexSet::default();
        keep.insert(redefined);
        keep.insert(TypeTable::I32);
        table.retain(&keep);

        assert_eq!(table.type_key(redefined), table.type_key(TypeTable::I32));
        assert_ne!(table.intern(ResolvedType::Ref(TypeTable::I32)), redefined);
    }

    /// A surviving anonymous struct spells itself with its field types, which
    /// nothing else may keep alive once the optimizer has split the value.
    #[test]
    fn retain_keeps_an_anonymous_shapes_field_types() {
        let mut table = TypeTable::new();
        let field = table.intern(ResolvedType::Ref(TypeTable::I64));
        let module = ModuleSource::Core {
            name: StringInterner::new().intern("test"),
        };
        let shape = table.intern_anon_struct(module, vec![("a".to_string(), field)]);
        let anon = table.make_struct(StructDef::Anon(shape));
        let mut keep = IndexSet::default();
        keep.insert(anon);
        table.retain(&keep);

        assert_eq!(table.type_name(anon), "{ a: &i64 }");
    }

    #[test]
    #[should_panic(expected = "type_id_of_decl")]
    fn type_id_of_decl_panics_when_unregistered() {
        let table = TypeTable::new();
        let unregistered = AstId::new(AstIdSpace::next(), 0);
        let _ = table.type_id_of_decl(unregistered);
    }

    fn make_projection(table: &mut TypeTable, base: TypeId, assoc: &str) -> TypeId {
        table.make_assoc_type_projection(
            base,
            DefId::for_test(0),
            None,
            assoc.to_string(),
            vec![],
            vec![],
        )
    }

    /// A use site's answer for a generic associated type is the binding, its
    /// own parameters open, so the projection's arguments fill them; the
    /// arguments themselves take the substitution first.
    #[test]
    fn a_family_answer_is_instantiated_at_the_projection_arguments() {
        let mut table = TypeTable::new();
        let self_param = table.make_type_param("Self".to_string(), 0);
        let method_param = table.make_type_param("E".to_string(), 1);
        let projection = table.make_assoc_type_projection(
            self_param,
            DefId::for_test(0),
            None,
            "Item".to_string(),
            vec![method_param],
            vec![],
        );
        let open = table.family_params(DefId::for_test(0), "Item", &["E".to_string()]);
        let family = table.make_builtin_array(open[0]);

        let substituted = table.substitute_type_params_with(
            projection,
            &IndexMap::from_iter([(1, TypeTable::U8)]),
            &answer_item(DefId::for_test(0), family),
        );

        assert_eq!(substituted, table.make_builtin_array(TypeTable::U8));
    }

    /// Instantiating one family leaves another's parameters standing: a
    /// binding of `Buf<E>` may mention `Thing`'s parameter at the same index.
    #[test]
    fn instantiating_a_family_leaves_another_familys_parameters() {
        let mut table = TypeTable::new();
        let buf = table.family_params(DefId::for_test(0), "Buf", &["E".to_string()])[0];
        let thing = table.family_params(DefId::for_test(0), "Thing", &["X".to_string()])[0];
        let of_buf = table.make_builtin_array(buf);
        let of_thing = table.make_builtin_array(thing);

        let at = |table: &mut TypeTable, family| {
            table.instantiate_family(DefId::for_test(0), "Buf", family, &[TypeTable::U8])
        };

        assert_eq!(
            at(&mut table, of_buf),
            table.make_builtin_array(TypeTable::U8)
        );
        assert_eq!(at(&mut table, of_thing), of_thing);
    }

    /// Substituting a projection's base rewrites the projection even when the
    /// replacement is itself a parameter. `Self::Item` under `Self := I` is
    /// `I::Item`, not `Self::Item` — a trait signature instantiated for an
    /// impl's receiver slot is the case that needs it.
    #[test]
    fn substitute_rewrites_projection_base_to_another_param() {
        let mut table = TypeTable::new();
        let self_param = table.make_type_param("Self".to_string(), 0);
        let projection = make_projection(&mut table, self_param, "Item");

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
        let projection = make_projection(&mut table, self_param, "Item");

        let receiver = table.make_type_param("I".to_string(), 1);
        let substituted = table.substitute_type_params_with(
            projection,
            &IndexMap::from_iter([(0, receiver)]),
            &answer_item(DefId::for_test(0), TypeTable::U8),
        );

        assert_eq!(substituted, TypeTable::U8);
    }

    fn answer_item(trait_: DefId, answer: TypeId) -> SlotProjections {
        SlotProjections::from_iter([(
            0,
            vec![ProjectionAnswer::at_any(trait_, "Item".to_string(), answer)],
        )])
    }

    /// An answer is for one trait's associated type: another trait declaring
    /// the same name on the same slot is a different projection.
    #[test]
    fn an_answer_for_another_trait_leaves_the_projection() {
        let mut table = TypeTable::new();
        let self_param = table.make_type_param("Self".to_string(), 0);
        let projection = make_projection(&mut table, self_param, "Item");

        let receiver = table.make_type_param("I".to_string(), 1);
        let substituted = table.substitute_type_params_with(
            projection,
            &IndexMap::from_iter([(0, receiver)]),
            &answer_item(DefId::for_test(1), TypeTable::U8),
        );

        let ResolvedType::AssocTypeProjection { param_id, .. } = table.get(substituted).clone()
        else {
            panic!("expected a projection, got {:?}", table.get(substituted));
        };
        assert_eq!(param_id, receiver);
    }

    /// An unanswered name leaves the projection abstract over the substituted
    /// base — the frame simply does not know, and inventing an answer would
    /// be worse than deferring to monomorphization.
    #[test]
    fn an_unanswered_projection_stays_abstract() {
        let mut table = TypeTable::new();
        let self_param = table.make_type_param("Self".to_string(), 0);
        let projection = make_projection(&mut table, self_param, "Iter");

        let receiver = table.make_type_param("I".to_string(), 1);
        let substituted = table.substitute_type_params_with(
            projection,
            &IndexMap::from_iter([(0, receiver)]),
            &answer_item(DefId::for_test(0), TypeTable::U8),
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
        let projection = make_projection(&mut table, self_param, "Item");

        let substitution = IndexMap::from_iter([(7, TypeTable::I32)]);
        assert_eq!(
            table.substitute_type_params(projection, &substitution),
            projection
        );
    }

    /// Records a block whose target is `whole`, naming a head with `args`, and
    /// asks whether it covers every instance of that head.
    fn covers(table: &mut TypeTable, whole: TypeId, args: Vec<TypeId>) -> bool {
        let block = DefId::for_test(1);
        table.record_impl_target(block, whole, args, vec![]);
        table.impl_covers_every_instance(block)
    }

    #[test]
    fn a_target_of_distinct_binders_covers_every_instance() {
        let mut table = TypeTable::new();
        let k = table.make_type_param("K".to_string(), 0);
        let v = table.make_type_param("V".to_string(), 1);
        let head = table.make_builtin_array(k);
        assert!(covers(&mut table, head, vec![k, v]));
    }

    #[test]
    fn a_target_writing_no_argument_covers_its_head() {
        let mut table = TypeTable::new();
        assert!(covers(&mut table, TypeTable::I32, vec![]));
    }

    #[test]
    fn a_pack_binder_covers_every_arity() {
        let mut table = TypeTable::new();
        let t = table.make_type_param("T".to_string(), 0);
        let rest = table.make_type_pack("Rest".to_string(), 1);
        let head = table.make_builtin_array(t);
        assert!(covers(&mut table, head, vec![t, rest]));
    }

    /// `impl<T> Tr for Pair<T, T>` reaches only the instances whose two
    /// arguments agree.
    #[test]
    fn a_binder_written_twice_reaches_some_instances() {
        let mut table = TypeTable::new();
        let t = table.make_type_param("T".to_string(), 0);
        let head = table.make_builtin_array(t);
        assert!(!covers(&mut table, head, vec![t, t]));
    }

    #[test]
    fn a_concrete_argument_reaches_some_instances() {
        let mut table = TypeTable::new();
        let t = table.make_type_param("T".to_string(), 0);
        let head = table.make_builtin_array(t);
        assert!(!covers(&mut table, head, vec![t, TypeTable::I32]));
    }

    /// `impl<T> Display for &Wrapper<T>` writes distinct binders for `Wrapper`,
    /// but its head is `&`, of which it reaches one kind of referent.
    #[test]
    fn a_reference_target_reaches_some_instances() {
        let mut table = TypeTable::new();
        let t = table.make_type_param("T".to_string(), 0);
        let wrapper = table.make_builtin_array(t);
        let whole = table.make_ref(wrapper);
        assert!(!covers(&mut table, whole, vec![t]));
    }

    #[test]
    fn a_unit_target_reaches_some_instances() {
        let mut table = TypeTable::new();
        assert!(!covers(&mut table, TypeTable::UNIT, vec![]));
    }

    #[test]
    #[should_panic(expected = "before its target is recorded")]
    fn a_block_with_no_recorded_target_is_not_answered_for() {
        let table = TypeTable::new();
        let _ = table.impl_covers_every_instance(DefId::for_test(1));
    }
}
