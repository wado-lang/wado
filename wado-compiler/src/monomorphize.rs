//! Monomorphization pass for Wado TIR
//!
//! This phase instantiates generic structs and functions with concrete types.
//! Monomorphization is a separate compilation phase that runs after type resolution
//! and before the lower phase.
//!
//! The monomorphization process:
//! 1. Collect all generic struct and function definitions
//! 2. Find instantiation sites (`GenericInstance` types, generic function calls)
//! 3. Generate concrete struct and function definitions
//! 4. Rewrite types and function calls to use monomorphized names

mod call_rewrite;
mod func_inst;
mod state;
mod struct_inst;
mod substitute;

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::name::{FreeFunctionName, ModuleSource};

/// Returns the key used to store/look up a generic function in the global function map.
///
/// Methods use their unqualified name — the struct name already provides namespace.
/// Free functions are module-qualified to keep same-named generics from different
/// modules distinct (e.g., `wrap<T>` in `mod_a` vs `mod_b`).
fn generic_function_key(is_method: bool, module_source: &ModuleSource, name: &str) -> String {
    if is_method {
        name.to_string()
    } else {
        FreeFunctionName::from_module_source(module_source, name).to_string()
    }
}

use crate::project::Project;
use crate::tir::{ResolvedType, TirFunction, TirModule, TirStruct, TypeId, TypeTable};

use state::Monomorphizer;

/// Monomorphize a single TIR module
///
/// This performs monomorphization of generic types and functions
/// within a single module without cross-module generic function support.
pub fn monomorphize_module(module: TirModule) -> TirModule {
    let module_source = module.module_source.clone();
    let mut monomorph = Monomorphizer::new(module_source);
    monomorph.monomorphize_with_externals(
        module,
        &IndexMap::default(),
        &IndexMap::default(),
        &IndexMap::default(),
    )
}

/// Monomorphize a Project (Project -> Project)
///
/// This is the main entry point for the monomorphize phase. It monomorphizes all TIR modules
/// in the project with cross-module generic function support.
pub fn monomorphize_project(mut project: Project) -> Project {
    project.tir_modules = monomorphize_modules_indexed(project.tir_modules);

    // Strip effect params from all functions. Effect params have been validated by the
    // effect checker (which runs before monomorphization) and are not needed downstream.
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            func.effects.retain(|e| !e.is_param());
        }
    }

    project
}

/// Monomorphize multiple modules with cross-module generic function and struct support
///
/// This function enables monomorphization of generic functions and structs defined in one module
/// but used in another (e.g., Array methods from prelude, `TreeMap` from prelude used in user code).
///
/// IMPORTANT: Requires unified type tables - all modules must share the same `TypeTable`
/// so that `TypeIds` are valid across modules.
pub fn monomorphize_modules_indexed(
    modules: IndexMap<ModuleSource, TirModule>,
) -> IndexMap<ModuleSource, TirModule> {
    // First pass: collect all generic functions from all modules.
    let mut all_generic_functions: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
    for (module_source, module) in &modules {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                let key = generic_function_key(func.is_method(), module_source, &func.name);
                all_generic_functions.insert(key, Rc::clone(func_rc));
            }
        }
    }

    // Collect all generic structs from all modules, tracking ALL source modules
    // (a struct name can appear in multiple modules due to shadowing)
    // This includes private structs as they may be needed for instantiating public structs
    // (e.g., TreeMap uses TreeMapNode internally)
    let mut all_generic_structs: IndexMap<String, Vec<(ModuleSource, TirStruct)>> =
        IndexMap::default();
    for (module_source, module) in &modules {
        for tir_struct in &module.structs {
            if !tir_struct.type_params.is_empty() {
                all_generic_structs
                    .entry(tir_struct.name.clone())
                    .or_default()
                    .push((module_source.clone(), tir_struct.clone()));
            }
        }
    }

    // Identify entry module and its generic struct names (for shadowing detection)
    // Entry module is the one with ModuleSource::EntryPoint or the last module (user's file)
    let entry_module_source = modules
        .keys()
        .find(|s| matches!(s, ModuleSource::EntryPoint { .. }))
        .cloned()
        .unwrap_or_else(|| {
            modules
                .keys()
                .last()
                .cloned()
                .expect("monomorphize_modules_indexed called with empty modules")
        });

    let entry_generic_struct_names: IndexSet<String> = modules
        .get(&entry_module_source)
        .map(|m| {
            m.structs
                .iter()
                .filter(|s| !s.type_params.is_empty())
                .map(|s| s.name.clone())
                .collect()
        })
        .unwrap_or_default();

    // Collect all concrete trait method functions from all modules.
    // Maps function name (e.g., "i32^Stringify::to_str") → module source.
    // This enables correct module resolution when monomorphizing type param
    // receiver calls (e.g., T^Trait::method → ConcreteType^Trait::method).
    let mut trait_method_locations: IndexMap<String, ModuleSource> = IndexMap::default();
    for (module_source, module) in &modules {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            // Only collect non-generic trait methods (concrete impls like "i32^Stringify::to_str")
            if !func.has_real_type_params()
                && func.impl_type_params.is_empty()
                && let Some(ref info) = func.method_info
                && info.trait_name.is_some()
            {
                trait_method_locations.insert(func.name.clone(), module_source.clone());
            }
        }
    }

    // Second pass: monomorphize each module using the combined generic functions and structs
    modules
        .into_iter()
        .map(|(module_source, module)| {
            (
                module_source.clone(),
                monomorphize_with_externals(
                    module,
                    &module_source,
                    &entry_module_source,
                    &entry_generic_struct_names,
                    &all_generic_functions,
                    &all_generic_structs,
                    &trait_method_locations,
                ),
            )
        })
        .collect()
}

