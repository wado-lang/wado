//! WIR build — translates optimized TIR (Package) into a `WirPackage`.
//!
//! Pipeline: `Package` → planning → `build_wir_package` → `WirPackage`
//!
//! Emission (`WirPackage` → Wasm bytes) is handled by `codegen`.

use crate::package::Package;
use crate::wir::WirPackage;

pub mod component_plan;
mod context;
mod functions;
mod translate;
mod types;

pub use context::DEFINED_FUNC_BASE;

/// Run the planning phase.
///
/// Sets `project.has_http_handler_export` from world analysis and
/// populates `project.component_plan` for use by `build_wir_package`.
pub fn plan_project(mut project: Package) -> Package {
    let world_info = project.world_registry.get(&project.target_world).cloned();
    if let Some(world_info) = world_info {
        project.has_http_handler_export = world_info.has_http_handler_export();
    }
    project.component_plan = Some(component_plan::build_component_plan(&project));
    project
}

/// Build a `WirPackage` from a planned Package.
pub fn build_wir_package(project: &Package) -> WirPackage {
    let mut ctx = context::WirContext::new(project);

    // Collect wasm_module attributes from TIR modules
    for (module_source, tir_mod) in &project.tir_modules {
        if let Some(wasm_mod_name) = &tir_mod.wasm_module {
            let prefix = module_source.to_string();
            ctx.wasm_module_sources
                .insert(prefix, wasm_mod_name.clone());
        }
    }

    // Step 1: Register all types
    types::register_types(&mut ctx);

    // Step 2: Collect and register all functions
    functions::collect_functions(&mut ctx);

    // Step 2.5: Register canonical closure wrapper functions
    translate::register_closure_wrappers(&mut ctx);

    // Step 3: Translate function bodies
    translate::translate_function_bodies(&mut ctx);

    // Step 4: Build the final WirPackage
    ctx.into_wir_package()
}
