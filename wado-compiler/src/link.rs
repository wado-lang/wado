//! Link phase — transforms a per-module `Package` into a linked `FlatPackage`.
//!
//! The link phase sits between type erasure and monomorphization in the pipeline:
//!
//! ```text
//! Package (per-module TIR)
//!   → link
//! FlatPackage (flat TIR)
//!   → monomorphize → lower → optimize → wir_build → codegen
//! ```
//!
//! The link phase flattens all per-module TIR data into flat lists and
//! performs component-model planning (world exports, test exports, bundled
//! functions).

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::name::ModuleSource;
use crate::package::Package;
use crate::tir::TypeTable;
use crate::wir_build::component_plan;

/// Link a `Package` into a `FlatPackage`.
///
/// Flattens per-module TIR data into flat lists and builds the component plan.
pub fn link(package: Package) -> FlatPackage {
    // Extract the shared type table from the first module.
    let type_table: Rc<RefCell<TypeTable>> = package
        .tir_modules
        .values()
        .next()
        .expect("package must have at least one module")
        .type_table
        .clone();

    // Determine HTTP handler export from world registry.
    let has_http_handler_export = package
        .world_registry
        .get(&package.target_world)
        .map(super::world_registry::WorldInfo::has_http_handler_export)
        .unwrap_or(false);

    // Build the component plan before consuming tir_modules.
    let is_test_world = package.target_world == super::world_registry::TEST_WORLD;
    let entry_tests = &package.entry_module().tests;
    let component_plan = component_plan::build_component_plan(
        is_test_world,
        &package.target_world,
        entry_tests,
        &package.export_binding_names,
        package.world_registry,
        package.wasi_registry,
    );

    // Flatten all per-module TIR into flat lists.
    let mut functions = Vec::new();
    let mut structs = Vec::new();
    let mut enums = Vec::new();
    let mut variants = Vec::new();
    let mut flags = Vec::new();
    let mut globals = Vec::new();
    let mut imports = Vec::new();
    let mut tests = Vec::new();
    let mut closure_functors = Vec::new();
    let mut wasm_module_sources: IndexMap<ModuleSource, String> = IndexMap::default();

    for (_ms, tir_mod) in package.tir_modules {
        let ms: ModuleSource = tir_mod.module_source.clone();
        let is_entry = ms == package.entry_module_source;

        // Functions: set module_source on each function
        for func_rc in tir_mod.functions {
            func_rc.borrow_mut().module_source = ms.clone();
            functions.push(func_rc);
        }

        structs.extend(tir_mod.structs);
        enums.extend(tir_mod.enums);
        variants.extend(tir_mod.variants);
        flags.extend(tir_mod.flags);
        globals.extend(tir_mod.globals);
        closure_functors.extend(tir_mod.closure_functors);

        if is_entry {
            imports = tir_mod.imports;
            tests = tir_mod.tests;
        }

        if let Some(wm) = tir_mod.wasm_module {
            wasm_module_sources.insert(ms.clone(), wm);
        }
    }

    // Build variant lookup indices.
    let mut variant_index = IndexMap::default();
    for (i, v) in variants.iter().enumerate() {
        variant_index
            .entry((v.module_source.clone(), v.name.clone()))
            .or_insert(i);
    }

    FlatPackage {
        entry_module_source: package.entry_module_source,
        type_table,
        functions,
        structs,
        enums,
        variants,
        variant_index,
        flags,
        globals,
        imports,
        tests,
        string_literals: Vec::new(),
        bytes_literals: Vec::new(),
        closure_functors,
        function_strings: IndexMap::default(),
        function_method_info: IndexMap::default(),
        wasm_module_sources,
        module_name: package.module_name,
        wasi_registry: package.wasi_registry,
        world_registry: package.world_registry,
        used_wasi_functions: package.used_wasi_functions,
        strip_names: package.strip_names,
        skip_validation: package.skip_validation,
        target_world: package.target_world,
        has_http_handler_export,
        export_binding_names: package.export_binding_names,
        component_plan,
        builtin_registry: package.builtin_registry,
        task_return_flat_params: package.task_return_flat_params,
        wasm_assets: package.wasm_assets,
        trait_env: package.trait_env,
    }
}