/// Monomorphize a single module with access to cross-module generic functions and structs
fn monomorphize_with_externals(
    module: TirModule,
    current_module_source: &ModuleSource,
    entry_module_source: &ModuleSource,
    entry_generic_struct_names: &IndexSet<String>,
    all_generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    all_generic_structs_with_sources: &IndexMap<String, Vec<(ModuleSource, TirStruct)>>,
    trait_method_locations: &IndexMap<String, ModuleSource>,
) -> TirModule {
    let is_entry_module = current_module_source == entry_module_source;

    // Find modules whose structs are shadowed by the entry module's definitions
    // This is computed globally, not per-module, because we want consistent shadowing
    let mut shadowed_modules: IndexSet<ModuleSource> = IndexSet::default();
    for entry_struct_name in entry_generic_struct_names {
        if let Some(sources) = all_generic_structs_with_sources.get(entry_struct_name) {
            // Find external modules that define this struct (not the entry module)
            for (external_module_source, _) in sources {
                if external_module_source != entry_module_source {
                    shadowed_modules.insert(external_module_source.clone());
                }
            }
        }
    }

    // Build generic structs map based on whether this is the entry module or not
    let mut all_generic_structs: IndexMap<String, TirStruct> = IndexMap::default();

    if is_entry_module {
        // Entry module: use its own structs + non-shadowed external structs
        for (name, sources) in all_generic_structs_with_sources {
            let mut selected: Option<&TirStruct> = None;

            // First, try to find local definition (entry module's own struct)
            for (source, tir_struct) in sources {
                if source == entry_module_source {
                    selected = Some(tir_struct);
                    break;
                }
            }

            // If no local definition, try external (from non-shadowed modules)
            if selected.is_none() {
                for (source, tir_struct) in sources {
                    if !shadowed_modules.contains(source) {
                        selected = Some(tir_struct);
                        break;
                    }
                }
            }

            if let Some(tir_struct) = selected {
                all_generic_structs.insert(name.clone(), tir_struct.clone());
            }
        }
    } else {
        // Non-entry module: use structs from any non-shadowed module.
        // This enables cross-module monomorphization (e.g., `./treemap-mod.wado`
        // can instantiate `ArrayIter<TreeMapEntry<String,Value>>` from core:prelude).
        for (name, sources) in all_generic_structs_with_sources {
            // Skip if this struct name is defined in entry module (shadowed)
            if entry_generic_struct_names.contains(name) {
                continue;
            }

            // Prefer structs from the current module, fall back to any non-shadowed module
            let mut selected: Option<&TirStruct> = None;
            for (source, tir_struct) in sources {
                if source == current_module_source {
                    selected = Some(tir_struct);
                    break;
                }
            }
            if selected.is_none() {
                for (source, tir_struct) in sources {
                    if !shadowed_modules.contains(source) {
                        selected = Some(tir_struct);
                        break;
                    }
                }
            }
            if let Some(tir_struct) = selected {
                all_generic_structs.insert(name.clone(), tir_struct.clone());
            }
        }
    }

    let mut monomorph = Monomorphizer::new(current_module_source.clone());
    monomorph.monomorphize_with_externals(
        module,
        all_generic_functions,
        &all_generic_structs,
        trait_method_locations,
    )
}

