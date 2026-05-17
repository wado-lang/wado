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
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::{self as name};
use crate::symbol::{Symbol, SymbolKey, SymbolTable};
use crate::tir::{ResolvedType, TirModule, TypeId, TypeTable};
use crate::world_registry::WorldRegistry;

use super::Resolver;
use super::trait_env::TraitEnv;
use super::types::{
    EnumCaseData, EnumInfo, FlagsInfo, FlagsMemberData, GenericNewtypeInfo, ResourceInfo,
    StructFieldInfo, TypeError, TypeLookup, VariantCaseData, VariantInfo,
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
    /// `ModuleSource` interner shared across phases. `Rc<RefCell<>>` so
    /// `&self` resolver methods can `borrow_mut()` it when constructing
    /// new module sources during name resolution.
    pub(crate) interner: Rc<RefCell<ModuleSourceInterner>>,
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
        interner: Rc<RefCell<ModuleSourceInterner>>,
    ) -> Result<(IndexMap<ModuleSource, TirModule>, Arc<TraitEnv>), Bail> {
        let state = Self::annotate_modules(
            symbols,
            modules,
            &entry_module_source,
            logger,
            invocations,
            interner,
            None,
        )?;
        let trait_env = state.trait_env.clone();
        let tir_modules = Self::lower_tir_from_state(
            &state,
            symbols,
            modules,
            entry_module_source,
            logger,
            included_files,
            None,
        )?;
        Ok((tir_modules, trait_env))
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
        interner: Rc<RefCell<ModuleSourceInterner>>,
        snapshot: Option<&crate::annotate::Annotated>,
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
        let mut all_newtypes: IndexMap<ModuleSource, IndexMap<String, TypeId>> = snapshot
            .map(|s| (*s.state.all_newtypes).clone())
            .unwrap_or_default();
        let mut all_generic_newtypes: IndexMap<ModuleSource, IndexMap<String, GenericNewtypeInfo>> =
            snapshot
                .map(|s| (*s.state.all_generic_newtypes).clone())
                .unwrap_or_default();
        let mut all_struct_fields: IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>> =
            snapshot
                .map(|s| (*s.state.all_struct_fields).clone())
                .unwrap_or_default();
        let mut all_variant_cases: IndexMap<ModuleSource, IndexMap<String, VariantInfo>> = snapshot
            .map(|s| (*s.state.all_variant_cases).clone())
            .unwrap_or_default();
        let mut all_enum_cases: IndexMap<ModuleSource, IndexMap<String, EnumInfo>> = snapshot
            .map(|s| (*s.state.all_enum_cases).clone())
            .unwrap_or_default();
        let mut all_flags_cases: IndexMap<ModuleSource, IndexMap<String, FlagsInfo>> = snapshot
            .map(|s| (*s.state.all_flags_cases).clone())
            .unwrap_or_default();
        let mut all_resource_types: IndexMap<ModuleSource, IndexMap<String, ResourceInfo>> =
            snapshot
                .map(|s| (*s.state.all_resource_types).clone())
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
                        );
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
                        super::item::register_trait_compiler_item(
                            &type_table,
                            &trait_decl.attrs,
                            &trait_decl.name,
                            module_source,
                        );
                    }
                    Item::TupleTypeDecl(decl) => {
                        super::item::register_tuple_compiler_item(
                            &type_table,
                            &decl.attrs,
                            module_source,
                        );
                    }
                    _ => {}
                }
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

        let (wasi_registry, world_registry) = {
            let _span = logger.span("resolve/wasi_registry");
            WasiRegistry::build_from_stdlib()
        };
        let builtin_registry = {
            let _span = logger.span("resolve/builtin_registry");
            let mut registry = if let Some(snap) = snapshot {
                // The snapshot's registry is bound to the snapshot's
                // `TypeTable`, whose entries we cloned into `type_table`
                // above — so the registered `TypeId`s remain valid.
                snap.state.builtin_registry.clone()
            } else {
                BuiltinRegistry::build_from_stdlib(&type_table)
            };
            // Fold in `#[canonical(...)]` no-body declarations from
            // loader-synthesised wasm-asset modules so calls into a
            // wat/wasm asset's exports lower through the same TirImport
            // path as `core:builtin` declarations.  Stdlib wasm assets
            // (e.g. `core:libm.wat`) are already in `snap.state.builtin_registry`.
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
        Self::register_all_generic_assoc_type_defs(modules, &type_table, &stdlib_set);

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
            &stdlib_set,
        )?;

        // Pre-build function name → index maps for all loaded modules (O(1) lookup)
        let all_module_func_indices: IndexMap<ModuleSource, IndexMap<String, usize>> = {
            let mut indices: IndexMap<ModuleSource, IndexMap<String, usize>> = snapshot
                .map(|s| s.state.all_module_func_indices.clone())
                .unwrap_or_default();
            for (src, module) in modules {
                if stdlib_set.contains(src) {
                    // Pre-populated from snapshot.
                    continue;
                }
                indices.insert(src.clone(), Self::build_func_index(&module.items));
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

        // Seed the use→def reference map and the local-symbol map with the
        // snapshot's pre-resolved stdlib entries so the LSP edges remain
        // consistent and user-module body resolution can extend on top.
        let references = Rc::new(RefCell::new(
            snapshot.map(|s| s.references.clone()).unwrap_or_default(),
        ));
        let local_symbols = Rc::new(RefCell::new(
            snapshot.map(|s| s.locals.clone()).unwrap_or_default(),
        ));

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
            references,
            local_symbols,
            invocations,
            interner,
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
        snapshot: Option<&crate::annotate::Annotated>,
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
        let _span = logger.span("resolve/modules");
        for module_source in &state.sorted_sources {
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
                        &state.type_table,
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
                    all_newtypes: &state.all_newtypes,
                    all_struct_fields: &state.all_struct_fields,
                    all_variant_cases: &state.all_variant_cases,
                    all_enum_cases: &state.all_enum_cases,
                    all_flags_cases: &state.all_flags_cases,
                    all_resource_types: &state.all_resource_types,
                    all_generic_newtypes: &state.all_generic_newtypes,
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
                                &mut state.type_table.borrow_mut(),
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

            let mut resolver = Resolver {
                type_table: Rc::clone(&state.type_table),
                symbols,
                loaded_modules: modules,
                all_newtypes: Rc::clone(&state.all_newtypes),
                all_generic_newtypes: Rc::clone(&state.all_generic_newtypes),
                all_struct_fields: Rc::clone(&state.all_struct_fields),
                all_variant_cases: Rc::clone(&state.all_variant_cases),
                all_enum_cases: Rc::clone(&state.all_enum_cases),
                all_flags_cases: Rc::clone(&state.all_flags_cases),
                all_resource_types: Rc::clone(&state.all_resource_types),
                imported_type_sources,
                import_original_names,
                local_struct_fields: IndexMap::default(),
                local_newtypes: IndexMap::default(),
                local_generic_newtypes: IndexMap::default(),
                local_enum_cases: IndexMap::default(),
                local_flags_cases: IndexMap::default(),
                local_variant_cases: IndexMap::default(),
                function_return_types,
                imported_functions,
                namespace_imports,
                logger,
                current_module_source: ModuleSource::entry_point_uninitialized(), // Set in resolve_module
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
                interner: Rc::clone(&state.interner),
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
    /// Temporarily swaps the resolver's "current module" perspective to
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
            ast::Expr::WithHandler(with_handler) => {
                // The LHS of `E = h` in a `with` clause is an effect
                // name, not a type name. The real resolver validates it
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

    /// Static version of `resolve_type` for use before the resolver is fully
    /// constructed. Reads type info via [`TypeLookup`] — the same path the
    /// fully-constructed resolver uses, so name resolution stays in one place.
    pub(super) fn resolve_type_static(
        ty: &Type,
        type_table: &mut TypeTable,
        lookup: &TypeLookup<'_>,
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check newtypes first
                if let Some(alias_type_id) = lookup.newtype(&named.name) {
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
                        // Use canonical name from info (not alias) for consistent TypeId interning
                        if let Some(info) = lookup.struct_fields(&named.name) {
                            type_table.make_struct(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = lookup.resource_type(&named.name) {
                            type_table.make_resource(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = lookup.variant_case(&named.name) {
                            type_table.make_variant(info.name.clone(), info.module_source.clone())
                        } else if let Some(info) = lookup.enum_case(&named.name) {
                            type_table.make_enum(info.name.clone(), info.module_source.clone())
                        } else {
                            // Flags are newtypes over u32 and should be picked up by the
                            // newtype branch above; this catches unknown names too.
                            TypeTable::UNKNOWN
                        }
                    }
                }
            }
            Type::Generic(generic) => match generic.name.as_str() {
                "Option" if !generic.args.is_empty() => {
                    let inner = Self::resolve_type_static(&generic.args[0], type_table, lookup);
                    type_table.make_option(inner)
                }
                _ => {
                    // Check if it's a generic struct type
                    if let Some(info) = lookup.struct_fields(&generic.name) {
                        let module_source = info.module_source.clone();
                        let type_args: Vec<TypeId> = generic
                            .args
                            .iter()
                            .map(|arg| Self::resolve_type_static(arg, type_table, lookup))
                            .collect();
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
                let inner_type = Self::resolve_type_static(inner, type_table, lookup);
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = Self::resolve_type_static(inner, type_table, lookup);
                type_table.make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => {
                // Handle builtin::array<T>
                if namespaced.namespace == "builtin"
                    && namespaced.name == "array"
                    && let Some(elem_ty) = namespaced.args.first()
                {
                    let elem = Self::resolve_type_static(elem_ty, type_table, lookup);
                    return type_table.make_builtin_array(elem);
                }
                TypeTable::UNKNOWN
            }
            Type::Tuple(elements) => {
                let elem_types: Vec<TypeId> = elements
                    .iter()
                    .map(|e| Self::resolve_type_static(e, type_table, lookup))
                    .collect();
                type_table.make_tuple(elem_types)
            }
            Type::Function(func_ty) => {
                // Resolve param / return types statically so that cross-module
                // consumers of `all_struct_fields` (e.g. the CM-boundary
                // closure check at item.rs) see a real `ResolvedType::Function`
                // rather than `UNKNOWN`. Effects/stores get resolved later when
                // a per-function scope exists; for this static pre-pass an
                // empty effect set is fine because callers only read
                // shape-level information (is the field a closure type?).
                let params: Vec<TypeId> = func_ty
                    .params
                    .iter()
                    .map(|p| Self::resolve_type_static(p, type_table, lookup))
                    .collect();
                let return_type =
                    Self::resolve_type_static(&func_ty.return_type, type_table, lookup);
                type_table.make_function_with_mut(
                    func_ty.is_mut,
                    params,
                    return_type,
                    Vec::new(),
                    Vec::new(),
                )
            }
            // TODO: ClosureType, etc. are not yet handled — returning UNKNOWN
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
                    if let Some(info) = lookup.struct_fields(&generic.name) {
                        let module_source = info.module_source.clone();
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
