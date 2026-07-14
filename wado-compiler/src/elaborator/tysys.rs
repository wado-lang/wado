//! [`TypeSystem`] — pipeline-wide type knowledge.
//!
//! # Ownership
//!
//! Every field is `'static`, [`Arc`]-wrapped, or [`Rc`]-wrapped, so
//! `TypeSystem` is `Clone` (a shallow copy) and is handed out cheaply to
//! per-module phases: the driver builds one during
//! [`super::orchestration::Elaborator::annotate_modules`], and each
//! per-module [`super::Elaborator`] holds a clone.
//!
//! # Membership rule
//!
//! A field belongs here only when the answer to "would this fit the type
//! system itself?" is yes. Per-call mutable state does not qualify even
//! when it looks cache-shaped: sharing e.g. the `type_implements_trait`
//! recursion stack across module walks would leak frames between them
//! (it lives on [`super::scope::Scope`] instead), and the removed
//! `method_info_cache` / `indexing_trait_cache` were both stale-prone
//! covers over scans that the prebuilt `TraitEnv` indices already
//! memoise.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{BinaryOp, Expr, ImplBlock, Literal, Type, UnaryOp};
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
    pub(crate) all_newtypes: Rc<IndexMap<ModuleSource, IndexMap<String, TypeId>>>,
    pub(crate) all_generic_newtypes:
        Rc<IndexMap<ModuleSource, IndexMap<String, GenericNewtypeInfo>>>,
    pub(crate) all_struct_fields: Rc<IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>>,
    pub(crate) all_variant_cases: Rc<IndexMap<ModuleSource, IndexMap<String, VariantInfo>>>,
    pub(crate) all_enum_cases: Rc<IndexMap<ModuleSource, IndexMap<String, EnumInfo>>>,
    pub(crate) all_flags_cases: Rc<IndexMap<ModuleSource, IndexMap<String, FlagsInfo>>>,
    pub(crate) all_resource_types: Rc<IndexMap<ModuleSource, IndexMap<String, ResourceInfo>>>,

    /// Immutable trait knowledge base: impl indices, trait declarations,
    /// and blanket impls. Built once by [`TraitEnv::build`] and shared
    /// across every per-module elaborator via `Arc`.
    pub(crate) trait_env: Arc<TraitEnv>,

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

    /// Program-wide impl-associated constants, keyed by canonical identity
    /// `(type-declaring module, "Type::CONST")` → `(impl-declaring module,
    /// declared type, value expr)`. Canonical keys cannot collide across
    /// modules; `lookup_associated_constant` on the walkers canonicalizes
    /// the queried prefix the same way the decl pass did.
    pub(crate) all_associated_constants:
        Rc<IndexMap<(ModuleSource, String), (ModuleSource, TypeId, Expr)>>,

    /// Program-wide interface / resource operation signatures, declaring
    /// module → `(decl name, op name)` → `(param types, return type)`.
    /// Read by `resolve_effect_op_signature`.
    pub(crate) all_effect_op_sigs:
        Rc<IndexMap<ModuleSource, IndexMap<(String, String), (Vec<TypeId>, Option<TypeId>)>>>,

    /// Program-wide canonical free-function signatures
    /// ([`super::sem::decls::FunctionSig`]), declaring module → name.
    pub(crate) all_function_sigs:
        Rc<IndexMap<ModuleSource, Rc<IndexMap<String, super::sem::decls::FunctionSig>>>>,

    /// Program-wide global-variable declarations, declaring module →
    /// name → `(declared type, is_mut)`.
    pub(crate) all_globals: Rc<IndexMap<ModuleSource, IndexMap<String, (TypeId, bool)>>>,

    /// Per-module `__DATA__` section contents; modules without one have
    /// no entry.
    pub(crate) data_sections: Rc<IndexMap<ModuleSource, String>>,
}

impl TypeSystem {
    /// Canonical signature of the free function `name` declared in
    /// `module`.
    pub(crate) fn function_sig(
        &self,
        module: &ModuleSource,
        name: &str,
    ) -> Option<&super::sem::decls::FunctionSig> {
        self.all_function_sigs.get(module)?.get(name)
    }

    /// Check if a name refers to a known type (struct, variant, enum,
    /// flags, newtype, or primitive). Uses the pre-built cache for O(1)
    /// lookup instead of scanning all module maps.
    pub(crate) fn is_known_type_name(&self, name: &str) -> bool {
        self.known_type_names_cache.contains(name)
    }

    /// The `TypeId` of each field of the struct `(name, module)` in declaration
    /// order, or `None` if no such struct is registered. Used by the resource
    /// move check to decide whether an aggregate transitively owns a resource.
    pub(crate) fn struct_field_type_ids(
        &self,
        name: &str,
        module: &ModuleSource,
    ) -> Option<Vec<crate::tir::TypeId>> {
        let info = self.all_struct_fields.get(module)?.get(name)?;
        Some(info.fields.iter().map(|(_, tid, _)| *tid).collect())
    }

    /// Whether `name` resolves to a declared type *from the perspective of
    /// `module`* — i.e. a type that module can actually see (its own
    /// declarations, the auto-imported prelude, a primitive, or a type it
    /// explicitly imports). Unlike [`Self::is_known_type_name`], which is a
    /// global union, this is immune to pollution by unrelated modules: a
    /// user module declaring a type named `E` does not make `E` "known" in
    /// `core:prelude/types`, so the prelude's `impl Result<T, E>` keeps
    /// treating `E` as a free type parameter rather than a concrete
    /// instantiation argument.
    ///
    /// Falls back to the global cache when `module` is unknown (e.g. a
    /// synthetic source not present in the per-module map), which preserves
    /// the prior behaviour for those edge cases.
    pub(crate) fn is_known_type_name_in(&self, module: &ModuleSource, name: &str) -> bool {
        match self.module_visible_types.get(module) {
            Some(visible) => visible.contains(name),
            None => self.is_known_type_name(name),
        }
    }

