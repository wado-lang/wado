//! Multi-module resolution orchestration.
//!
//! Resolution runs in two phases:
//!
//! - [`Elaborator::annotate_modules`] collects decl-level type information
//!   (struct field maps, variant cases, flags, newtypes, resource methods)
//!   and interns every declaration in the shared [`TypeTable`]. It also
//!   populates [`TypeTable::type_by_symbol`]/[`TypeTable::symbol_by_type`]
//!   so LSP queries can resolve a [`SymbolKey`] to a decl-backed type
//!   without running TIR lowering. The output is an [`AnnotateState`] that
//!   both `build_tir` and the LSP consume.
//! - [`Elaborator::build_tir_from_state`] reads that state and produces one
//!   [`TirModule`] per source module. It also extends the per-module
//!   [`super::sem::ModuleSemantics`] entries on `state.module_semantics`
//!   with the body-walk results (use→def edges, local symbols, local
//!   types), and new types created during lowering (anonymous structs,
//!   monomorphic instances) are interned through the shared
//!   `Rc<RefCell<TypeTable>>`.
//!
//! This split keeps the annotate phase self-contained and cheap enough to
//! run on every `didChange` while reusing its results for the full
//! compilation pipeline.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, Item, Module, Type};
use crate::builtin_registry::BuiltinRegistry;
use crate::compiler_host::CompilerHost;
use crate::component_model::CmInterfaceRegistry;
use crate::logger::{Bail, Logger};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::{self as name};
use crate::symbol::SymbolTable;
use crate::tir::{ResolvedType, TirModule, TypeId, TypeTable};
use crate::world_registry::WorldRegistry;

use super::Elaborator;
use super::trait_env::TraitEnv;
use super::types::{
    EnumCaseData, EnumInfo, FlagsInfo, FlagsMemberData, GenericNewtypeInfo, ResourceInfo,
    StructFieldInfo, TypeError, TypeLookup, VariantCaseData, VariantInfo,
};
use super::tysys::TypeSystem;

/// Analysis state produced by [`Elaborator::annotate_modules`] and consumed by
/// [`Elaborator::build_tir_from_state`].
///
/// The shared [`TypeSystem`] internals (type arena, decl maps, registries)
/// are reference-counted so per-module elaborators can clone them cheaply.
/// The [`TypeTable`] itself remains behind `Rc<RefCell<…>>` because lowering
/// interns additional types (anonymous structs, monomorphic instances) into
/// the same table.
///
/// Per-module semantic facts (use→def edges, local symbols, decl tables,
/// import context) live in [`Self::module_semantics`] as a `IndexMap` of
/// [`super::sem::ModuleSemantics`]. Each entry is owned by exactly one
/// place at a time — the driver hands the entry to the body walk via
/// `swap_remove` + `insert`, so no shared-mutability plumbing is needed
/// (WEP 2026-05-26, Stage 3).
///
/// Migration markers on each field below trace the WEP 2026-05-26
/// elaborator re-architecture. `AnnotateState` itself disappears after
/// Stage 7 — its contents redistribute into [`super::tysys::TypeSystem`]
/// (pipeline-wide), per-module [`super::sem::ModuleSemantics`] instances,
/// and cross-cutting driver state.
pub(crate) struct AnnotateState {
    /// Pipeline-wide type knowledge: the type arena, decl-interned type
    /// tables, registries, included-files map, and read-only caches
    /// built once at annotate time. See [`TypeSystem`].
    pub(crate) tysys: TypeSystem,
    /// Component-Model world specifications. Built by the same
    /// [`CmInterfaceRegistry::build_from_stdlib`] call that populates
    /// [`TypeSystem::cm_interface_registry`], but lives here rather than on
    /// `TypeSystem`: the elaborator never asks "what does world X
    /// export?" — only post-elaborator stages (link, synthesis, DCE,
    /// world-existence validation in [`crate::lib`]) do. Keeping it on
    /// the driver state respects the `TypeSystem` membership rule
    /// ("would this fit the type system itself?" — see
    /// [`super::tysys`] module docs).
    pub(crate) world_registry: &'static WorldRegistry,
    /// Topological order of modules; the per-module body walk in
    /// [`Elaborator::build_tir_from_state`] visits sources in this order
    /// so a `TirModule`'s position in the result map matches the
    /// dependency order downstream phases expect.
    pub(crate) sorted_sources: Vec<ModuleSource>,
    /// Per-module semantic facts produced by the elaborator. One entry per
    /// loaded module. Stdlib entries are seeded from the snapshot in
    /// [`Self::annotate_modules`]; per-compile entries are produced by the
    /// body walk in [`Self::build_tir_from_state`].
    ///
    /// Replaces the previous trio of `Rc<RefCell<…>>` maps
    /// (`references` / `local_symbols` / `local_types`) — each module's
    /// data now lives in its own owned [`super::sem::ModuleSemantics`], so
    /// the body walk's `&mut` access stays disjoint across modules and the
    /// shared-mutability plumbing disappears (WEP 2026-05-26, Stage 3).
    pub(crate) module_semantics: IndexMap<ModuleSource, super::sem::ModuleSemantics>,
    /// Kiln invocation redirects consulted by `resolve_import` call sites
    /// when walking `use` declarations. Populated from [`crate::loader::LoadResult`].
    // MIGRATION: cross-cutting input (loader-provided redirect map).
    pub(crate) invocations: Rc<crate::kiln::InvocationIndex>,
    /// `ModuleSource` interner shared across phases. `Rc<RefCell<>>` so
    /// `&self` elaborator methods can `borrow_mut()` it when constructing
    /// new module sources during name resolution.
    // MIGRATION: cross-cutting input (shared interner).
    pub(crate) interner: Rc<RefCell<ModuleSourceInterner>>,
}

impl<'a, H: CompilerHost> Elaborator<'a, H> {
    /// Run the full resolve pipeline: annotate, then lower to TIR.
    ///
    /// This is a thin wrapper over [`Elaborator::annotate_modules`] +
    /// [`Elaborator::build_tir_from_state`]. Callers that want access to the
    /// annotate output (e.g. LSP) should call the two phases separately.
    pub(crate) fn elaborate_all_modules(
        symbols: &'a SymbolTable,
        modules: &'a IndexMap<ModuleSource, Module>,
        entry_module_source: ModuleSource,
        logger: &'a Logger<'a, H>,
        included_files: Rc<IndexMap<[String; 2], Vec<u8>>>,
        invocations: crate::kiln::InvocationIndex,
        interner: Rc<RefCell<ModuleSourceInterner>>,
    ) -> Result<(IndexMap<ModuleSource, TirModule>, Arc<TraitEnv>), Bail> {
        let mut state = Self::annotate_modules(
            symbols,
            modules,
            &entry_module_source,
            logger,
            included_files,
            invocations,
            interner,
            None,
        )?;
        let trait_env = state.tysys.trait_env.clone();
        let tir_modules = Self::build_tir_from_state(
            &mut state,
            symbols,
            modules,
            entry_module_source,
            logger,
            None,
        )?;
        Ok((tir_modules, trait_env))
    }

