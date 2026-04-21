//! Multi-module resolution orchestration.
//!
//! Resolution runs in two phases:
//!
//! - [`Resolver::annotate_modules`] collects decl-level type information
//!   (struct field maps, variant cases, flags, newtypes, resource methods)
//!   and interns every declaration in the shared [`TypeTable`]. It also
//!   populates [`TypeTable::type_by_symbol`]/[`TypeTable::symbol_by_type`]
//!   so LSP queries can resolve a [`SymbolKey`] to a decl-backed type
//!   without running TIR lowering. The output is an [`AnnotateState`] that
//!   both `lower_tir` and the LSP consume.
//! - [`Resolver::lower_tir_from_state`] reads that state and produces one
//!   [`TirModule`] per source module. It does not mutate the annotate
//!   output; all new types created during lowering (anonymous structs,
//!   monomorphic instances) are written through the shared
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
use crate::component_model::WasiRegistry;
use crate::logger::{Bail, Logger};
use crate::name::{self as name, ModuleSource};
use crate::symbol::{Symbol, SymbolKey, SymbolTable};
use crate::tir::{ResolvedType, TirModule, TypeId, TypeTable};
use crate::world_registry::WorldRegistry;

use super::Resolver;
use super::trait_env::TraitEnv;
use super::types::{
    EnumCaseData, EnumInfo, FlagsInfo, FlagsMemberData, GenericNewtypeInfo, ModuleTypeMaps,
    ResourceInfo, StructFieldInfo, TypeError, VariantCaseData, VariantInfo,
};

/// Analysis state produced by [`Resolver::annotate_modules`] and consumed by
/// [`Resolver::lower_tir_from_state`].
///
/// All expensive maps are stored behind `Rc` so the state is cheap to share
/// between LSP queries and the lowering pipeline without cloning the
/// underlying data. The [`TypeTable`] itself is behind `Rc<RefCell<…>>`
/// because lowering interns additional types (anonymous structs,
/// monomorphized instances) into the same table.
pub(crate) struct AnnotateState {
    pub(crate) type_table: Rc<RefCell<TypeTable>>,
    pub(crate) trait_env: Arc<TraitEnv>,
    pub(crate) sorted_sources: Vec<ModuleSource>,
    pub(crate) all_newtypes: Rc<IndexMap<ModuleSource, IndexMap<String, TypeId>>>,
    pub(crate) all_generic_newtypes:
        Rc<IndexMap<ModuleSource, IndexMap<String, GenericNewtypeInfo>>>,
    pub(crate) all_struct_fields: Rc<IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>>,
    pub(crate) all_variant_cases: Rc<IndexMap<ModuleSource, IndexMap<String, VariantInfo>>>,
    pub(crate) all_enum_cases: Rc<IndexMap<ModuleSource, IndexMap<String, EnumInfo>>>,
    pub(crate) all_flags_cases: Rc<IndexMap<ModuleSource, IndexMap<String, FlagsInfo>>>,
    pub(crate) all_resource_types: Rc<IndexMap<ModuleSource, IndexMap<String, ResourceInfo>>>,
    pub(crate) wasi_registry: &'static WasiRegistry,
    pub(crate) world_registry: &'static WorldRegistry,
    pub(crate) builtin_registry: BuiltinRegistry,
    pub(crate) global_known_type_names: IndexSet<String>,
    pub(crate) all_module_func_indices: IndexMap<ModuleSource, IndexMap<String, usize>>,
    /// Use→def map for local variables: `(module, IdentExpr.id)` →
    /// `(module, defining AstId)`. Populated by [`Resolver::resolve_ident`]
    /// whenever a name resolves to a local binding. Consumed by LSP
    /// `definition` / `hover` to jump to the defining pattern / parameter.
    pub(crate) references: Rc<RefCell<IndexMap<SymbolKey, SymbolKey>>>,
    /// Locally-defined [`Symbol`]s (let bindings, parameters, closure
    /// parameters). Keyed by the binding's defining [`SymbolKey`]. Populated
    /// alongside [`Self::references`] as the resolver walks function bodies.
    pub(crate) local_symbols: Rc<RefCell<IndexMap<SymbolKey, Symbol>>>,
    /// Kiln invocation redirects consulted by `resolve_import` call sites
    /// when walking `use` declarations. Populated from [`crate::loader::LoadResult`].
    pub(crate) invocations: Rc<crate::kiln::InvocationIndex>,
}

