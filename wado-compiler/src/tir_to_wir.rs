//! TIR-to-WIR translation — converts optimized TIR (Project) into a WIR module,
//! then emits it as a Wasm component binary.
//!
//! This module is the entry point for the WIR pipeline:
//!   `compile_with_wir(Project) -> Vec<u8>`
//!
//! The pipeline: Project → planning → `build_wir_module` (`WirModule`) → `emit` (Wasm bytes)

use crate::project::Project;
use crate::wir::WirModule;

mod component;
mod context;
mod emit;
mod functions;
mod translate;
mod types;

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

/// Compile a Project to Wasm bytes using the WIR pipeline.
///
/// Pipeline: planning → `build_wir_module` (`WirModule`) → `wir_emit` (Wasm bytes)
pub fn compile_with_wir(project: Project) -> Vec<u8> {
    // Planning (was formerly the standalone wasm_plan phase)
    let project = plan_project(project);

    // Phase 1: Build WirModule from Project
    let wir_module = build_wir_module(&project);

    // Phase 2: Emit core module bytes from WirModule
    let core_module = emit::emit_core_module(&wir_module);

    // Phase 2.5: Validate core module (catch errors before component wrapping)
    validate_core_module(&core_module);

    // Phase 3: Wrap in Component Model
    let wasm = component::build_component(&project, &core_module, &wir_module);

    // Phase 4: Validate
    validate_wasm(&wasm);

    wasm
}

/// Build a `WirModule` from a Project.
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

/// Validate core Wasm module (before component wrapping).
fn validate_core_module(wasm: &[u8]) {
    let features = wasmparser::WasmFeatures::all();
    let mut validator = wasmparser::Validator::new_with_features(features);
    if let Err(e) = validator.validate_all(wasm) {
        // Always write WAT for analysis
        if let Ok(wat) = wasmprinter::print_bytes(wasm) {
            let _ = std::fs::write("/tmp/wir_debug_core.wat", &wat);
        }
        let _ = std::fs::write("/tmp/wir_debug_error.txt", format!("{e}"));
        panic!(
            "Internal compiler error: WIR pipeline generated invalid core Wasm module\n\
             Validation error: {e}"
        );
    }
}

/// Validate generated Wasm binary using wasmparser.
fn validate_wasm(wasm: &[u8]) {
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    if let Err(e) = validator.validate_all(wasm) {
        if let Ok(wat) = wasmprinter::print_bytes(wasm) {
            let _ = std::fs::write("/tmp/wir_debug_component.wat", &wat);
        }
        panic!(
            "Internal compiler error: WIR pipeline generated invalid Wasm\n\
             This is a bug in the Wado compiler. Please report it.\n\
             Validation error: {e}"
        );
    }
}
