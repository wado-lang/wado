//! Link phase — transforms a per-module `Package` into a linked `FlatPackage`.
//!
//! The link phase sits between optimization and WIR building in the pipeline:
//!
//! ```text
//! Package (per-module TIR)
//!   → link
//! FlatPackage (flat TIR)
//!   → wir_build
//! WirPackage (Wasm IR)
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
    let component_plan = component_plan::build_component_plan(&package);

    // Flatten all per-module TIR into flat lists.
    let mut functions = Vec::new();
    let mut structs = Vec::new();
    let mut enums = Vec::new();
    let mut variants = Vec::new();
    let mut flags = Vec::new();
    let mut newtypes = Vec::new();
    let mut globals = Vec::new();
    let mut imports = Vec::new();
    let mut tests = Vec::new();
    let mut traits = Vec::new();
    let mut impls = Vec::new();
    let mut string_literals = Vec::new();
    let mut bytes_literals = Vec::new();
    let mut closure_functors = Vec::new();
    let mut data_section = None;
    let mut function_strings = IndexMap::default();
    let mut function_method_info = IndexMap::default();
    let mut wasm_module_sources: IndexMap<String, String> = IndexMap::default();

    for (_ms, tir_mod) in package.tir_modules {
        let ms: ModuleSource = tir_mod.module_source.clone();
        let is_entry = ms == package.entry_module_source;

        // Functions: pair each with its module source
        for func_rc in tir_mod.functions {
            functions.push((ms.clone(), func_rc));
        }

        // Dedup monomorphized structs by (name, module_source). The monomorphizer
        // may create cross-contaminated copies (e.g., Wrapper<i32> in module A
        // instantiated from module B's generic, yielding wrong fields). When two
        // mono structs share the same (name, module_source), prefer the one whose
        // source module (`ms`) matches `module_source` (i.e., the struct was
        // instantiated in the module that owns the generic definition).
        for s in tir_mod.structs {
            if s.monomorph_info.is_some()
                && let Some(pos) =
                    structs.iter().position(|existing: &crate::tir::TirStruct| {
                        existing.name == s.name
                            && existing.module_source == s.module_source
                            && existing.monomorph_info.is_some()
                    })
            {
                // Prefer the struct from the defining module
                if s.module_source == ms {
                    structs[pos] = s;
                }
                continue;
            }
            structs.push(s);
        }
        enums.extend(tir_mod.enums);
        variants.extend(tir_mod.variants);
        flags.extend(tir_mod.flags);
        newtypes.extend(tir_mod.newtypes);
        globals.extend(tir_mod.globals);
        traits.extend(tir_mod.traits);
        impls.extend(tir_mod.impls);
        string_literals.extend(tir_mod.string_literals);
        bytes_literals.extend(tir_mod.bytes_literals);
        closure_functors.extend(tir_mod.closure_functors);
        // Merge function_strings: closure functions from different modules can share
        // names (e.g., `__Closure_0::__call`), so we must append string lists rather
        // than overwriting to avoid losing string associations.
        for (func_name, strings) in tir_mod.function_strings {
            function_strings
                .entry(func_name)
                .or_insert_with(Vec::new)
                .extend(strings);
        }
        function_method_info.extend(tir_mod.function_method_info);

        if is_entry {
            imports = tir_mod.imports;
            tests = tir_mod.tests;
            data_section = tir_mod.data_section;
        }

        if let Some(wm) = tir_mod.wasm_module {
            wasm_module_sources.insert(ms.to_string(), wm);
        }
    }

    FlatPackage {
        entry_module_source: package.entry_module_source,
        type_table,
        functions,
        structs,
        enums,
        variants,
        flags,
        newtypes,
        globals,
        imports,
        tests,
        traits,
        impls,
        string_literals,
        bytes_literals,
        closure_functors,
        data_section,
        function_strings,
        function_method_info,
        wasm_module_sources,
        module_name: package.module_name,
        wasi_registry: package.wasi_registry,
        world_registry: package.world_registry,
        reachable_functions: package.reachable_functions,
        used_wasi_functions: package.used_wasi_functions,
        strip_names: package.strip_names,
        skip_validation: package.skip_validation,
        target_world: package.target_world,
        has_http_handler_export,
        export_binding_names: package.export_binding_names,
        component_plan,
        builtin_registry: package.builtin_registry,
        task_return_flat_params: package.task_return_flat_params,
    }
}