/// Determine the module where trait implementations for a concrete type are defined.
/// Used when substituting a type parameter receiver (e.g., `T^Ord::cmp` → `i32^Ord::cmp`)
/// to set the correct `module_source` so DCE can find the target function.
fn module_source_for_trait_impl(type_table: &TypeTable, type_id: TypeId) -> Option<ModuleSource> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(_) => Some(ModuleSource::primitive()),
        ResolvedType::BuiltinArray(_) => Some(ModuleSource::prelude()),
        ResolvedType::Struct { module_source, .. }
        | ResolvedType::GenericInstance { module_source, .. }
        | ResolvedType::Enum { module_source, .. }
        | ResolvedType::Variant { module_source, .. } => Some(module_source.clone()),
        ResolvedType::Tuple(_) => Some(ModuleSource::core("serde")),
        _ => None,
    }
}

impl Monomorphizer {
    /// Perform monomorphization on a module, optionally with access to external generic
    /// functions and structs from other modules (e.g., Array methods from prelude).
    ///
    /// IMPORTANT: Requires unified type tables - `TypeIds` in external generics
    /// must be valid in the module's `type_table`.
    fn monomorphize_with_externals(
        &mut self,
        mut module: TirModule,
        external_generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        external_generic_structs: &IndexMap<String, TirStruct>,
        trait_method_locations: &IndexMap<String, ModuleSource>,
    ) -> TirModule {
        self.functions
            .trait_method_locations
            .clone_from(trait_method_locations);

        // Phase 1: Collect all generic struct definitions
        // Include both local structs AND external generic structs from other modules
        let mut generic_structs: IndexMap<String, TirStruct> = external_generic_structs.clone();

        // Local generic structs override external ones (allows module-local specialization)
        // This handles the case where user defines their own TreeMap that shadows prelude's
        for tir_struct in &module.structs {
            if !tir_struct.type_params.is_empty() {
                generic_structs.insert(tir_struct.name.clone(), tir_struct.clone());
            }
        }

        // Store in module for later phases
        module.generic_structs.clone_from(&generic_structs);

        // Build set of valid struct names for collection
        let valid_struct_names: IndexSet<String> = generic_structs.keys().cloned().collect();

        // Phase 2-4: Collect and instantiate structs iteratively
        // This is done in a loop because instantiating a struct (like TreeMap<String,i32>)
        // may create new GenericInstance types in its fields (like BTreeNode<String,i32>)
        // that also need to be instantiated.
        let mut new_structs = Vec::new();
        loop {
            // Collect instantiation sites from current type table
            self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);

            // If no new structs to instantiate, we're done
            if self.structs.pending.is_empty() {
                break;
            }

            // Process all pending struct instantiations
            while let Some(key) = self.structs.pending.pop() {
                if let Some(generic_struct) = generic_structs.get(&key.name)
                    && let Some(concrete) = self.instantiate_struct(
                        generic_struct,
                        &key,
                        &mut module.type_table.borrow_mut(),
                    )
                {
                    new_structs.push(concrete);
                }
            }
        }

        // Add monomorphized structs to module
        module.structs.extend(new_structs);

        // Phase 5: Remove generic structs from the concrete struct list
        module
            .structs
            .retain(|s| s.type_params.is_empty() || s.monomorph_info.is_some());

