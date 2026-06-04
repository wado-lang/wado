//! [`TypeSystem`] — pipeline-wide type knowledge.
//!
//! Introduced by [`wep-2026-05-26-elaborator-rearchitecture.md`]. Stage 1
//! placed the empty skeleton; Stage 2 fills it with the cross-module type
//! tables, registries, and read-only caches that the WEP §"`TypeSystem`
//! surface" requires.
//!
//! # Ownership
//!
//! Every field is either `'static`, [`Arc`]-wrapped, or [`Rc`]-wrapped, so
//! `TypeSystem` is `Clone` (a shallow Rc/Arc copy) and can be handed out
//! cheaply to per-module phases. The driver builds one `TypeSystem`
//! during [`super::orchestration::Elaborator::annotate_modules`]; each
//! per-module [`super::Elaborator`] holds a clone in its
//! [`super::Elaborator::tysys`] field.
//!
//! # Membership rule
//!
//! A field belongs on `TypeSystem` only when the answer to
//! "would this fit the type system itself?" is yes. The criterion is
//! mechanical and gates drift back toward the God-Object pattern that
//! motivated the WEP.
//!
//! # Deferred fields
//!
//! Two [`super::Elaborator`] fields are marked
//! `MIGRATION: → TypeSystem` but **stay on `Elaborator` through Stage 2**:
//! `indexing_trait_cache` and `method_info_cache`. They are genuine
//! type-system caches (lookup `(TypeId, name) → MethodInfo` or
//! `(struct_name, base_type, trait, method, assoc_type) → impl info`)
//! whose keys live entirely in the shared type domain, so they belong on
//! `TypeSystem` in spirit. But they carry per-Elaborator mutable state
//! today (constructed fresh per module, populated by the body walk), and
//! moving them to a shared `TypeSystem` requires either making them
//! pipeline-wide caches (a behaviour change) or interior-mutability
//! plumbing. The migration markers on those fields point at this future
//! home; the move itself is deferred to a later stage where the cache
//! lifetime story is settled.
//!
//! [`super::Elaborator::trait_check_stack`] looks superficially similar
//! — `RefCell<Vec<…>>` mutable state on `Elaborator` — but is **not** a
//! cache. It is the per-call frame stack used by
//! `Elaborator::type_implements_trait` to break recursion on recursive
//! types (e.g. a variant case whose payload eventually contains itself):
//! frames are pushed on entry, popped on return, and the stack is empty
//! at every quiescent point. Moving it to a shared `TypeSystem` would
//! either leak stale frames across module walks (producing wrong
//! "recursive, optimistically true" answers in trait resolution — a
//! soundness bug) or require per-call save/restore plumbing that
//! defeats the move. The migration marker on that field accordingly
//! targets the same "transient annotate-time scope" bucket as
//! [`super::Elaborator::trait_ctx`], not `TypeSystem`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{BinaryOp, Expr, Literal, UnaryOp};
use crate::builtin_registry::BuiltinRegistry;
use crate::compiler_item::CompilerItem;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{TypeId, TypeTable};

use super::trait_env::TraitEnv;
use super::types::{
    EnumInfo, FlagsInfo, GenericNewtypeInfo, ResourceInfo, StructFieldInfo, VariantInfo,
};

/// Pipeline-wide type knowledge — the type arena, the cross-module decl
/// indices, the registries, and the read-only caches built once at
/// `annotate_modules` time.
///
/// See the module-level documentation for the membership rule and the
/// migration plan around the deferred `Elaborator` caches.
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
    pub(crate) cm_interface_registry: &'static CmInterfaceRegistry,
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
}

impl TypeSystem {
    /// Check if a name refers to a known type (struct, variant, enum,
    /// flags, newtype, or primitive). Uses the pre-built cache for O(1)
    /// lookup instead of scanning all module maps.
    pub(crate) fn is_known_type_name(&self, name: &str) -> bool {
        self.known_type_names_cache.contains(name)
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
                    .compiler_items()
                    .trait_name(CompilerItem::Eq)
                    .to_string(),
                "eq",
            )),
            BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => Some((
                self.type_table
                    .borrow()
                    .compiler_items()
                    .trait_name(CompilerItem::Ord)
                    .to_string(),
                "cmp",
            )),
            // Logical `&&` / `||` short-circuit on `bool`; no trait dispatch.
            BinaryOp::And | BinaryOp::Or => None,
        }
    }
}
