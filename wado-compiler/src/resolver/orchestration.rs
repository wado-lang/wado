//! Multi-module resolution orchestration.
//!
//! This module handles resolving all modules in dependency order,
//! including topological sorting and cross-module type collection.

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
use crate::symbol::SymbolTable;
use crate::tir::{ResolvedType, TirModule, TypeId, TypeTable};

use super::Resolver;
use super::types::{
    EnumCaseData, EnumInfo, FlagsInfo, FlagsMemberData, GenericNewtypeInfo, ModuleTypeMaps,
    ResourceInfo, StructFieldInfo, VariantCaseData, VariantInfo,
};

impl<'a, H: CompilerHost> Resolver<'a, H> {
    pub(crate) fn resolve_all_modules(
        symbols: &'a SymbolTable,
        modules: &'a IndexMap<ModuleSource, Module>,
        _entry_module_source: ModuleSource,
        logger: &'a Logger<'a, H>,
        included_files: &'a IndexMap<[String; 2], Vec<u8>>,
    ) -> Result<IndexMap<ModuleSource, TirModule>, Bail> {
        let mut result = IndexMap::default();

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
                                    module_source: module_source.clone(),
                                    fields: Vec::new(),
                                    type_param_bounds,
                                    type_param_type_ids: Vec::new(), // filled in second pass
                                },
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
                                EnumInfo {
                                    module_source: module_source.clone(),
                                    cases: Vec::new(),
                                },
                            );
                    }
                    Item::Resource(resource_decl) => {
                        all_resource_types
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                resource_decl.name.clone(),
                                ResourceInfo {
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
                    _ => {}
                }
            }
        }

        // Second sub-pass: resolve struct fields and newtypes
        for (module_source, module) in modules {
            // Build imported type sources for this module
            let (imported_type_sources, import_original_names) =
                Self::build_imported_type_sources(module, module_source);

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
                        // This allows infer_type_args_from_fields to fill phantom type params
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
                            module_source: module_source.clone(),
                            fields,
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
                            })
                            .collect();
                        all_enum_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                enum_decl.name.clone(),
                                EnumInfo {
                                    module_source: module_source.clone(),
                                    cases,
                                },
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

        let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
        let builtin_registry = BuiltinRegistry::build_from_stdlib(&type_table);

        // Build trait lookup indices once for all modules.
        // This allows find_trait_method_for_type and find_indexing_trait_impl to do O(1)
        // lookups by type name instead of scanning all items in all modules per method call.
        // Also runs orphan rule checking; violations are emitted as errors.
        let (trait_env, orphan_violations) = super::trait_env::TraitEnv::build(modules);
        for violation in orphan_violations {
            let _ = logger.error(violation);
        }

        // Second pass: resolve each module with per-module function_return_types and imports
        for module_source in &sorted_sources {
            let module = modules.get(module_source).expect("module should exist");

            // Build imported type sources and module-specific flat maps for this module
            let (imported_type_sources, import_original_names) =
                Self::build_imported_type_sources(module, module_source);
            let newtypes = Self::build_module_map(
                &all_newtypes,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let generic_newtype_defs = Self::build_module_map(
                &all_generic_newtypes,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let struct_fields = Self::build_module_map(
                &all_struct_fields,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let variant_cases = Self::build_module_map(
                &all_variant_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let enum_cases = Self::build_module_map(
                &all_enum_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let flags_cases = Self::build_module_map(
                &all_flags_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let resource_types = Self::build_module_map(
                &all_resource_types,
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
                            &mut type_table.borrow_mut(),
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

            // Collect imported function names from this module's use declarations
            let mut imported_functions = IndexSet::default();
            for item in &module.items {
                if let Item::Use(use_decl) = item {
                    for use_item in &use_decl.items {
                        match use_item {
                            crate::ast::UseItem::Simple { name, alias } => {
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
                            crate::ast::UseItem::Wildcard => {
                                // Wildcard import: no function names to collect
                            }
                        }
                    }
                }
            }

            let mut resolver = Resolver {
                type_table: Rc::clone(&type_table),
                symbols,
                loaded_modules: modules,
                newtypes,
                generic_newtype_defs,
                struct_fields,
                variant_cases,
                enum_cases,
                flags_cases,
                resource_types,
                all_newtypes: all_newtypes.clone(),
                all_struct_fields: all_struct_fields.clone(),
                all_variant_cases: all_variant_cases.clone(),
                all_enum_cases: all_enum_cases.clone(),
                all_flags_cases: all_flags_cases.clone(),
                all_resource_types: all_resource_types.clone(),
                function_return_types,
                imported_functions,
                logger,
                current_module_source: ModuleSource::entry_point_with_filename("<uninitialized>"), // Set in resolve_module
                current_module_items: Vec::new(), // Set in resolve_module
                trait_ctx: super::trait_env::TraitContext::default(),
                generic_struct_names: IndexSet::default(),
                generic_function_params: IndexMap::default(),
                generic_function_resolved_param_types: IndexMap::default(),
                generic_method_params: IndexMap::default(),
                generic_method_resolved_param_types: IndexMap::default(),
                wasi_registry,
                builtin_registry: &builtin_registry,
                current_module_globals: IndexMap::default(),
                imported_globals: IndexMap::default(),
                associated_constants: IndexMap::default(),
                module_type_maps_cache: IndexMap::default(),
                trait_env: Arc::clone(&trait_env),
                included_files,
                known_type_names_cache: IndexSet::default(),
                indexing_trait_cache: IndexMap::default(),
            };
            resolver.rebuild_known_type_names_cache();

            // Set file context so diagnostics emitted during resolution
            // carry the correct module filename (not the entry module).
            logger.set_file(module_source.diagnostic_filename());

            // Errors are emitted to the logger; if resolve_module returns Bail,
            // we continue to resolve remaining modules to collect more errors
            if let Ok(tir_module) = resolver.resolve_module(module, module_source.clone()) {
                result.insert(module_source.clone(), tir_module);
            }
        }

        logger.ok_or_bail(result)
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
            if let Some(name_map) = per_module.get(import_src)
                && let Some(value) = name_map.get(lookup_name)
            {
                result.insert(local_name.clone(), value.clone());
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
    ) -> (IndexMap<String, ModuleSource>, IndexMap<String, String>) {
        let mut sources = IndexMap::default();
        let mut original_names = IndexMap::default();
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                let source = name::resolve_import(from_module, &use_decl.source);
                for use_item in &use_decl.items {
                    match use_item {
                        ast::UseItem::Simple { name, alias } => {
                            let local_name = alias.as_ref().unwrap_or(name);
                            sources.insert(local_name.clone(), source.clone());
                            if alias.is_some() {
                                original_names.insert(local_name.clone(), name.clone());
                            }
                        }
                        ast::UseItem::EffectFunctions { .. } | ast::UseItem::Wildcard => {}
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
        let (imported_sources, import_names) =
            Self::build_imported_type_sources(module, module_source);
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
            ResolvedType::Tuple(elems) => {
                for elem in elems {
                    Self::collect_cross_module_deps(*elem, type_table, out);
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
                        if let Some(info) = struct_fields.get(&named.name) {
                            type_table.make_struct(named.name.clone(), info.module_source.clone())
                        } else if let Some(info) = resource_types.get(&named.name) {
                            type_table.make_resource(named.name.clone(), info.module_source.clone())
                        } else if let Some(info) = enum_cases.get(&named.name) {
                            type_table.make_enum(named.name.clone(), info.module_source.clone())
                        } else if let Some(info) = variant_cases.get(&named.name) {
                            type_table.make_variant(named.name.clone(), info.module_source.clone())
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
                        // Check if it's a struct type
                        if let Some(info) = struct_fields.get(&named.name) {
                            type_table.make_struct(named.name.clone(), info.module_source.clone())
                        } else if let Some(info) = resource_types.get(&named.name) {
                            type_table.make_resource(named.name.clone(), info.module_source.clone())
                        } else if let Some(info) = enum_cases.get(&named.name) {
                            type_table.make_enum(named.name.clone(), info.module_source.clone())
                        } else if let Some(info) = variant_cases.get(&named.name) {
                            type_table.make_variant(named.name.clone(), info.module_source.clone())
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
            _ => TypeTable::UNKNOWN,
        }
    }
}