impl<'a, H: CompilerHost> Resolver<'a, H> {
    /// Run the full resolve pipeline: annotate, then lower to TIR.
    ///
    /// This is a thin wrapper over [`Resolver::annotate_modules`] +
    /// [`Resolver::lower_tir_from_state`]. Callers that want access to the
    /// annotate output (e.g. LSP) should call the two phases separately.
    pub(crate) fn resolve_all_modules(
        symbols: &'a SymbolTable,
        modules: &'a IndexMap<ModuleSource, Module>,
        entry_module_source: ModuleSource,
        logger: &'a Logger<'a, H>,
        included_files: &'a IndexMap<[String; 2], Vec<u8>>,
        invocations: crate::kiln::InvocationIndex,
    ) -> Result<IndexMap<ModuleSource, TirModule>, Bail> {
        let state =
            Self::annotate_modules(symbols, modules, &entry_module_source, logger, invocations)?;
        Self::lower_tir_from_state(
            &state,
            symbols,
            modules,
            entry_module_source,
            logger,
            included_files,
        )
    }

    /// Annotate phase: collect decl-level type information and intern every
    /// declaration in the shared [`TypeTable`]. Produces an [`AnnotateState`]
    /// that downstream phases (`lower_tir`, LSP queries) consume read-mostly
    /// via `Rc`.
    pub(crate) fn annotate_modules(
        symbols: &'a SymbolTable,
        modules: &'a IndexMap<ModuleSource, Module>,
        entry_module_source: &ModuleSource,
        logger: &'a Logger<'a, H>,
        invocations: crate::kiln::InvocationIndex,
    ) -> Result<AnnotateState, Bail> {
        let invocations = Rc::new(invocations);
        // Create a shared type table wrapped in Rc<RefCell<>> for cross-module sharing
        let type_table = Rc::new(RefCell::new(TypeTable::new()));
        let mut all_newtypes: IndexMap<ModuleSource, IndexMap<String, TypeId>> =
            IndexMap::default();
        let mut all_generic_newtypes: IndexMap<ModuleSource, IndexMap<String, GenericNewtypeInfo>> =
            IndexMap::default();
        let mut all_struct_fields: IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>> =
            IndexMap::default();
        let mut all_variant_cases: IndexMap<ModuleSource, IndexMap<String, VariantInfo>> =
            IndexMap::default();
        let mut all_enum_cases: IndexMap<ModuleSource, IndexMap<String, EnumInfo>> =
            IndexMap::default();
        let mut all_flags_cases: IndexMap<ModuleSource, IndexMap<String, FlagsInfo>> =
            IndexMap::default();
        let mut all_resource_types: IndexMap<ModuleSource, IndexMap<String, ResourceInfo>> =
            IndexMap::default();

        // First pass: collect struct, variant, enum, and resource names from all modules (for forward references)
        for (module_source, module) in modules {
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
                        let comp_features = super::item::extract_comp_features(&struct_decl.attrs);
                        if comp_features & crate::wir::COMP_FEATURE_BOX != 0 {
                            type_table.borrow_mut().box_module_source = Some(module_source.clone());
                        }
                        if comp_features & crate::wir::COMP_FEATURE_RANGE_EXCLUSIVE != 0 {
                            type_table.borrow_mut().range_exclusive_module_source =
                                Some(module_source.clone());
                        }
                        if comp_features & crate::wir::COMP_FEATURE_RANGE_INCLUSIVE != 0 {
                            type_table.borrow_mut().range_inclusive_module_source =
                                Some(module_source.clone());
                        }
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
                        let comp_features = super::item::extract_comp_features(&variant_decl.attrs);
                        if comp_features != 0 {
                            type_table.borrow_mut().register_comp_feature_variant(
                                comp_features,
                                module_source.clone(),
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
                        let comp_features = super::item::extract_comp_features(&trait_decl.attrs);
                        if comp_features != 0 {
                            type_table
                                .borrow_mut()
                                .register_comp_feature_trait(comp_features, module_source.clone());
                        }
                    }
                    Item::TupleTypeDecl(decl) => {
                        let comp_features = super::item::extract_comp_features(&decl.attrs);
                        if comp_features & crate::wir::COMP_FEATURE_TUPLE != 0 {
                            type_table
                                .borrow_mut()
                                .register_tuple_module_source(module_source.clone());
                        }
                    }
                    _ => {}
                }
            }
        }

