//! [`TypeSystem`] — pipeline-wide type knowledge. Every field is `'static`,
//! [`Arc`]- or [`Rc`]-wrapped, so a `Clone` is a shallow copy each per-module
//! [`super::Elaborator`] holds. A field belongs here only if it fits the type
//! system itself: per-call mutable state does not, even when cache-shaped, since
//! sharing a recursion stack across module walks would leak frames between them.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{self, BinaryOp, Expr, Literal, Type, UnaryOp};
use crate::builtin_registry::BuiltinRegistry;
use crate::compiler_item::CompilerItem;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::trait_env::TraitEnv;
use super::types::{
    EnumInfo, FlagsInfo, GenericNewtypeInfo, ResourceInfo, StructFieldInfo, VariantInfo,
};

/// Pipeline-wide type knowledge — the type arena, the cross-module decl
/// indices, the registries, and the read-only caches built once at
/// `annotate_modules` time.
///
/// See the module-level documentation for the membership rule and the
/// rationale for the `Elaborator` caches that were removed rather than
/// migrated here.
#[derive(Clone)]
pub(crate) struct TypeSystem {
    /// Shared type arena. Anonymous structs synthesised from struct
    /// literals and monomorphised instances created during reify intern
    /// through this same table; the `Rc<RefCell<…>>` is the one piece of
    /// shared interior mutability the WEP explicitly preserves.
    pub(crate) type_table: Rc<RefCell<TypeTable>>,

    /// Decl-interned type tables (one per loaded module). Built during
    /// the annotate-decls pass; read-only afterwards. [`super::types::TypeLookup`]
    /// resolves type names against these without cloning into per-module
    /// flat maps.
    pub(crate) all_newtypes: Rc<IndexMap<crate::defs::DefId, TypeId>>,
    pub(crate) all_generic_newtypes: Rc<IndexMap<crate::defs::DefId, GenericNewtypeInfo>>,
    pub(crate) all_struct_fields: Rc<IndexMap<crate::defs::DefId, StructFieldInfo>>,
    pub(crate) all_variant_cases: Rc<IndexMap<crate::defs::DefId, VariantInfo>>,
    pub(crate) all_enum_cases: Rc<IndexMap<crate::defs::DefId, EnumInfo>>,
    pub(crate) all_flags_cases: Rc<IndexMap<crate::defs::DefId, FlagsInfo>>,
    pub(crate) all_resource_types: Rc<IndexMap<crate::defs::DefId, ResourceInfo>>,

    /// What every type/trait reference site in the program refers to, resolved
    /// once from the module that wrote it. The single producer of declaration
    /// identity from written syntax (WEP 2026-08-12).
    pub(crate) resolutions: Rc<crate::resolve::Resolutions>,

    /// Immutable trait knowledge base: impl indices, trait declarations,
    /// and blanket impls. Built once by [`TraitEnv::build`] and shared
    /// across every per-module elaborator via `Arc`.
    pub(crate) trait_env: Arc<TraitEnv>,
    /// The solver's view of the program, built once every declaration is
    /// resolved; `None` until then (WEP 2026-09-01, "How the order is guaranteed").
    pub(crate) solver: Option<Rc<super::solver_bridge::SolverBridge>>,

    /// Registries the elaborator queries. The Component-Model
    /// `WorldRegistry` is built by the same `CmInterfaceRegistry::build_from_stdlib`
    /// call but lives on [`super::orchestration::AnnotateState`] instead
    /// of here — the elaborator never asks "what does world X export?",
    /// only post-elaborator stages (link, synthesis, DCE) do.
    pub(crate) cm_interface_registry: std::sync::Arc<CmInterfaceRegistry>,
    pub(crate) builtin_registry: Rc<BuiltinRegistry>,

    /// Pre-loaded file contents for `#include_str` / `#include_bytes`.
    /// Key: `[module_source_display, raw_path]`, value: raw bytes.
    pub(crate) included_files: Rc<IndexMap<[String; 2], Vec<u8>>>,

