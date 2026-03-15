//! Global trait knowledge base: trait declarations, impl blocks, and blanket impls.
//!
//! `TraitEnv` is built once before resolution begins and is immutable thereafter.
//! It provides O(1) lookup of trait implementations by type name and trait name,
//! replacing linear scans across all modules.

use std::sync::Arc;

use crate::ast::{self, Item, Module};
use crate::hashmap::IndexMap;
use crate::name::ModuleSource;
use crate::tir::TypeId;

/// Pre-built index: type name → list of (`ModuleSource`, item index) for trait impl blocks.
/// Built once from all loaded modules to avoid O(all items) scans per method call.
pub(super) type TraitImplIndex = IndexMap<String, Vec<(ModuleSource, usize)>>;

/// Pre-built index: trait name → (`ModuleSource`, item index) for trait declarations.
pub(super) type TraitDeclIndex = IndexMap<String, (ModuleSource, usize)>;

/// Pre-built list of blanket trait impls: `impl<T: Trait> OtherTrait for T`.
/// These are impl blocks where the impl type is a free type parameter with trait bounds.
/// Stored separately because they can't be indexed by concrete type name.
pub(super) type BlanketTraitImplIndex = Vec<(ModuleSource, usize)>;

/// Immutable global knowledge base for trait resolution.
///
/// Contains pre-built indices for fast lookup of trait implementations,
/// trait declarations, and blanket impls. Built once before resolution
/// begins and shared (via `Arc`) across all module resolvers.
pub(super) struct TraitEnv {
    /// Type name → impl blocks that implement traits for that type.
    pub(super) impl_index: TraitImplIndex,
    /// Trait name → trait declaration location.
    pub(super) decl_index: TraitDeclIndex,
    /// Blanket impls (`impl<T: Bound> Trait for T`), checked as fallback.
    pub(super) blanket_impl_index: BlanketTraitImplIndex,
}

impl TraitEnv {
    /// Build trait indices from all loaded modules.
    ///
    /// Called once in `resolve_all_modules` before per-module resolution begins.
    /// The indices enable O(1) trait lookup by type/trait name instead of scanning all modules.
    pub(super) fn build(modules: &IndexMap<ModuleSource, Module>) -> Arc<Self> {
        let mut impl_index: TraitImplIndex = IndexMap::default();
        let mut decl_index: TraitDeclIndex = IndexMap::default();
        let mut blanket_impl_index: BlanketTraitImplIndex = Vec::new();

        for (module_source, module) in modules {
            for (item_idx, item) in module.items.iter().enumerate() {
                match item {
                    Item::Impl(impl_block) if impl_block.trait_type.is_some() => {
                        let type_name = get_type_name_static(&impl_block.ty);
                        // Detect blanket impls: impl_ty is a type parameter from type_params
                        let is_blanket = impl_block
                            .type_params
                            .iter()
                            .any(|tp| tp.name == type_name && !tp.bounds.is_empty());
                        if is_blanket {
                            blanket_impl_index.push((module_source.clone(), item_idx));
                        }
                        impl_index
                            .entry(type_name)
                            .or_default()
                            .push((module_source.clone(), item_idx));
                    }
                    Item::Trait(trait_decl) => {
                        decl_index
                            .entry(trait_decl.name.clone())
                            .or_insert((module_source.clone(), item_idx));
                    }
                    _ => {}
                }
            }
        }

        Arc::new(Self {
            impl_index,
            decl_index,
            blanket_impl_index,
        })
    }
}

/// Mutable trait resolution context scoped to the current resolution site.
///
/// Groups all state that changes when entering/leaving generic scopes
/// (impl blocks, trait method lookups, etc). By cloning this struct before
/// entering a scope and restoring it afterward, we avoid scattered save/restore
/// patterns and make the scope boundary explicit.
#[derive(Clone, Default)]
pub(super) struct TraitContext {
    /// Type parameters currently in scope (name → (index, `TypeId`)).
    /// Set when resolving generic structs, functions, or impl blocks.
    pub(super) type_params: IndexMap<String, (u32, TypeId)>,
    /// Trait bounds on type parameters in scope (name → full bounds with assoc types).
    /// Used for resolving trait methods on type params (e.g., `T.cmp()` when T: Ord).
    pub(super) type_param_bounds: IndexMap<String, Vec<ast::TraitBound>>,
    /// Associated type bindings in scope (`Self::Name` → resolved type).
    /// Set when resolving trait implementations.
    pub(super) assoc_type_bindings: IndexMap<String, TypeId>,
    /// Current `Self` type in scope (the type being implemented in an impl block).
    pub(super) self_type: Option<TypeId>,
}

/// Extract a type name from an AST type without needing a Resolver instance.
fn get_type_name_static(ty: &ast::Type) -> String {
    match ty {
        ast::Type::Named(named) => named.name.clone(),
        ast::Type::Generic(generic) => generic.name.clone(),
        ast::Type::Reference(_) => "&".to_string(),
        ast::Type::MutReference(_) => "&mut".to_string(),
        _ => "Unknown".to_string(),
    }
}