    /// Check if an expression is a numeric literal (possibly negated).
    ///
    /// The non-numeric arms are enumerated explicitly rather than a
    /// catch-all `_` so that adding a new [`Expr`] variant produces a
    /// compile error here, forcing a deliberate decision about whether
    /// the new shape should participate in numeric-literal coercion.
    /// Whether `expr` is the bare `null` literal. A bare `null` initially
    /// resolves to `Option<UNKNOWN>` and only acquires its inner type from an
    /// expected-type context, so callers that can supply one (e.g. binary
    /// operands) check this to route the type through.
    pub(crate) fn is_null_literal(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Literal(lit) if matches!(lit.value, Literal::Null))
    }

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
            | Expr::LabeledBlock(_)
            | Expr::TryOp(_)
            | Expr::Spread(..)
            | Expr::Range(_)
            | Expr::WithHandler(_)
            | Expr::Resume(_)
            | Expr::Error(_) => false,
        }
    }

    /// Map a binary operator to its `(trait_name, method_name)` pair, or
    /// `None` for short-circuit operators that don't dispatch through a
    /// trait.
    ///
    /// For operators that dispatch through a [`CompilerItem`] trait
    /// (`Eq` for `==` / `!=`), the trait name is resolved through the
    /// compiler-item registry so a rename on the stdlib side stays
    /// transparent. For traits that don't yet have a `CompilerItem`
    /// anchor (`Add`, `Sub`, …, `Ord`), the canonical stdlib name is
    /// returned as a literal.
    ///
    /// The non-trait arms (`And` / `Or`) are explicit rather than a
    /// catch-all `_` so that adding a new [`BinaryOp`] variant produces
    /// a compile error here instead of silently returning `None`.
    pub(crate) fn operator_trait_method(&self, op: &BinaryOp) -> Option<(String, &'static str)> {
        match op {
            BinaryOp::Add => Some(("Add".to_string(), "add")),
            BinaryOp::Sub => Some(("Sub".to_string(), "sub")),
            BinaryOp::Mul => Some(("Mul".to_string(), "mul")),
            BinaryOp::Div => Some(("Div".to_string(), "div")),
            BinaryOp::Mod => Some(("Rem".to_string(), "rem")),
            BinaryOp::BitAnd => Some(("BitAnd".to_string(), "bitand")),
            BinaryOp::BitOr => Some(("BitOr".to_string(), "bitor")),
            BinaryOp::BitXor => Some(("BitXor".to_string(), "bitxor")),
            BinaryOp::Shl => Some(("Shl".to_string(), "shl")),
            BinaryOp::Shr => Some(("Shr".to_string(), "shr")),
            BinaryOp::Eq | BinaryOp::NotEq => Some((
                self.type_table
                    .borrow()
                    .compiler_trait_name(CompilerItem::Eq)
                    .to_string(),
                "eq",
            )),
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => Some((
                self.type_table
                    .borrow()
                    .compiler_trait_name(CompilerItem::Ord)
                    .to_string(),
                "cmp",
            )),
            // Logical `&&` / `||` short-circuit on `bool`; no trait dispatch.
            BinaryOp::And | BinaryOp::Or => None,
        }
    }
}

/// Pure type-shape helpers answerable from the type table alone (peel
/// references, extract a declared type's name, newtype-base resolution, type
/// stringification). They touch only `self.type_table`; the body walk and
/// reify both reach them through `self.tysys`.
impl TypeSystem {
    /// Build declared type params for an impl block, filtering out known type names.
    pub(crate) fn build_declared_type_params(&self, impl_block: &ImplBlock) -> IndexSet<String> {
        let mut declared: IndexSet<String> = impl_block
            .type_params
            .iter()
            .map(|p| p.name.clone())
            .filter(|name| !self.is_known_type_name(name))
            .collect();
        if let Type::Generic(g) = &impl_block.ty {
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
            ResolvedType::Struct { name, .. }
            | ResolvedType::GenericInstance { name, .. }
            | ResolvedType::Newtype { name, .. }
            | ResolvedType::Flags { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    /// For newtypes, get the base type name and ID for trait impl lookup fallback.
    /// Returns (`base_name`, `base_type_id`) if the type is a newtype; otherwise returns the same name/id.
    pub(crate) fn newtype_base_lookup(&self, name: &str, type_id: TypeId) -> (String, TypeId) {
        let tt = self.type_table.borrow();
        if let Some(base_id) = tt.get_newtype_base(type_id) {
            drop(tt);
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
                ResolvedType::Struct { name, .. } => return name,
                ResolvedType::GenericInstance { name, .. } => return name,
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
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
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
                            name,
                            module_source,
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
            ResolvedType::Struct { name, .. } => name,
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
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
            ResolvedType::Unit => "()".to_string(),
            ResolvedType::Never => "!".to_string(),
            ResolvedType::Unknown => "<unknown>".to_string(),
            ResolvedType::Error => "<error>".to_string(),
            _ => format!("{resolved:?}"),
        }
    }
}