    /// Flat set of every name that resolves to a declared type
    /// (primitive, struct, enum, variant, flags, newtype, resource).
    /// Built globally during annotate; read-only afterwards. Powers fast
    /// `is_known_type_name` lookups in the body walk.
    pub(crate) known_type_names_cache: Rc<IndexSet<String>>,

    /// Per-module *visible* type names: the type names each module can
    /// actually resolve — its own declarations, the auto-imported prelude,
    /// the primitives, and the types it explicitly `use`s. Always a subset
    /// of [`Self::known_type_names_cache`]; unlike that global union it is
    /// **not** polluted by type names from unrelated modules. This is what
    /// distinguishes a free impl type parameter (`E` in the prelude's
    /// `impl Result<T, E>`, which `core:prelude/types` cannot resolve) from
    /// a concrete instantiation argument (`u8` in `impl List<u8>`), even
    /// when a *user* module declares a type that happens to be named `E`.
    pub(crate) module_visible_types: Rc<IndexMap<ModuleSource, IndexSet<String>>>,

    /// Per-module index from function name → position in `module.items`
    /// for O(1) lookup. Built globally during annotate; read-only
    /// afterwards.
    pub(crate) loaded_module_func_indices: Rc<IndexMap<ModuleSource, IndexMap<String, usize>>>,

    /// Every source declaration's decl-pass facts — signatures, globals,
    /// associated constants, data sections. See [`super::sig::Signatures`]
    /// for the membership rule.
    pub(crate) signatures: Rc<super::sig::Signatures>,
}

impl TypeSystem {
    /// Check if a name refers to a known type (struct, variant, enum,
    /// flags, newtype, or primitive). Uses the pre-built cache for O(1)
    /// lookup instead of scanning all module maps.
    pub(crate) fn is_known_type_name(&self, name: &str) -> bool {
        self.known_type_names_cache.contains(name)
    }

    /// The `TypeId` of each field of the struct `type_id` names, in declaration
    /// order, or `None` if it names no registered struct. Keyed by the type
    /// itself rather than a spelling of it, which is what every caller holds:
    /// each reached one by destructuring a `ResolvedType`. Used by the resource
    /// move check to decide whether an aggregate transitively owns a resource.
    pub(crate) fn struct_field_type_ids_of(
        &self,
        type_id: TypeId,
    ) -> Option<Vec<crate::tir::TypeId>> {
        let info = self.all_struct_fields.get(&self.type_def(type_id)?)?;
        Some(info.fields.iter().map(|(_, tid, _)| *tid).collect())
    }

    /// Whether `type_id` is, or transitively carries, an affine resource
    /// (`Resource` / `GenericResource`, or a struct / tuple / `Result` holding
    /// one). Mirrors `resource_move_check::type_carries_resource`; used to
    /// permit a by-value `self` receiver on an aggregate that owns a resource.
    /// A reference stops the walk — a borrowed place owns nothing.
    pub(crate) fn carries_resource(&self, type_id: TypeId) -> bool {
        self.carries_resource_rec(type_id, &mut Vec::new())
    }

    fn carries_resource_rec(&self, type_id: TypeId, visited: &mut Vec<TypeId>) -> bool {
        use crate::tir::ResolvedType;
        let base = self.type_table.borrow().representation_head(type_id);
        if visited.contains(&base) {
            return false;
        }
        visited.push(base);
        let children: Vec<TypeId> = match self.type_table.borrow().get(base).clone() {
            ResolvedType::Resource { def } => {
                return !self.type_table.borrow().is_extern_handle_resource(def);
            }
            ResolvedType::GenericResource { .. } => return true,
            ResolvedType::Ref(_) | ResolvedType::MutRef(_) => return false,
            ResolvedType::Struct { .. } => self.struct_field_type_ids_of(base).unwrap_or_default(),
            ResolvedType::GenericInstance { type_args, .. }
                if self.type_table.borrow().is_result(base) =>
            {
                type_args
            }
            _ => self.type_table.borrow().as_tuple(base).unwrap_or_default(),
        };
        children
            .into_iter()
            .any(|t| self.carries_resource_rec(t, visited))
    }