        // Second sub-pass: resolve struct fields and newtypes
        for (module_source, module) in modules {
            // Build imported type sources for this module
            let (imported_type_sources, import_original_names) = Self::build_imported_type_sources(
                module,
                module_source,
                Some(entry_module_source),
                &invocations,
            );

            // Build module-specific flat maps for resolving types in this module
            let mut flat_newtypes = Self::build_module_map(
                &all_newtypes,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let mut flat_struct_fields = Self::build_module_map(
                &all_struct_fields,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let flat_resource_types = Self::build_module_map(
                &all_resource_types,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let flat_enum_cases = Self::build_module_map(
                &all_enum_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let flat_variant_cases = Self::build_module_map(
                &all_variant_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let flat_flags_cases = Self::build_module_map(
                &all_flags_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );

            for item in &module.items {
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
                                    &flat_newtypes,
                                    &flat_struct_fields,
                                    &flat_resource_types,
                                    &flat_enum_cases,
                                    &flat_variant_cases,
                                    &flat_flags_cases,
                                )
                            } else {
                                Self::resolve_type_static_with_params(
                                    &field.ty,
                                    &mut type_table.borrow_mut(),
                                    &flat_newtypes,
                                    &flat_struct_fields,
                                    &flat_resource_types,
                                    &flat_enum_cases,
                                    &flat_variant_cases,
                                    &flat_flags_cases,
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

                        // Update the nested map entry with actual fields
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
                            .insert(struct_decl.name.clone(), info.clone());
                        // Also update flat map for subsequent items in this module
                        flat_struct_fields.insert(struct_decl.name.clone(), info);
                    }
                    Item::Newtype(newtype_decl) => {
                        if newtype_decl.type_params.is_empty() {
                            // Concrete newtype: resolve immediately
                            let base_type_id = Self::resolve_type_static(
                                &newtype_decl.ty,
                                &mut type_table.borrow_mut(),
                                &flat_newtypes,
                                &flat_struct_fields,
                                &flat_resource_types,
                                &flat_enum_cases,
                                &flat_variant_cases,
                                &flat_flags_cases,
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
                            flat_newtypes.insert(newtype_decl.name.clone(), newtype_id);
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
                                    &flat_newtypes,
                                    &flat_struct_fields,
                                    &flat_resource_types,
                                    &flat_enum_cases,
                                    &flat_variant_cases,
                                    &flat_flags_cases,
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
                        // Also update flat_newtypes for subsequent items in this module
                        flat_newtypes.insert(flags_decl.name.clone(), flags_type);
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

        let (wasi_registry, world_registry) = {
            let _span = logger.span("resolve/wasi_registry");
            WasiRegistry::build_from_stdlib()
        };
        let builtin_registry = {
            let _span = logger.span("resolve/builtin_registry");
            BuiltinRegistry::build_from_stdlib(&type_table)
        };

        // Build trait lookup indices once for all modules.
        // This allows find_trait_method_for_type and find_indexing_trait_impl to do O(1)
        // lookups by type name instead of scanning all items in all modules per method call.
        // Also runs orphan rule checking; violations are emitted as errors.
        let (trait_env, orphan_violations) = {
            let _span = logger.span("resolve/trait_env");
            super::trait_env::TraitEnv::build(modules)
        };
        for violation in orphan_violations {
            let _ = logger.error(violation);
        }

        // Pre-pass: register generic associated type defs from ALL modules before any module
        // is resolved. This ensures that when resolving module X, it can look up associated
        // types from module Y's impl blocks even if Y hasn't been processed yet in the main
        // second pass (e.g., user module is sorted before prelude modules).
        Self::register_all_generic_assoc_type_defs(modules, &type_table);

        // Wrap all_* maps in Rc for cheap sharing across per-module resolvers
        let all_newtypes = Rc::new(all_newtypes);
        let all_struct_fields = Rc::new(all_struct_fields);
        let all_variant_cases = Rc::new(all_variant_cases);
        let all_enum_cases = Rc::new(all_enum_cases);
        let all_flags_cases = Rc::new(all_flags_cases);
        let all_resource_types = Rc::new(all_resource_types);
        let all_generic_newtypes = Rc::new(all_generic_newtypes);

        // Pre-compute the global known type names cache once (shared across all modules)
        let global_known_type_names = {
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
        // Resource type names are kept separate from global_known_type_names because
        // adding them would break is_known_type_name() used in impl block type parameter
        // inference (e.g., `impl Request { ... }` would stop recognizing Request's methods).
        let resource_type_names: IndexSet<String> = all_resource_types
            .values()
            .flat_map(|m| m.keys().cloned())
            .collect();
        Self::validate_type_definitions(
            modules,
            &global_known_type_names,
            &resource_type_names,
            logger,
        )?;

        // Pre-build function name → index maps for all loaded modules (O(1) lookup)
        let all_module_func_indices: IndexMap<ModuleSource, IndexMap<String, usize>> = modules
            .iter()
            .map(|(src, module)| (src.clone(), Self::build_func_index(&module.items)))
            .collect();

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
        );

        // Populate `TypeTable::type_by_symbol` / `symbol_by_type` so LSP queries
        // can resolve a `SymbolKey` to a decl-backed type without running the
        // lower phase.
        Self::register_symbol_key_type_indices(symbols, &type_table);

        Ok(AnnotateState {
            type_table,
            trait_env,
            sorted_sources,
            all_newtypes,
            all_generic_newtypes,
            all_struct_fields,
            all_variant_cases,
            all_enum_cases,
            all_flags_cases,
            all_resource_types,
            wasi_registry,
            world_registry,
            builtin_registry,
            global_known_type_names,
            all_module_func_indices,
            references: Rc::new(RefCell::new(IndexMap::default())),
            local_symbols: Rc::new(RefCell::new(IndexMap::default())),
            invocations,
        })
    }

    /// Lower phase: emit one [`TirModule`] per source module using the state
    /// produced by [`Resolver::annotate_modules`]. Errors are collected in the
    /// logger; the function returns [`Bail`] if any module failed.
    pub(crate) fn lower_tir_from_state(
        state: &AnnotateState,
        symbols: &'a SymbolTable,
        modules: &'a IndexMap<ModuleSource, Module>,
        entry_module_source: ModuleSource,
        logger: &'a Logger<'a, H>,
        included_files: &'a IndexMap<[String; 2], Vec<u8>>,
    ) -> Result<IndexMap<ModuleSource, TirModule>, Bail> {
        let mut result = IndexMap::default();

        // Per-module resolution: build each TirModule using the shared type
        // table and the annotate-phase decl maps. Errors are emitted to the
        // logger; we keep going so one broken module doesn't mask others.
        let _span = logger.span("resolve/modules");
        for module_source in &state.sorted_sources {
            let module = modules.get(module_source).expect("module should exist");

            // Build imported type sources and module-specific flat maps for this module
            let (imported_type_sources, import_original_names) = Self::build_imported_type_sources(
                module,
                module_source,
                Some(&entry_module_source),
                &state.invocations,
            );
            let newtypes = Self::build_module_map(
                &state.all_newtypes,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let generic_newtype_defs = Self::build_module_map(
                &state.all_generic_newtypes,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let struct_fields = Self::build_module_map(
                &state.all_struct_fields,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let variant_cases = Self::build_module_map(
                &state.all_variant_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let enum_cases = Self::build_module_map(
                &state.all_enum_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let flags_cases = Self::build_module_map(
                &state.all_flags_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let resource_types = Self::build_module_map(
                &state.all_resource_types,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );

            // Build function_return_types for this module only
            // (functions defined in this module)
            let mut function_return_types = IndexMap::default();
            for item in &module.items {
                if let Item::Function(func) = item {
                    let return_type = if let Some(ret_ty) = &func.return_type {
                        Self::resolve_type_static(
                            ret_ty,
                            &mut state.type_table.borrow_mut(),
                            &newtypes,
                            &struct_fields,
                            &resource_types,
                            &enum_cases,
                            &variant_cases,
                            &flags_cases,
                        )
                    } else {
                        TypeTable::UNIT
                    };
                    function_return_types.insert(func.name.clone(), return_type);
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
                            crate::ast::UseItem::EffectFunctions { functions, .. } => {
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

            let mut resolver = Resolver {
                type_table: Rc::clone(&state.type_table),
                symbols,
                loaded_modules: modules,
                newtypes,
                generic_newtype_defs,
                struct_fields,
                variant_cases,
                enum_cases,
                flags_cases,
                resource_types,
                all_newtypes: Rc::clone(&state.all_newtypes),
                all_struct_fields: Rc::clone(&state.all_struct_fields),
                all_variant_cases: Rc::clone(&state.all_variant_cases),
                all_enum_cases: Rc::clone(&state.all_enum_cases),
                all_flags_cases: Rc::clone(&state.all_flags_cases),
                all_resource_types: Rc::clone(&state.all_resource_types),
                function_return_types,
                imported_functions,
                namespace_imports,
                logger,
                current_module_source: ModuleSource::entry_point_with_filename("<uninitialized>"), // Set in resolve_module
                entry_module_source: entry_module_source.clone(),
                current_module_items: &[], // Set in resolve_module
                effect_sources: IndexMap::default(), // Populated per-module in resolve_module
                current_effect_params: IndexSet::default(),
                current_effect_param_decls: IndexMap::default(),
                trait_ctx: super::trait_env::TraitContext::default(),
                generic_struct_names: IndexSet::default(),
                generic_function_params: IndexMap::default(),
                generic_function_resolved_param_types: IndexMap::default(),
                generic_function_resolved_return_types: IndexMap::default(),
                generic_method_params: IndexMap::default(),
                generic_method_resolved_param_types: IndexMap::default(),
                wasi_registry: state.wasi_registry,
                builtin_registry: &state.builtin_registry,
                current_module_globals: IndexMap::default(),
                imported_globals: IndexMap::default(),
                associated_constants: IndexMap::default(),
                module_type_maps_cache: IndexMap::default(),
                trait_env: Arc::clone(&state.trait_env),
                included_files,
                known_type_names_cache: state.global_known_type_names.clone(),
                indexing_trait_cache: IndexMap::default(),
                trait_check_stack: RefCell::new(Vec::new()),
                method_info_cache: IndexMap::default(),
                pending_anonymous_structs: Vec::new(),
                current_module_func_index: IndexMap::default(), // Built in resolve_module
                loaded_module_func_indices: state.all_module_func_indices.clone(),
                references: Rc::clone(&state.references),
                local_symbols: Rc::clone(&state.local_symbols),
                default_scope_module: None,
                invocations: Rc::clone(&state.invocations),
            };
            // known_type_names_cache is pre-computed globally; no per-module rebuild needed

            // Set file context so diagnostics emitted during resolution
            // carry the correct module filename (not the entry module).
            logger.set_file(module_source.diagnostic_filename());

            // Errors are emitted to the logger; if resolve_module returns Bail,
            // we continue to resolve remaining modules to collect more errors
            if let Ok(tir_module) = resolver.resolve_module(module, module_source.clone()) {
                result.insert(module_source.clone(), tir_module);
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
    ) {
        for (module_source, module) in modules {
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
    /// type-declaring symbol and looking up the `TypeId` the resolver created
    /// for it.
    ///
    /// This runs as a post-pass over the whole symbol table rather than being
    /// instrumented at each `make_struct` / `make_enum` / ... call site: the
    /// decl-creation sites are spread across resolver/module.rs,
    /// `resolver/type_resolution.rs`, resolver/orchestration.rs, resolver/call.rs,
    /// and resolver/expr.rs, and threading a `SymbolKey` through every one of
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

    /// Build a module-specific flat map from per-module entries.
    ///
    /// Priority: current module > imported types > any available definition.
    pub(super) fn build_module_map<V: Clone>(
        per_module: &IndexMap<ModuleSource, IndexMap<String, V>>,
        current_module: &ModuleSource,
        imported_type_sources: &IndexMap<String, ModuleSource>,
        import_original_names: &IndexMap<String, String>,
    ) -> IndexMap<String, V> {
        let mut result = IndexMap::default();
        // First: add all entries from all modules (arbitrary winner for conflicts)
        for name_map in per_module.values() {
            for (name, value) in name_map {
                result.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
        // Second: override with imported modules' types
        for (local_name, import_src) in imported_type_sources {
            // Use original name to look up in the source module (handles `use { Foo as Bar }`)
            let lookup_name = import_original_names.get(local_name).unwrap_or(local_name);
            // Try exact module first, then sub-modules for umbrella imports
            // (e.g., `use { ErrorCode } from "wasi:http"` should find ErrorCode
            // in `wasi:http/types.wado` when `wasi:http` is an umbrella module).
            let found = per_module
                .get(import_src)
                .and_then(|m| m.get(lookup_name))
                .cloned()
                .or_else(|| {
                    if let ModuleSource::Wasi { interface } = import_src {
                        let prefix = format!("{interface}/");
                        for (src, name_map) in per_module {
                            if let ModuleSource::Wasi {
                                interface: sub_iface,
                            } = src
                                && sub_iface.starts_with(&prefix)
                                && let Some(value) = name_map.get(lookup_name)
                            {
                                return Some(value.clone());
                            }
                        }
                    }
                    None
                });
            if let Some(value) = found {
                result.insert(local_name.clone(), value);
            }
        }
        // Third: override with current module's types (highest priority)
        if let Some(name_map) = per_module.get(current_module) {
            for (name, value) in name_map {
                result.insert(name.clone(), value.clone());
            }
        }
        result
    }

    /// Build a map of imported names to their source modules from use declarations.
    /// Build a mapping from local import names to their source modules and original names.
    ///
    /// Returns `(local_name -> module_source, local_name -> original_name)`.
    /// The `original_name` is different from `local_name` when `use { Foo as Bar }` is used.
    pub(super) fn build_imported_type_sources(
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
                        ast::UseItem::EffectFunctions { .. }
                        | ast::UseItem::Wildcard
                        | ast::UseItem::Namespace { .. } => {}
                    }
                }
            }
        }
        (sources, original_names)
    }

    /// Lazily build and cache per-module type maps for cross-module type resolution.
    ///
    /// Must be called before borrowing `loaded_modules` for the same module,
    /// so the cache is populated without borrow conflicts. After calling this,
    /// use `self.module_type_maps_cache.remove(module_source)` to get the cached
    /// maps and swap them into the resolver's active maps.
    pub(super) fn ensure_module_maps_cached(&mut self, module_source: &ModuleSource) {
        if self.module_type_maps_cache.contains_key(module_source) {
            return;
        }
        let Some(module) = self.loaded_modules.get(module_source) else {
            return;
        };
        let (imported_sources, import_names) = Self::build_imported_type_sources(
            module,
            module_source,
            Some(&self.entry_module_source),
            &self.invocations,
        );
        let maps = ModuleTypeMaps {
            struct_fields: Self::build_module_map(
                &self.all_struct_fields,
                module_source,
                &imported_sources,
                &import_names,
            ),
            variant_cases: Self::build_module_map(
                &self.all_variant_cases,
                module_source,
                &imported_sources,
                &import_names,
            ),
            enum_cases: Self::build_module_map(
                &self.all_enum_cases,
                module_source,
                &imported_sources,
                &import_names,
            ),
            flags_cases: Self::build_module_map(
                &self.all_flags_cases,
                module_source,
                &imported_sources,
                &import_names,
            ),
            newtypes: Self::build_module_map(
                &self.all_newtypes,
                module_source,
                &imported_sources,
                &import_names,
            ),
            resource_types: Self::build_module_map(
                &self.all_resource_types,
                module_source,
                &imported_sources,
                &import_names,
            ),
        };
        self.module_type_maps_cache
            .insert(module_source.clone(), maps);
    }

    /// Resolve an optional AST return type using the source module's type context.
    ///
    /// Temporarily swaps the active type maps with those of `module_source` so that
    /// same-named types from different modules are resolved correctly.
    /// Requires `ensure_module_maps_cached(module_source)` to have been called first.
    pub(super) fn resolve_return_type_in_module(
        &mut self,
        module_source: &ModuleSource,
        return_type: Option<&crate::ast::Type>,
    ) -> crate::tir::TypeId {
        let mut cached = self
            .module_type_maps_cache
            .shift_remove(module_source)
            .expect("cache populated by ensure_module_maps_cached");
        std::mem::swap(&mut self.struct_fields, &mut cached.struct_fields);
        std::mem::swap(&mut self.variant_cases, &mut cached.variant_cases);
        std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
        std::mem::swap(&mut self.flags_cases, &mut cached.flags_cases);
        std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
        std::mem::swap(&mut self.resource_types, &mut cached.resource_types);

        let result = return_type
            .map(|t| self.resolve_type(t))
            .unwrap_or(crate::tir::TypeTable::UNIT);

        std::mem::swap(&mut self.struct_fields, &mut cached.struct_fields);
        std::mem::swap(&mut self.variant_cases, &mut cached.variant_cases);
        std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
        std::mem::swap(&mut self.flags_cases, &mut cached.flags_cases);
        std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
        std::mem::swap(&mut self.resource_types, &mut cached.resource_types);
        self.module_type_maps_cache
            .insert(module_source.clone(), cached);

        result
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
    ) -> Result<(), Bail> {
        for (module_source, module) in modules {
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
            ast::Stmt::Break(_) | ast::Stmt::Continue(_) => {}
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
            ast::Expr::Ident(_) | ast::Expr::Literal(_) => {}
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

    /// Static version of `resolve_type` for use before the resolver is fully constructed
    pub(super) fn resolve_type_static(
        ty: &Type,
        type_table: &mut TypeTable,
        newtypes: &IndexMap<String, TypeId>,
        struct_fields: &IndexMap<String, StructFieldInfo>,
        resource_types: &IndexMap<String, ResourceInfo>,
        enum_cases: &IndexMap<String, EnumInfo>,
        variant_cases: &IndexMap<String, VariantInfo>,
        flags_cases: &IndexMap<String, FlagsInfo>,
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check newtypes first
                if let Some(&alias_type_id) = newtypes.get(&named.name) {
                    return alias_type_id;
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
                        // Check if it's a struct type
                        // Use canonical name from info (not alias) for consistent TypeId interning
                        if let Some(info) = struct_fields.get(&named.name) {
                            type_table.make_struct(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = resource_types.get(&named.name) {
                            type_table.make_resource(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = variant_cases.get(&named.name) {
                            type_table.make_variant(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = enum_cases.get(&named.name) {
                            type_table.make_enum(info.name.clone(), info.module_source.clone())
                        } else if flags_cases.contains_key(&named.name) {
                            // Flags are newtypes over u32, should be handled by newtypes check above.
                            // This is a fallback in case the newtype wasn't registered yet.
                            TypeTable::UNKNOWN
                        } else {
                            TypeTable::UNKNOWN
                        }
                    }
                }
            }
            Type::Generic(generic) => match generic.name.as_str() {
                "Option" if !generic.args.is_empty() => {
                    let inner = Self::resolve_type_static(
                        &generic.args[0],
                        type_table,
                        newtypes,
                        struct_fields,
                        resource_types,
                        enum_cases,
                        variant_cases,
                        flags_cases,
                    );
                    type_table.make_option(inner)
                }
                _ => {
                    // Check if it's a generic struct type
                    if let Some(info) = struct_fields.get(&generic.name) {
                        // Resolve type arguments
                        let type_args: Vec<TypeId> = generic
                            .args
                            .iter()
                            .map(|arg| {
                                Self::resolve_type_static(
                                    arg,
                                    type_table,
                                    newtypes,
                                    struct_fields,
                                    resource_types,
                                    enum_cases,
                                    variant_cases,
                                    flags_cases,
                                )
                            })
                            .collect();
                        type_table.make_generic_instance(
                            generic.name.clone(),
                            info.module_source.clone(),
                            type_args,
                        )
                    } else {
                        TypeTable::UNKNOWN
                    }
                }
            },
            Type::Reference(inner) => {
                let inner_type = Self::resolve_type_static(
                    inner,
                    type_table,
                    newtypes,
                    struct_fields,
                    resource_types,
                    enum_cases,
                    variant_cases,
                    flags_cases,
                );
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = Self::resolve_type_static(
                    inner,
                    type_table,
                    newtypes,
                    struct_fields,
                    resource_types,
                    enum_cases,
                    variant_cases,
                    flags_cases,
                );
                type_table.make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => {
                // Handle builtin::array<T>
                if namespaced.namespace == "builtin"
                    && namespaced.name == "array"
                    && let Some(elem_ty) = namespaced.args.first()
                {
                    let elem = Self::resolve_type_static(
                        elem_ty,
                        type_table,
                        newtypes,
                        struct_fields,
                        resource_types,
                        enum_cases,
                        variant_cases,
                        flags_cases,
                    );
                    return type_table.make_builtin_array(elem);
                }
                TypeTable::UNKNOWN
            }
            Type::Tuple(elements) => {
                let elem_types: Vec<TypeId> = elements
                    .iter()
                    .map(|e| {
                        Self::resolve_type_static(
                            e,
                            type_table,
                            newtypes,
                            struct_fields,
                            resource_types,
                            enum_cases,
                            variant_cases,
                            flags_cases,
                        )
                    })
                    .collect();
                type_table.make_tuple(elem_types)
            }
            // TODO: Function, ClosureType, etc. are not yet handled — returning UNKNOWN
            // causes stale/wrong TypeIds in all_struct_fields when used as struct field types.
            _ => TypeTable::UNKNOWN,
        }
    }

    /// Static version of `resolve_type` with type parameters for variant payload resolution.
    /// This is similar to `resolve_type_static` but also handles type parameters (like T, E)
    /// that appear in generic variant definitions (like `Result<T, E>`).
    pub(super) fn resolve_type_static_with_params(
        ty: &Type,
        type_table: &mut TypeTable,
        newtypes: &IndexMap<String, TypeId>,
        struct_fields: &IndexMap<String, StructFieldInfo>,
        resource_types: &IndexMap<String, ResourceInfo>,
        enum_cases: &IndexMap<String, EnumInfo>,
        variant_cases: &IndexMap<String, VariantInfo>,
        flags_cases: &IndexMap<String, FlagsInfo>,
        type_params: &[String],
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check newtypes first
                if let Some(&alias_type_id) = newtypes.get(&named.name) {
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
                        // Use canonical name from info (not alias) for consistent TypeId interning
                        if let Some(info) = struct_fields.get(&named.name) {
                            type_table.make_struct(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = resource_types.get(&named.name) {
                            type_table.make_resource(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = variant_cases.get(&named.name) {
                            type_table.make_variant(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = enum_cases.get(&named.name) {
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
                        newtypes,
                        struct_fields,
                        resource_types,
                        enum_cases,
                        variant_cases,
                        flags_cases,
                        type_params,
                    );
                    type_table.make_option(inner)
                }
                _ => {
                    // Check if it's a generic struct type
                    if let Some(info) = struct_fields.get(&generic.name) {
                        // Resolve type arguments
                        let type_args: Vec<TypeId> = generic
                            .args
                            .iter()
                            .map(|arg| {
                                Self::resolve_type_static_with_params(
                                    arg,
                                    type_table,
                                    newtypes,
                                    struct_fields,
                                    resource_types,
                                    enum_cases,
                                    variant_cases,
                                    flags_cases,
                                    type_params,
                                )
                            })
                            .collect();
                        type_table.make_generic_instance(
                            generic.name.clone(),
                            info.module_source.clone(),
                            type_args,
                        )
                    } else {
                        TypeTable::UNKNOWN
                    }
                }
            },
            Type::Reference(inner) => {
                let inner_type = Self::resolve_type_static_with_params(
                    inner,
                    type_table,
                    newtypes,
                    struct_fields,
                    resource_types,
                    enum_cases,
                    variant_cases,
                    flags_cases,
                    type_params,
                );
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = Self::resolve_type_static_with_params(
                    inner,
                    type_table,
                    newtypes,
                    struct_fields,
                    resource_types,
                    enum_cases,
                    variant_cases,
                    flags_cases,
                    type_params,
                );
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
                        newtypes,
                        struct_fields,
                        resource_types,
                        enum_cases,
                        variant_cases,
                        flags_cases,
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
                        Self::resolve_type_static_with_params(
                            e,
                            type_table,
                            newtypes,
                            struct_fields,
                            resource_types,
                            enum_cases,
                            variant_cases,
                            flags_cases,
                            type_params,
                        )
                    })
                    .collect();
                type_table.make_tuple(elem_types)
            }
            Type::Function(func_type) => {
                let params: Vec<TypeId> = func_type
                    .params
                    .iter()
                    .map(|p| {
                        Self::resolve_type_static_with_params(
                            p,
                            type_table,
                            newtypes,
                            struct_fields,
                            resource_types,
                            enum_cases,
                            variant_cases,
                            flags_cases,
                            type_params,
                        )
                    })
                    .collect();
                let return_type = Self::resolve_type_static_with_params(
                    &func_type.return_type,
                    type_table,
                    newtypes,
                    struct_fields,
                    resource_types,
                    enum_cases,
                    variant_cases,
                    flags_cases,
                    type_params,
                );
                type_table.intern(crate::tir::ResolvedType::Function {
                    params,
                    return_type,
                    effects: vec![],
                    stores: vec![],
                })
            }
            _ => TypeTable::UNKNOWN,
        }
    }

    /// Pre-pass: scan all modules and register `generic_assoc_type_defs` for every
    /// non-concrete trait impl with associated types. This runs before any module is
    /// fully resolved so that `find_trait_method_for_type` can look up associated types
    /// across module boundaries regardless of resolution order.
    fn register_all_generic_assoc_type_defs(
        modules: &IndexMap<ModuleSource, Module>,
        type_table: &Rc<RefCell<TypeTable>>,
    ) {
        for module in modules.values() {
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