    /// Annotate phase: collect decl-level type information and intern every
    /// declaration in the shared [`TypeTable`]. Produces an [`AnnotateState`]
    /// that downstream phases (`build_tir`, LSP queries) consume read-mostly
    /// via `Rc`.
    pub(crate) fn annotate_modules(
        symbols: &'a SymbolTable,
        modules: &'a IndexMap<ModuleSource, Module>,
        entry_module_source: &ModuleSource,
        logger: &'a Logger<'a, H>,
        included_files: Rc<IndexMap<[String; 2], Vec<u8>>>,
        invocations: crate::kiln::InvocationIndex,
        interner: Rc<RefCell<ModuleSourceInterner>>,
        snapshot: Option<&crate::semantics::Semantics>,
    ) -> Result<AnnotateState, Bail> {
        let invocations = Rc::new(invocations);
        // Set of stdlib module sources covered by the snapshot.  When non-empty,
        // the per-module passes below skip these — their decl info is already
        // present in the seeded maps.
        let stdlib_set: IndexSet<ModuleSource> = snapshot
            .map(crate::stdlib_snapshot::stdlib_sources)
            .unwrap_or_default();
        // Seed the shared type table from the snapshot when available so
        // stdlib `TypeId`s occupy the same indices as in cached `TirModule`s.
        let type_table = Rc::new(RefCell::new(
            snapshot.map_or_else(TypeTable::new, |s| s.types.clone()),
        ));
        // The stdlib snapshot is built by `stdlib_snapshot::build_snapshot`
        // which asserts `is_complete()`, so `state` is always populated when
        // a snapshot is handed in here.
        let snapshot_state = snapshot.map(|s| {
            s.state
                .as_ref()
                .expect("stdlib snapshot must have a populated annotate state")
        });
        let mut all_newtypes: IndexMap<ModuleSource, IndexMap<String, TypeId>> = snapshot_state
            .map(|s| (*s.tysys.all_newtypes).clone())
            .unwrap_or_default();
        let mut all_generic_newtypes: IndexMap<ModuleSource, IndexMap<String, GenericNewtypeInfo>> =
            snapshot_state
                .map(|s| (*s.tysys.all_generic_newtypes).clone())
                .unwrap_or_default();
        let mut all_struct_fields: IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>> =
            snapshot_state
                .map(|s| (*s.tysys.all_struct_fields).clone())
                .unwrap_or_default();
        let mut all_variant_cases: IndexMap<ModuleSource, IndexMap<String, VariantInfo>> =
            snapshot_state
                .map(|s| (*s.tysys.all_variant_cases).clone())
                .unwrap_or_default();
        let mut all_enum_cases: IndexMap<ModuleSource, IndexMap<String, EnumInfo>> = snapshot_state
            .map(|s| (*s.tysys.all_enum_cases).clone())
            .unwrap_or_default();
        let mut all_flags_cases: IndexMap<ModuleSource, IndexMap<String, FlagsInfo>> =
            snapshot_state
                .map(|s| (*s.tysys.all_flags_cases).clone())
                .unwrap_or_default();
        let mut all_resource_types: IndexMap<ModuleSource, IndexMap<String, ResourceInfo>> =
            snapshot_state
                .map(|s| (*s.tysys.all_resource_types).clone())
                .unwrap_or_default();

        // First pass: collect struct, variant, enum, and resource names from all modules (for forward references)
        for (module_source, module) in modules {
            if stdlib_set.contains(module_source) {
                // Already covered by the snapshot seed above.
                continue;
            }
            for item in &module.items {
                match item {
                    Item::Struct(struct_decl) => {
                        // Insert with empty fields first - will be populated in second sub-pass
                        // Extract type parameter bounds
                        let type_param_bounds: Vec<(String, Vec<String>)> = struct_decl
                            .type_params
                            .iter()
                            .map(|p| {
                                (
                                    p.name.clone(),
                                    p.bounds.iter().map(|b| b.name.clone()).collect(),
                                )
                            })
                            .collect();
                        all_struct_fields
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                struct_decl.name.clone(),
                                StructFieldInfo {
                                    name: struct_decl.name.clone(),
                                    module_source: module_source.clone(),
                                    fields: Vec::new(),
                                    field_ast_ids: Vec::new(),
                                    field_defaults: Vec::new(),
                                    type_param_bounds,
                                    type_param_type_ids: Vec::new(), // filled in second pass
                                },
                            );
                        super::item::register_struct_compiler_item(
                            &type_table,
                            &struct_decl.attrs,
                            &struct_decl.name,
                            module_source,
                            struct_decl.span,
                            logger,
                        );
                    }
                    Item::Variant(variant_decl) => {
                        // Insert with empty cases first - will be populated in second sub-pass
                        let type_params: Vec<String> = variant_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect();
                        all_variant_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                variant_decl.name.clone(),
                                VariantInfo {
                                    name: variant_decl.name.clone(),
                                    module_source: module_source.clone(),
                                    type_params,
                                    cases: Vec::new(),
                                    type_param_type_ids: Vec::new(),
                                },
                            );
                        super::item::register_variant_compiler_item(
                            &type_table,
                            &variant_decl.attrs,
                            &variant_decl.name,
                            module_source,
                            variant_decl.span,
                            logger,
                        );
                        for (case_index, case) in variant_decl.cases.iter().enumerate() {
                            super::item::register_variant_case_compiler_item(
                                &type_table,
                                &case.attrs,
                                &variant_decl.name,
                                &case.name,
                                case_index as u32,
                                module_source,
                                case.span,
                                logger,
                            );
                        }
                    }
                    Item::Enum(enum_decl) => {
                        // Insert with empty cases first - will be populated in second sub-pass
                        all_enum_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                enum_decl.name.clone(),
                                EnumInfo::new(
                                    enum_decl.name.clone(),
                                    module_source.clone(),
                                    Vec::new(),
                                ),
                            );
                        super::item::register_enum_compiler_item(
                            &type_table,
                            &enum_decl.attrs,
                            &enum_decl.name,
                            module_source,
                            enum_decl.span,
                            logger,
                        );
                        for (case_index, case) in enum_decl.cases.iter().enumerate() {
                            super::item::register_enum_case_compiler_item(
                                &type_table,
                                &case.attrs,
                                &enum_decl.name,
                                &case.name,
                                case_index as u32,
                                module_source,
                                case.span,
                                logger,
                            );
                        }
                    }
                    Item::Resource(resource_decl) => {
                        all_resource_types
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                resource_decl.name.clone(),
                                ResourceInfo {
                                    name: resource_decl.name.clone(),
                                    module_source: module_source.clone(),
                                    methods: resource_decl
                                        .methods
                                        .iter()
                                        .map(|m| m.name.clone())
                                        .collect(),
                                },
                            );
                    }
                    Item::Trait(trait_decl) => {
                        super::item::register_trait_compiler_item(
                            &type_table,
                            &trait_decl.attrs,
                            &trait_decl.name,
                            &trait_decl.methods,
                            &trait_decl.associated_types,
                            module_source,
                            trait_decl.span,
                            logger,
                        );
                    }
                    Item::TupleTypeDecl(decl) => {
                        super::item::register_tuple_compiler_item(
                            &type_table,
                            &decl.attrs,
                            module_source,
                            decl.span,
                            logger,
                        );
                    }
                    _ => {}
                }
            }
        }

        // Newtype pre-pass: resolve every module's concrete newtypes (and
        // record generic-newtype defs) BEFORE any struct field is resolved.
        //
        // Newtypes are not name-registered in the first pass (unlike structs
        // / variants / enums), so a struct field that references a newtype
        // defined in another module would otherwise resolve to `unknown`
        // whenever that module is processed later in `modules` order — e.g. a
        // user `struct Color { v: f32x4 }` resolved before `core:simd`'s
        // `pub type f32x4 = v128` is registered. Resolving all newtypes up
        // front removes the ordering dependency. Newtype bases reference
        // primitives or structs (name-registered above); newtype→newtype
        // chains across modules are resolved in dependency order by repeating
        // to a fixpoint.
        loop {
            let mut newly_resolved = false;
            for (module_source, module) in modules {
                if stdlib_set.contains(module_source) {
                    continue;
                }
                let (imported_type_sources, import_original_names) =
                    Self::build_imported_type_sources(
                        &mut interner.borrow_mut(),
                        module,
                        module_source,
                        Some(entry_module_source),
                        &invocations,
                    );
                let empty_struct: IndexMap<String, StructFieldInfo> = IndexMap::default();
                let empty_newtype: IndexMap<String, TypeId> = IndexMap::default();
                let empty_enum: IndexMap<String, EnumInfo> = IndexMap::default();
                let empty_flags: IndexMap<String, FlagsInfo> = IndexMap::default();
                let empty_gnt: IndexMap<String, GenericNewtypeInfo> = IndexMap::default();
                let empty_variant: IndexMap<String, VariantInfo> = IndexMap::default();
                for item in &module.items {
                    let Item::Newtype(newtype_decl) = item else {
                        continue;
                    };
                    if newtype_decl.type_params.is_empty() {
                        // Skip if already resolved (fixpoint convergence).
                        if all_newtypes
                            .get(module_source)
                            .is_some_and(|m| m.contains_key(&newtype_decl.name))
                        {
                            continue;
                        }
                        let lookup = TypeLookup {
                            current_module_source: module_source,
                            imported_type_sources: &imported_type_sources,
                            import_original_names: &import_original_names,
                            all_newtypes: &all_newtypes,
                            all_struct_fields: &all_struct_fields,
                            all_variant_cases: &all_variant_cases,
                            all_enum_cases: &all_enum_cases,
                            all_flags_cases: &all_flags_cases,
                            all_resource_types: &all_resource_types,
                            all_generic_newtypes: &all_generic_newtypes,
                            local_struct_fields: &empty_struct,
                            local_newtypes: &empty_newtype,
                            local_enum_cases: &empty_enum,
                            local_flags_cases: &empty_flags,
                            local_generic_newtypes: &empty_gnt,
                            local_variant_cases: &empty_variant,
                        };
                        let base_type_id = Self::resolve_type_static(
                            &newtype_decl.ty,
                            &mut type_table.borrow_mut(),
                            &lookup,
                        );
                        let newtype_id = type_table.borrow_mut().make_newtype(
                            newtype_decl.name.clone(),
                            module_source.clone(),
                            base_type_id,
                        );
                        all_newtypes
                            .entry(module_source.clone())
                            .or_default()
                            .insert(newtype_decl.name.clone(), newtype_id);
                        newly_resolved = true;
                    } else if !all_generic_newtypes
                        .get(module_source)
                        .is_some_and(|m| m.contains_key(&newtype_decl.name))
                    {
                        let type_params = newtype_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect();
                        all_generic_newtypes
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                newtype_decl.name.clone(),
                                GenericNewtypeInfo {
                                    module_source: module_source.clone(),
                                    type_params,
                                    base_type_ast: newtype_decl.ty.clone(),
                                },
                            );
                        newly_resolved = true;
                    }
                }
            }
            if !newly_resolved {
                break;
            }
        }

        // Second sub-pass: resolve struct fields and newtypes.
        // Each module's lookup goes directly through the in-progress shared
        // tables (`all_*`) via [`TypeLookup`] — no per-module flat-map cloning.
        for (module_source, module) in modules {
            if stdlib_set.contains(module_source) {
                // Stdlib fields are already resolved in the seeded maps.
                continue;
            }
            let (imported_type_sources, import_original_names) = Self::build_imported_type_sources(
                &mut interner.borrow_mut(),
                module,
                module_source,
                Some(entry_module_source),
                &invocations,
            );

            // Helper closure: build a fresh TypeLookup pointed at the
            // current state of the shared tables. Recreated per call site so
            // that the previous borrow is released before each `borrow_mut()`
            // on `type_table`.
            let empty_struct: IndexMap<String, StructFieldInfo> = IndexMap::default();
            let empty_newtype: IndexMap<String, TypeId> = IndexMap::default();
            let empty_enum: IndexMap<String, EnumInfo> = IndexMap::default();
            let empty_flags: IndexMap<String, FlagsInfo> = IndexMap::default();
            let empty_gnt: IndexMap<String, GenericNewtypeInfo> = IndexMap::default();
            let empty_variant: IndexMap<String, VariantInfo> = IndexMap::default();

            for item in &module.items {
                let lookup = TypeLookup {
                    current_module_source: module_source,
                    imported_type_sources: &imported_type_sources,
                    import_original_names: &import_original_names,
                    all_newtypes: &all_newtypes,
                    all_struct_fields: &all_struct_fields,
                    all_variant_cases: &all_variant_cases,
                    all_enum_cases: &all_enum_cases,
                    all_flags_cases: &all_flags_cases,
                    all_resource_types: &all_resource_types,
                    all_generic_newtypes: &all_generic_newtypes,
                    local_struct_fields: &empty_struct,
                    local_newtypes: &empty_newtype,
                    local_enum_cases: &empty_enum,
                    local_flags_cases: &empty_flags,
                    local_generic_newtypes: &empty_gnt,
                    local_variant_cases: &empty_variant,
                };
                match item {
                    Item::Struct(struct_decl) => {
                        let mut fields = Vec::new();
                        let mut field_ast_ids = Vec::new();
                        let mut field_defaults: Vec<Option<ast::Expr>> = Vec::new();
                        // Extract type parameter names for generic structs
                        let type_params: Vec<String> = struct_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect();
                        for field in &struct_decl.fields {
                            // Use resolve_type_static_with_params for generic structs
                            // so that type params like K in Node<K> become TypeParam types
                            let type_id = if type_params.is_empty() {
                                Self::resolve_type_static(
                                    &field.ty,
                                    &mut type_table.borrow_mut(),
                                    &lookup,
                                )
                            } else {
                                Self::resolve_type_static_with_params(
                                    &field.ty,
                                    &mut type_table.borrow_mut(),
                                    &lookup,
                                    &type_params,
                                )
                            };
                            fields.push((field.name.clone(), type_id, field.is_pub));
                            field_ast_ids.push(field.id);
                            field_defaults.push(field.default.clone());
                        }
                        // Extract type parameter bounds
                        let type_param_bounds: Vec<(String, Vec<String>)> = struct_decl
                            .type_params
                            .iter()
                            .map(|p| {
                                (
                                    p.name.clone(),
                                    p.bounds.iter().map(|b| b.name.clone()).collect(),
                                )
                            })
                            .collect();
                        // Collect TypeIds for struct's own type params in declaration order.
                        // This allows infer_struct_type_args to fill phantom type params
                        // that don't appear in any field (e.g., D in struct DirMap<D, V>).
                        let type_param_type_ids: Vec<TypeId> = type_params
                            .iter()
                            .enumerate()
                            .map(|(i, name)| {
                                type_table
                                    .borrow_mut()
                                    .make_type_param(name.clone(), i as u32)
                            })
                            .collect();

                        // Drop lookup so we can mutate `all_struct_fields`.

                        // Update the nested map entry with actual fields. The
                        // next iteration's `lookup` will see the new entry via
                        // the "current module" path, so no flat-map echo is
                        // needed.
                        let info = StructFieldInfo {
                            name: struct_decl.name.clone(),
                            module_source: module_source.clone(),
                            fields,
                            field_ast_ids,
                            field_defaults,
                            type_param_bounds,
                            type_param_type_ids,
                        };
                        all_struct_fields
                            .entry(module_source.clone())
                            .or_default()
                            .insert(struct_decl.name.clone(), info);
                    }
                    Item::Newtype(newtype_decl) => {
                        if newtype_decl.type_params.is_empty() {
                            // Concrete newtype: resolve immediately
                            let base_type_id = Self::resolve_type_static(
                                &newtype_decl.ty,
                                &mut type_table.borrow_mut(),
                                &lookup,
                            );
                            let newtype_id = type_table.borrow_mut().make_newtype(
                                newtype_decl.name.clone(),
                                module_source.clone(),
                                base_type_id,
                            );
                            all_newtypes
                                .entry(module_source.clone())
                                .or_default()
                                .insert(newtype_decl.name.clone(), newtype_id);
                        } else {
                            // Generic newtype: store definition for lazy instantiation
                            let type_params = newtype_decl
                                .type_params
                                .iter()
                                .map(|p| p.name.clone())
                                .collect();
                            let info = GenericNewtypeInfo {
                                module_source: module_source.clone(),
                                type_params,
                                base_type_ast: newtype_decl.ty.clone(),
                            };
                            all_generic_newtypes
                                .entry(module_source.clone())
                                .or_default()
                                .insert(newtype_decl.name.clone(), info);
                        }
                    }
                    Item::Variant(variant_decl) => {
                        // Resolve variant case field types
                        let type_params: Vec<String> = variant_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect();
                        let mut cases = Vec::new();
                        for case in &variant_decl.cases {
                            // Each variant case has exactly one payload type.
                            // Unit variants have `()` (unit type) payload.
                            let payload = if let Some(payload_ty) = &case.payload {
                                // Use resolve_type_static_with_params for variant payloads
                                // so that type params like T in Ok(T) become TypeParam types
                                Self::resolve_type_static_with_params(
                                    payload_ty,
                                    &mut type_table.borrow_mut(),
                                    &lookup,
                                    &type_params,
                                )
                            } else {
                                // Unit variant: payload is unit type
                                TypeTable::UNIT
                            };
                            cases.push(VariantCaseData {
                                name: case.name.clone(),
                                payload,
                                ast_id: case.id,
                            });
                        }
                        let type_param_type_ids: Vec<TypeId> = type_params
                            .iter()
                            .enumerate()
                            .map(|(i, name)| {
                                type_table
                                    .borrow_mut()
                                    .make_type_param(name.clone(), i as u32)
                            })
                            .collect();
                        all_variant_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                variant_decl.name.clone(),
                                VariantInfo {
                                    name: variant_decl.name.clone(),
                                    module_source: module_source.clone(),
                                    type_params,
                                    cases,
                                    type_param_type_ids,
                                },
                            );
                    }
                    Item::Enum(enum_decl) => {
                        // Populate enum cases (no field types, just names and indices)
                        let cases: Vec<EnumCaseData> = enum_decl
                            .cases
                            .iter()
                            .enumerate()
                            .map(|(index, case)| EnumCaseData {
                                name: case.name.clone(),
                                index: index as u32,
                                ast_id: case.id,
                            })
                            .collect();
                        all_enum_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                enum_decl.name.clone(),
                                EnumInfo::new(enum_decl.name.clone(), module_source.clone(), cases),
                            );
                    }
                    Item::Flags(flags_decl) => {
                        // Create a distinct Flags type (not a newtype over u32)
                        let flags_type = type_table
                            .borrow_mut()
                            .make_flags(flags_decl.name.clone(), module_source.clone());
                        // Add to newtypes so it can be used as a type name in signatures
                        all_newtypes
                            .entry(module_source.clone())
                            .or_default()
                            .insert(flags_decl.name.clone(), flags_type);
                        // Store member info with bitmask values (1 << index)
                        let members: Vec<FlagsMemberData> = flags_decl
                            .flags
                            .iter()
                            .enumerate()
                            .map(|(i, m)| FlagsMemberData {
                                name: m.name.clone(),
                                bitmask: 1u32 << i,
                                ast_id: m.id,
                            })
                            .collect();
                        all_flags_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                flags_decl.name.clone(),
                                FlagsInfo {
                                    type_id: flags_type,
                                    members,
                                    module_source: module_source.clone(),
                                },
                            );
                    }
                    _ => {}
                }
            }
        }

        // Topologically sort modules based on struct field type dependencies
        // A module depends on another if it has a struct with a field of a type defined there
        let sorted_sources =
            Self::topological_sort_modules(modules, &all_struct_fields, &type_table.borrow());

        let (cm_interface_registry, world_registry) = {
            let _span = logger.span("elaborate/cm_interface_registry");
            CmInterfaceRegistry::build_from_stdlib()
        };
        let builtin_registry = {
            let _span = logger.span("elaborate/builtin_registry");
            let mut registry = if let Some(snap_state) = snapshot_state {
                // The snapshot's registry is bound to the snapshot's
                // `TypeTable`, whose entries we cloned into `type_table`
                // above — so the registered `TypeId`s remain valid.
                (*snap_state.tysys.builtin_registry).clone()
            } else {
                BuiltinRegistry::build_from_stdlib(&type_table)
            };
            // Fold in `#[canonical(...)]` no-body declarations from
            // loader-synthesised wasm-asset modules so calls into a
            // wat/wasm asset's exports lower through the same TirImport
            // path as `core:builtin` declarations.  Stdlib wasm assets
            // (e.g. `core:libm.wat`) are already in
            // `snapshot.state.tysys.builtin_registry`.
            for (ms, module) in modules {
                if matches!(ms, ModuleSource::Wasm { .. }) && !stdlib_set.contains(ms) {
                    registry.register_wasm_module(module, &type_table);
                }
            }
            registry
        };

        // Build trait lookup indices once for all modules.
        // This allows find_trait_method_for_type and find_indexing_trait_impl to do O(1)
        // lookups by type name instead of scanning all items in all modules per method call.
        // Also runs orphan rule checking; violations are emitted as errors.
        let (trait_env, orphan_violations) = {
            let _span = logger.span("elaborate/trait_env");
            super::trait_env::TraitEnv::build(modules, symbols)
        };
        for violation in orphan_violations {
            let _ = logger.error(violation);
        }

        // Pre-pass: register generic associated type defs from ALL modules before any module
        // is resolved. This ensures that when resolving module X, it can look up associated
        // types from module Y's impl blocks even if Y hasn't been processed yet in the main
        // second pass (e.g., user module is sorted before prelude modules).
        Self::register_all_generic_assoc_type_defs(modules, &type_table, &stdlib_set);

        // Wrap all_* maps in Rc for cheap sharing across per-module elaborators
        let all_newtypes = Rc::new(all_newtypes);
        let all_struct_fields = Rc::new(all_struct_fields);
        let all_variant_cases = Rc::new(all_variant_cases);
        let all_enum_cases = Rc::new(all_enum_cases);
        let all_flags_cases = Rc::new(all_flags_cases);
        let all_resource_types = Rc::new(all_resource_types);
        let all_generic_newtypes = Rc::new(all_generic_newtypes);

        // Pre-compute the global known type names cache once (shared across all modules)
        let known_type_names_cache = {
            let mut cache = IndexSet::default();
            for m in all_struct_fields.values() {
                for name in m.keys() {
                    cache.insert(name.clone());
                }
            }
            for m in all_variant_cases.values() {
                for name in m.keys() {
                    cache.insert(name.clone());
                }
            }
            for m in all_enum_cases.values() {
                for name in m.keys() {
                    cache.insert(name.clone());
                }
            }
            for m in all_flags_cases.values() {
                for name in m.keys() {
                    cache.insert(name.clone());
                }
            }
            for m in all_newtypes.values() {
                for name in m.keys() {
                    cache.insert(name.clone());
                }
            }
            for m in all_generic_newtypes.values() {
                for name in m.keys() {
                    cache.insert(name.clone());
                }
            }
            for name in crate::tir::PrimitiveType::all_primitive_names() {
                cache.insert(name.to_string());
            }
            cache
        };

        // Validate type names in struct fields, variant payloads, and newtype definitions.
        // At this point all type names from all modules are known, so any unrecognized
        // Named type is truly undefined. This catches undefined types that would silently
        // become UNKNOWN in static pre-resolution.
        // Resource type names are kept separate from known_type_names_cache because
        // adding them would break is_known_type_name() used in impl block type parameter
        // inference (e.g., `impl Request { ... }` would stop recognizing Request's methods).
        let resource_type_names: IndexSet<String> = all_resource_types
            .values()
            .flat_map(|m| m.keys().cloned())
            .collect();
        Self::validate_type_definitions(
            modules,
            &known_type_names_cache,
            &resource_type_names,
            logger,
            &stdlib_set,
        )?;

        // Pre-build function name → index maps for all loaded modules (O(1) lookup)
        let loaded_module_func_indices: IndexMap<ModuleSource, IndexMap<String, usize>> = {
            let mut indices: IndexMap<ModuleSource, IndexMap<String, usize>> = snapshot_state
                .map(|s| (*s.tysys.loaded_module_func_indices).clone())
                .unwrap_or_default();
            for (src, module) in modules {
                if stdlib_set.contains(src) {
                    // Pre-populated from snapshot.
                    continue;
                }
                indices.insert(src.clone(), super::build_func_index(&module.items));
            }
            indices
        };

        // Intern every declaration in the TypeTable so `find_decl_type_by_name`
        // (used by `register_symbol_key_type_indices` below) resolves for every
        // symbol, including types that aren't referenced as a field anywhere.
        // Without this, decl-only types (e.g., a standalone `struct Unused {}`)
        // would only appear in the table after TIR lowering — too late for the
        // annotate phase to index them by `SymbolKey`.
        Self::intern_all_decl_types(
            modules,
            &all_struct_fields,
            &all_resource_types,
            &type_table,
            &stdlib_set,
        );

        // Populate `TypeTable::type_by_symbol` / `symbol_by_type` so LSP queries
        // can resolve a `SymbolKey` to a decl-backed type without running the
        // lower phase.
        Self::register_symbol_key_type_indices(symbols, &type_table);

        // Seed per-module semantics with the snapshot's pre-resolved stdlib
        // entries so the LSP edges remain consistent and the body walk on
        // user modules can extend on top. The snapshot stores `references` /
        // `locals` / `local_types` as flat maps keyed by `SymbolKey`; we
        // split them by `key.module` because each module's data now lives in
        // its own [`super::sem::ModuleSemantics`].
        let mut module_semantics: IndexMap<ModuleSource, super::sem::ModuleSemantics> =
            IndexMap::default();
        // Ensure every loaded module has an entry; the body walk in
        // `build_tir_from_state` requires a `ModuleSemantics` to swap in
        // for each user module it processes.
        for ms in modules.keys() {
            module_semantics.entry(ms.clone()).or_default();
        }
        if let Some(snap) = snapshot {
            // Snapshot keys must always reference modules in the current
            // compile's `modules` set — the loader's implicit-modules pass
            // pulls in the snapshot's stdlib closure on every compile. Use
            // `get_mut` so a divergence shows up as a `debug_assert` failure
            // rather than silently creating phantom `ModuleSemantics` entries
            // that LSP would later flatten into `Semantics::references`.
            let snapshot_invariant = "snapshot module must be in the current compile's loaded set";
            for (use_key, def_key) in &snap.references {
                let Some(sem) = module_semantics.get_mut(&use_key.module) else {
                    debug_assert!(false, "{snapshot_invariant}: {:?}", use_key.module);
                    continue;
                };
                sem.bindings
                    .references
                    .insert(use_key.clone(), def_key.clone());
            }
            for (key, sym) in &snap.locals {
                let Some(sem) = module_semantics.get_mut(&key.module) else {
                    debug_assert!(false, "{snapshot_invariant}: {:?}", key.module);
                    continue;
                };
                sem.bindings.local_symbols.insert(key.clone(), sym.clone());
            }
            for (key, type_id) in &snap.local_types {
                let Some(sem) = module_semantics.get_mut(&key.module) else {
                    debug_assert!(false, "{snapshot_invariant}: {:?}", key.module);
                    continue;
                };
                sem.types.local_types.insert(key.clone(), *type_id);
            }
            for (key, type_id) in &snap.expression_types {
                let Some(sem) = module_semantics.get_mut(&key.module) else {
                    debug_assert!(false, "{snapshot_invariant}: {:?}", key.module);
                    continue;
                };
                sem.types.expression_types.insert(key.clone(), *type_id);
            }
            for (key, dispatch) in &snap.method_dispatch {
                let Some(sem) = module_semantics.get_mut(&key.module) else {
                    debug_assert!(false, "{snapshot_invariant}: {:?}", key.module);
                    continue;
                };
                sem.types
                    .method_dispatch
                    .insert(key.clone(), dispatch.clone());
            }
            for (key, choice) in &snap.coercions {
                let Some(sem) = module_semantics.get_mut(&key.module) else {
                    debug_assert!(false, "{snapshot_invariant}: {:?}", key.module);
                    continue;
                };
                sem.types.coercions.insert(key.clone(), choice.clone());
            }
            for (key, kind) in &snap.desugars {
                let Some(sem) = module_semantics.get_mut(&key.module) else {
                    debug_assert!(false, "{snapshot_invariant}: {:?}", key.module);
                    continue;
                };
                sem.types.desugars.insert(key.clone(), *kind);
            }
        }

        let tysys = TypeSystem {
            type_table,
            all_newtypes,
            all_generic_newtypes,
            all_struct_fields,
            all_variant_cases,
            all_enum_cases,
            all_flags_cases,
            all_resource_types,
            trait_env,
            cm_interface_registry,
            builtin_registry: Rc::new(builtin_registry),
            included_files,
            known_type_names_cache: Rc::new(known_type_names_cache),
            loaded_module_func_indices: Rc::new(loaded_module_func_indices),
        };
        Ok(AnnotateState {
            tysys,
            world_registry,
            sorted_sources,
            module_semantics,
            invocations,
            interner,
        })
    }

    /// Lower phase: emit one [`TirModule`] per source module using the state
    /// produced by [`Elaborator::annotate_modules`]. Errors are collected in the
    /// logger; the function returns [`Bail`] if any module failed.
    pub(crate) fn build_tir_from_state(
        state: &mut AnnotateState,
        symbols: &'a SymbolTable,
        modules: &'a IndexMap<ModuleSource, Module>,
        entry_module_source: ModuleSource,
        logger: &'a Logger<'a, H>,
        snapshot: Option<&crate::semantics::Semantics>,
    ) -> Result<IndexMap<ModuleSource, TirModule>, Bail> {
        let mut result = IndexMap::default();
        // Per-rehydration memo: maps each cached function `Rc`'s pointer
        // identity to its per-compile clone, so that aliasing between
        // `functions` and `generic_functions` within a single stdlib
        // module is preserved.  Lives across the whole loop so aliases
        // between distinct stdlib modules (e.g. a generic helper shared
        // between two `core:prelude/*` modules) are preserved too.
        let mut fn_remap: IndexMap<
            *const RefCell<crate::tir::TirFunction>,
            Rc<RefCell<crate::tir::TirFunction>>,
        > = IndexMap::default();

        // Per-module resolution: walk modules in the per-compile
        // topological order so a `TirModule`'s position in the result map
        // matches the dependency order downstream phases expect.  For
        // each module either rehydrate from the snapshot cache (stdlib)
        // or run the full body-level resolve pass (user code).  Errors
        // are emitted to the logger; we keep going so one broken module
        // doesn't mask others.
        let _span = logger.span("elaborate/modules");
        // Iterate a snapshot of the source order: we need `&mut state` to
        // swap each module's `ModuleSemantics` in and out, which would
        // otherwise conflict with borrowing `state.sorted_sources`.
        let sorted_sources = state.sorted_sources.clone();
        for module_source in &sorted_sources {
            // Cache hit: deep-clone the cached `TirModule` into the
            // per-compile shared type table.  Only `Core` / `Wasi` /
            // `Wasm` variants are eligible — `ModuleSource::EntryPoint`
            // values compare equal regardless of filename (one entry
            // per compile), so `snap.tir_modules.get` would otherwise
            // match the snapshot's synthetic empty entry against the
            // user's real entry and silently substitute it.
            if matches!(
                module_source,
                ModuleSource::Core { .. } | ModuleSource::Wasi { .. } | ModuleSource::Wasm { .. }
            ) && let Some(snap_module) = snapshot.and_then(|s| s.tir_modules.get(module_source))
            {
                result.insert(
                    module_source.clone(),
                    crate::stdlib_snapshot::rehydrate_tir_module(
                        snap_module,
                        &state.tysys.type_table,
                        &mut fn_remap,
                    ),
                );
                continue;
            }
            let module = modules.get(module_source).expect("module should exist");

            // Build imported type sources and module-specific flat maps for this module
            let (imported_type_sources, import_original_names) = Self::build_imported_type_sources(
                &mut state.interner.borrow_mut(),
                module,
                module_source,
                Some(&entry_module_source),
                &state.invocations,
            );
            // Build function_return_types for this module only
            // (functions defined in this module). The lookup borrows the
            // shared `all_*` tables; no per-module flat-map cloning.
            let mut function_return_types = IndexMap::default();
            {
                let empty_struct: IndexMap<String, StructFieldInfo> = IndexMap::default();
                let empty_newtype: IndexMap<String, TypeId> = IndexMap::default();
                let empty_enum: IndexMap<String, EnumInfo> = IndexMap::default();
                let empty_flags: IndexMap<String, FlagsInfo> = IndexMap::default();
                let empty_gnt: IndexMap<String, GenericNewtypeInfo> = IndexMap::default();
                let empty_variant: IndexMap<String, VariantInfo> = IndexMap::default();
                let lookup = TypeLookup {
                    current_module_source: module_source,
                    imported_type_sources: &imported_type_sources,
                    import_original_names: &import_original_names,
                    all_newtypes: &state.tysys.all_newtypes,
                    all_struct_fields: &state.tysys.all_struct_fields,
                    all_variant_cases: &state.tysys.all_variant_cases,
                    all_enum_cases: &state.tysys.all_enum_cases,
                    all_flags_cases: &state.tysys.all_flags_cases,
                    all_resource_types: &state.tysys.all_resource_types,
                    all_generic_newtypes: &state.tysys.all_generic_newtypes,
                    local_struct_fields: &empty_struct,
                    local_newtypes: &empty_newtype,
                    local_enum_cases: &empty_enum,
                    local_flags_cases: &empty_flags,
                    local_generic_newtypes: &empty_gnt,
                    local_variant_cases: &empty_variant,
                };
                for item in &module.items {
                    if let Item::Function(func) = item {
                        let return_type = if let Some(ret_ty) = &func.return_type {
                            Self::resolve_type_static(
                                ret_ty,
                                &mut state.tysys.type_table.borrow_mut(),
                                &lookup,
                            )
                        } else {
                            TypeTable::UNIT
                        };
                        function_return_types.insert(func.name.clone(), return_type);
                    }
                }
            }

            // Collect imported function names and namespace aliases from use declarations
            let mut imported_functions = IndexSet::default();
            let mut namespace_imports: IndexMap<String, ModuleSource> = IndexMap::default();
            for item in &module.items {
                if let Item::Use(use_decl) = item {
                    for use_item in &use_decl.items {
                        match use_item {
                            crate::ast::UseItem::Simple { name, alias, .. } => {
                                // Add both original name and alias (if any)
                                imported_functions.insert(name.clone());
                                if let Some(a) = alias {
                                    imported_functions.insert(a.clone());
                                }
                            }
                            crate::ast::UseItem::InterfaceFunctions { functions, .. } => {
                                // Effect functions are imported by their function name
                                for func_item in functions {
                                    imported_functions.insert(func_item.name.clone());
                                    if let Some(a) = &func_item.alias {
                                        imported_functions.insert(a.clone());
                                    }
                                }
                            }
                            crate::ast::UseItem::Namespace { name } => {
                                // Namespace import: all symbols from source module are available
                                let source = crate::name::resolve_import_with_invocations(
                                    &mut state.interner.borrow_mut(),
                                    module_source,
                                    &use_decl.source,
                                    Some(&entry_module_source),
                                    &state.invocations,
                                );
                                for sym in symbols.get_module_symbols(&source) {
                                    imported_functions.insert(sym.name.clone());
                                }
                                namespace_imports.insert(name.clone(), source);
                            }
                            crate::ast::UseItem::Wildcard => {
                                // Wildcard import: no individual function names to collect
                            }
                        }
                    }
                }
            }

            // Take this module's `ModuleSemantics` out of `state` so the
            // elaborator can own it for the body walk. The map entry was
            // pre-populated in `annotate_modules` for every `modules.keys()`
            // (and `sorted_sources` is derived from `modules`), so the entry
            // is guaranteed to exist; `expect` rather than `unwrap_or_default`
            // surfaces any future divergence between `sorted_sources` and
            // `module_semantics.keys()` loudly. Any bindings the snapshot
            // contributed survive in the taken instance. We seed the
            // imports / decls populated above and reinstall the populated
            // instance after `resolve_module` returns.
            let mut sem = state
                .module_semantics
                .swap_remove(module_source)
                .expect("module_semantics is pre-populated by annotate_modules");
            sem.imports.imported_type_sources = imported_type_sources;
            sem.imports.import_original_names = import_original_names;
            sem.imports.namespace_imports = namespace_imports;
            sem.decls.function_return_types = function_return_types;
            sem.decls.imported_functions = imported_functions;

            let mut elaborator = Elaborator {
                tysys: state.tysys.clone(),
                sem,
                symbols,
                loaded_modules: modules,
                logger,
                current_module_source: ModuleSource::entry_point_uninitialized(), // Set in Elaborator::resolve_module
                entry_module_source: entry_module_source.clone(),
                current_module_items: &[], // Set in Elaborator::resolve_module
                current_effect_params: IndexSet::default(),
                current_effect_param_decls: IndexMap::default(),
                trait_ctx: super::trait_env::TraitContext::default(),
                indexing_trait_cache: IndexMap::default(),
                trait_check_stack: RefCell::new(Vec::new()),
                method_info_cache: IndexMap::default(),
                default_scope_module: None,
                ann_module_override: None,
                invocations: Rc::clone(&state.invocations),
                interner: Rc::clone(&state.interner),
                pending_method_dispatch: None,
                pending_operator_ast_id: None,
                // Reify consumes the per-element tuple-for-of overlays the
                // body walk captures here, so they are always recorded.
                capture_tuple_overlays: true,
            };

            // Set file context so diagnostics emitted during resolution
            // carry the correct module filename (not the entry module).
            logger.set_file(module_source.diagnostic_filename());

            // Phase 1 — `annotate_bodies`: run the body walk to populate
            // `ModuleSemantics`. The combined walk's own TIR is discarded;
            // reify (Phase 2) is the sole source of the final TIR for every
            // module, stdlib and snapshot included (Stage 7).
            let resolve_result = elaborator.resolve_module(module, module_source.clone());
            let saved_sem = elaborator.sem;
            // Re-install the (now-populated) `ModuleSemantics` even on bail
            // so the LSP can answer cursor queries against whatever bindings
            // the elaborator did reach before bailing.
            state
                .module_semantics
                .insert(module_source.clone(), saved_sem);

            if resolve_result.is_ok() {
                // Phase 2 — `reify_modules`: walk the AST + `ModuleSemantics`
                // to produce the TIR. Reify is the source of truth.
                let sem_ref = state
                    .module_semantics
                    .get(module_source)
                    .expect("just re-installed above");
                let mut reify = super::reify::Reify::new(
                    state.tysys.clone(),
                    sem_ref,
                    &state.module_semantics,
                    symbols,
                    modules,
                    logger,
                    Rc::clone(&state.interner),
                );
                if let Ok(reified) = reify.reify_module(module, module_source.clone()) {
                    result.insert(module_source.clone(), reified);
                }
            }
        }

        drop(_span);
        logger.ok_or_bail(result)
    }

    /// Intern every declaration (struct/enum/variant/resource) in the type
    /// table so that `find_decl_type_by_name` returns a `TypeId` for every
    /// declared symbol. Flags and newtypes are already interned during the
    /// annotate second sub-pass, so this covers only the remaining four kinds.
    ///
    /// Generic structs with type parameters use the mangled monomorphic form
    /// at each usage site; the base decl is interned here with the canonical
    /// name so `register_symbol_key_type_indices` can resolve the owning
    /// symbol. Monomorphizations created during lowering are separate
    /// `TypeId`s and do not collide with this base entry.
    fn intern_all_decl_types(
        modules: &IndexMap<ModuleSource, Module>,
        all_struct_fields: &IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>,
        all_resource_types: &IndexMap<ModuleSource, IndexMap<String, ResourceInfo>>,
        type_table: &Rc<RefCell<TypeTable>>,
        stdlib_set: &IndexSet<ModuleSource>,
    ) {
        for (module_source, module) in modules {
            if stdlib_set.contains(module_source) {
                // Stdlib decls were interned when the snapshot was built.
                continue;
            }
            let mut tt = type_table.borrow_mut();
            for item in &module.items {
                match item {
                    Item::Struct(struct_decl) => {
                        // Resolve via struct_fields so the canonical name/module
                        // from `StructFieldInfo` wins over anything else.
                        let (name, ms) = all_struct_fields
                            .get(module_source)
                            .and_then(|m| m.get(&struct_decl.name))
                            .map(|info| (info.name.clone(), info.module_source.clone()))
                            .unwrap_or_else(|| (struct_decl.name.clone(), module_source.clone()));
                        tt.make_struct(name, ms);
                    }
                    Item::Enum(enum_decl) => {
                        tt.make_enum(enum_decl.name.clone(), module_source.clone());
                    }
                    Item::Variant(variant_decl) => {
                        tt.make_variant(variant_decl.name.clone(), module_source.clone());
                    }
                    Item::Resource(resource_decl) => {
                        let (name, ms) = all_resource_types
                            .get(module_source)
                            .and_then(|m| m.get(&resource_decl.name))
                            .map(|info| (info.name.clone(), info.module_source.clone()))
                            .unwrap_or_else(|| (resource_decl.name.clone(), module_source.clone()));
                        tt.make_resource(name, ms);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Populate `TypeTable::type_by_symbol` / `symbol_by_type` by walking every
    /// type-declaring symbol and looking up the `TypeId` the elaborator created
    /// for it.
    ///
    /// This runs as a post-pass over the whole symbol table rather than being
    /// instrumented at each `make_struct` / `make_enum` / ... call site: the
    /// decl-creation sites are spread across elaborator/module.rs,
    /// `elaborator/type_resolution.rs`, elaborator/orchestration.rs, elaborator/call.rs,
    /// and elaborator/expr.rs, and threading a `SymbolKey` through every one of
    /// them would churn ~40 call sites. The symbol-table walk is O(symbols) and
    /// touches only declarations, so the cost is negligible.
    fn register_symbol_key_type_indices(
        symbols: &SymbolTable,
        type_table: &Rc<RefCell<TypeTable>>,
    ) {
        use crate::symbol::SymbolKind;
        let mut tt = type_table.borrow_mut();
        for symbol in symbols.all_symbols() {
            let is_type_decl = matches!(
                symbol.kind,
                SymbolKind::Struct(_)
                    | SymbolKind::Enum(_)
                    | SymbolKind::Variant(_)
                    | SymbolKind::Flags(_)
                    | SymbolKind::Newtype(_)
                    | SymbolKind::Resource(_)
            );
            if !is_type_decl {
                continue;
            }
            let key = symbol.defined_at.clone();
            if let Some(type_id) = tt.find_decl_type_by_name(&symbol.name, &key.module) {
                tt.register_decl_type(key, type_id);
            }
        }
    }

    /// Build a map of imported names to their source modules from use declarations.
    /// Build a mapping from local import names to their source modules and original names.
    ///
    /// Returns `(local_name -> module_source, local_name -> original_name)`.
    /// The `original_name` is different from `local_name` when `use { Foo as Bar }` is used.
    pub(super) fn build_imported_type_sources(
        interner: &mut ModuleSourceInterner,
        module: &Module,
        from_module: &ModuleSource,
        entry_module: Option<&ModuleSource>,
        invocations: &crate::kiln::InvocationIndex,
    ) -> (IndexMap<String, ModuleSource>, IndexMap<String, String>) {
        let mut sources = IndexMap::default();
        let mut original_names = IndexMap::default();
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                let source = name::resolve_import_with_invocations(
                    interner,
                    from_module,
                    &use_decl.source,
                    entry_module,
                    invocations,
                );
                for use_item in &use_decl.items {
                    match use_item {
                        ast::UseItem::Simple { name, alias, .. } => {
                            let local_name = alias.as_ref().unwrap_or(name);
                            sources.insert(local_name.clone(), source.clone());
                            if alias.is_some() {
                                original_names.insert(local_name.clone(), name.clone());
                            }
                        }
                        ast::UseItem::InterfaceFunctions { .. }
                        | ast::UseItem::Wildcard
                        | ast::UseItem::Namespace { .. } => {}
                    }
                }
            }
        }
        (sources, original_names)
    }

    /// Resolve an optional AST return type using the source module's type context.
    ///
    /// Temporarily swaps the elaborator's "current module" perspective to
    /// `module_source` so that same-named types from different modules are
    /// resolved correctly. The shared `all_*` tables stay intact; only the
    /// import context (and locals) is swapped.
    pub(super) fn resolve_return_type_in_module(
        &mut self,
        module_source: &ModuleSource,
        return_type: Option<&crate::ast::Type>,
    ) -> crate::tir::TypeId {
        let (imports, originals) = self
            .loaded_modules
            .get(module_source)
            .map(|module| {
                Self::build_imported_type_sources(
                    &mut self.interner.borrow_mut(),
                    module,
                    module_source,
                    Some(&self.entry_module_source),
                    &self.invocations,
                )
            })
            .unwrap_or_default();
        self.with_module_perspective(module_source.clone(), imports, originals, |s| {
            return_type
                .map(|t| s.resolve_type(t))
                .unwrap_or(crate::tir::TypeTable::UNIT)
        })
    }

    /// Topologically sort modules based on struct field type dependencies.
    ///
    /// Recursively collect cross-module struct/variant dependencies from a type.
    /// Unwraps all wrapper types (`Ref`, `MutRef`, `Option`, `GenericInstance`, `Tuple`, etc.)
    /// to find underlying Struct/Variant types defined in other modules.
    pub(super) fn collect_cross_module_deps(
        type_id: TypeId,
        type_table: &TypeTable,
        out: &mut Vec<(String, ModuleSource)>,
    ) {
        match type_table.get(type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            }
            | ResolvedType::Variant {
                name,
                module_source,
                ..
            } => {
                out.push((name.clone(), module_source.clone()));
            }
            ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::BuiltinArray(inner)
            | ResolvedType::Reactive(inner) => {
                Self::collect_cross_module_deps(*inner, type_table, out);
            }
            ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. } => {
                for arg in type_args {
                    Self::collect_cross_module_deps(*arg, type_table, out);
                }
            }
            _ => {}
        }
    }

    /// A module A depends on module B if A contains a struct with a field whose type
    /// is a struct defined in B. This ensures that when we register struct types in
    /// codegen, dependency structs are registered before the structs that reference them.
    pub(super) fn topological_sort_modules(
        modules: &IndexMap<ModuleSource, Module>,
        all_struct_fields: &IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>,
        type_table: &TypeTable,
    ) -> Vec<ModuleSource> {
        // Collect and sort sources for deterministic ordering
        let mut sources: Vec<&ModuleSource> = modules.keys().collect();
        sources.sort_by_key(std::string::ToString::to_string);
        let source_to_idx: IndexMap<&ModuleSource, usize> =
            sources.iter().enumerate().map(|(i, s)| (*s, i)).collect();

        // Track dependency counts directly (no need for full dependency sets)
        let mut dependency_count: Vec<usize> = vec![0; sources.len()];
        // Track which edges we've already added to avoid duplicates
        let mut seen_edges: IndexSet<(usize, usize)> = IndexSet::default();
        // Build reverse graph: dependents[i] = modules that depend on module i
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); sources.len()];

        // Analyze struct fields to find cross-module dependencies.
        // Recursively unwrap wrapper types (Ref, MutRef, Option, GenericInstance,
        // Tuple, etc.) to detect dependencies through any nesting level.
        for (module_src, name_map) in all_struct_fields {
            let Some(&from_idx) = source_to_idx.get(module_src) else {
                continue;
            };
            for (struct_name, info) in name_map {
                for (_field_name, field_type_id, _) in &info.fields {
                    let mut dep_sources = Vec::new();
                    Self::collect_cross_module_deps(*field_type_id, type_table, &mut dep_sources);
                    for (ref_name, ref_module_source) in dep_sources {
                        // Skip self-references (same struct or same module)
                        if ref_name == *struct_name || ref_module_source == *module_src {
                            continue;
                        }
                        if let Some(&to_idx) = source_to_idx.get(&ref_module_source) {
                            // from_idx depends on to_idx (dependency edge)
                            if seen_edges.insert((from_idx, to_idx)) {
                                dependency_count[from_idx] += 1;
                                dependents[to_idx].push(from_idx);
                            }
                        }
                    }
                }
            }
        }

        // Kahn's algorithm: start with modules that have no dependencies
        let mut queue: VecDeque<usize> = dependency_count
            .iter()
            .enumerate()
            .filter(|(_, count)| **count == 0)
            .map(|(i, _)| i)
            .collect();

        let mut sorted_indices = Vec::with_capacity(sources.len());
        while let Some(idx) = queue.pop_front() {
            sorted_indices.push(idx);
            // Update dependents using reverse graph (O(1) per edge)
            for &dependent_idx in &dependents[idx] {
                dependency_count[dependent_idx] -= 1;
                if dependency_count[dependent_idx] == 0 {
                    queue.push_back(dependent_idx);
                }
            }
        }

        // Cycle detection with warning (O(n) using IndexSet)
        if sorted_indices.len() < sources.len() {
            let sorted_set: IndexSet<usize> = sorted_indices.iter().copied().collect();
            let in_cycle: Vec<usize> = (0..sources.len())
                .filter(|i| !sorted_set.contains(i))
                .collect();
            let cycle_modules: Vec<_> = in_cycle.iter().map(|&i| sources[i].to_string()).collect();
            eprintln!(
                "Warning: circular struct dependencies detected among modules: {}",
                cycle_modules.join(", ")
            );
            // Append remaining in deterministic order (already sorted by index)
            sorted_indices.extend(in_cycle);
        }

        // Convert indices back to sources
        sorted_indices.iter().map(|&i| sources[i].clone()).collect()
    }

    /// Validate that all named types in type definitions and explicit type annotations
    /// refer to known types. Runs after the second sub-pass when all type names from
    /// all modules have been collected.
    fn validate_type_definitions(
        modules: &IndexMap<ModuleSource, Module>,
        known_type_names: &IndexSet<String>,
        resource_type_names: &IndexSet<String>,
        logger: &Logger<'_, H>,
        stdlib_set: &IndexSet<ModuleSource>,
    ) -> Result<(), Bail> {
        for (module_source, module) in modules {
            if stdlib_set.contains(module_source) {
                continue;
            }
            logger.set_file(module_source.diagnostic_filename());

            // Build per-module known names: global names + import aliases + trait names
            let mut module_known_names = known_type_names.clone();
            for item in &module.items {
                match item {
                    Item::Use(use_decl) => {
                        for use_item in &use_decl.items {
                            if let ast::UseItem::Simple { name, alias, .. } = use_item {
                                module_known_names.insert(alias.as_ref().unwrap_or(name).clone());
                            }
                        }
                    }
                    Item::Trait(trait_decl) => {
                        module_known_names.insert(trait_decl.name.clone());
                    }
                    _ => {}
                }
            }

            for item in &module.items {
                match item {
                    Item::Struct(struct_decl) => {
                        let type_params: Vec<&str> = struct_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect();
                        for field in &struct_decl.fields {
                            Self::validate_ast_type_names(
                                &field.ty,
                                &module_known_names,
                                resource_type_names,
                                &type_params,
                                logger,
                            )?;
                        }
                    }
                    Item::Variant(variant_decl) => {
                        let type_params: Vec<&str> = variant_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect();
                        for case in &variant_decl.cases {
                            if let Some(payload_ty) = &case.payload {
                                Self::validate_ast_type_names(
                                    payload_ty,
                                    &module_known_names,
                                    resource_type_names,
                                    &type_params,
                                    logger,
                                )?;
                            }
                        }
                    }
                    Item::Newtype(newtype_decl) => {
                        let type_params: Vec<&str> = newtype_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect();
                        Self::validate_ast_type_names(
                            &newtype_decl.ty,
                            &module_known_names,
                            resource_type_names,
                            &type_params,
                            logger,
                        )?;
                    }
                    Item::Function(func) => {
                        let type_params: Vec<&str> =
                            func.type_params.iter().map(|p| p.name.as_str()).collect();
                        for param in &func.params {
                            Self::validate_ast_type_names(
                                &param.ty,
                                &module_known_names,
                                resource_type_names,
                                &type_params,
                                logger,
                            )?;
                        }
                        if let Some(return_ty) = &func.return_type {
                            Self::validate_ast_type_names(
                                return_ty,
                                &module_known_names,
                                resource_type_names,
                                &type_params,
                                logger,
                            )?;
                        }
                        if let Some(body) = &func.body {
                            Self::validate_block_type_names(
                                body,
                                &module_known_names,
                                resource_type_names,
                                &type_params,
                                logger,
                            )?;
                        }
                    }
                    Item::Impl(impl_block) => {
                        let mut type_params: Vec<&str> = impl_block
                            .type_params
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect();
                        // Infer implicit type params from the target type
                        // (e.g., `impl Option<T>` without `impl<T>`)
                        if let Type::Generic(g) = &impl_block.ty {
                            for arg in &g.args {
                                if let Type::Named(n) = arg
                                    && !module_known_names.contains(&n.name)
                                    && !resource_type_names.contains(&n.name)
                                {
                                    type_params.push(&n.name);
                                }
                            }
                        }
                        for method in &impl_block.methods {
                            let mut method_type_params = type_params.clone();
                            for p in &method.type_params {
                                method_type_params.push(p.name.as_str());
                            }
                            for param in &method.params {
                                Self::validate_ast_type_names(
                                    &param.ty,
                                    &module_known_names,
                                    resource_type_names,
                                    &method_type_params,
                                    logger,
                                )?;
                            }
                            if let Some(return_ty) = &method.return_type {
                                Self::validate_ast_type_names(
                                    return_ty,
                                    &module_known_names,
                                    resource_type_names,
                                    &method_type_params,
                                    logger,
                                )?;
                            }
                            if let Some(body) = &method.body {
                                Self::validate_block_type_names(
                                    body,
                                    &module_known_names,
                                    resource_type_names,
                                    &method_type_params,
                                    logger,
                                )?;
                            }
                        }
                    }
                    Item::Trait(trait_decl) => {
                        let type_params: Vec<&str> = trait_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.as_str())
                            .collect();
                        for method in &trait_decl.methods {
                            let mut method_type_params = type_params.clone();
                            method_type_params.push("Self");
                            for p in &method.type_params {
                                method_type_params.push(p.name.as_str());
                            }
                            // Add associated type names as type params
                            for assoc in &trait_decl.associated_types {
                                method_type_params.push(&assoc.name);
                            }
                            for param in &method.params {
                                Self::validate_ast_type_names(
                                    &param.ty,
                                    &module_known_names,
                                    resource_type_names,
                                    &method_type_params,
                                    logger,
                                )?;
                            }
                            if let Some(return_ty) = &method.return_type {
                                Self::validate_ast_type_names(
                                    return_ty,
                                    &module_known_names,
                                    resource_type_names,
                                    &method_type_params,
                                    logger,
                                )?;
                            }
                            if let Some(body) = &method.body {
                                Self::validate_block_type_names(
                                    body,
                                    &module_known_names,
                                    resource_type_names,
                                    &method_type_params,
                                    logger,
                                )?;
                            }
                        }
                    }
                    Item::Global(global_decl) => {
                        Self::validate_ast_type_names(
                            &global_decl.ty,
                            &module_known_names,
                            resource_type_names,
                            &[],
                            logger,
                        )?;
                    }
                    Item::Test(test_decl) => {
                        Self::validate_block_type_names(
                            &test_decl.body,
                            &module_known_names,
                            resource_type_names,
                            &[],
                            logger,
                        )?;
                    }
                    _ => {}
                }
            }
        }
        logger.clear_file();
        Ok(())
    }

    /// Validate type names in a block (let-stmt type annotations and cast expressions).
    fn validate_block_type_names(
        block: &ast::Block,
        known_type_names: &IndexSet<String>,
        resource_type_names: &IndexSet<String>,
        type_params: &[&str],
        logger: &Logger<'_, H>,
    ) -> Result<(), Bail> {
        for stmt in &block.stmts {
            Self::validate_stmt_type_names(
                stmt,
                known_type_names,
                resource_type_names,
                type_params,
                logger,
            )?;
        }
        Ok(())
    }

    /// Validate type names in a statement.
    fn validate_stmt_type_names(
        stmt: &ast::Stmt,
        known_type_names: &IndexSet<String>,
        resource_type_names: &IndexSet<String>,
        type_params: &[&str],
        logger: &Logger<'_, H>,
    ) -> Result<(), Bail> {
        match stmt {
            ast::Stmt::Let(let_stmt) => {
                if let Some(ty) = &let_stmt.ty {
                    Self::validate_ast_type_names(
                        ty,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                if let Some(value) = &let_stmt.value {
                    Self::validate_expr_type_names(
                        value,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Stmt::Expr(expr_stmt) => {
                Self::validate_expr_type_names(
                    &expr_stmt.expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::Return(ret) => {
                if let Some(value) = &ret.value {
                    Self::validate_expr_type_names(
                        value,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Stmt::TaskReturn(task_ret) => {
                Self::validate_expr_type_names(
                    &task_ret.value,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::If(if_stmt) => {
                Self::validate_condition_type_names(
                    &if_stmt.condition,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_block_type_names(
                    &if_stmt.then_block,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                if let Some(else_block) = &if_stmt.else_block {
                    Self::validate_block_type_names(
                        else_block,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Stmt::While(while_stmt) => {
                Self::validate_condition_type_names(
                    &while_stmt.condition,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_block_type_names(
                    &while_stmt.body,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::For(for_stmt) => {
                if let Some(init) = &for_stmt.init {
                    Self::validate_stmt_type_names(
                        init,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                if let Some(condition) = &for_stmt.condition {
                    Self::validate_condition_type_names(
                        condition,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                if let Some(update) = &for_stmt.update {
                    Self::validate_expr_type_names(
                        update,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                Self::validate_block_type_names(
                    &for_stmt.body,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::ForOf(for_of) => {
                Self::validate_expr_type_names(
                    &for_of.iterable,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_block_type_names(
                    &for_of.body,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::Loop(loop_stmt) => {
                Self::validate_block_type_names(
                    &loop_stmt.body,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::Match(match_expr) => {
                Self::validate_expr_type_names(
                    &ast::Expr::Match(match_expr.clone()),
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::Assert(assert_stmt) => {
                Self::validate_expr_type_names(
                    &assert_stmt.condition,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::LabeledBlock(lb) => {
                Self::validate_block_type_names(
                    &lb.block,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Stmt::Break(_) | ast::Stmt::Continue(_) | ast::Stmt::Error(_) => {}
        }
        Ok(())
    }

    /// Validate type names in a condition.
    fn validate_condition_type_names(
        condition: &ast::Condition,
        known_type_names: &IndexSet<String>,
        resource_type_names: &IndexSet<String>,
        type_params: &[&str],
        logger: &Logger<'_, H>,
    ) -> Result<(), Bail> {
        match condition {
            ast::Condition::Expr(expr) => {
                Self::validate_expr_type_names(
                    expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Condition::LetChain { elements, .. } => {
                for elem in elements {
                    match elem {
                        ast::ConditionElement::Let { expr, .. } => {
                            Self::validate_expr_type_names(
                                expr,
                                known_type_names,
                                resource_type_names,
                                type_params,
                                logger,
                            )?;
                        }
                        ast::ConditionElement::Expr(expr) => {
                            Self::validate_expr_type_names(
                                expr,
                                known_type_names,
                                resource_type_names,
                                type_params,
                                logger,
                            )?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Validate type names in an expression (cast targets, closure params, turbofish, etc.).
    fn validate_expr_type_names(
        expr: &ast::Expr,
        known_type_names: &IndexSet<String>,
        resource_type_names: &IndexSet<String>,
        type_params: &[&str],
        logger: &Logger<'_, H>,
    ) -> Result<(), Bail> {
        match expr {
            ast::Expr::Cast(cast) => {
                Self::validate_ast_type_names(
                    &cast.target_type,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_expr_type_names(
                    &cast.expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::Closure(closure) => {
                for param in &closure.params {
                    if let Some(ty) = &param.ty {
                        Self::validate_ast_type_names(
                            ty,
                            known_type_names,
                            resource_type_names,
                            type_params,
                            logger,
                        )?;
                    }
                }
                Self::validate_expr_type_names(
                    &closure.body,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::Call(call) => {
                for ty in &call.type_args {
                    Self::validate_ast_type_names(
                        ty,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                Self::validate_expr_type_names(
                    &call.callee,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                for arg in &call.args {
                    Self::validate_expr_type_names(
                        arg,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::MethodCall(mc) => {
                for ty in &mc.type_args {
                    Self::validate_ast_type_names(
                        ty,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                Self::validate_expr_type_names(
                    &mc.receiver,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                for arg in &mc.args {
                    Self::validate_expr_type_names(
                        arg,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::StaticMethodCall(smc) => {
                Self::validate_ast_type_names(
                    &smc.target_type,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                for ty in &smc.type_args {
                    Self::validate_ast_type_names(
                        ty,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                for arg in &smc.args {
                    Self::validate_expr_type_names(
                        arg,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::Binary(bin) => {
                Self::validate_expr_type_names(
                    &bin.left,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_expr_type_names(
                    &bin.right,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::Unary(un) => {
                Self::validate_expr_type_names(
                    &un.expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::Assign(assign) => {
                Self::validate_expr_type_names(
                    &assign.target,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_expr_type_names(
                    &assign.value,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::CompoundAssign(ca) => {
                Self::validate_expr_type_names(
                    &ca.target,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_expr_type_names(
                    &ca.value,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::ComparisonChain(cc) => {
                Self::validate_expr_type_names(
                    &cc.first,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                for cmp in &cc.comparisons {
                    Self::validate_expr_type_names(
                        &cmp.right,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::Index(idx) => {
                Self::validate_expr_type_names(
                    &idx.expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_expr_type_names(
                    &idx.index,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::FieldAccess(fa) => {
                Self::validate_expr_type_names(
                    &fa.expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::Block(block) => {
                Self::validate_block_type_names(
                    block,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::If(if_expr) => {
                Self::validate_condition_type_names(
                    &if_expr.condition,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_block_type_names(
                    &if_expr.then_block,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                if let Some(else_block) = &if_expr.else_block {
                    Self::validate_block_type_names(
                        else_block,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::Match(match_expr) => {
                Self::validate_expr_type_names(
                    &match_expr.expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                for arm in &match_expr.arms {
                    if let Some(guard) = &arm.guard {
                        Self::validate_expr_type_names(
                            guard,
                            known_type_names,
                            resource_type_names,
                            type_params,
                            logger,
                        )?;
                    }
                    Self::validate_expr_type_names(
                        &arm.body,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::Matches(matches_expr) => {
                Self::validate_expr_type_names(
                    &matches_expr.expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                if let Some(guard) = &matches_expr.guard {
                    Self::validate_expr_type_names(
                        guard,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::StructLiteral(sl) => {
                for field in &sl.fields {
                    Self::validate_expr_type_names(
                        &field.value,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::TupleLiteral(tl) => {
                for elem in &tl.elements {
                    Self::validate_expr_type_names(
                        elem,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::TemplateString(ts) => {
                for part in &ts.parts {
                    if let ast::TemplatePart::Interpolation { expr, .. } = part {
                        Self::validate_expr_type_names(
                            expr,
                            known_type_names,
                            resource_type_names,
                            type_params,
                            logger,
                        )?;
                    }
                }
            }
            ast::Expr::LabeledBlock(lb) => {
                Self::validate_block_type_names(
                    &lb.block,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::TryOp(try_op) => {
                Self::validate_expr_type_names(
                    &try_op.expr,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::Spread(inner, _) => {
                Self::validate_expr_type_names(
                    inner,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::Range(range) => {
                Self::validate_expr_type_names(
                    &range.start,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
                Self::validate_expr_type_names(
                    &range.end,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::WithHandler(with_handler) => {
                // The LHS of `E = h` in a `with` clause is an effect
                // name, not a type name. The real elaborator validates it
                // against the effect declaration index in
                // `resolve_with_handler`; here we only walk the handler
                // expression and the body for type-name references.
                for binding in &with_handler.handlers {
                    Self::validate_expr_type_names(
                        &binding.handler,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                for stmt in &with_handler.body.stmts {
                    Self::validate_stmt_type_names(
                        stmt,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
            }
            ast::Expr::Resume(resume) => {
                Self::validate_expr_type_names(
                    &resume.value,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )?;
            }
            ast::Expr::Ident(_) | ast::Expr::Literal(_) | ast::Expr::Error(_) => {}
        }
        Ok(())
    }

    /// Walk an AST type expression and emit errors for unknown Named types.
    /// Generic type names (Array, Result, etc.) are not checked here since they
    /// may be builtins not present in the type name registry; only their type
    /// arguments are validated recursively.
    fn validate_ast_type_names(
        ty: &Type,
        known_type_names: &IndexSet<String>,
        resource_type_names: &IndexSet<String>,
        type_params: &[&str],
        logger: &Logger<'_, H>,
    ) -> Result<(), Bail> {
        match ty {
            Type::Named(named) => {
                if named.name == "()" || named.name == "!" || named.name == "Self" {
                    return Ok(());
                }
                if type_params.contains(&named.name.as_str()) {
                    return Ok(());
                }
                if known_type_names.contains(&named.name) {
                    return Ok(());
                }
                if resource_type_names.contains(&named.name) {
                    return Ok(());
                }
                logger.error(TypeError::UnknownType {
                    name: named.name.clone(),
                    span: named.span,
                })?;
                Ok(())
            }
            Type::Generic(generic) => {
                for arg in &generic.args {
                    Self::validate_ast_type_names(
                        arg,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                Ok(())
            }
            Type::NamespacedGeneric(ng) => {
                for arg in &ng.args {
                    Self::validate_ast_type_names(
                        arg,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                Ok(())
            }
            Type::Reference(inner) | Type::MutReference(inner) => Self::validate_ast_type_names(
                inner,
                known_type_names,
                resource_type_names,
                type_params,
                logger,
            ),
            Type::Tuple(elems) => {
                for elem in elems {
                    Self::validate_ast_type_names(
                        elem,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                Ok(())
            }
            Type::Function(ft) => {
                for param in &ft.params {
                    Self::validate_ast_type_names(
                        param,
                        known_type_names,
                        resource_type_names,
                        type_params,
                        logger,
                    )?;
                }
                Self::validate_ast_type_names(
                    &ft.return_type,
                    known_type_names,
                    resource_type_names,
                    type_params,
                    logger,
                )
            }
            Type::TypePackSpread(_, _) => Ok(()),
        }
    }

    /// Static version of `resolve_type` for use before the elaborator is fully
    /// constructed. Reads type info via [`TypeLookup`] — the same path the
    /// fully-constructed elaborator uses, so name resolution stays in one place.
    pub(super) fn resolve_type_static(
        ty: &Type,
        type_table: &mut TypeTable,
        lookup: &TypeLookup<'_>,
    ) -> TypeId {
        // Delegate to the type-param-aware resolver with an empty scope.
        // Keeping a single implementation avoids the two arms drifting:
        // `resolve_type_static` previously lacked the generic
        // resource / variant / enum handling, so a struct field typed
        // `Stream<u8>` / `Result<T, E>` resolved to `UNKNOWN`.
        Self::resolve_type_static_with_params(ty, type_table, lookup, &[])
    }

    /// Static version of `resolve_type` with type parameters for variant payload resolution.
    /// This is similar to `resolve_type_static` but also handles type parameters (like T, E)
    /// that appear in generic variant definitions (like `Result<T, E>`).
    pub(super) fn resolve_type_static_with_params(
        ty: &Type,
        type_table: &mut TypeTable,
        lookup: &TypeLookup<'_>,
        type_params: &[String],
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check newtypes first
                if let Some(alias_type_id) = lookup.newtype(&named.name) {
                    return alias_type_id;
                }

                // Check if it's a type parameter (e.g., T in Result<T, E>)
                if let Some(index) = type_params.iter().position(|p| p == &named.name) {
                    return type_table.make_type_param(named.name.clone(), index as u32);
                }

                // Built-in primitives
                match named.name.as_str() {
                    "bool" => TypeTable::BOOL,
                    "char" => TypeTable::CHAR,
                    "v128" => TypeTable::V128,
                    "i8" => TypeTable::I8,
                    "i16" => TypeTable::I16,
                    "i32" => TypeTable::I32,
                    "i64" => TypeTable::I64,
                    "u8" => TypeTable::U8,
                    "u16" => TypeTable::U16,
                    "u32" => TypeTable::U32,
                    "u64" => TypeTable::U64,
                    "f32" => TypeTable::F32,
                    "f64" => TypeTable::F64,
                    "()" => TypeTable::UNIT,
                    "!" => TypeTable::NEVER,
                    _ => {
                        if let Some(info) = lookup.struct_fields(&named.name) {
                            type_table.make_struct(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = lookup.resource_type(&named.name) {
                            type_table.make_resource(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = lookup.variant_case(&named.name) {
                            type_table.make_variant(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = lookup.enum_case(&named.name) {
                            type_table.make_enum(info.name.clone(), info.module_source.clone())
                        } else {
                            TypeTable::UNKNOWN
                        }
                    }
                }
            }
            Type::Generic(generic) => match generic.name.as_str() {
                "Option" if !generic.args.is_empty() => {
                    let inner = Self::resolve_type_static_with_params(
                        &generic.args[0],
                        type_table,
                        lookup,
                        type_params,
                    );
                    type_table.make_option(inner)
                }
                _ => {
                    let type_args: Vec<TypeId> = generic
                        .args
                        .iter()
                        .map(|arg| {
                            Self::resolve_type_static_with_params(
                                arg,
                                type_table,
                                lookup,
                                type_params,
                            )
                        })
                        .collect();
                    // A generic newtype (`type MyArray<T> = Array<T>`)
                    // resolves to a `Newtype` over the instantiated base,
                    // mirroring `type_resolution.rs:418`. Without this it
                    // falls through to `UNKNOWN`, the newtype's inherited
                    // base methods (`MyArray<i32>::len` → `Array<i32>::len`)
                    // never resolve, and monomorphization can't reach them.
                    if let Some(gn_info) = lookup.generic_newtype(&generic.name).cloned() {
                        let concrete_base = super::type_resolution::substitute_type_params(
                            &gn_info.base_type_ast,
                            &gn_info.type_params,
                            &generic.args,
                        );
                        let base_type_id = Self::resolve_type_static_with_params(
                            &concrete_base,
                            type_table,
                            lookup,
                            type_params,
                        );
                        let arg_names: Vec<String> =
                            type_args.iter().map(|&t| type_table.type_name(t)).collect();
                        let display_name = format!("{}<{}>", generic.name, arg_names.join(", "));
                        return type_table.make_newtype(
                            display_name,
                            gn_info.module_source,
                            base_type_id,
                        );
                    }
                    // A generic resource (`Stream<u8>`, `Future<T>`) must
                    // resolve to a `GenericResource`, not a
                    // `GenericInstance` — otherwise the resource-store
                    // inference (effect_check `signature_resources`) does
                    // not see the resource a signature implies, and
                    // `consume(rx: Stream<u8>)` fails with
                    // `missing resource 'Stream'`.
                    if let Some(info) = lookup.resource_type(&generic.name) {
                        return type_table.intern(ResolvedType::GenericResource {
                            name: generic.name.clone(),
                            module_source: info.module_source.clone(),
                            type_args,
                        });
                    }
                    // A generic application `Name<args...>` may name a
                    // struct, a variant (`Result<T, E>`), or an enum. The
                    // `Type::Named` arm above already checks all three; the
                    // generic arm must too, otherwise generic variants /
                    // enums resolve to `UNKNOWN`. `make_generic_instance`
                    // is name-based, so the same call shape works for every
                    // kind.
                    let module_source = lookup
                        .struct_fields(&generic.name)
                        .map(|info| info.module_source.clone())
                        .or_else(|| {
                            lookup
                                .variant_case(&generic.name)
                                .map(|info| info.module_source.clone())
                        })
                        .or_else(|| {
                            lookup
                                .enum_case(&generic.name)
                                .map(|info| info.module_source.clone())
                        });
                    if let Some(module_source) = module_source {
                        type_table.make_generic_instance(
                            generic.name.clone(),
                            module_source,
                            type_args,
                        )
                    } else {
                        TypeTable::UNKNOWN
                    }
                }
            },
            Type::Reference(inner) => {
                let inner_type =
                    Self::resolve_type_static_with_params(inner, type_table, lookup, type_params);
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type =
                    Self::resolve_type_static_with_params(inner, type_table, lookup, type_params);
                type_table.make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => {
                // Handle builtin::array<T>
                if namespaced.namespace == "builtin"
                    && namespaced.name == "array"
                    && let Some(elem_ty) = namespaced.args.first()
                {
                    let elem = Self::resolve_type_static_with_params(
                        elem_ty,
                        type_table,
                        lookup,
                        type_params,
                    );
                    return type_table.make_builtin_array(elem);
                }
                // Handle T::AssocType where T is a type parameter
                if let Some(index) = type_params.iter().position(|p| p == &namespaced.namespace) {
                    let param_id =
                        type_table.make_type_param(namespaced.namespace.clone(), index as u32);
                    return type_table.make_assoc_type_projection(
                        param_id,
                        namespaced.name.clone(),
                        vec![],
                        vec![],
                    );
                }
                TypeTable::UNKNOWN
            }
            Type::Tuple(elements) => {
                let elem_types: Vec<TypeId> = elements
                    .iter()
                    .map(|e| {
                        Self::resolve_type_static_with_params(e, type_table, lookup, type_params)
                    })
                    .collect();
                type_table.make_tuple(elem_types)
            }
            Type::Function(func_type) => {
                let params: Vec<TypeId> = func_type
                    .params
                    .iter()
                    .map(|p| {
                        Self::resolve_type_static_with_params(p, type_table, lookup, type_params)
                    })
                    .collect();
                let return_type = Self::resolve_type_static_with_params(
                    &func_type.return_type,
                    type_table,
                    lookup,
                    type_params,
                );
                type_table.intern(crate::tir::ResolvedType::Function {
                    is_mut: func_type.is_mut,
                    params,
                    return_type,
                    effects: vec![],
                    stores: vec![],
                })
            }
            // A variadic type-pack spread `..T` resolves to a `TypePack`
            // keyed by the param's positional index, mirroring the instance
            // resolver's `trait_ctx.type_params` lookup (type_resolution.rs:103).
            // Without this arm a `[..T]` parameter resolves its element to
            // `UNKNOWN`, so a generic tuple method (`Tuple<..T>^Eq::eq`)
            // monomorphizes against `Tuple<unknown>` and never registers at
            // WIR build.
            Type::TypePackSpread(name, _span) => {
                if let Some(index) = type_params.iter().position(|p| p == name) {
                    type_table.make_type_pack(name.clone(), index as u32)
                } else {
                    TypeTable::UNKNOWN
                }
            }
        }
    }

    /// Pre-pass: scan all modules and register `generic_assoc_type_defs` for every
    /// non-concrete trait impl with associated types. This runs before any module is
    /// fully resolved so that `find_trait_method_for_type` can look up associated types
    /// across module boundaries regardless of resolution order.
    fn register_all_generic_assoc_type_defs(
        modules: &IndexMap<ModuleSource, Module>,
        type_table: &Rc<RefCell<TypeTable>>,
        stdlib_set: &IndexSet<ModuleSource>,
    ) {
        for (module_source, module) in modules {
            if stdlib_set.contains(module_source) {
                // Stdlib generic-assoc defs are baked into the seeded
                // `TypeTable` via the snapshot.
                continue;
            }
            for item in &module.items {
                let Item::Impl(impl_block) = item else {
                    continue;
                };
                if impl_block.trait_type.is_none() || impl_block.associated_types.is_empty() {
                    continue;
                }
                // Determine the struct name (base name without type args)
                let struct_name = Self::get_type_name_static(&impl_block.ty);

                // Build a mapping from type param name to index from the explicit `impl<...>` header.
                let type_param_idx: IndexMap<String, u32> = impl_block
                    .type_params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| (p.name.clone(), i as u32))
                    .collect();

                // Skip impls with no type params (concrete impls are handled differently)
                if type_param_idx.is_empty() {
                    continue;
                }

                for binding in &impl_block.associated_types {
                    let type_param_id = match &binding.ty {
                        // Simple case: `type Item = T` — T is a type param
                        Type::Named(named) => {
                            if let Some(&idx) = type_param_idx.get(&named.name) {
                                type_table
                                    .borrow_mut()
                                    .make_type_param(named.name.clone(), idx)
                            } else {
                                // Not a type param (e.g., a concrete type) — skip
                                continue;
                            }
                        }
                        // Chained case: `type Item = I::InnerName` — I is a type param
                        Type::NamespacedGeneric(ns) if ns.args.is_empty() => {
                            if let Some(&idx) = type_param_idx.get(&ns.namespace) {
                                let inner_param_id = type_table
                                    .borrow_mut()
                                    .make_type_param(ns.namespace.clone(), idx);
                                type_table.borrow_mut().make_assoc_type_projection_simple(
                                    inner_param_id,
                                    ns.name.clone(),
                                )
                            } else {
                                continue;
                            }
                        }
                        _ => continue,
                    };

                    type_table.borrow_mut().register_generic_assoc_type_def(
                        struct_name.clone(),
                        binding.name.clone(),
                        type_param_id,
                    );
                }
            }
        }
    }
}