    /// Whether `name` resolves to a declared type *from `module`'s perspective*
    /// — its own declarations, the prelude, a primitive, or an explicit import.
    /// Unlike the global union [`Self::is_known_type_name`], no unrelated module
    /// can pollute it: a user type named `E` does not stop the prelude's
    /// `impl Result<T, E>` treating `E` as free. Unknown modules use the union.
    pub(crate) fn is_known_type_name_in(&self, module: &ModuleSource, name: &str) -> bool {
        match self.module_visible_types.get(module) {
            Some(visible) => visible.contains(name),
            None => self.is_known_type_name(name),
        }
    }

    /// The `Type::Case` spelling of the case the resolve walk names at a bare
    /// identifier site: the hint when no expected type supplies one.
    pub(crate) fn bare_case_at(&self, site: crate::ast::AstId) -> Option<String> {
        let case = self.resolutions.declared_if_walked(site)?;
        let defs = self.resolutions.defs();
        if !defs.kind(case).is_case() {
            return None;
        }
        let owner = defs
            .parent(case)
            .expect("a case is a member of the type declaring it");
        Some(self.qualified_case(owner, defs.name(case)))
    }

    /// `type_id` with each `TypeParam { index: i }` replaced by `type_args[i]`.
    pub(crate) fn substitute_type_params(&self, type_id: TypeId, type_args: &[TypeId]) -> TypeId {
        if type_args.is_empty() {
            return type_id;
        }
        let substitution: crate::hashmap::IndexMap<u32, TypeId> = type_args
            .iter()
            .enumerate()
            .map(|(i, &t)| (i as u32, t))
            .collect();
        self.type_table
            .borrow_mut()
            .substitute_type_params(type_id, &substitution)
    }

    /// The `Type::Case` spelling of `case` under `owner`.
    pub(crate) fn qualified_case(&self, owner: crate::defs::DefId, case: &str) -> String {
        format!("{}::{case}", self.resolutions.defs().name(owner))
    }

    /// The method `name` that `owner` — an `impl` block or a `trait`
    /// declaration — declares. Answered from the declaration table, so two
    /// blocks on one type each declaring `name` stay distinct.
    pub(crate) fn declared_method(
        &self,
        owner: crate::defs::DefId,
        name: &str,
    ) -> Option<crate::defs::DefId> {
        let defs = self.resolutions.defs();
        defs.members(owner)
            .iter()
            .copied()
            .find(|&member| defs.name(member) == name)
    }

    /// Whether an impl target's generic argument names a type parameter of
    /// that impl rather than a concrete type: either the impl declares it, or
    /// the impl's module knows no type by that name. `String` in
    /// `impl Tr for Foo<String>` fills an argument position but binds no slot.
    pub(crate) fn is_impl_target_param(
        &self,
        module: &ModuleSource,
        declared: &[crate::ast::GenericParam],
        name: &str,
    ) -> bool {
        declared.iter().any(|p| p.name == name) || !self.is_known_type_name_in(module, name)
    }

    /// Whether `expr` is the bare `null` literal. A bare `null` resolves to
    /// `Option<!>` — a value of every `Option` and of nothing else — and
    /// acquires its inner type from an expected-type context, so callers that
    /// can supply one (e.g. binary operands) check this to route the type
    /// through.
    pub(crate) fn is_null_literal(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Literal(lit) if matches!(lit.value, Literal::Null))
    }

    /// Whether `expr` is a numeric literal, possibly negated. The non-numeric
    /// arms are enumerated rather than caught by `_`, so a new [`Expr`] variant
    /// forces a decision about numeric-literal coercion here.
    pub(crate) fn is_numeric_literal(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Literal(lit) => matches!(lit.value, Literal::Number(_)),
            Expr::Unary(unary) if unary.op == UnaryOp::Neg => {
                matches!(&unary.expr, Expr::Literal(lit) if matches!(lit.value, Literal::Number(_)))
            }
            Expr::Unary(_)
            | Expr::Ident(_)
            | Expr::Binary(_)
            | Expr::Assign(_)
            | Expr::CompoundAssign(_)
            | Expr::ComparisonChain(_)
            | Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::StaticMethodCall(_)
            | Expr::FieldAccess(_)
            | Expr::Index(_)
            | Expr::Block(_)
            | Expr::If(_)
            | Expr::Match(_)
            | Expr::Matches(_)
            | Expr::Closure(_)
            | Expr::TemplateString(_)
            | Expr::Cast(_)
            | Expr::StructLiteral(_)
            | Expr::TupleLiteral(_)
            | Expr::TupleComprehension(_)
            | Expr::LabeledBlock(_)
            | Expr::TryOp(_)
            | Expr::Spread(..)
            | Expr::Range(_)
            | Expr::WithHandler(_)
            | Expr::Resume(_)
            | Expr::Error(_) => false,
        }
    }
}

