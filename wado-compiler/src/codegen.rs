//! Wasm code generation — emits a WIR module as a Wasm component binary.
//!
//! Takes a planned `Project` and a `WirModule` and produces the final
//! Wasm component bytes.
//!
//! Pipeline: `WirModule` → `emit` (core bytes) → `component` (wrapped) → `Vec<u8>`

use crate::project::Project;
use crate::wir::WirModule;

mod component;
mod component_context;
mod emit;
mod postprocess;

/// Emit a Wasm component binary from a planned project and its WIR module.
pub fn emit_wasm(project: &Project, wir_module: &WirModule) -> Vec<u8> {
    // Step 1: Emit core module bytes from WirModule
    let core_module = emit::emit_core_module(wir_module, project.strip_names);

    // Step 2: Validate core module (catch errors before component wrapping)
    if !project.skip_validation {
        validate_core_module(&core_module);
    }

    // Step 3: Wrap in Component Model
    let wasm = component::build_component(project, &core_module, wir_module);

    // Step 4: Validate
    if !project.skip_validation {
        validate_wasm(&wasm);
    }

    wasm
}

/// Validate core Wasm module (before component wrapping).
fn validate_core_module(wasm: &[u8]) {
    let features = wasmparser::WasmFeatures::all();
    let mut validator = wasmparser::Validator::new_with_features(features);
    if let Err(e) = validator.validate_all(wasm) {
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
        panic!(
            "Internal compiler error: WIR pipeline generated invalid Wasm\n\
             This is a bug in the Wado compiler. Please report it.\n\
             Validation error: {e}"
        );
    }
}
