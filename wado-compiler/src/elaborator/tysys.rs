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
//! Three [`super::Elaborator`] fields are marked
//! `MIGRATION: → TypeSystem` but **stay on `Elaborator` through Stage 2**:
//! `indexing_trait_cache`, `method_info_cache`, `trait_check_stack`. They
//! carry per-Elaborator mutable state today (constructed fresh per
//! module, populated by the body walk), and moving them to a shared
//! `TypeSystem` requires either making them pipeline-wide caches (a
//! behaviour change) or interior-mutability plumbing. The migration
//! markers on those fields point at this future home; the move itself is
//! deferred to a later stage where the cache lifetime story is settled.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::builtin_registry::BuiltinRegistry;
use crate::component_model::WasiRegistry;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::tir::{TypeId, TypeTable};
use crate::world_registry::WorldRegistry;

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

    /// Registries.
    pub(crate) wasi_registry: &'static WasiRegistry,
    pub(crate) world_registry: &'static WorldRegistry,
    pub(crate) builtin_registry: Rc<BuiltinRegistry>,

    /// Pre-loaded file contents for `#include_str` / `#include_bytes`.
    /// Key: `[module_source_display, raw_path]`, value: raw bytes.
    pub(crate) included_files: Rc<IndexMap<[String; 2], Vec<u8>>>,

    /// Flat set of every name that resolves to a declared type
    /// (primitive, struct, enum, variant, flags, newtype, resource).
    /// Built globally during annotate; read-only afterwards. Powers fast
    /// `is_known_type_name` lookups in the body walk.
    pub(crate) known_type_names_cache: Rc<IndexSet<String>>,

    /// Per-module index from function name → position in `module.items`
    /// for O(1) lookup. Built globally during annotate; read-only
    /// afterwards.
    pub(crate) loaded_module_func_indices: Rc<IndexMap<ModuleSource, IndexMap<String, usize>>>,
}