/// The trait `op` dispatches through and the method it calls, or `None` for
/// the short-circuit operators, which dispatch through no trait. `And` / `Or`
/// are explicit arms, so a new [`BinaryOp`] variant fails the build here.
pub(crate) fn operator_trait_method(op: &BinaryOp) -> Option<(CompilerItem, &'static str)> {
    match op {
        BinaryOp::Add => Some((CompilerItem::Add, "add")),
        BinaryOp::Sub => Some((CompilerItem::Sub, "sub")),
        BinaryOp::Mul => Some((CompilerItem::Mul, "mul")),
        BinaryOp::Div => Some((CompilerItem::Div, "div")),
        BinaryOp::Mod => Some((CompilerItem::Rem, "rem")),
        BinaryOp::BitAnd => Some((CompilerItem::BitAnd, "bitand")),
        BinaryOp::BitOr => Some((CompilerItem::BitOr, "bitor")),
        BinaryOp::BitXor => Some((CompilerItem::BitXor, "bitxor")),
        BinaryOp::Shl => Some((CompilerItem::Shl, "shl")),
        BinaryOp::Shr => Some((CompilerItem::Shr, "shr")),
        BinaryOp::Eq | BinaryOp::NotEq => Some((CompilerItem::Eq, "eq")),
        BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
            Some((CompilerItem::Ord, "cmp"))
        }
        BinaryOp::And | BinaryOp::Or => None,
    }
}

/// The compiler item `op` dispatches through.
pub(crate) fn operator_compiler_item(op: &BinaryOp) -> Option<CompilerItem> {
    operator_trait_method(op).map(|(item, _)| item)
}

/// Pure type-shape helpers answerable from the type table alone (peel
/// references, extract a declared type's name, newtype-base resolution, type
/// stringification). They touch only `self.type_table`; the body walk and
/// reify both reach them through `self.tysys`.
impl TypeSystem {
    /// Build declared type params for an impl block, filtering out known type names.
    pub(crate) fn build_declared_type_params(
        &self,
        impl_ty: &Type,
        impl_type_params: &[ast::GenericParam],
    ) -> IndexSet<String> {
        let mut declared: IndexSet<String> = impl_type_params
            .iter()
            .map(|p| p.name.clone())
            .filter(|name| !self.is_known_type_name(name))
            .collect();
        if let Type::Generic(g) = impl_ty {
            for arg in &g.args {
                if let Type::Named(n) = arg
                    && !self.is_known_type_name(&n.name)
                {
                    declared.insert(n.name.clone());
                }
            }
        }
        declared
    }