        // Phase 6: Rewrite all GenericInstance type_ids to concrete struct type_ids
        self.rewrite_types_in_module(&mut module);

        // Phase 7: Collect all generic function definitions
        // Include both local functions AND external generic functions from other modules
        let mut generic_functions: IndexMap<String, Rc<RefCell<TirFunction>>> =
            external_generic_functions.clone();

        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                let key =
                    generic_function_key(func.is_method(), &self.current_module_source, &func.name);
                generic_functions.insert(key, Rc::clone(func_rc));
            }
        }

        // Store in module for later phases
        module.generic_functions.clone_from(&generic_functions);

        // Phase 8: Collect function instantiation sites from Call expressions
        self.collect_function_instantiation_sites(&module, &generic_functions);

        // Phase 9: Process function instantiations and generate concrete functions
        // Use iterative approach: each newly instantiated function may have method calls
        // that need to be instantiated too (e.g., a generic method calling another generic
        // method on self, like sort() -> sort_by())
        //
        // For transitive scanning, exclude bodyless functions (builtins like array_new,
        // array_set, etc.) which are codegen intrinsics and must not be re-monomorphized.
        let scannable_generic_functions: IndexMap<String, Rc<RefCell<TirFunction>>> =
            generic_functions
                .iter()
                .filter(|(_, f)| f.borrow().body.is_some())
                .map(|(k, v)| (k.clone(), Rc::clone(v)))
                .collect();

        // Phase 9: Unified instantiation loop
        // Process functions and structs together until fixpoint. Function instantiation
        // may create new GenericInstance types that require struct instantiation, which
        // in turn may create new function instantiation sites. Processing them in a
        // single loop eliminates the need for the separate "Phase 13" second pass.
        let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        loop {
            let mut made_progress = false;

            // Process all pending function instantiations
            while let Some(key) = self.functions.pending.pop() {
                let concrete = {
                    let generic_func = generic_functions.get(&key.name);
                    if let Some(gf) = generic_func {
                        let gf_borrowed = gf.borrow();
                        self.instantiate_function(
                            &gf_borrowed,
                            &key,
                            &mut module.type_table.borrow_mut(),
                        )
                    } else {
                        None
                    }
                };

                if let Some(concrete) = concrete {
                    // Collect instantiation sites from the newly created function body
                    if let Some(body) = &concrete.body {
                        let type_table = module.type_table.borrow();
                        let mut collector = func_inst::InstantiationCollector {
                            mono: self,
                            generic_functions: &scannable_generic_functions,
                            type_table: &type_table,
                        };
                        use crate::tir_visitor::TirRefVisitor;
                        collector.visit_block(body);
                    }
                    new_functions.push(Rc::new(RefCell::new(concrete)));
                    made_progress = true;
                }
            }

            // Check for new struct instantiations created by function monomorphization
            self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);
            while let Some(key) = self.structs.pending.pop() {
                if let Some(generic_struct) = generic_structs.get(&key.name)
                    && let Some(concrete) = self.instantiate_struct(
                        generic_struct,
                        &key,
                        &mut module.type_table.borrow_mut(),
                    )
                {
                    module.structs.push(concrete);
                    made_progress = true;
                }
            }

            if !made_progress {
                break;
            }
        }

        // Phase 10: Add monomorphized functions to module
        module.functions.extend(new_functions);

        // Phase 11: Remove generic functions from the functions list
        module.functions.retain(|f| {
            let func = f.borrow();
            (!func.has_real_type_params() && func.impl_type_params.is_empty())
                || func.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module);

        // Phase 12.5: Lower remaining comparison operators on non-primitive types to
        // trait method calls. This handles comparisons in concrete (non-generic) functions
        // on Struct/Variant/GenericInstance types that weren't resolved at resolve time.
        func_inst::lower_comparisons_in_module(&mut module, &self.functions.trait_method_locations);

        // Phase 13: Rewrite types (single pass — unified loop above ensures all structs exist)
        self.rewrite_types_in_module(&mut module);

        module
    }
}
