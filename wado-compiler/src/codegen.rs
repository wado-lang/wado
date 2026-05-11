//! Wasm code generation — emits a WIR module as a Wasm component binary.
//!
//! Takes a linked `FlatPackage` and a `WirPackage` and produces the final
//! Wasm component bytes.
//!
//! Pipeline: `WirPackage` → `emit` (core bytes) → `component` (wrapped) → `Vec<u8>`

use crate::flat_package::FlatPackage;
use crate::module_source::ModuleSource;
use crate::wir::WirPackage;

mod component;
mod component_context;
mod emit;
mod postprocess;

/// Emit a Wasm component binary from a linked package and its WIR module.
pub fn emit_wasm(package: &FlatPackage, wir_package: &WirPackage) -> Vec<u8> {
    // Step 1: Emit core module bytes from WirPackage
    let core_module = emit::emit_core_module(wir_package, package.strip_names);

    // Step 2: Validate core module (catch errors before component wrapping)
    if !package.skip_validation {
        validate_core_module(&core_module, &package.entry_module_source);
    }

    // Step 3: Wrap in Component Model
    let wasm = component::build_component(package, &core_module, wir_package);

    // Step 4: Validate
    if !package.skip_validation {
        validate_wasm(&wasm, &package.entry_module_source);
    }

    wasm
}

/// Validate core Wasm module (before component wrapping).
fn validate_core_module(wasm: &[u8], entry_module: &ModuleSource) {
    let features = wasmparser::WasmFeatures::all();
    let mut validator = wasmparser::Validator::new_with_features(features);
    if let Err(e) = validator.validate_all(wasm) {
        // Save invalid Wasm for debugging
        let _ = std::fs::write("/tmp/invalid_core.wasm", wasm);
        panic!(
            "Internal compiler error: WIR pipeline generated invalid core Wasm module\n\
             Entry module: {entry_module}\n\
             Validation error: {e}"
        );
    }
}

/// Validate generated Wasm binary using wasmparser.
fn validate_wasm(wasm: &[u8], entry_module: &ModuleSource) {
    let mut validator = wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all());
    if let Err(e) = validator.validate_all(wasm) {
        panic!(
            "Internal compiler error: WIR pipeline generated invalid Wasm\n\
             Entry module: {entry_module}\n\
             This is a bug in the Wado compiler. Please report it.\n\
             Validation error: {e}"
        );
    }
}