    /// Get the struct name from a type ID, if it's a struct, generic instance, newtype, or flags.
    pub(crate) fn struct_name_for_type(&self, type_id: TypeId) -> Option<String> {
        match self.type_table.borrow().get(type_id) {
            ResolvedType::Struct { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Newtype { .. }
            | ResolvedType::Flags { .. } => self
                .type_table
                .borrow()
                .nominal_head(type_id)
                .map(|(n, _)| n),
            // `Array<T>` is declared definitionless, so it has no nominal head
            // to read; its declaration names it `Array` and carries its impls.
            ResolvedType::BuiltinArray(_) => {
                Some(crate::tir::TypeTable::ARRAY_TYPE_NAME.to_string())
            }
            _ => None,
        }
    }

    /// The fq receiver name of `type_id`'s head: the name a method defined on
    /// this type is spelled with, module included and type arguments dropped
    /// (`List<i32>` → `core:prelude/list.wado/List`). Reading the module off
    /// the resolved type is what makes this exact — a written name would have
    /// to be re-resolved in the current scope, which the type already did.
    pub(crate) fn fq_receiver_head(&self, type_id: TypeId) -> crate::name::FqTypeName {
        self.type_table.borrow().fq_base_type_name(type_id)
    }

    /// The first link at or below `type_id` — itself included — writing its own
    /// impl of `trait_`, stopping above a scalar base: a primitive's operator
    /// impl *is* the instruction, not one a newtype inherits.
    pub(crate) fn own_impl_link(
        &self,
        type_id: TypeId,
        trait_: crate::defs::DefId,
    ) -> Option<TypeId> {
        let mut tid = type_id;
        loop {
            let key = self.type_table.borrow().impl_receiver_key(tid);
            if self
                .trait_env
                .has_any_methodful_impl_by_receiver(&key, trait_)
            {
                return Some(tid);
            }
            let base = self.type_table.borrow().get_newtype_base(tid)?;
            if !matches!(
                self.type_table.borrow().get(base),
                ResolvedType::Newtype { .. }
                    | ResolvedType::Struct { .. }
                    | ResolvedType::GenericInstance { .. }
                    | ResolvedType::Variant { .. }
            ) {
                return None;
            }
            tid = base;
        }
    }

    /// [`Self::newtype_base_lookup`] for a trait dispatch: an impl a link below
    /// the receiver wrote still answers for it, so stop at that link rather
    /// than one peel down, where a longer chain carries none.
    pub(crate) fn trait_impl_base_lookup(
        &self,
        name: &str,
        type_id: TypeId,
        trait_: crate::defs::DefId,
    ) -> (String, TypeId) {
        match self.own_impl_link(type_id, trait_) {
            Some(link) if link != type_id => (self.type_table.borrow().base_type_name(link), link),
            _ => self.newtype_base_lookup(name, type_id),
        }
    }

    /// A newtype's base name and `TypeId` for a trait-impl lookup fallback,
    /// else the name and id given.
    pub(crate) fn newtype_base_lookup(&self, name: &str, type_id: TypeId) -> (String, TypeId) {
        let tt = self.type_table.borrow();
        if let Some(base_id) = tt.get_newtype_base(type_id) {
            let is_builtin_array = matches!(tt.get(base_id), ResolvedType::BuiltinArray(_));
            drop(tt);
            if is_builtin_array {
                return (crate::tir::TypeTable::ARRAY_TYPE_NAME.to_string(), base_id);
            }
            if let Some(base_name) = self.struct_name_for_type(base_id) {
                return (base_name, base_id);
            }
        }
        (name.to_string(), type_id)
    }

    /// Peel a chain of trailing newtypes / generic instances down to the
    /// ultimate base struct (or builtin) name that owns its methods.
    pub(crate) fn get_ultimate_base_struct_name(&self, type_id: TypeId) -> String {
        let mut current = type_id;
        loop {
            match self.type_table.borrow().get(current).clone() {
                ResolvedType::Struct { def, .. } => {
                    return self.type_table.borrow().struct_head_name(def);
                }
                ResolvedType::GenericInstance { def, .. } => {
                    return self.type_table.borrow().def_name(def).to_string();
                }
                ResolvedType::Newtype { base_type, .. } => current = base_type,
                ResolvedType::Flags { .. } => return "u32".to_string(),
                // The raw GC array's base method-owner name is "Array"
                // (its type args are carried separately), not the full
                // `type_name` spelling `Array<T>`.
                ResolvedType::BuiltinArray(_) => return TypeTable::ARRAY_TYPE_NAME.to_string(),
                _ => return self.type_table.borrow().type_name(current),
            }
        }
    }

    /// Peel reference / mutable-reference wrappers to reach the underlying type.
    pub(crate) fn get_base_type(&self, type_id: TypeId) -> TypeId {
        let mut current = type_id;
        loop {
            match self.type_table.borrow().get(current).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    current = inner;
                }
                _ => return current,
            }
        }
    }

    /// Whether `type_id` is a kind that participates in `Eq`/`Ord` auto-derive.
    pub(crate) fn auto_derive_eligible_kind(&self, type_id: TypeId) -> bool {
        matches!(
            self.type_table.borrow().get(type_id),
            ResolvedType::Struct { .. }
                | ResolvedType::Variant { .. }
                | ResolvedType::Enum { .. }
                | ResolvedType::GenericInstance { .. }
        )
    }

    /// Substitute every occurrence of `base_type` with `newtype` inside
    /// `type_id`, recursing through references and generic-instance args.
    /// Returns the original id unchanged when no occurrence is found.
    pub(crate) fn substitute_newtype_in_type(
        &self,
        type_id: TypeId,
        base_type: TypeId,
        newtype: TypeId,
    ) -> TypeId {
        let ty = self.type_table.borrow().get(type_id).clone();
        match ty {
            // Direct match: base type -> newtype
            _ if type_id == base_type => newtype,

            // Reference: substitute the inner type
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_newtype_in_type(inner, base_type, newtype);
                if new_inner == inner {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(new_inner))
                }
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_newtype_in_type(inner, base_type, newtype);
                if new_inner == inner {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::MutRef(new_inner))
                }
            }

            // Generic instance (e.g., Option<T>, List<T>): substitute in type args
            ResolvedType::GenericInstance { def, type_args } => {
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute_newtype_in_type(arg, base_type, newtype))
                    .collect();
                if new_args == type_args {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::GenericInstance {
                            def,
                            type_args: new_args,
                        })
                }
            }

            // Other types: no substitution
            _ => type_id,
        }
    }

    /// Render a type as a user-facing Wado type string (used in diagnostics
    /// and synthesized names). Recurses through references, generic args,
    /// tuples, and function types.
    pub(crate) fn type_id_to_string(&self, type_id: TypeId) -> String {
        let resolved = self.type_table.borrow().get(type_id).clone();
        match resolved {
            ResolvedType::Primitive(prim) => format!("{prim:?}").to_lowercase(),
            ResolvedType::Struct { def, .. } => self.type_table.borrow().struct_head_name(def),
            ResolvedType::GenericInstance { def, type_args } => {
                let name = self.type_table.borrow().def_name(def).to_string();
                if TypeTable::is_tuple_type(&name) {
                    let parts: Vec<String> = type_args
                        .iter()
                        .map(|&t| self.type_id_to_string(t))
                        .collect();
                    format!("[{}]", parts.join(", "))
                } else if type_args.is_empty() {
                    name
                } else {
                    let args: Vec<String> = type_args
                        .iter()
                        .map(|&t| self.type_id_to_string(t))
                        .collect();
                    format!("{}<{}>", name, args.join(", "))
                }
            }
            ResolvedType::BuiltinArray(elem) => {
                format!("Array<{}>", self.type_id_to_string(elem))
            }
            ResolvedType::Ref(inner) => format!("&{}", self.type_id_to_string(inner)),
            ResolvedType::MutRef(inner) => format!("&mut {}", self.type_id_to_string(inner)),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                let param_strs: Vec<String> =
                    params.iter().map(|&t| self.type_id_to_string(t)).collect();
                let ret_str = self.type_id_to_string(return_type);
                format!("fn({}) -> {}", param_strs.join(", "), ret_str)
            }
            ResolvedType::TypeParam { name, .. } => name,
            ResolvedType::InferVar(var) => var.to_string(),
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Newtype { def, .. }
            | ResolvedType::Flags { def } => self.type_table.borrow().def_name(def).to_string(),
            ResolvedType::GenericResource { def, type_args } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|&t| self.type_id_to_string(t))
                    .collect();
                let name = self.type_table.borrow().def_name(def).to_string();
                format!("{}<{}>", name, args.join(", "))
            }
            ResolvedType::Reactive(inner) => {
                format!("Reactive<{}>", self.type_id_to_string(inner))
            }
            ResolvedType::TypePack { name, .. } => format!("..{name}"),
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                ..
            } => format!("{}::{}", self.type_id_to_string(param_id), assoc_name),
            ResolvedType::Unit => "()".to_string(),
            ResolvedType::Never => "!".to_string(),
            ResolvedType::Unknown => "<unknown>".to_string(),
            ResolvedType::Error => "<error>".to_string(),
        }
    }
}
