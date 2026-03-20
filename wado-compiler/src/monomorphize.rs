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

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::name::{
    FreeFunctionName, LocalMethodName, MethodName, ModuleSource, mangle_generic_name,
};

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
use crate::tir::{
    CallArg, FunctionRef, InstantiationKey, MonomorphInfo, ResolvedType, TirBinaryOp, TirBlock,
    TirExpr, TirExprKind, TirField, TirFunction, TirModule, TirParam, TirPattern, TirStmt,
    TirStmtKind, TirStruct, TirTemplatePart, TirUnaryOp, TypeId, TypeTable,
};
use crate::token::Span;

/// Monomorphize a single TIR module
///
/// This performs monomorphization of generic types and functions
/// within a single module without cross-module generic function support.
pub fn monomorphize_module(module: TirModule) -> TirModule {
    let module_source = module.module_source.clone();
    let mut monomorph = Monomorphizer::new(module_source);
    monomorph.monomorphize(module)
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

/// Monomorphizer collects generic instantiations and generates concrete types
struct Monomorphizer {
    /// The module source where monomorphized entities are being generated
    current_module_source: ModuleSource,
    /// Map from (`generic_name`, `type_args`) to mangled name for structs
    instantiated: IndexMap<InstantiationKey, String>,
    /// Work queue of pending struct instantiations
    pending: Vec<InstantiationKey>,
    /// Map from `GenericInstance` `TypeId` to monomorphized Struct `TypeId`
    type_substitutions: IndexMap<TypeId, TypeId>,
    /// Map from `GenericInstance` `TypeId` to mangled struct name
    type_to_mangled_name: IndexMap<TypeId, String>,
    /// Map from (`generic_func_name`, `type_args`) to mangled function name
    function_instantiated: IndexMap<InstantiationKey, String>,
    /// Work queue of pending function instantiations
    function_pending: Vec<InstantiationKey>,
    /// Reverse lookup: mangled struct name -> `InstantiationKey`
    mangled_struct_to_key: IndexMap<String, InstantiationKey>,
    /// Reverse lookup: mangled function name -> `InstantiationKey`
    mangled_func_to_key: IndexMap<String, InstantiationKey>,
    /// Map from concrete trait method function name → module where it's defined.
    /// Used to resolve the correct module when substituting type param receivers.
    trait_method_locations: IndexMap<String, ModuleSource>,
}

impl Monomorphizer {
    fn new(current_module_source: ModuleSource) -> Self {
        Self {
            current_module_source,
            instantiated: IndexMap::default(),
            pending: Vec::new(),
            type_substitutions: IndexMap::default(),
            type_to_mangled_name: IndexMap::default(),
            function_instantiated: IndexMap::default(),
            function_pending: Vec::new(),
            mangled_struct_to_key: IndexMap::default(),
            mangled_func_to_key: IndexMap::default(),
            trait_method_locations: IndexMap::default(),
        }
    }

    /// Perform monomorphization on a module
    fn monomorphize(&mut self, mut module: TirModule) -> TirModule {
        // Phase 1: Collect all generic struct definitions
        let generic_structs: IndexMap<String, TirStruct> = module
            .structs
            .iter()
            .filter(|s| !s.type_params.is_empty())
            .map(|s| (s.name.clone(), s.clone()))
            .collect();

        // Store in module for later phases
        module.generic_structs = generic_structs.clone();

        // Phase 2-4: Collect and instantiate structs iteratively
        // This is done in a loop because instantiating a struct (like TreeMap<String,i32>)
        // may create new GenericInstance types in its fields (like BTreeNode<String,i32>)
        // that also need to be instantiated.
        // Build set of valid struct names for collection
        let valid_struct_names: IndexSet<String> = generic_structs.keys().cloned().collect();

        let mut new_structs = Vec::new();
        loop {
            // Collect instantiation sites from current type table
            self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);

            // If no new structs to instantiate, we're done
            if self.pending.is_empty() {
                break;
            }

            // Process all pending struct instantiations
            while let Some(key) = self.pending.pop() {
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
        // (they stay in generic_structs for reference)
        module
            .structs
            .retain(|s| s.type_params.is_empty() || s.monomorph_info.is_some());

        // Phase 6: Rewrite all GenericInstance type_ids to concrete struct type_ids
        self.rewrite_types_in_module(&mut module);

        // Phase 7: Collect all generic function definitions
        // Include both functions with method-level type params AND methods from generic impl blocks
        let generic_functions: IndexMap<String, Rc<RefCell<TirFunction>>> = module
            .functions
            .iter()
            .filter(|f| {
                let func = f.borrow();
                func.has_real_type_params() || !func.impl_type_params.is_empty()
            })
            .map(|f| (f.borrow().name.clone(), Rc::clone(f)))
            .collect();

        // Store in module for later phases
        module.generic_functions = generic_functions.clone();

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

        let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        while let Some(key) = self.function_pending.pop() {
            // Instantiate the function (needs mutable borrow)
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
                // This handles transitive monomorphization (e.g., a generic method calling
                // another generic method on self, like sort() -> sort_by())
                if let Some(body) = &concrete.body {
                    self.collect_func_instantiation_sites_in_block(
                        body,
                        &scannable_generic_functions,
                        &module.type_table.borrow(),
                    );
                }
                new_functions.push(Rc::new(RefCell::new(concrete)));
            }
        }

        // Phase 10: Add monomorphized functions to module
        module.functions.extend(new_functions);

        // Phase 11: Remove generic functions from the functions list
        // (they stay in generic_functions for reference)
        // Remove functions with type_params OR impl_type_params (unless monomorphized)
        // Effect-only params don't count as generic (they're erased at compile time).
        module.functions.retain(|f| {
            let func = f.borrow();
            (!func.has_real_type_params() && func.impl_type_params.is_empty())
                || func.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module);

        // Phase 12.5: Desugar comparison operators on non-primitive types in non-generic functions.
        // (Generic functions are handled during substitute_types_in_expr, but non-generic
        // functions with variant/struct == never go through that path.)
        self.desugar_comparisons_in_module(&mut module);

        // Phase 13: Second pass of struct instantiation
        // Function monomorphization may have created new GenericInstance types
        // (e.g., BTreeNode<String,i32>) that weren't in the type table during Phase 2.
        // Collect and instantiate these now.
        self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);
        let mut second_pass_structs = Vec::new();
        while let Some(key) = self.pending.pop() {
            if let Some(generic_struct) = generic_structs.get(&key.name)
                && let Some(concrete) = self.instantiate_struct(
                    generic_struct,
                    &key,
                    &mut module.type_table.borrow_mut(),
                )
            {
                second_pass_structs.push(concrete);
            }
        }
        module.structs.extend(second_pass_structs);
        // Rewrite types again for any new struct instantiations
        self.rewrite_types_in_module(&mut module);

        module
    }

    /// Perform monomorphization with access to external generic functions
    ///
    /// This enables monomorphization of generic functions and structs defined in other modules
    /// (e.g., Array methods from prelude, `TreeMap` from prelude used in user code).
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
        self.trait_method_locations = trait_method_locations.clone();

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
        module.generic_structs = generic_structs.clone();

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
            if self.pending.is_empty() {
                break;
            }

            // Process all pending struct instantiations
            while let Some(key) = self.pending.pop() {
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
        module.generic_functions = generic_functions.clone();

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

        let mut new_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
        while let Some(key) = self.function_pending.pop() {
            // Instantiate the function (needs mutable borrow)
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
                // This handles transitive monomorphization (e.g., a generic method calling
                // another generic method on self, like sort() -> sort_by())
                if let Some(body) = &concrete.body {
                    self.collect_func_instantiation_sites_in_block(
                        body,
                        &scannable_generic_functions,
                        &module.type_table.borrow(),
                    );
                }
                new_functions.push(Rc::new(RefCell::new(concrete)));
            }
        }

        // Phase 10: Add monomorphized functions to module
        module.functions.extend(new_functions);

        // Phase 11: Remove generic functions from the functions list
        // Remove functions with type_params OR impl_type_params (unless monomorphized)
        // Effect-only params don't count as generic (they're erased at compile time).
        module.functions.retain(|f| {
            let func = f.borrow();
            (!func.has_real_type_params() && func.impl_type_params.is_empty())
                || func.monomorph_info.is_some()
        });

        // Phase 12: Rewrite function calls to use monomorphized names
        self.rewrite_function_calls_in_module(&mut module);

        // Phase 12.5: Desugar comparison operators on non-primitive types in non-generic functions.
        // (Generic functions are handled during substitute_types_in_expr, but non-generic
        // functions with variant/struct == never go through that path.)
        self.desugar_comparisons_in_module(&mut module);

        // Phase 13: Second pass of struct instantiation
        // Function monomorphization may have created new GenericInstance types
        // (e.g., BTreeNode<String,i32>) that weren't in the type table during Phase 2.
        // Collect and instantiate these now.
        self.collect_instantiation_sites(&module.type_table.borrow(), &valid_struct_names);
        let mut second_pass_structs = Vec::new();
        while let Some(key) = self.pending.pop() {
            if let Some(generic_struct) = generic_structs.get(&key.name)
                && let Some(concrete) = self.instantiate_struct(
                    generic_struct,
                    &key,
                    &mut module.type_table.borrow_mut(),
                )
            {
                second_pass_structs.push(concrete);
            }
        }
        module.structs.extend(second_pass_structs);
        // Rewrite types again for any new struct instantiations
        self.rewrite_types_in_module(&mut module);

        module
    }

    /// Rewrite all `GenericInstance` `type_ids` in expressions to use monomorphized struct types
    fn rewrite_types_in_module(&self, module: &mut TirModule) {
        // Rewrite struct field types
        for strct in &mut module.structs {
            for field in &mut strct.fields {
                field.type_id =
                    self.rewrite_type_id(field.type_id, &mut module.type_table.borrow_mut());
            }
        }

        // Rewrite function signatures and bodies
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            // Rewrite function parameters
            for param in &mut func.params {
                param.type_id =
                    self.rewrite_type_id(param.type_id, &mut module.type_table.borrow_mut());
            }
            // Rewrite return type
            func.return_type =
                self.rewrite_type_id(func.return_type, &mut module.type_table.borrow_mut());
            // Rewrite local_types
            for local_type in &mut func.local_types {
                *local_type =
                    self.rewrite_type_id(*local_type, &mut module.type_table.borrow_mut());
            }
            // Rewrite function body
            if let Some(body) = &mut func.body {
                self.rewrite_types_in_block(body, &mut module.type_table.borrow_mut());
            }
        }

        // Rewrite global variable initializers
        for global in &mut module.globals {
            global.ty = self.rewrite_type_id(global.ty, &mut module.type_table.borrow_mut());
            self.rewrite_types_in_expr(
                &mut global.initializer,
                &mut module.type_table.borrow_mut(),
            );
        }
    }

    fn rewrite_types_in_block(&self, block: &mut TirBlock, type_table: &mut TypeTable) {
        for stmt in &mut block.stmts {
            self.rewrite_types_in_stmt(stmt, type_table);
        }
    }

    fn rewrite_types_in_stmt(&self, stmt: &mut TirStmt, type_table: &mut TypeTable) {
        match &mut stmt.kind {
            TirStmtKind::Let { type_id, value, .. } => {
                *type_id = self.rewrite_type_id(*type_id, type_table);
                self.rewrite_types_in_expr(value, type_table);
            }
            TirStmtKind::Expr(expr) => {
                self.rewrite_types_in_expr(expr, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.rewrite_types_in_expr(expr, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.rewrite_types_in_expr(condition, type_table);
                self.rewrite_types_in_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.rewrite_types_in_block(else_blk, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.rewrite_types_in_block(body, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.rewrite_types_in_expr(v, type_table);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.rewrite_types_in_block(block, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.rewrite_types_in_expr(scrutinee, type_table);
                self.rewrite_types_in_pattern(pattern, type_table);
                self.rewrite_types_in_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.rewrite_types_in_block(else_blk, type_table);
                }
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.rewrite_types_in_pattern(pattern, type_table);
                self.rewrite_types_in_expr(value, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    fn rewrite_types_in_pattern(&self, pattern: &mut TirPattern, type_table: &mut TypeTable) {
        match pattern {
            TirPattern::Wildcard | TirPattern::Binding { .. } | TirPattern::Literal(_) => {}
            TirPattern::Tuple(patterns) => {
                for p in patterns {
                    self.rewrite_types_in_pattern(p, type_table);
                }
            }
            TirPattern::Variant {
                enum_type,
                bindings,
                ..
            } => {
                *enum_type = self.rewrite_type_id(*enum_type, type_table);
                for binding in bindings {
                    self.rewrite_types_in_pattern(binding, type_table);
                }
            }
            TirPattern::Enum { enum_type, .. } => {
                *enum_type = self.rewrite_type_id(*enum_type, type_table);
            }
            TirPattern::Struct {
                struct_type,
                fields,
                ..
            } => {
                *struct_type = self.rewrite_type_id(*struct_type, type_table);
                for field in fields {
                    self.rewrite_types_in_pattern(&mut field.pattern, type_table);
                }
            }
        }
    }

    fn rewrite_types_in_expr(&self, expr: &mut TirExpr, type_table: &mut TypeTable) {
        // Rewrite the expression's own type_id
        expr.type_id = self.rewrite_type_id(expr.type_id, type_table);

        // Recursively rewrite types in sub-expressions
        match &mut expr.kind {
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                let original_type_id = *struct_type;
                let new_type_id = self.rewrite_type_id(original_type_id, type_table);
                *struct_type = new_type_id;
                // Update struct_name if it was monomorphized
                // First try lookup by original type_id
                if let Some(mangled_name) = self.type_to_mangled_name.get(&original_type_id) {
                    *struct_name = mangled_name.clone();
                } else {
                    // Derive struct_name from the resolved type
                    match type_table.get(new_type_id) {
                        ResolvedType::Struct { name, .. } => {
                            *struct_name = name.clone();
                        }
                        ResolvedType::GenericInstance {
                            name, type_args, ..
                        } => {
                            // Build mangled name from GenericInstance
                            let type_names: Vec<String> = type_args
                                .iter()
                                .map(|&arg| type_table.mangle_type_name(arg))
                                .collect();
                            *struct_name = mangle_generic_name(name, &type_names);
                        }
                        _ => {}
                    }
                }
                for field in fields {
                    self.rewrite_types_in_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.rewrite_types_in_expr(left, type_table);
                self.rewrite_types_in_expr(right, type_table);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.rewrite_types_in_expr(inner, type_table);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.rewrite_types_in_expr(&mut arg.expr, type_table);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.rewrite_types_in_expr(receiver, type_table);
                for arg in args {
                    self.rewrite_types_in_expr(&mut arg.expr, type_table);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.rewrite_types_in_expr(arg, type_table);
                }
            }
            TirExprKind::Block(block) => {
                self.rewrite_types_in_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.rewrite_types_in_expr(condition, type_table);
                self.rewrite_types_in_block(then_branch, type_table);
                if let Some(else_blk) = else_branch {
                    self.rewrite_types_in_block(else_blk, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.rewrite_types_in_expr(elem, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.rewrite_types_in_expr(target, type_table);
                self.rewrite_types_in_expr(value, type_table);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.rewrite_types_in_expr(inner, type_table);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.rewrite_types_in_expr(inner, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.rewrite_types_in_expr(array, type_table);
                self.rewrite_types_in_expr(index, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.rewrite_types_in_expr(scrutinee, type_table);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.rewrite_types_in_expr(guard, type_table);
                    }
                    self.rewrite_types_in_expr(&mut arm.body, type_table);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.rewrite_types_in_expr(body, type_table);
            }
            // Literals and simple expressions don't need rewriting
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::GlobalVarSet { value, .. } => {
                self.rewrite_types_in_expr(value, type_table);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.rewrite_types_in_expr(payload_expr, type_table);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.rewrite_types_in_expr(callee, type_table);
                for arg in args {
                    self.rewrite_types_in_expr(arg, type_table);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.rewrite_types_in_expr(functor, type_table);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.rewrite_types_in_block(block, type_table);
            }
            TirExprKind::VariantTag { expr } => {
                self.rewrite_types_in_expr(expr, type_table);
            }
            TirExprKind::VariantTest { expr, .. } => {
                self.rewrite_types_in_expr(expr, type_table);
            }
            TirExprKind::VariantPayload { expr, .. } => {
                self.rewrite_types_in_expr(expr, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.rewrite_types_in_expr(scrutinee, type_table);
                for arm in arms {
                    self.rewrite_types_in_block(arm, type_table);
                }
                self.rewrite_types_in_block(default, type_table);
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.rewrite_types_in_expr(inner, type_table);
                    }
                }
            }
        }
    }

    /// Rewrite a single `type_id`: if it's a `GenericInstance`, return the concrete struct `type_id`.
    /// Also handles container types (Array, Option, Tuple) that may contain `GenericInstance`.
    fn rewrite_type_id(&self, type_id: TypeId, type_table: &mut TypeTable) -> TypeId {
        // First check direct substitution
        if let Some(&new_id) = self.type_substitutions.get(&type_id) {
            return new_id;
        }

        // Handle container types that may contain GenericInstance
        match type_table.get(type_id).clone() {
            ResolvedType::BuiltinArray(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.intern(ResolvedType::BuiltinArray(new_inner_id))
                }
            }
            ResolvedType::Ref(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.make_ref(new_inner_id)
                }
            }
            ResolvedType::MutRef(inner_id) => {
                let new_inner_id = self.rewrite_type_id(inner_id, type_table);
                if new_inner_id == inner_id {
                    type_id
                } else {
                    type_table.make_mut_ref(new_inner_id)
                }
            }
            ResolvedType::Tuple(elem_ids) => {
                let new_elem_ids: Vec<TypeId> = elem_ids
                    .iter()
                    .map(|&id| self.rewrite_type_id(id, type_table))
                    .collect();
                if new_elem_ids == elem_ids {
                    type_id
                } else {
                    type_table.make_tuple(new_elem_ids)
                }
            }
            // Handle GenericInstance types that weren't in the direct substitution map
            // This can happen when function substitution creates new GenericInstance types
            // with different TypeIds for the type arguments
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Skip Array - it has special codegen handling and should remain
                // as GenericInstance, not be rewritten to Struct
                if name == "Array" {
                    return type_id;
                }

                // Build the mangled name using type names (not TypeIds)
                let type_names: Vec<String> = type_args
                    .iter()
                    .map(|&arg| type_table.mangle_type_name(arg))
                    .collect();
                let mangled_name = mangle_generic_name(&name, &type_names);

                // Look for existing Struct with this mangled name (ignore module_source)
                for tid in type_table.iter_type_ids() {
                    if let ResolvedType::Struct {
                        name: struct_name, ..
                    } = type_table.get(tid)
                        && struct_name == &mangled_name
                    {
                        return tid;
                    }
                }
                // If not found, return original type_id
                type_id
            }
            _ => type_id,
        }
    }

    /// Collect all `GenericInstance` types from the type table
    /// Only collects types whose base struct is in `valid_struct_names`
    fn collect_instantiation_sites(
        &mut self,
        type_table: &TypeTable,
        valid_struct_names: &IndexSet<String>,
    ) {
        for id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(id)
            {
                // Skip empty type_args (invalid generic instances)
                if type_args.is_empty() {
                    continue;
                }

                // Skip Array - it has special codegen handling and should not be
                // monomorphized as a regular struct
                if name == "Array" {
                    continue;
                }

                // Only collect if the struct is in our valid set
                // This prevents library modules from trying to instantiate entry module's structs
                if !valid_struct_names.contains(name) {
                    continue;
                }

                // Only process if all type args are concrete (no TypeParams)
                let all_concrete = type_args
                    .iter()
                    .all(|&arg| !type_table.contains_type_param(arg));

                if all_concrete {
                    let key = InstantiationKey {
                        name: name.clone(),
                        type_args: type_args.clone(),
                        method_info: None, // Struct instantiation,
                    };

                    if !self.instantiated.contains_key(&key) {
                        let mangled = self.instantiation_name(&key, type_table);
                        self.instantiated.insert(key.clone(), mangled.clone());
                        self.mangled_struct_to_key.insert(mangled, key.clone());
                        self.pending.push(key);
                    }
                }
            }
        }
    }

    /// Generate monomorphized struct name: `Box` + `[i32]` -> `"Box<i32>"`
    fn instantiation_name(&self, key: &InstantiationKey, type_table: &TypeTable) -> String {
        let args: Vec<String> = key
            .type_args
            .iter()
            .map(|&t| type_table.mangle_type_name(t))
            .collect();
        mangle_generic_name(&key.name, &args)
    }

    /// Instantiate a generic struct with concrete type arguments
    fn instantiate_struct(
        &mut self,
        generic: &TirStruct,
        key: &InstantiationKey,
        type_table: &mut TypeTable,
    ) -> Option<TirStruct> {
        let mangled_name = self.instantiated.get(key)?.clone();

        // Find the GenericInstance's module_source from the type table.
        // Use the generic's original module (where it was defined) for the struct type,
        // ensuring consistency across modules that share the same type table.
        let struct_module_source = type_table
            .iter_type_ids()
            .find_map(|id| {
                if let ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } = type_table.get(id)
                    && name == &key.name
                    && type_args == &key.type_args
                {
                    Some(module_source.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| self.current_module_source.clone());

        // Register the concrete struct type in the type table BEFORE substituting field types.
        // This is critical for self-referential structs like:
        //   struct Node<T> { left: Option<&mut Node<T>>, right: Option<&mut Node<T>> }
        // When substituting field types, the inner Node<T> needs to resolve to the
        // monomorphized struct type, not a GenericInstance.
        let concrete_type_id = type_table.make_monomorphized_struct(
            mangled_name.clone(),
            struct_module_source,
            key.name.clone(), // base_name: the original generic struct name
        );

        // Find the GenericInstance TypeId and record the substitution early
        // so that substitute_type can use it for self-references
        for id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(id)
                && name == &key.name
                && type_args == &key.type_args
            {
                self.type_substitutions.insert(id, concrete_type_id);
                self.type_to_mangled_name
                    .insert(id, self.instantiated.get(key).cloned().unwrap_or_default());
            }
        }

        // Build substitution map: type param index -> concrete type
        let substitution: IndexMap<u32, TypeId> = generic
            .type_params
            .iter()
            .zip(key.type_args.iter())
            .map(|(param, &arg)| (param.index, arg))
            .collect();

        // Substitute types in fields (now self-references can be resolved)
        let fields: Vec<TirField> = generic
            .fields
            .iter()
            .map(|field| {
                let new_type_id = self.substitute_type(field.type_id, &substitution, type_table);
                TirField {
                    name: field.name.clone(),
                    is_pub: field.is_pub,
                    type_id: new_type_id,
                    index: field.index,
                    span: field.span,
                    is_hidden: field.is_hidden,
                    serde_rename: field.serde_rename.clone(),
                    serde_default: field.serde_default,
                }
            })
            .collect();

        // Create the monomorphized struct
        let concrete = TirStruct {
            name: mangled_name.clone(),
            is_pub: generic.is_pub,
            type_params: vec![], // Concrete struct has no type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                type_args: key.type_args.clone(),
                is_blanket: false,
            }),
            fields,
            span: generic.span,
            serde_rename_all: generic.serde_rename_all.clone(),
        };

        Some(concrete)
    }

    /// Substitute type parameters in a type with concrete types
    fn substitute_type(
        &self,
        type_id: TypeId,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) -> TypeId {
        match type_table.get(type_id).clone() {
            ResolvedType::TypeParam { index, .. } => {
                // Direct substitution
                *substitution.get(&index).unwrap_or(&type_id)
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type(elem, substitution, type_table);
                type_table.intern(ResolvedType::BuiltinArray(new_elem))
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_ref(new_inner)
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type(inner, substitution, type_table);
                type_table.make_mut_ref(new_inner)
            }
            ResolvedType::Tuple(elems) => {
                let new_elems: Vec<TypeId> = elems
                    .iter()
                    .map(|&e| self.substitute_type(e, substitution, type_table))
                    .collect();
                type_table.make_tuple(new_elems)
            }
            ResolvedType::Function {
                params,
                return_type,
                effects,
                stores,
            } => {
                // Substitute type parameters in function parameter types and return type
                let new_params: Vec<TypeId> = params
                    .iter()
                    .map(|&p| self.substitute_type(p, substitution, type_table))
                    .collect();
                let new_return_type = self.substitute_type(return_type, substitution, type_table);
                type_table.make_function(new_params, new_return_type, effects, stores)
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // Handle invalid GenericInstance with empty type_args
                // This can occur when a generic type is referenced without type arguments in its own methods
                // e.g., in Container<T>::new(), the return value Container { ... } has type Container<T>
                // but may be represented as GenericInstance with empty type_args
                if type_args.is_empty() {
                    // If we're in a substitution context (substitution map is not empty),
                    // try to infer the type args from the substitution
                    if !substitution.is_empty() {
                        // Build mangled name using ALL values in substitution map
                        // Sort by param index to get correct order
                        let mut indexed_args: Vec<(u32, TypeId)> =
                            substitution.iter().map(|(&idx, &tid)| (idx, tid)).collect();
                        indexed_args.sort_by_key(|(idx, _)| *idx);

                        // Build name using new format: Name<Type1,Type2>
                        let type_names: Vec<String> = indexed_args
                            .iter()
                            .map(|(_, arg_id)| type_table.mangle_type_name(*arg_id))
                            .collect();
                        let mangled_name = mangle_generic_name(&name, &type_names);

                        // Look for monomorphized struct with this name
                        for tid in type_table.iter_type_ids() {
                            if let ResolvedType::Struct {
                                name: struct_name, ..
                            } = type_table.get(tid)
                                && struct_name == &mangled_name
                            {
                                return tid;
                            }
                        }
                    }

                    // Fallback: look for plain struct
                    for tid in type_table.iter_type_ids() {
                        if let ResolvedType::Struct {
                            name: struct_name,
                            module_source: struct_source,
                            ..
                        } = type_table.get(tid)
                            && struct_name == &name
                            && struct_source == &module_source
                        {
                            return tid;
                        }
                    }
                    // If not found, just return the original type_id
                    return type_id;
                }

                // Recursively substitute in nested generic instances
                let new_args: Vec<TypeId> = type_args
                    .iter()
                    .map(|&arg| self.substitute_type(arg, substitution, type_table))
                    .collect();

                // Check if there's already a monomorphized struct for this instance
                // Build the mangled name: Container<i32> (using type names)
                let type_names: Vec<String> = new_args
                    .iter()
                    .map(|&arg| type_table.mangle_type_name(arg))
                    .collect();
                let mangled_name = mangle_generic_name(&name, &type_names);

                // Look for existing struct with this name
                for tid in type_table.iter_type_ids() {
                    if let ResolvedType::Struct {
                        name: struct_name, ..
                    } = type_table.get(tid)
                        && struct_name == &mangled_name
                    {
                        return tid;
                    }
                }

                // Fallback to GenericInstance if no monomorphized struct found
                type_table.make_generic_instance(name, module_source, new_args)
            }
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                ..
            } => {
                // Substitute the underlying type param to get the concrete type
                let concrete_id = self.substitute_type(param_id, substitution, type_table);
                if concrete_id != param_id
                    && let Some(resolved) = type_table.resolve_assoc_type(concrete_id, &assoc_name)
                {
                    return resolved;
                }
                // Fallback: return the original type (projection unresolved)
                type_id
            }
            // Other types don't contain type parameters
            _ => type_id,
        }
    }

    /// Collect function instantiation sites from Call/MethodCall/StaticCall expressions
    fn collect_function_instantiation_sites(
        &mut self,
        module: &TirModule,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
    ) {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            // Skip generic functions - their bodies contain TypeParam references that
            // would incorrectly queue instantiations with TypeParam TypeIds instead of
            // concrete types. We only scan concrete functions; generic function bodies
            // are scanned after instantiation in Phase 9.
            // Effect-only params don't count as generic.
            if func.has_real_type_params() || !func.impl_type_params.is_empty() {
                continue;
            }
            if let Some(body) = &func.body {
                self.collect_func_instantiation_sites_in_block(
                    body,
                    generic_functions,
                    &module.type_table.borrow(),
                );
            }
        }

        // Also scan global variable initializers for function instantiation sites
        for global in &module.globals {
            self.collect_func_instantiation_sites_in_expr(
                &global.initializer,
                generic_functions,
                &module.type_table.borrow(),
            );
        }
    }

    fn collect_func_instantiation_sites_in_block(
        &mut self,
        block: &TirBlock,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
    ) {
        for stmt in &block.stmts {
            self.collect_func_instantiation_sites_in_stmt(stmt, generic_functions, type_table);
        }
    }

    fn collect_func_instantiation_sites_in_stmt(
        &mut self,
        stmt: &TirStmt,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
    ) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_func_instantiation_sites_in_expr(value, generic_functions, type_table);
            }
            TirStmtKind::Expr(expr) => {
                self.collect_func_instantiation_sites_in_expr(expr, generic_functions, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.collect_func_instantiation_sites_in_expr(
                        expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    condition,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_block(
                    then_block,
                    generic_functions,
                    type_table,
                );
                if let Some(else_blk) = else_block {
                    self.collect_func_instantiation_sites_in_block(
                        else_blk,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirStmtKind::Loop { body } => {
                self.collect_func_instantiation_sites_in_block(body, generic_functions, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.collect_func_instantiation_sites_in_expr(v, generic_functions, type_table);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.collect_func_instantiation_sites_in_block(
                    block,
                    generic_functions,
                    type_table,
                );
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    scrutinee,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_block(
                    then_block,
                    generic_functions,
                    type_table,
                );
                if let Some(else_blk) = else_block {
                    self.collect_func_instantiation_sites_in_block(
                        else_blk,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.collect_func_instantiation_sites_in_expr(value, generic_functions, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    fn collect_func_instantiation_sites_in_expr(
        &mut self,
        expr: &TirExpr,
        generic_functions: &IndexMap<String, Rc<RefCell<TirFunction>>>,
        type_table: &TypeTable,
    ) {
        match &expr.kind {
            TirExprKind::Call {
                func,
                type_args,
                args,
                ..
            } => {
                let qualified_func_name =
                    generic_function_key(func.is_method(), &func.module_source, &func.name);
                // Check if this is a call to a generic function with explicit type args
                if !type_args.is_empty() && generic_functions.contains_key(&qualified_func_name) {
                    let key = InstantiationKey {
                        name: qualified_func_name.clone(),
                        type_args: type_args.clone(),
                        method_info: func.method_info.clone(),
                    };
                    if !self.function_instantiated.contains_key(&key) {
                        let mangled = self.function_instantiation_name(&key, type_table);
                        self.function_instantiated
                            .insert(key.clone(), mangled.clone());
                        self.mangled_func_to_key.insert(mangled, key.clone());
                        self.function_pending.push(key);
                    }
                }
                // Also check if this is a static method call on a monomorphized struct
                // (formerly StaticCall). Use method_info metadata to get struct/method name.
                if let FunctionRef {
                    method_info: Some(info),
                    monomorph_info: Some(monomorph),
                    ..
                } = func
                {
                    let mono_type_args = &monomorph.type_args;
                    if !mono_type_args.is_empty() {
                        let mut names_to_try = vec![MethodName::format_local(
                            &info.base_struct_name,
                            info.trait_name.as_deref(),
                            &info.method_name,
                        )];
                        if info.struct_name != info.base_struct_name {
                            names_to_try.push(MethodName::format_local(
                                &info.struct_name,
                                info.trait_name.as_deref(),
                                &info.method_name,
                            ));
                        }
                        for generic_method_name in names_to_try {
                            if let Some(generic_func_rc) =
                                generic_functions.get(&generic_method_name)
                            {
                                let generic_func = generic_func_rc.borrow();
                                if mono_type_args.len() >= generic_func.impl_type_params.len() {
                                    let method_info = generic_func.method_info.clone();
                                    let key = InstantiationKey {
                                        name: generic_method_name,
                                        type_args: mono_type_args.clone(),
                                        method_info,
                                    };
                                    if !self.function_instantiated.contains_key(&key) {
                                        let impl_type_arg_count = mono_type_args
                                            .len()
                                            .saturating_sub(generic_func.type_params.len());
                                        let mangled = self.method_instantiation_name(
                                            &key,
                                            type_table,
                                            impl_type_arg_count,
                                        );
                                        self.function_instantiated
                                            .insert(key.clone(), mangled.clone());
                                        self.mangled_func_to_key.insert(mangled, key.clone());
                                        self.function_pending.push(key);
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        &arg.expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                type_args,
                args,
                ..
            } => {
                // Extract method name from method_info or fall back to function name
                let method_name = method_func
                    .method_info
                    .clone()
                    .map(|info| info.method_name)
                    .unwrap_or_else(|| method_func.name.clone());
                // Check if this is a method call with explicit type args
                if !type_args.is_empty() {
                    // Get the struct name from the receiver type
                    if let Some(struct_name) =
                        self.get_struct_name_from_type(receiver.type_id, type_table)
                    {
                        // Try both inherent method and trait method formats
                        let trait_name_opt = method_func
                            .method_info
                            .clone()
                            .and_then(|info| info.trait_name.clone());
                        let mut names_to_try: Vec<(String, Option<String>)> = vec![(
                            MethodName::format_local(&struct_name, None, &method_name),
                            None,
                        )];
                        if let Some(ref tn) = trait_name_opt {
                            names_to_try.push((
                                MethodName::format_local(&struct_name, Some(tn), &method_name),
                                Some(tn.clone()),
                            ));
                        }

                        let mut found = false;
                        for (full_method_name, tn) in &names_to_try {
                            if let Some(gf) = generic_functions.get(full_method_name) {
                                let method_info =
                                    gf.borrow().method_info.clone().unwrap_or_else(|| {
                                        LocalMethodName::new(
                                            struct_name.clone(),
                                            tn.clone(),
                                            method_name.clone(),
                                        )
                                    });
                                let key = InstantiationKey {
                                    name: full_method_name.clone(),
                                    type_args: type_args.clone(),
                                    method_info: Some(method_info),
                                };
                                if !self.function_instantiated.contains_key(&key) {
                                    let mangled =
                                        self.function_instantiation_name(&key, type_table);
                                    self.function_instantiated
                                        .insert(key.clone(), mangled.clone());
                                    self.mangled_func_to_key.insert(mangled, key.clone());
                                    self.function_pending.push(key);
                                }
                                found = true;
                                break;
                            }
                        }
                        // Handle "double generics": method call with type_args on a monomorphized generic struct
                        // e.g., c.transform::<i64>(100) where c: Container<i32> and transform<U>
                        // Also handles GenericInstance receivers (e.g., Option<i32>)
                        if !found {
                            let base_info = self
                                .mangled_struct_to_key
                                .get(&struct_name)
                                .map(|k| (k.name.clone(), k.type_args.clone()))
                                .or_else(|| {
                                    self.get_struct_info_from_type(receiver.type_id, type_table)
                                        .filter(|(_, args)| !args.is_empty())
                                });
                            if let Some((base_struct, impl_type_args)) = base_info {
                                // Try both inherent and trait method formats
                                let mut dg_names: Vec<(String, Option<String>)> = vec![(
                                    MethodName::format_local(&base_struct, None, &method_name),
                                    None,
                                )];
                                if let Some(ref tn) = trait_name_opt {
                                    dg_names.push((
                                        MethodName::format_local(
                                            &base_struct,
                                            Some(tn),
                                            &method_name,
                                        ),
                                        Some(tn.clone()),
                                    ));
                                }

                                for (generic_method_name, tn) in &dg_names {
                                    if let Some(generic_func_rc) =
                                        generic_functions.get(generic_method_name)
                                    {
                                        let generic_func = generic_func_rc.borrow();
                                        if impl_type_args.len()
                                            >= generic_func.impl_type_params.len()
                                        {
                                            let mut combined_type_args = impl_type_args.clone();
                                            combined_type_args.extend(type_args.iter().copied());

                                            let method_info = LocalMethodName::new(
                                                base_struct.clone(),
                                                tn.clone(),
                                                method_name.clone(),
                                            );
                                            let key = InstantiationKey {
                                                name: generic_method_name.clone(),
                                                type_args: combined_type_args,
                                                method_info: Some(method_info),
                                            };
                                            if !self.function_instantiated.contains_key(&key) {
                                                let mangled = self.method_instantiation_name(
                                                    &key,
                                                    type_table,
                                                    impl_type_args.len(),
                                                );
                                                self.function_instantiated
                                                    .insert(key.clone(), mangled.clone());
                                                self.mangled_func_to_key
                                                    .insert(mangled, key.clone());
                                                self.function_pending.push(key);
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Also check if the receiver is a monomorphized generic struct
                // e.g., c.get() where c: Counter<i32>, or arr.append() where arr: Array<fn(i32)->i32>
                let struct_info = self.get_struct_info_from_type(receiver.type_id, type_table);
                if let Some((base_struct, impl_type_args)) = struct_info
                    && !impl_type_args.is_empty()
                {
                    // Try both regular method and trait method formats
                    // Method names to try: BaseStruct::method, BaseStruct^Trait::method (from method_info)
                    let mut names_to_try =
                        vec![MethodName::format_local(&base_struct, None, &method_name)];
                    // If the function is a trait method, also try the trait method format
                    if let Some(ref info) = method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                    {
                        names_to_try.push(MethodName::format_local(
                            &base_struct,
                            Some(trait_name),
                            &method_name,
                        ));
                    }

                    for generic_method_name in &names_to_try {
                        if let Some(generic_func_rc) = generic_functions.get(generic_method_name) {
                            let generic_func = generic_func_rc.borrow();
                            // Check if method has its own type params (double generics)
                            let has_method_type_params = generic_func.has_real_type_params();
                            // Queue if we have at least enough impl type args.
                            // impl_type_args may be longer than impl_type_params when the impl
                            // fixes some struct type params to concrete types
                            // (e.g., `impl Trait for Foo<Array<String>, V>` where only V is free).
                            if impl_type_args.len() >= generic_func.impl_type_params.len() {
                                // Combine impl type args with method type args
                                let mut combined_type_args = impl_type_args.clone();
                                if has_method_type_params && !type_args.is_empty() {
                                    combined_type_args.extend(type_args.iter().copied());
                                }
                                let method_info = generic_func.method_info.clone();
                                let key = InstantiationKey {
                                    name: generic_method_name.clone(),
                                    type_args: combined_type_args,
                                    method_info,
                                };
                                if !self.function_instantiated.contains_key(&key) {
                                    // Pass total struct type args count for name generation so that
                                    // concrete positions are included in the mangled struct name.
                                    let mangled = self.method_instantiation_name(
                                        &key,
                                        type_table,
                                        impl_type_args.len(),
                                    );
                                    self.function_instantiated
                                        .insert(key.clone(), mangled.clone());
                                    self.mangled_func_to_key.insert(mangled, key.clone());
                                    self.function_pending.push(key);
                                }
                                break; // Found a match, no need to try other names
                            }
                        }
                    }
                }

                // Also handle already-monomorphized structs via reverse lookup
                // e.g., c.add(10) where c: Container<i32>
                // Use get_struct_name_from_type to properly unwrap reference types (&T, &mut T)
                if let Some(struct_name) =
                    self.get_struct_name_from_type(receiver.type_id, type_table)
                    && let Some(struct_key) = self.mangled_struct_to_key.get(&struct_name)
                {
                    let base_struct = &struct_key.name;
                    let impl_type_args = struct_key.type_args.clone();

                    // Try both regular method and trait method formats
                    // Method names to try: BaseStruct::method, BaseStruct^Trait::method (from method_info)
                    let mut names_to_try =
                        vec![MethodName::format_local(base_struct, None, &method_name)];
                    // If the function is a trait method, also try the trait method format
                    if let Some(ref info) = method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                    {
                        names_to_try.push(MethodName::format_local(
                            base_struct,
                            Some(trait_name),
                            &method_name,
                        ));
                    }

                    for generic_method_name in names_to_try {
                        if let Some(generic_func_rc) = generic_functions.get(&generic_method_name) {
                            let generic_func = generic_func_rc.borrow();
                            // Check if method has its own type params (double generics)
                            let has_method_type_params = generic_func.has_real_type_params();
                            // Queue if we have at least enough impl type args.
                            // impl_type_args may be longer than impl_type_params when the impl
                            // fixes some struct type params to concrete types
                            // (e.g., `impl Trait for Foo<Array<String>, V>` where only V is free).
                            if impl_type_args.len() >= generic_func.impl_type_params.len() {
                                // Combine impl type args with method type args
                                let mut combined_type_args = impl_type_args.clone();
                                if has_method_type_params && !type_args.is_empty() {
                                    combined_type_args.extend(type_args.iter().copied());
                                }
                                let method_info = generic_func.method_info.clone();
                                let key = InstantiationKey {
                                    name: generic_method_name.clone(),
                                    type_args: combined_type_args,
                                    method_info,
                                };
                                if !self.function_instantiated.contains_key(&key) {
                                    // Pass total struct type args count for name generation so that
                                    // concrete positions are included in the mangled struct name.
                                    let mangled = self.method_instantiation_name(
                                        &key,
                                        type_table,
                                        impl_type_args.len(),
                                    );
                                    self.function_instantiated
                                        .insert(key.clone(), mangled.clone());
                                    self.mangled_func_to_key.insert(mangled, key.clone());
                                    self.function_pending.push(key);
                                }
                                break; // Found a match, no need to try other names
                            }
                        }
                    }
                }

                // Blanket impl fallback: if the FunctionRef has monomorph_info from a
                // blanket impl that matches a generic function template, queue the
                // instantiation using that template function.
                if let FunctionRef {
                    monomorph_info: Some(mono),
                    ..
                } = method_func
                    && mono.is_blanket
                    && let Some(generic_func_rc) = generic_functions.get(&mono.generic_name)
                {
                    let generic_func = generic_func_rc.borrow();
                    let method_info = generic_func.method_info.clone();
                    let key = InstantiationKey {
                        name: mono.generic_name.clone(),
                        type_args: mono.type_args.clone(),
                        method_info,
                    };
                    if !self.function_instantiated.contains_key(&key) {
                        let impl_type_params_count = generic_func.impl_type_params.len();
                        let mangled = self.method_instantiation_name_inner(
                            &key,
                            type_table,
                            impl_type_params_count,
                            &generic_func.impl_type_params,
                        );
                        self.function_instantiated
                            .insert(key.clone(), mangled.clone());
                        self.mangled_func_to_key.insert(mangled, key.clone());
                        self.function_pending.push(key);
                    }
                }

                self.collect_func_instantiation_sites_in_expr(
                    receiver,
                    generic_functions,
                    type_table,
                );
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        &arg.expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        arg,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_func_instantiation_sites_in_expr(left, generic_functions, type_table);
                self.collect_func_instantiation_sites_in_expr(right, generic_functions, type_table);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.collect_func_instantiation_sites_in_expr(inner, generic_functions, type_table);
            }
            TirExprKind::Block(block) => {
                self.collect_func_instantiation_sites_in_block(
                    block,
                    generic_functions,
                    type_table,
                );
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    condition,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_block(
                    then_branch,
                    generic_functions,
                    type_table,
                );
                if let Some(else_blk) = else_branch {
                    self.collect_func_instantiation_sites_in_block(
                        else_blk,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_func_instantiation_sites_in_expr(
                        elem,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_func_instantiation_sites_in_expr(
                    target,
                    generic_functions,
                    type_table,
                );
                self.collect_func_instantiation_sites_in_expr(value, generic_functions, type_table);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.collect_func_instantiation_sites_in_expr(inner, generic_functions, type_table);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.collect_func_instantiation_sites_in_expr(inner, generic_functions, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_func_instantiation_sites_in_expr(array, generic_functions, type_table);
                self.collect_func_instantiation_sites_in_expr(index, generic_functions, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    scrutinee,
                    generic_functions,
                    type_table,
                );
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.collect_func_instantiation_sites_in_expr(
                            guard,
                            generic_functions,
                            type_table,
                        );
                    }
                    self.collect_func_instantiation_sites_in_expr(
                        &arm.body,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.collect_func_instantiation_sites_in_expr(body, generic_functions, type_table);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_func_instantiation_sites_in_expr(
                        &field.value,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_func_instantiation_sites_in_expr(
                    callee,
                    generic_functions,
                    type_table,
                );
                for arg in args {
                    self.collect_func_instantiation_sites_in_expr(
                        arg,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.collect_func_instantiation_sites_in_expr(
                    functor,
                    generic_functions,
                    type_table,
                );
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.collect_func_instantiation_sites_in_expr(
                        payload_expr,
                        generic_functions,
                        type_table,
                    );
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_func_instantiation_sites_in_block(
                    block,
                    generic_functions,
                    type_table,
                );
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.collect_func_instantiation_sites_in_expr(value, generic_functions, type_table);
            }
            TirExprKind::VariantTag { expr } => {
                self.collect_func_instantiation_sites_in_expr(expr, generic_functions, type_table);
            }
            TirExprKind::VariantTest { expr, .. } => {
                self.collect_func_instantiation_sites_in_expr(expr, generic_functions, type_table);
            }
            TirExprKind::VariantPayload { expr, .. } => {
                self.collect_func_instantiation_sites_in_expr(expr, generic_functions, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.collect_func_instantiation_sites_in_expr(
                    scrutinee,
                    generic_functions,
                    type_table,
                );
                for arm in arms {
                    self.collect_func_instantiation_sites_in_block(
                        arm,
                        generic_functions,
                        type_table,
                    );
                }
                self.collect_func_instantiation_sites_in_block(
                    default,
                    generic_functions,
                    type_table,
                );
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.collect_func_instantiation_sites_in_expr(
                            inner,
                            generic_functions,
                            type_table,
                        );
                    }
                }
            }
            // Literals and simple expressions
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
        }
    }

    /// Get the struct name from a `type_id`, unwrapping references if needed
    /// For generic instances, returns the mangled name with type args (e.g., "Array<i32>")
    fn get_struct_name_from_type(&self, type_id: TypeId, type_table: &TypeTable) -> Option<String> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. } => Some(name.clone()),
            ResolvedType::Primitive(prim) => Some(prim.as_str().to_string()),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Return the mangled name with type args (e.g., "Array<i32>", "Box<String>")
                let args: Vec<String> = type_args
                    .iter()
                    .map(|arg| type_table.mangle_type_name(*arg))
                    .collect();
                Some(mangle_generic_name(name, &args))
            }
            ResolvedType::Tuple(elems) => {
                let args: Vec<String> = elems
                    .iter()
                    .map(|t| type_table.mangle_type_name(*t))
                    .collect();
                Some(mangle_generic_name("Tuple", &args))
            }
            ResolvedType::BuiltinArray(elem) => {
                let arg = type_table.mangle_type_name(*elem);
                Some(mangle_generic_name("Array", &[arg]))
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_name_from_type(*inner, type_table)
            }
            _ => None,
        }
    }

    /// Get the base struct name and type args from a `type_id`, unwrapping references if needed
    /// Returns (`base_name`, `type_args`) for `GenericInstance`, (name, []) for Struct
    fn get_struct_info_from_type(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
    ) -> Option<(String, Vec<TypeId>)> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. } => {
                // For monomorphized structs with names like "Array<i32>", look up the
                // original InstantiationKey to get the base name and type_args
                if let Some(key) = self.mangled_struct_to_key.get(name) {
                    Some((key.name.clone(), key.type_args.clone()))
                } else {
                    Some((name.clone(), vec![]))
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => Some((name.clone(), type_args.clone())),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_info_from_type(*inner, type_table)
            }
            _ => None,
        }
    }

    /// Generate instantiated function name: `identity` + `[i32]` -> `"identity<i32>"`
    fn function_instantiation_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
    ) -> String {
        let args: Vec<String> = key
            .type_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();
        mangle_generic_name(&key.name, &args)
    }

    /// Generate instantiated method name
    /// Format: `StructWithImplArgs::methodWithMethodArgs`
    /// e.g., `Container::transform` with `[i32, i64]` and `impl_type_params_count=1` -> `"Container<i32>::transform<i64>"`
    fn method_instantiation_name(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
        impl_type_params_count: usize,
    ) -> String {
        self.method_instantiation_name_inner(key, type_table, impl_type_params_count, &[])
    }

    fn method_instantiation_name_inner(
        &self,
        key: &InstantiationKey,
        type_table: &TypeTable,
        impl_type_params_count: usize,
        impl_type_params: &[crate::tir::TirTypeParam],
    ) -> String {
        // Use method_info metadata instead of parsing key.name
        let Some(ref method_info) = key.method_info else {
            // Fallback to regular function naming if no method_info
            return self.function_instantiation_name(key, type_table);
        };

        // Split type_args into impl args and method args
        let (impl_args, method_args) = key
            .type_args
            .split_at(std::cmp::min(impl_type_params_count, key.type_args.len()));

        let impl_arg_names: Vec<String> = impl_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();

        // Blanket impl: struct name IS the type param (e.g., "I").
        // Detected by checking if base_struct_name matches an impl type param name.
        let is_blanket = impl_type_params
            .iter()
            .any(|p| p.name == method_info.base_struct_name);

        let mangled_struct = if is_blanket && !impl_arg_names.is_empty() {
            // Replace struct name entirely: "I" → "StrCharIter"
            MethodName::format_struct_with_args(
                &impl_arg_names[0],
                &[],
                method_info.trait_name.as_deref(),
            )
        } else {
            // Normal: append type args: "Array" → "Array<i32>"
            MethodName::format_struct_with_args(
                &method_info.struct_name,
                &impl_arg_names,
                method_info.trait_name.as_deref(),
            )
        };

        // Build method name: transform<i64> (using method type args)
        let method_arg_names: Vec<String> = method_args
            .iter()
            .map(|t| type_table.mangle_type_name(*t))
            .collect();
        let mangled_method =
            MethodName::format_method_with_args(&method_info.method_name, &method_arg_names);

        MethodName::join_struct_method(&mangled_struct, &mangled_method)
    }

    /// Instantiate a generic function with concrete type arguments
    fn instantiate_function(
        &mut self,
        generic: &TirFunction,
        key: &InstantiationKey,
        type_table: &mut TypeTable,
    ) -> Option<TirFunction> {
        let mangled_name = self.function_instantiated.get(key)?.clone();

        // Build substitution map: type param index -> concrete type
        // Include both method-level type params AND impl block type params
        let mut substitution: IndexMap<u32, TypeId> = IndexMap::default();

        // Add impl block type params first (e.g., T from impl Counter<T>)
        // Use param.index for lookup so that impls with concrete type args at earlier positions
        // (e.g., `impl Trait for Foo<ConcreteType, V>` where V has index 1) work correctly.
        for param in &generic.impl_type_params {
            if let Some(&arg) = key.type_args.get(param.index as usize) {
                substitution.insert(param.index, arg);
            }
        }

        // Add method-level type params (offset by impl type params count)
        // The method's type_params have their own indices (0, 1, ...) but in the type table,
        // method type params are offset by impl type params count.
        // e.g., impl<T> { fn foo<U>() } - T has index 0, U has index 1 in type table
        let offset = generic.impl_type_params.len() as u32;
        for (i, (param, &arg)) in generic
            .type_params
            .iter()
            .zip(key.type_args.iter().skip(offset as usize))
            .enumerate()
        {
            // Use offset + param.index to get the correct index in the type table
            substitution.insert(offset + param.index, arg);
            let _ = i; // suppress unused warning
        }

        // Substitute types in parameters
        let params: Vec<TirParam> = generic
            .params
            .iter()
            .map(|param| TirParam {
                name: param.name.clone(),
                type_id: self.substitute_type(param.type_id, &substitution, type_table),
                local_index: param.local_index,
                is_mut: param.is_mut,
                span: param.span,
            })
            .collect();

        // Substitute return type
        let return_type = self.substitute_type(generic.return_type, &substitution, type_table);

        // Substitute types in local_types
        let local_types: Vec<TypeId> = generic
            .local_types
            .iter()
            .map(|&t| self.substitute_type(t, &substitution, type_table))
            .collect();

        // Clone and substitute types in body
        let body = generic.body.as_ref().map(|b| {
            let mut new_body = b.clone();
            self.substitute_types_in_block(&mut new_body, &substitution, type_table);
            new_body
        });

        Some(TirFunction {
            is_async: generic.is_async,
            name: mangled_name,
            is_pub: generic.is_pub,
            is_export: generic.is_export, // Inherit from generic
            type_params: vec![],          // Concrete function has no type params
            impl_type_params: vec![],     // Already monomorphized, no impl type params
            monomorph_info: Some(MonomorphInfo {
                generic_name: generic.name.clone(),
                type_args: key.type_args.clone(),
                is_blanket: false,
            }),
            // Update method_info with mangled struct name including impl type args
            // and method type args (from the method's own type params)
            method_info: generic.method_info.as_ref().map(|info| {
                // Impl type args are all struct type args (key.type_args minus the method's
                // own type params). This handles impls with concrete type args at fixed
                // positions (e.g. `impl Trait for Foo<ConcreteType, V>` where only V is
                // a free impl_type_param but Foo still has 2 total type args).
                let impl_type_args_count = key
                    .type_args
                    .len()
                    .saturating_sub(generic.type_params.len());
                let impl_type_args: Vec<String> = key
                    .type_args
                    .iter()
                    .take(impl_type_args_count)
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                // Method type args are the remaining elements (from method's own type params)
                let method_type_args: Vec<String> = key
                    .type_args
                    .iter()
                    .skip(impl_type_args_count)
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                // Blanket impl: struct name IS the type param (e.g., "I").
                // Replace it with the concrete type name instead of appending type args.
                let is_blanket = generic
                    .impl_type_params
                    .iter()
                    .any(|p| p.name == info.base_struct_name);
                if is_blanket && !impl_type_args.is_empty() {
                    let base = type_table.base_type_name(key.type_args[0]);
                    info.with_substituted_struct_name(&impl_type_args[0], &base)
                } else {
                    info.with_type_args(&impl_type_args, &method_type_args)
                }
            }),
            params,
            return_type,
            effects: generic.effects.clone(),
            stores: generic.stores.clone(),
            body,
            span: generic.span,
            local_count: generic.local_count,
            local_types,
            address_taken_locals: generic.address_taken_locals.clone(),
            // Scratch local fields - computed by lower phase (after monomorphization)
            is_cm_adapter: false,
            inline_hint: generic.inline_hint,
            comp_features: generic.comp_features,
            export_name: generic.export_name.clone(),
            allocator_tag: generic.allocator_tag.clone(),
        })
    }

    /// Substitute type parameters in a block
    fn substitute_types_in_block(
        &self,
        block: &mut TirBlock,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        for stmt in &mut block.stmts {
            self.substitute_types_in_stmt(stmt, substitution, type_table);
        }
    }

    fn substitute_types_in_stmt(
        &self,
        stmt: &mut TirStmt,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                *type_id = self.substitute_type(*type_id, substitution, type_table);
                self.substitute_types_in_expr(value, substitution, type_table);
            }
            TirStmtKind::Expr(expr) => {
                self.substitute_types_in_expr(expr, substitution, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.substitute_types_in_expr(expr, substitution, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.substitute_types_in_expr(condition, substitution, type_table);
                self.substitute_types_in_block(then_block, substitution, type_table);
                if let Some(else_blk) = else_block {
                    self.substitute_types_in_block(else_blk, substitution, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.substitute_types_in_block(body, substitution, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.substitute_types_in_expr(v, substitution, type_table);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.substitute_types_in_block(block, substitution, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.substitute_types_in_expr(scrutinee, substitution, type_table);
                self.substitute_types_in_pattern(pattern, substitution, type_table);
                self.substitute_types_in_block(then_block, substitution, type_table);
                if let Some(else_blk) = else_block {
                    self.substitute_types_in_block(else_blk, substitution, type_table);
                }
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.substitute_types_in_pattern(pattern, substitution, type_table);
                self.substitute_types_in_expr(value, substitution, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    fn substitute_types_in_pattern(
        &self,
        pattern: &mut TirPattern,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        match pattern {
            TirPattern::Wildcard | TirPattern::Literal(_) => {}
            TirPattern::Binding { type_id, .. } => {
                // Substitute the binding's type (e.g., type parameter T -> i32)
                *type_id = self.substitute_type(*type_id, substitution, type_table);
            }
            TirPattern::Tuple(patterns) => {
                for p in patterns {
                    self.substitute_types_in_pattern(p, substitution, type_table);
                }
            }
            TirPattern::Variant {
                enum_type,
                bindings,
                payload_type,
                ..
            } => {
                *enum_type = self.substitute_type(*enum_type, substitution, type_table);
                // Also substitute the payload type (e.g., type parameter U -> i32)
                *payload_type = self.substitute_type(*payload_type, substitution, type_table);
                for binding in bindings {
                    self.substitute_types_in_pattern(binding, substitution, type_table);
                }
            }
            TirPattern::Enum { enum_type, .. } => {
                *enum_type = self.substitute_type(*enum_type, substitution, type_table);
            }
            TirPattern::Struct {
                struct_type,
                fields,
                ..
            } => {
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);
                for field in fields {
                    self.substitute_types_in_pattern(&mut field.pattern, substitution, type_table);
                }
            }
        }
    }

    fn substitute_types_in_expr(
        &self,
        expr: &mut TirExpr,
        substitution: &IndexMap<u32, TypeId>,
        type_table: &mut TypeTable,
    ) {
        // Substitute the expression's own type
        expr.type_id = self.substitute_type(expr.type_id, substitution, type_table);

        // Recurse into sub-expressions
        match &mut expr.kind {
            TirExprKind::Call {
                func: call_func,
                type_args,
                args,
                ..
            } => {
                // Substitute type args themselves
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                // For static method calls (formerly StaticCall), also update the func name
                // by delegating to the StaticCall substitution logic below via a flag.
                let is_static_method = call_func.method_info.is_some();
                if is_static_method {
                    // Inline the StaticCall substitution logic
                    if !substitution.is_empty()
                        && let Some(info) = call_func.method_info.clone()
                    {
                        let has_explicit_type_params = info.struct_name != info.base_struct_name;
                        let return_type_is_generic = matches!(
                            type_table.get(expr.type_id),
                            ResolvedType::Struct {
                                is_monomorphized: true,
                                ..
                            } | ResolvedType::GenericInstance { .. }
                                | ResolvedType::BuiltinArray(_)
                        );
                        let needs_struct_type_args = has_explicit_type_params
                            || info.is_type_param_receiver
                            || return_type_is_generic;

                        let old_func_name = call_func.name.clone();
                        let module_source = call_func.module_source.clone();

                        let existing_monomorph_type_args: Option<Vec<TypeId>> =
                            if has_explicit_type_params {
                                if let FunctionRef {
                                    monomorph_info: Some(mi),
                                    ..
                                } = &*call_func
                                {
                                    Some(mi.type_args.clone())
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                        let mut sorted_entries: Vec<_> = substitution.iter().collect();
                        sorted_entries.sort_by_key(|(idx, _)| **idx);
                        let (type_names, sub_type_args) =
                            if let Some(ref existing) = existing_monomorph_type_args {
                                let sub_args: Vec<TypeId> = existing
                                    .iter()
                                    .map(|&tid| self.substitute_type(tid, substitution, type_table))
                                    .collect();
                                let sub_names: Vec<String> = sub_args
                                    .iter()
                                    .map(|&tid| type_table.mangle_type_name(tid))
                                    .collect();
                                (sub_names, sub_args)
                            } else {
                                let names: Vec<String> = sorted_entries
                                    .iter()
                                    .map(|(_, tid)| type_table.mangle_type_name(**tid))
                                    .collect();
                                let tids: Vec<TypeId> =
                                    sorted_entries.iter().map(|(_, tid)| **tid).collect();
                                (names, tids)
                            };

                        let new_info = if info.is_type_param_receiver && !type_names.is_empty() {
                            let base = type_table.base_type_name(*sorted_entries[0].1);
                            let mut substituted =
                                info.with_substituted_struct_name(&type_names[0], &base);
                            if let FunctionRef {
                                monomorph_info: Some(mi),
                                ..
                            } = &*call_func
                            {
                                let new_method_type_args: Vec<String> = mi
                                    .type_args
                                    .iter()
                                    .map(|&tid| {
                                        let sub =
                                            self.substitute_type(tid, substitution, type_table);
                                        type_table.mangle_type_name(sub)
                                    })
                                    .collect();
                                substituted.method_type_args = new_method_type_args;
                            }
                            substituted
                        } else if needs_struct_type_args {
                            info.with_struct_type_args(&type_names)
                        } else {
                            info.clone()
                        };
                        let new_func_name = new_info.to_mangled_name();

                        if new_func_name != old_func_name {
                            if info.is_type_param_receiver {
                                let concrete_module = self
                                    .trait_method_locations
                                    .get(&new_func_name)
                                    .cloned()
                                    .or_else(|| {
                                        let concrete_type_id = sorted_entries[0].1;
                                        module_source_for_trait_impl(type_table, *concrete_type_id)
                                    });
                                let new_monomorph = if new_info.method_type_args.is_empty() {
                                    None
                                } else {
                                    let base_info = LocalMethodName::new(
                                        new_info.base_struct_name.clone(),
                                        new_info.trait_name.clone(),
                                        new_info.method_name.clone(),
                                    );
                                    let generic_name = base_info.to_mangled_name();
                                    let method_type_arg_tids: Vec<TypeId> = if let FunctionRef {
                                        monomorph_info: Some(mi),
                                        ..
                                    } = &*call_func
                                    {
                                        mi.type_args
                                            .iter()
                                            .map(|&tid| {
                                                self.substitute_type(tid, substitution, type_table)
                                            })
                                            .collect()
                                    } else {
                                        Vec::new()
                                    };
                                    let concrete_type_id = *sorted_entries[0].1;
                                    let impl_type_arg_tids: Vec<TypeId> = type_table
                                        .generic_type_args(concrete_type_id)
                                        .unwrap_or_default();
                                    let mut all_type_args = impl_type_arg_tids;
                                    all_type_args.extend(method_type_arg_tids);
                                    Some(MonomorphInfo {
                                        generic_name,
                                        type_args: all_type_args,
                                        is_blanket: false,
                                    })
                                };
                                *call_func = FunctionRef {
                                    module_source: concrete_module
                                        .unwrap_or_else(|| module_source.clone()),
                                    name: new_func_name,
                                    monomorph_info: new_monomorph,
                                    method_info: Some(new_info),
                                    is_cm_adapter: false,
                                };
                            } else {
                                let monomorph_info = Some(MonomorphInfo {
                                    generic_name: old_func_name,
                                    type_args: sub_type_args,
                                    is_blanket: false,
                                });
                                *call_func = FunctionRef {
                                    module_source: module_source.clone(),
                                    name: new_func_name,
                                    monomorph_info,
                                    method_info: Some(new_info),
                                    is_cm_adapter: false,
                                };
                            }
                        }
                    }
                }
                for arg in args {
                    self.substitute_types_in_expr(&mut arg.expr, substitution, type_table);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                type_args,
                args,
                ..
            } => {
                self.substitute_types_in_expr(receiver, substitution, type_table);
                for type_arg in type_args.iter_mut() {
                    *type_arg = self.substitute_type(*type_arg, substitution, type_table);
                }
                for arg in args {
                    self.substitute_types_in_expr(&mut arg.expr, substitution, type_table);
                }

                // Also update the method func name if receiver type contains type params
                // e.g., Array<T>::len -> Array<i32>::len when T->i32
                if !substitution.is_empty()
                    && let Some(info) = method_func.method_info.clone()
                {
                    // Check if the struct actually needs type arg substitution.
                    // Skip for non-generic structs (e.g., String::append from template strings)
                    // that happen to appear inside a generic impl block.
                    let has_explicit_type_params = info.struct_name != info.base_struct_name;
                    let receiver_is_generic = {
                        let mut base = receiver.type_id;
                        while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                            type_table.get(base).clone()
                        {
                            base = inner;
                        }
                        matches!(
                            type_table.get(base),
                            ResolvedType::GenericInstance { .. }
                                | ResolvedType::GenericResource { .. }
                                | ResolvedType::BuiltinArray(_)
                                | ResolvedType::Struct {
                                    is_monomorphized: true,
                                    ..
                                }
                        )
                    };
                    let needs_struct_type_args = has_explicit_type_params
                        || info.is_type_param_receiver
                        || receiver_is_generic;

                    // Use structured method_info instead of parsing strings
                    let old_func_name = method_func.name.clone();
                    let module_source = method_func.module_source.clone();

                    // Build type args from substitution
                    let mut sorted_entries: Vec<_> = substitution.iter().collect();
                    sorted_entries.sort_by_key(|(idx, _)| **idx);
                    let type_names: Vec<String> = sorted_entries
                        .iter()
                        .map(|(_, tid)| type_table.mangle_type_name(**tid))
                        .collect();
                    let type_args: Vec<TypeId> =
                        sorted_entries.iter().map(|(_, tid)| **tid).collect();

                    // Apply type args to get monomorphized method info
                    // If the struct is a type param (e.g., T^Ord::cmp), substitute the struct
                    // name directly instead of adding type args.
                    // Skip for non-generic structs that don't use the enclosing type params.
                    let new_info = if info.is_type_param_receiver && !type_names.is_empty() {
                        // Use the (already-substituted) receiver type to find the concrete name.
                        // type_names[0] would be wrong when there are multiple type params
                        // (e.g. Result<T,E>: the Err(E) branch should use E's substitution,
                        // not T's).
                        let mut inner = receiver.type_id;
                        while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                            type_table.get(inner).clone()
                        {
                            inner = t;
                        }
                        let mangled = type_table.mangle_type_name_resolving_newtypes(inner);
                        let base = type_table.base_type_name(inner);
                        info.with_substituted_struct_name(&mangled, &base)
                    } else if needs_struct_type_args {
                        info.with_struct_type_args(&type_names)
                    } else {
                        info.clone()
                    };
                    let new_func_name = new_info.to_mangled_name();

                    if new_func_name != old_func_name {
                        if info.is_type_param_receiver {
                            // Type param receiver substitution redirects to a concrete method
                            // (e.g., T^Ord::cmp -> i32^Ord::cmp). The target is not a
                            // monomorphized function - it's a concrete method defined in the
                            // module where the impl block lives.
                            // First, look up the actual module from the trait method locations
                            // map. This handles user-defined trait impls on primitive types
                            // (e.g., `impl Stringify for i32` in the entry module).
                            // Fall back to type-based heuristic for built-in impls.
                            let concrete_module = self
                                .trait_method_locations
                                .get(&new_func_name)
                                .cloned()
                                .or_else(|| {
                                    let mut inner = receiver.type_id;
                                    while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                        type_table.get(inner).clone()
                                    {
                                        inner = t;
                                    }
                                    module_source_for_trait_impl(type_table, inner)
                                });
                            // For blanket impl methods (e.g., I^IntoIterator::into_iter where
                            // the concrete function doesn't exist directly), set is_blanket=true
                            // so the monomorphizer can queue instantiation of the template.
                            // - Direct concrete method (e.g., StrUtf8ByteIter^Iterator::next):
                            //   found in trait_method_locations → monomorph_info = None
                            // - Generic impl method (e.g., Array<u8>^IntoIterator::into_iter):
                            //   receiver has type_args → handled by receiver-based scan → None
                            // - Blanket impl method (e.g., StrUtf8ByteIter^IntoIterator::into_iter):
                            //   not in trait_method_locations, receiver has no type_args → is_blanket
                            let receiver_has_type_args = {
                                let mut inner = receiver.type_id;
                                while let ResolvedType::Ref(t) | ResolvedType::MutRef(t) =
                                    type_table.get(inner).clone()
                                {
                                    inner = t;
                                }
                                matches!(
                                    type_table.get(inner),
                                    ResolvedType::GenericInstance {
                                        type_args: args, ..
                                    } if !args.is_empty()
                                ) || matches!(type_table.get(inner), ResolvedType::BuiltinArray(_))
                            };
                            let monomorph_info =
                                if self.trait_method_locations.contains_key(&new_func_name) {
                                    // Direct concrete method found — no monomorphization needed
                                    None
                                } else if receiver_has_type_args {
                                    // Generic impl (e.g., Array<T>) — handled by receiver scan
                                    None
                                } else {
                                    // Potential blanket impl method — mark for blanket instantiation
                                    Some(MonomorphInfo {
                                        generic_name: old_func_name.clone(),
                                        type_args: type_args.clone(),
                                        is_blanket: true,
                                    })
                                };
                            *method_func = FunctionRef {
                                module_source: concrete_module
                                    .unwrap_or_else(|| module_source.clone()),
                                name: new_func_name,
                                monomorph_info,
                                method_info: Some(new_info),
                                is_cm_adapter: false,
                            };
                        } else {
                            // Normal monomorphization (e.g., Array<T>::len -> Array<i32>::len)
                            let (existing_generic_name, existing_type_args, existing_is_blanket) =
                                match method_func {
                                    FunctionRef {
                                        monomorph_info: Some(mi),
                                        ..
                                    } => (
                                        Some(mi.generic_name.clone()),
                                        Some(mi.type_args.clone()),
                                        mi.is_blanket,
                                    ),
                                    _ => (None, None, false),
                                };
                            // For blanket impl calls (e.g., I^IntoIterator::into_iter),
                            // substitute the existing type_args rather than building from
                            // the enclosing substitution map. This correctly maps
                            // ArrayIter<T> → ArrayIter<i32> instead of T → i32.
                            let final_type_args = if existing_is_blanket {
                                if let Some(args) = existing_type_args {
                                    args.iter()
                                        .map(|&tid| {
                                            self.substitute_type(tid, substitution, type_table)
                                        })
                                        .collect()
                                } else {
                                    type_args
                                }
                            } else {
                                type_args
                            };
                            let monomorph_info = Some(MonomorphInfo {
                                generic_name: existing_generic_name.unwrap_or(old_func_name),
                                type_args: final_type_args,
                                is_blanket: existing_is_blanket,
                            });
                            // Use the original module_source: the monomorphized method
                            // belongs to the module where the generic was defined, not the
                            // module that triggered monomorphization.
                            *method_func = FunctionRef {
                                module_source: module_source.clone(),
                                name: new_func_name,
                                monomorph_info,
                                method_info: Some(new_info),
                                is_cm_adapter: false,
                            };
                        }
                    }
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            TirExprKind::Binary { op, left, right } => {
                self.substitute_types_in_expr(left, substitution, type_table);
                self.substitute_types_in_expr(right, substitution, type_table);

                // Check if this is a comparison operator on a struct type
                // If so, desugar to trait method call
                if let Some(new_kind) =
                    self.try_desugar_comparison(expr.span, *op, left, right, type_table)
                {
                    expr.kind = new_kind;
                }
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.substitute_types_in_expr(inner, substitution, type_table);
            }
            TirExprKind::Block(block) => {
                self.substitute_types_in_block(block, substitution, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.substitute_types_in_expr(condition, substitution, type_table);
                self.substitute_types_in_block(then_branch, substitution, type_table);
                if let Some(else_blk) = else_branch {
                    self.substitute_types_in_block(else_blk, substitution, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.substitute_types_in_expr(elem, substitution, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.substitute_types_in_expr(target, substitution, type_table);
                self.substitute_types_in_expr(value, substitution, type_table);
            }
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                *target_type = self.substitute_type(*target_type, substitution, type_table);
                self.substitute_types_in_expr(inner, substitution, type_table);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.substitute_types_in_expr(inner, substitution, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.substitute_types_in_expr(array, substitution, type_table);
                self.substitute_types_in_expr(index, substitution, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.substitute_types_in_expr(scrutinee, substitution, type_table);
                for arm in arms {
                    self.substitute_types_in_pattern(&mut arm.pattern, substitution, type_table);
                    if let Some(guard) = &mut arm.guard {
                        self.substitute_types_in_expr(guard, substitution, type_table);
                    }
                    self.substitute_types_in_expr(&mut arm.body, substitution, type_table);
                }
            }
            TirExprKind::Closure {
                params,
                body,
                captures,
                ..
            } => {
                for (_, type_id) in params {
                    *type_id = self.substitute_type(*type_id, substitution, type_table);
                }
                for cap in captures {
                    cap.type_id = self.substitute_type(cap.type_id, substitution, type_table);
                }
                self.substitute_types_in_expr(body, substitution, type_table);
            }
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                // First substitute field expressions (which will update expr.type_id)
                for field in fields {
                    self.substitute_types_in_expr(&mut field.value, substitution, type_table);
                }

                // Then substitute struct_type
                *struct_type = self.substitute_type(*struct_type, substitution, type_table);

                // Important: expr.type_id has already been substituted (line 1605 above)
                // Use it to get the correct struct type and name
                // This handles the case where struct_type is a plain Struct but expr.type_id
                // has been properly substituted to the monomorphized version
                if expr.type_id != *struct_type {
                    *struct_type = expr.type_id;
                }

                // Update struct_name to match the (possibly monomorphized) struct_type
                match type_table.get(*struct_type) {
                    ResolvedType::Struct { name, .. } => {
                        *struct_name = name.clone();
                    }
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        if type_args.is_empty() && !substitution.is_empty() {
                            // GenericInstance with empty type_args in a substitution context
                            // Build the name using the substitution map
                            let mut sorted_entries: Vec<_> = substitution.iter().collect();
                            sorted_entries.sort_by_key(|(idx, _)| **idx);
                            let args: Vec<String> = sorted_entries
                                .iter()
                                .map(|(_, tid)| type_table.mangle_type_name(**tid))
                                .collect();
                            *struct_name = mangle_generic_name(name, &args);
                        } else {
                            // For generic instances like Container<i32>, compute the mangled name
                            let args: Vec<String> = type_args
                                .iter()
                                .map(|arg| type_table.mangle_type_name(*arg))
                                .collect();
                            *struct_name = mangle_generic_name(name, &args);
                        }
                    }
                    _ => {}
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.substitute_types_in_expr(callee, substitution, type_table);
                for arg in args {
                    self.substitute_types_in_expr(arg, substitution, type_table);
                }
            }
            TirExprKind::ClosureToCanonical {
                functor,
                target_fn_type,
                ..
            } => {
                self.substitute_types_in_expr(functor, substitution, type_table);
                *target_fn_type = self.substitute_type(*target_fn_type, substitution, type_table);
            }
            TirExprKind::VariantConstruct {
                variant_type,
                payload,
                ..
            } => {
                *variant_type = self.substitute_type(*variant_type, substitution, type_table);
                let original_payload_type = payload.as_ref().map(|p| p.type_id);
                if let Some(payload_expr) = payload {
                    self.substitute_types_in_expr(payload_expr, substitution, type_table);
                }
                // After substitution, if variant_type is still a bare Variant (from
                // generic library code), convert it to a GenericInstance using the
                // payload type as type arg (e.g., Option + &mut Node<String> → Option<&mut Node<String>>).
                // Only promote if the payload type was actually changed by substitution,
                // indicating the variant is generic. Non-generic variants like
                // `Shape { Circle(f64), Point }` have concrete payload types that aren't
                // affected by substitution and should NOT be promoted to GenericInstance.
                if let ResolvedType::Variant { ref name, .. } =
                    type_table.get(*variant_type).clone()
                    && let Some(payload_expr) = payload
                    && original_payload_type.is_some_and(|orig| orig != payload_expr.type_id)
                {
                    // Use make_option for Option to ensure canonical module_source
                    let new_id = if name == "Option" {
                        type_table.make_option(payload_expr.type_id)
                    } else {
                        let module_source = if let ResolvedType::Variant { module_source, .. } =
                            type_table.get(*variant_type)
                        {
                            module_source.clone()
                        } else {
                            unreachable!()
                        };
                        type_table.make_generic_instance(
                            name.clone(),
                            module_source,
                            vec![payload_expr.type_id],
                        )
                    };
                    *variant_type = new_id;
                    expr.type_id = new_id;
                }
                // Unit cases (None) will be handled by the translator's fallback
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.substitute_types_in_block(block, substitution, type_table);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.substitute_types_in_expr(value, substitution, type_table);
            }
            TirExprKind::VariantTag { expr } => {
                self.substitute_types_in_expr(expr, substitution, type_table);
            }
            TirExprKind::VariantTest { expr, .. } => {
                self.substitute_types_in_expr(expr, substitution, type_table);
            }
            TirExprKind::VariantPayload {
                expr, payload_type, ..
            } => {
                self.substitute_types_in_expr(expr, substitution, type_table);
                *payload_type = self.substitute_type(*payload_type, substitution, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.substitute_types_in_expr(scrutinee, substitution, type_table);
                for arm in arms {
                    self.substitute_types_in_block(arm, substitution, type_table);
                }
                self.substitute_types_in_block(default, substitution, type_table);
            }
            // Literals and other simple expressions
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.substitute_types_in_expr(inner, substitution, type_table);
                    }
                }
            }
        }
    }

    /// Rewrite function calls in all functions to use monomorphized names
    fn rewrite_function_calls_in_module(&self, module: &mut TirModule) {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(mut body) = func.body.take() {
                self.rewrite_function_calls_in_block(&mut body, &module.type_table.borrow());
                // Sync local_types with Let statement types
                Self::sync_local_types_from_lets(&body, &mut func.local_types);
                // Update all Local expression types based on local_types
                Self::update_local_expr_types(&mut body, &func.local_types);
                func.body = Some(body);
            }
        }

        // Rewrite function calls in global variable initializers
        for global in &mut module.globals {
            self.rewrite_function_calls_in_expr(
                &mut global.initializer,
                &module.type_table.borrow(),
            );
        }
    }

    /// Sync `local_types` array from Let statements that may have been updated
    fn sync_local_types_from_lets(block: &TirBlock, local_types: &mut [TypeId]) {
        for stmt in &block.stmts {
            match &stmt.kind {
                TirStmtKind::Let {
                    local_index,
                    type_id,
                    ..
                } => {
                    if let Some(local_type) = local_types.get_mut(*local_index as usize) {
                        *local_type = *type_id;
                    }
                }
                TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } => {
                    Self::sync_local_types_from_lets(then_block, local_types);
                    if let Some(else_blk) = else_block {
                        Self::sync_local_types_from_lets(else_blk, local_types);
                    }
                }
                TirStmtKind::Loop { body } => {
                    Self::sync_local_types_from_lets(body, local_types);
                }
                _ => {}
            }
        }
    }

    /// Update all Local expression types based on `local_types` array
    fn update_local_expr_types(block: &mut TirBlock, local_types: &[TypeId]) {
        for stmt in &mut block.stmts {
            Self::update_local_expr_types_in_stmt(stmt, local_types);
        }
    }

    fn update_local_expr_types_in_stmt(stmt: &mut TirStmt, local_types: &[TypeId]) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                Self::update_local_expr_types_in_expr(value, local_types);
            }
            TirStmtKind::Expr(expr) => {
                Self::update_local_expr_types_in_expr(expr, local_types);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    Self::update_local_expr_types_in_expr(expr, local_types);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                Self::update_local_expr_types_in_expr(condition, local_types);
                Self::update_local_expr_types(then_block, local_types);
                if let Some(else_blk) = else_block {
                    Self::update_local_expr_types(else_blk, local_types);
                }
            }
            TirStmtKind::Loop { body } => {
                Self::update_local_expr_types(body, local_types);
            }
            _ => {}
        }
    }

    fn update_local_expr_types_in_expr(expr: &mut TirExpr, local_types: &[TypeId]) {
        match &mut expr.kind {
            TirExprKind::Local { index, .. } => {
                if let Some(&local_type) = local_types.get(*index as usize) {
                    expr.type_id = local_type;
                }
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    Self::update_local_expr_types_in_expr(&mut arg.expr, local_types);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    Self::update_local_expr_types_in_expr(arg, local_types);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::update_local_expr_types_in_expr(receiver, local_types);
                for arg in args {
                    Self::update_local_expr_types_in_expr(&mut arg.expr, local_types);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::update_local_expr_types_in_expr(left, local_types);
                Self::update_local_expr_types_in_expr(right, local_types);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                Self::update_local_expr_types_in_expr(inner, local_types);
            }
            TirExprKind::Block(block) => {
                Self::update_local_expr_types(block, local_types);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::update_local_expr_types_in_expr(condition, local_types);
                Self::update_local_expr_types(then_branch, local_types);
                if let Some(else_blk) = else_branch {
                    Self::update_local_expr_types(else_blk, local_types);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::update_local_expr_types_in_expr(elem, local_types);
                }
            }
            TirExprKind::Index { expr, index } => {
                Self::update_local_expr_types_in_expr(expr, local_types);
                Self::update_local_expr_types_in_expr(index, local_types);
            }
            TirExprKind::Assign { target, value } => {
                Self::update_local_expr_types_in_expr(target, local_types);
                Self::update_local_expr_types_in_expr(value, local_types);
            }
            TirExprKind::Match { expr, arms } => {
                Self::update_local_expr_types_in_expr(expr, local_types);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        Self::update_local_expr_types_in_expr(guard, local_types);
                    }
                    Self::update_local_expr_types_in_expr(&mut arm.body, local_types);
                }
            }
            TirExprKind::Closure { .. } => {
                // Closures have their own local scope, don't update with parent's local_types
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::update_local_expr_types_in_expr(&mut field.value, local_types);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::update_local_expr_types_in_expr(callee, local_types);
                for arg in args {
                    Self::update_local_expr_types_in_expr(arg, local_types);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                Self::update_local_expr_types_in_expr(functor, local_types);
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        Self::update_local_expr_types_in_expr(inner, local_types);
                    }
                }
            }
            _ => {}
        }
    }

    fn rewrite_function_calls_in_block(&self, block: &mut TirBlock, type_table: &TypeTable) {
        for stmt in &mut block.stmts {
            self.rewrite_function_calls_in_stmt(stmt, type_table);
        }
    }

    fn rewrite_function_calls_in_stmt(&self, stmt: &mut TirStmt, type_table: &TypeTable) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                self.rewrite_function_calls_in_expr(value, type_table);
                // Update the Let's type_id if it was a type parameter that got substituted
                // The value's type_id may have been updated during rewriting
                if type_table.contains_type_param(*type_id)
                    && !type_table.contains_type_param(value.type_id)
                {
                    *type_id = value.type_id;
                }
            }
            TirStmtKind::Expr(expr) => {
                self.rewrite_function_calls_in_expr(expr, type_table);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.rewrite_function_calls_in_expr(expr, type_table);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.rewrite_function_calls_in_expr(condition, type_table);
                self.rewrite_function_calls_in_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.rewrite_function_calls_in_block(else_blk, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.rewrite_function_calls_in_block(body, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.rewrite_function_calls_in_expr(v, type_table);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.rewrite_function_calls_in_block(block, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.rewrite_function_calls_in_expr(scrutinee, type_table);
                self.rewrite_function_calls_in_block(then_block, type_table);
                if let Some(else_blk) = else_block {
                    self.rewrite_function_calls_in_block(else_blk, type_table);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.rewrite_function_calls_in_expr(value, type_table);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }

    fn rewrite_function_calls_in_expr(&self, expr: &mut TirExpr, type_table: &TypeTable) {
        match &mut expr.kind {
            TirExprKind::Call {
                func,
                type_args,
                args,
                ..
            } => {
                let original_func_name = func.name.clone();
                let original_method_info = func.method_info.clone();
                let qualified_func_name =
                    generic_function_key(func.is_method(), &func.module_source, &func.name);
                // If this is a generic call, rewrite to monomorphized name
                if !type_args.is_empty() {
                    let key = InstantiationKey {
                        name: qualified_func_name.clone(),
                        type_args: type_args.clone(),
                        method_info: original_method_info.clone(),
                    };
                    if let Some(mangled) = self.function_instantiated.get(&key) {
                        *func = FunctionRef {
                            module_source: self.current_module_source.clone(),
                            name: mangled.clone(),
                            monomorph_info: Some(MonomorphInfo {
                                generic_name: original_func_name,
                                type_args: key.type_args.clone(),
                                is_blanket: false,
                            }),
                            method_info: original_method_info,
                            is_cm_adapter: false,
                        };

                        // Update the expression's type_id if it's a type parameter
                        // This handles cross-module generic function calls where
                        // the return type needs to be substituted
                        if let ResolvedType::TypeParam { index, .. } = type_table.get(expr.type_id)
                            && let Some(&concrete) = key.type_args.get(*index as usize)
                        {
                            expr.type_id = concrete;
                        }

                        type_args.clear(); // Clear type args - now using concrete function
                    }
                }
                // Also handle static method calls (formerly StaticCall) that need rewriting
                if let FunctionRef {
                    monomorph_info: Some(monomorph),
                    method_info: Some(info),
                    ..
                } = func
                    && !monomorph.type_args.is_empty()
                {
                    let mut names_to_try = vec![MethodName::format_local(
                        &info.base_struct_name,
                        info.trait_name.as_deref(),
                        &info.method_name,
                    )];
                    if info.struct_name != info.base_struct_name {
                        names_to_try.push(MethodName::format_local(
                            &info.struct_name,
                            info.trait_name.as_deref(),
                            &info.method_name,
                        ));
                    }
                    for generic_method_name in names_to_try {
                        let key = InstantiationKey {
                            name: generic_method_name.clone(),
                            type_args: monomorph.type_args.clone(),
                            method_info: Some(info.clone()),
                        };
                        if let Some(mangled) = self.function_instantiated.get(&key) {
                            let original_method_info = func.method_info.clone();
                            *func = FunctionRef {
                                module_source: self.current_module_source.clone(),
                                name: mangled.clone(),
                                monomorph_info: Some(MonomorphInfo {
                                    generic_name: generic_method_name,
                                    type_args: key.type_args.clone(),
                                    is_blanket: false,
                                }),
                                method_info: original_method_info,
                                is_cm_adapter: false,
                            };
                            break;
                        }
                    }
                }
                for arg in args {
                    self.rewrite_function_calls_in_expr(&mut arg.expr, type_table);
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                type_args,
                args,
                ..
            } => {
                // Extract method name from method_info or fall back to function name
                let method_name = method_func
                    .method_info
                    .clone()
                    .map(|info| info.method_name)
                    .unwrap_or_else(|| method_func.name.clone());
                let _module_source = method_func.module_source.clone();
                // If this is a generic method call, rewrite to monomorphized name
                if !type_args.is_empty()
                    && let Some(struct_name) =
                        self.get_struct_name_from_type(receiver.type_id, type_table)
                {
                    // Try both inherent method and trait method formats
                    let trait_name_opt = method_func
                        .method_info
                        .clone()
                        .and_then(|info| info.trait_name.clone());
                    let mut names_to_try = vec![(
                        MethodName::format_local(&struct_name, None, &method_name),
                        None::<String>,
                    )];
                    if let Some(ref tn) = trait_name_opt {
                        names_to_try.push((
                            MethodName::format_local(&struct_name, Some(tn), &method_name),
                            Some(tn.clone()),
                        ));
                    }

                    let mut rewritten = false;
                    for (full_method_name, _tn) in &names_to_try {
                        let key = InstantiationKey {
                            name: full_method_name.clone(),
                            type_args: type_args.clone(),
                            method_info: None,
                        };
                        if let Some(mangled) = self.function_instantiated.get(&key) {
                            let original_method_info = method_func.method_info.clone();
                            *method_func = FunctionRef {
                                module_source: self.current_module_source.clone(),
                                name: mangled.clone(),
                                monomorph_info: Some(MonomorphInfo {
                                    generic_name: full_method_name.clone(),
                                    type_args: key.type_args.clone(),
                                    is_blanket: false,
                                }),
                                method_info: original_method_info,
                                is_cm_adapter: false,
                            };
                            type_args.clear();
                            rewritten = true;
                            break;
                        }
                    }
                    // Handle "double generics": method on monomorphized generic struct
                    // e.g., c.transform::<i64>() where c: Container<i32>
                    // Also handles GenericInstance receivers (e.g., Option<i32>)
                    if !rewritten {
                        let base_info = self
                            .mangled_struct_to_key
                            .get(&struct_name)
                            .map(|k| (k.name.clone(), k.type_args.clone()))
                            .or_else(|| {
                                self.get_struct_info_from_type(receiver.type_id, type_table)
                                    .filter(|(_, args)| !args.is_empty())
                            });
                        if let Some((base_struct, impl_type_args)) = base_info {
                            let mut combined_type_args = impl_type_args.clone();
                            combined_type_args.extend(type_args.iter().copied());

                            // Try both inherent and trait method formats
                            let mut dg_names = vec![(
                                MethodName::format_local(&base_struct, None, &method_name),
                                None::<String>,
                            )];
                            if let Some(ref tn) = trait_name_opt {
                                dg_names.push((
                                    MethodName::format_local(&base_struct, Some(tn), &method_name),
                                    Some(tn.clone()),
                                ));
                            }

                            for (generic_method_name, _tn) in &dg_names {
                                let combined_key = InstantiationKey {
                                    name: generic_method_name.clone(),
                                    type_args: combined_type_args.clone(),
                                    method_info: None,
                                };
                                if let Some(mangled) = self.function_instantiated.get(&combined_key)
                                {
                                    let original_method_info = method_func.method_info.clone();
                                    *method_func = FunctionRef {
                                        module_source: self.current_module_source.clone(),
                                        name: mangled.clone(),
                                        monomorph_info: Some(MonomorphInfo {
                                            generic_name: generic_method_name.clone(),
                                            type_args: combined_key.type_args.clone(),
                                            is_blanket: false,
                                        }),
                                        method_info: original_method_info,
                                        is_cm_adapter: false,
                                    };
                                    type_args.clear();

                                    if let ResolvedType::TypeParam { index, .. } =
                                        type_table.get(expr.type_id)
                                    {
                                        let impl_count = impl_type_args.len() as u32;
                                        if *index < impl_count {
                                            if let Some(&concrete) =
                                                combined_type_args.get(*index as usize)
                                            {
                                                expr.type_id = concrete;
                                            }
                                        } else {
                                            let method_index = *index - impl_count;
                                            if let Some(&concrete) = combined_type_args
                                                .get((impl_count + method_index) as usize)
                                            {
                                                expr.type_id = concrete;
                                            }
                                        }
                                    }
                                    break;
                                }
                            }
                        }
                    }
                }
                // Also handle case where type_args is empty but receiver is a GenericInstance
                // e.g., nums.index_value(0) where nums: Triple<i32>
                else if let Some((base_struct, impl_type_args)) =
                    self.get_struct_info_from_type(receiver.type_id, type_table)
                    && !impl_type_args.is_empty()
                {
                    // Try trait method name format first (e.g., Triple^IndexValue::index_value)
                    let mut possible_keys = Vec::new();
                    if let Some(ref info) = method_func.method_info.clone()
                        && let Some(ref trait_name) = info.trait_name
                    {
                        let trait_method_name =
                            MethodName::format_local(&base_struct, Some(trait_name), &method_name);
                        possible_keys.push(InstantiationKey {
                            name: trait_method_name,
                            type_args: impl_type_args.clone(),
                            method_info: None,
                        });
                    }
                    // Also try regular method format
                    possible_keys.push(InstantiationKey {
                        name: MethodName::format_local(&base_struct, None, &method_name),
                        type_args: impl_type_args.clone(),
                        method_info: None,
                    });

                    for key in possible_keys {
                        if let Some(mangled) = self.function_instantiated.get(&key) {
                            // Preserve original method_info
                            let original_method_info = method_func.method_info.clone();
                            *method_func = FunctionRef {
                                module_source: self.current_module_source.clone(),
                                name: mangled.clone(),
                                monomorph_info: Some(MonomorphInfo {
                                    generic_name: key.name.clone(),
                                    type_args: key.type_args.clone(),
                                    is_blanket: false,
                                }),
                                method_info: original_method_info,
                                is_cm_adapter: false,
                            };
                            break;
                        }
                    }
                }
                // Blanket impl fallback: if the FunctionRef has monomorph_info from a
                // blanket impl, rewrite to the monomorphized function name.
                {
                    let blanket_lookup = if let FunctionRef {
                        monomorph_info: Some(mono),
                        ..
                    } = &*method_func
                        && mono.is_blanket
                    {
                        let key = InstantiationKey {
                            name: mono.generic_name.clone(),
                            type_args: mono.type_args.clone(),
                            method_info: None,
                        };
                        self.function_instantiated.get(&key).map(|mangled| {
                            (mangled.clone(), mono.generic_name.clone(), key.type_args)
                        })
                    } else {
                        None
                    };
                    if let Some((mangled, generic_name, type_args)) = blanket_lookup {
                        let original_method_info = method_func.method_info.clone();
                        *method_func = FunctionRef {
                            module_source: self.current_module_source.clone(),
                            name: mangled,
                            monomorph_info: Some(MonomorphInfo {
                                generic_name,
                                type_args,
                                is_blanket: true,
                            }),
                            method_info: original_method_info,
                            is_cm_adapter: false,
                        };
                    }
                }
                self.rewrite_function_calls_in_expr(receiver, type_table);
                for arg in args {
                    self.rewrite_function_calls_in_expr(&mut arg.expr, type_table);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.rewrite_function_calls_in_expr(arg, type_table);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.rewrite_function_calls_in_expr(left, type_table);
                self.rewrite_function_calls_in_expr(right, type_table);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.rewrite_function_calls_in_expr(inner, type_table);
            }
            TirExprKind::Block(block) => {
                self.rewrite_function_calls_in_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.rewrite_function_calls_in_expr(condition, type_table);
                self.rewrite_function_calls_in_block(then_branch, type_table);
                if let Some(else_blk) = else_branch {
                    self.rewrite_function_calls_in_block(else_blk, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.rewrite_function_calls_in_expr(elem, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.rewrite_function_calls_in_expr(target, type_table);
                self.rewrite_function_calls_in_expr(value, type_table);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.rewrite_function_calls_in_expr(inner, type_table);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.rewrite_function_calls_in_expr(inner, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.rewrite_function_calls_in_expr(array, type_table);
                self.rewrite_function_calls_in_expr(index, type_table);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.rewrite_function_calls_in_expr(scrutinee, type_table);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.rewrite_function_calls_in_expr(guard, type_table);
                    }
                    self.rewrite_function_calls_in_expr(&mut arm.body, type_table);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.rewrite_function_calls_in_expr(body, type_table);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.rewrite_function_calls_in_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.rewrite_function_calls_in_expr(callee, type_table);
                for arg in args {
                    self.rewrite_function_calls_in_expr(arg, type_table);
                }
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.rewrite_function_calls_in_expr(functor, type_table);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.rewrite_function_calls_in_expr(payload_expr, type_table);
                }
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.rewrite_function_calls_in_block(block, type_table);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.rewrite_function_calls_in_expr(value, type_table);
            }
            TirExprKind::VariantTag { expr } => {
                self.rewrite_function_calls_in_expr(expr, type_table);
            }
            TirExprKind::VariantTest { expr, .. } => {
                self.rewrite_function_calls_in_expr(expr, type_table);
            }
            TirExprKind::VariantPayload { expr, .. } => {
                self.rewrite_function_calls_in_expr(expr, type_table);
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.rewrite_function_calls_in_expr(scrutinee, type_table);
                for arm in arms {
                    self.rewrite_function_calls_in_block(arm, type_table);
                }
                self.rewrite_function_calls_in_block(default, type_table);
            }
            // Literals and simple expressions
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.rewrite_function_calls_in_expr(inner, type_table);
                    }
                }
            }
        }
    }

    /// Try to desugar a comparison operator to a trait method call.
    ///
    /// This handles comparison operators on struct types that have `Eq` or `Ord`
    /// trait implementations. During initial resolution, generic type parameters
    /// can't be desugared because the concrete type isn't known. After type
    /// substitution during monomorphization, we can now desugar these operators.
    ///
    /// Returns `Some(new_kind)` if the binary expression should be replaced,
    /// or `None` if it should remain as is (for primitives).
    fn try_desugar_comparison(
        &self,
        span: Span,
        op: TirBinaryOp,
        left: &TirExpr,
        right: &TirExpr,
        type_table: &mut TypeTable,
    ) -> Option<TirExprKind> {
        // Get the base struct name and type args from the operand type
        let operand_type = type_table.get(left.type_id);
        let (base_struct_name, impl_type_args, type_module_source): (
            String,
            Vec<String>,
            Option<ModuleSource>,
        ) = match operand_type {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), vec![], Some(module_source.clone())),
            ResolvedType::Variant {
                name,
                module_source,
                ..
            } => (name.clone(), vec![], Some(module_source.clone())),
            ResolvedType::GenericInstance {
                name,
                type_args,
                module_source,
                ..
            } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|&t| type_table.mangle_type_name(t))
                    .collect();
                (name.clone(), args, Some(module_source.clone()))
            }
            // Primitives don't use trait-based comparison
            _ => return None,
        };

        // Handle Eq trait (== and !=)
        if matches!(op, TirBinaryOp::Eq | TirBinaryOp::NotEq) {
            let needs_negation = op == TirBinaryOp::NotEq;

            // Create receiver with reference (trait methods take &self)
            let receiver_ref_type = type_table.intern(ResolvedType::Ref(left.type_id));
            let receiver = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(left.clone()),
                },
                receiver_ref_type,
                span,
            );

            // Create argument with reference (other: &Self)
            let arg_ref_type = type_table.intern(ResolvedType::Ref(right.type_id));
            let arg_ref = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(right.clone()),
                },
                arg_ref_type,
                span,
            );

            let method_info =
                LocalMethodName::new(base_struct_name, Some("Eq".to_string()), "eq".to_string())
                    .with_struct_type_args(&impl_type_args);
            let mangled_name = method_info.to_mangled_name();

            // Resolve the module where the trait impl lives.
            // First check trait_method_locations (populated during cross-module collection),
            // then fall back to the type's own module_source (impl is in same module as type).
            let method_module_source = self
                .trait_method_locations
                .get(&mangled_name)
                .cloned()
                .or(type_module_source)
                .unwrap_or_else(|| self.current_module_source.clone());

            let method_call = TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                func: FunctionRef {
                    module_source: method_module_source,
                    name: mangled_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                    is_cm_adapter: false,
                },
                type_args: vec![],
                args: vec![CallArg::new(arg_ref, false)],
            };

            if needs_negation {
                let bool_type =
                    type_table.intern(ResolvedType::Primitive(crate::tir::PrimitiveType::Bool));
                return Some(TirExprKind::Unary {
                    op: TirUnaryOp::Not,
                    expr: Box::new(TirExpr::new(method_call, bool_type, span)),
                });
            }
            return Some(method_call);
        }

        // Handle Ord trait (<, >, <=, >=)
        // Ord::cmp returns Ordering enum with discriminants: Less=0, Equal=1, Greater=2
        if matches!(
            op,
            TirBinaryOp::Lt | TirBinaryOp::Gt | TirBinaryOp::LtEq | TirBinaryOp::GtEq
        ) {
            // Create receiver with reference (trait methods take &self)
            let receiver_ref_type = type_table.intern(ResolvedType::Ref(left.type_id));
            let receiver = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(left.clone()),
                },
                receiver_ref_type,
                span,
            );

            // Create argument with reference (other: &Self)
            let arg_ref_type = type_table.intern(ResolvedType::Ref(right.type_id));
            let arg_ref = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(right.clone()),
                },
                arg_ref_type,
                span,
            );

            // Get Ordering type for cmp return value
            let ordering_type_id = type_table.intern(ResolvedType::Enum {
                name: "Ordering".to_string(),
                module_source: ModuleSource::prelude(),
            });

            let method_info =
                LocalMethodName::new(base_struct_name, Some("Ord".to_string()), "cmp".to_string())
                    .with_struct_type_args(&impl_type_args);
            let mangled_name = method_info.to_mangled_name();

            // Resolve the module where the trait impl lives.
            let ord_method_module_source = self
                .trait_method_locations
                .get(&mangled_name)
                .cloned()
                .or(type_module_source)
                .unwrap_or_else(|| self.current_module_source.clone());

            let cmp_call = TirExpr::new(
                TirExprKind::MethodCall {
                    receiver: Box::new(receiver),
                    func: FunctionRef {
                        module_source: ord_method_module_source,
                        name: mangled_name,
                        monomorph_info: None,
                        method_info: Some(method_info),
                        is_cm_adapter: false,
                    },
                    type_args: vec![],
                    args: vec![CallArg::new(arg_ref, false)],
                },
                ordering_type_id,
                span,
            );

            // Determine comparison operator and Ordering variant:
            // < : cmp(a, b) == Ordering::Less
            // > : cmp(a, b) == Ordering::Greater
            // <= : cmp(a, b) != Ordering::Greater
            // >= : cmp(a, b) != Ordering::Less
            let (compare_op, case_name, case_index): (TirBinaryOp, &str, u32) = match op {
                TirBinaryOp::Lt => (TirBinaryOp::Eq, "Less", 0),
                TirBinaryOp::Gt => (TirBinaryOp::Eq, "Greater", 2),
                TirBinaryOp::LtEq => (TirBinaryOp::NotEq, "Greater", 2),
                TirBinaryOp::GtEq => (TirBinaryOp::NotEq, "Less", 0),
                _ => unreachable!(),
            };

            // Create Ordering enum value for comparison
            let ordering_variant = TirExpr::new(
                TirExprKind::EnumConstruct {
                    enum_type: ordering_type_id,
                    case_name: case_name.to_string(),
                    case_index,
                },
                ordering_type_id,
                span,
            );

            return Some(TirExprKind::Binary {
                op: compare_op,
                left: Box::new(cmp_call),
                right: Box::new(ordering_variant),
            });
        }

        None
    }

    /// Desugar comparison operators on non-primitive types in all functions.
    ///
    /// This is needed for non-generic functions (where `substitute_types_in_expr` is
    /// never called) that use `==`, `!=`, `<`, etc. on struct/variant types.
    /// Without this pass, those operators fall through to the codegen's `I32Eq` fallback,
    /// which is wrong for GC reference types (variants, structs with custom Eq).
    fn desugar_comparisons_in_module(&self, module: &mut TirModule) {
        let type_table_rc = module.type_table.clone();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(mut body) = func.body.take() {
                self.desugar_comparisons_in_block(&mut body, &type_table_rc);
                func.body = Some(body);
            }
        }
    }

    fn desugar_comparisons_in_block(
        &self,
        block: &mut TirBlock,
        type_table: &Rc<RefCell<TypeTable>>,
    ) {
        for stmt in &mut block.stmts {
            self.desugar_comparisons_in_stmt(stmt, type_table);
        }
    }

    fn desugar_comparisons_in_stmt(&self, stmt: &mut TirStmt, type_table: &Rc<RefCell<TypeTable>>) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.desugar_comparisons_in_expr(value, type_table);
            }
            TirStmtKind::Expr(expr) => self.desugar_comparisons_in_expr(expr, type_table),
            TirStmtKind::Return { value } => {
                if let Some(e) = value {
                    self.desugar_comparisons_in_expr(e, type_table);
                }
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.desugar_comparisons_in_expr(condition, type_table);
                self.desugar_comparisons_in_block(then_block, type_table);
                if let Some(e) = else_block {
                    self.desugar_comparisons_in_block(e, type_table);
                }
            }
            TirStmtKind::Loop { body } => {
                self.desugar_comparisons_in_block(body, type_table);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.desugar_comparisons_in_expr(v, type_table);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.desugar_comparisons_in_block(block, type_table);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.desugar_comparisons_in_expr(scrutinee, type_table);
                self.desugar_comparisons_in_block(then_block, type_table);
                if let Some(e) = else_block {
                    self.desugar_comparisons_in_block(e, type_table);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.desugar_comparisons_in_expr(value, type_table);
            }
        }
    }

    fn desugar_comparisons_in_expr(&self, expr: &mut TirExpr, type_table: &Rc<RefCell<TypeTable>>) {
        match &mut expr.kind {
            TirExprKind::Binary { op, left, right } => {
                self.desugar_comparisons_in_expr(left, type_table);
                self.desugar_comparisons_in_expr(right, type_table);
                if let Some(new_kind) = self.try_desugar_comparison(
                    expr.span,
                    *op,
                    left,
                    right,
                    &mut type_table.borrow_mut(),
                ) {
                    expr.kind = new_kind;
                }
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::GlobalVarSet { value: inner, .. }
            | TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::ClosureToCanonical { functor: inner, .. }
            | TirExprKind::Closure { body: inner, .. } => {
                self.desugar_comparisons_in_expr(inner, type_table);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.desugar_comparisons_in_expr(&mut arg.expr, type_table);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.desugar_comparisons_in_expr(receiver, type_table);
                for arg in args {
                    self.desugar_comparisons_in_expr(&mut arg.expr, type_table);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.desugar_comparisons_in_expr(arg, type_table);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.desugar_comparisons_in_expr(callee, type_table);
                for arg in args {
                    self.desugar_comparisons_in_expr(arg, type_table);
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.desugar_comparisons_in_block(block, type_table);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.desugar_comparisons_in_expr(condition, type_table);
                self.desugar_comparisons_in_block(then_branch, type_table);
                if let Some(e) = else_branch {
                    self.desugar_comparisons_in_block(e, type_table);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.desugar_comparisons_in_expr(target, type_table);
                self.desugar_comparisons_in_expr(value, type_table);
            }
            TirExprKind::Index { expr: array, index } => {
                self.desugar_comparisons_in_expr(array, type_table);
                self.desugar_comparisons_in_expr(index, type_table);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.desugar_comparisons_in_expr(&mut field.value, type_table);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.desugar_comparisons_in_expr(elem, type_table);
                }
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.desugar_comparisons_in_expr(p, type_table);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.desugar_comparisons_in_expr(scrutinee, type_table);
                for arm in arms {
                    if let Some(guard) = &mut arm.guard {
                        self.desugar_comparisons_in_expr(guard, type_table);
                    }
                    self.desugar_comparisons_in_expr(&mut arm.body, type_table);
                }
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.desugar_comparisons_in_expr(scrutinee, type_table);
                for arm in arms {
                    self.desugar_comparisons_in_block(arm, type_table);
                }
                self.desugar_comparisons_in_block(default, type_table);
            }
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.desugar_comparisons_in_expr(inner, type_table);
                    }
                }
            }
            // Leaf expressions - no sub-expressions to desugar
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::BytesLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::FuncRef { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. } => {}
        }
    }
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
