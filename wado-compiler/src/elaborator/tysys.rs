//! [`TypeSystem`] — pipeline-wide type knowledge.
//!
//! Stage 1 skeleton introduced by
//! [`wep-2026-05-26-elaborator-rearchitecture.md`]. The type is empty at this
//! stage; the migration plan populates it across stages 2-7 by moving fields
//! off [`super::Elaborator`] and [`super::orchestration::AnnotateState`].
//!
//! # Future ownership and shape (per the WEP)
//!
//! `TypeSystem` is owned by the batch driver and by
//! [`crate::semantics::Semantics`]; per-module phases (`annotate_bodies`,
//! `reify`) take `&mut TypeSystem`. It does **not** know about
//! [`crate::ast::Module`], [`crate::ast::AstId`], or any
//! [`crate::module_source::ModuleSource`]-keyed per-module state — those
//! belong to [`super::sem::ModuleSemantics`].
//!
//! # Membership rule
//!
//! A field belongs on `TypeSystem` only when the answer to
//! "would this fit the type system itself?" is yes. The criterion is
//! mechanical and gates drift back toward the God-Object pattern that
//! motivated the WEP.
//!
//! # Planned contents
//!
//! - **Arenas / registries**: the shared [`crate::tir::TypeTable`], the
//!   [`super::trait_env::TraitEnv`], the builtin / WASI / world registries,
//!   the included-files map.
//! - **Decl-interned type tables**: the cross-module `all_newtypes`,
//!   `all_generic_newtypes`, `all_struct_fields`, `all_variant_cases`,
//!   `all_enum_cases`, `all_flags_cases`, `all_resource_types` maps.
//! - **Type-system caches**: `method_info_cache`, `indexing_trait_cache`,
//!   `trait_check_stack`, the global known-type-name set, the per-module
//!   function-index maps.
//! - **Operations**: interning, coercion, inference, type checking, trait
//!   queries, method lookup — taking only [`crate::tir::TypeId`]s and decl
//!   indices, never per-module flat maps.

#[expect(
    dead_code,
    reason = "Stage 1 skeleton; populated by stages 2-7 of the elaborator re-architecture."
)]
pub(crate) struct TypeSystem {}
