//! WIR build — translates optimized TIR (Project) into a `WirModule`.
//!
//! Pipeline: `Project` → planning → `build_wir_module` → `WirModule`
//!
//! Emission (`WirModule` → Wasm bytes) is handled by `codegen`.

use crate::project::Project;
use crate::wir::WirModule;

mod context;
mod functions;
mod translate;
mod types;

pub use context::shorten_import_module;

/// Run the planning phase — previously the standalone `wasm_plan` pipeline step.
///
/// Sets `project.has_http_handler_export` from world analysis and
/// populates `project.component_plan` for use by `build_wir_module`.
pub fn plan_project(mut project: Project) -> Project {
    let world_info = project.world_registry.get(&project.target_world).cloned();
    if let Some(world_info) = world_info {
        project.has_http_handler_export = world_info.has_http_handler_export();
    }
    project.component_plan = Some(crate::wasm_plan::build_component_plan(&project));
    project
}

/// Build a `WirModule` from a planned Project.
pub fn build_wir_module(project: &Project) -> WirModule {
    let mut ctx = context::WirContext::new(project);

    // Step 1: Register all types
    types::register_types(&mut ctx);

    // Step 2: Collect and register all functions
    functions::collect_functions(&mut ctx);

    // Step 2.5: Register canonical closure wrapper functions
    translate::register_closure_wrappers(&mut ctx);

    // Step 3: Translate function bodies
    translate::translate_function_bodies(&mut ctx);

    // Step 4: Build the final WirModule
    ctx.into_wir_module()
}
