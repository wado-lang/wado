//! Wasm code generation — emits a WIR module as a Wasm component binary.
//!
//! Takes a linked `NirPackage` and a `WirPackage` and produces the final
//! Wasm component bytes.
//!
//! Pipeline: `WirPackage` → `emit` (core bytes) → `component` (wrapped) → `Vec<u8>`

use crate::module_source::ModuleSource;
use crate::nir_package::NirPackage;
use crate::wir::WirPackage;

mod component;
mod component_context;
mod emit;
mod postprocess;

/// Emit a Wasm component binary from a linked package and its WIR module.
pub fn emit_wasm(package: &NirPackage, wir_package: &WirPackage) -> Vec<u8> {
    // Step 1: Emit core module bytes from WirPackage
    let core_module =
        emit::emit_core_module(wir_package, package.strip_names, package.codegen_flags);

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
        let loc = locate_offset_function(wasm, e.offset())
            .map(|(idx, name)| format!("func #{idx} {}", name.unwrap_or_default()))
            .unwrap_or_else(|| "<unknown function>".to_string());
        panic!(
            "Internal compiler error: WIR pipeline generated invalid core Wasm module\n\
             Entry module: {entry_module}\n\
             Offending function: {loc}\n\
             Validation error: {e}"
        );
    }
}

/// Find the function whose body byte range contains `offset`, with its name
/// from the custom `name` section. Debug aid for validation ICEs.
fn locate_offset_function(wasm: &[u8], offset: usize) -> Option<(u32, Option<String>)> {
    use std::collections::HashMap;
    use wasmparser::{Name, Parser, Payload};
    let mut import_funcs = 0u32;
    let mut defined = 0u32;
    let mut bodies: Vec<(u32, std::ops::Range<usize>)> = Vec::new();
    let mut names: HashMap<u32, String> = HashMap::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.ok()? {
            Payload::ImportSection(reader) => {
                for imports in reader.into_iter().flatten() {
                    for entry in imports {
                        if let Ok((_, imp)) = entry
                            && matches!(imp.ty, wasmparser::TypeRef::Func(_))
                        {
                            import_funcs += 1;
                        }
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                bodies.push((import_funcs + defined, body.range()));
                defined += 1;
            }
            Payload::CustomSection(c) if c.name() == "name" => {
                let reader = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(
                    c.data(),
                    c.data_offset(),
                ));
                for sub in reader {
                    if let Ok(Name::Function(map)) = sub {
                        for naming in map.into_iter().flatten() {
                            names.insert(naming.index, naming.name.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    bodies
        .into_iter()
        .find(|(_, range)| range.contains(&offset))
        .map(|(idx, _)| (idx, names.get(&idx).cloned()))
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
