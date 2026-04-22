//! Pre-synthesis hooks for Kiln generator packages.
//!
//! When the compiling package targets the `core:kiln/generator` world, this
//! phase locates the generator entry's `Options` struct and appends a
//! `SynthesisRequest` for the `Deserialize` trait. `serde_synth` (which
//! runs immediately afterwards) materializes `Options::deserialize` without
//! the user having to write `impl Deserialize for Options;` explicitly,
//! preserving the WEP contract that "authors never touch JSON".
//!
//! The CM boundary wrapper (`_kiln_generate_cm_wrapper`) is synthesized
//! later by `cm_binding`, which can rely on the deserializer existing.
//!
//! See WEP 2026-04-12 (Kiln) §"Options schema".

use crate::package::Package;
use crate::tir::SynthesisRequest;

/// Target world that identifies a Kiln generator package.
pub const KILN_GENERATOR_WORLD: &str = "core:kiln/generator";

pub fn prepare_kiln(project: &mut Package) {
    if project.target_world != KILN_GENERATOR_WORLD {
        return;
    }

    let entry = project.entry_module_source.clone();
    let Some(entry_module) = project.tir_modules.get_mut(&entry) else {
        return;
    };

    let options_struct = entry_module
        .structs
        .iter()
        .find(|s| s.name == "Options")
        .cloned();
    let Some(options) = options_struct else {
        // Missing `pub struct Options` is reported by
        // `kiln::options::extract_options_descriptor` later in the
        // pipeline. Nothing to do here.
        return;
    };

    let already_requested = entry_module
        .synthesis_requests
        .iter()
        .any(|req| req.trait_name == "Deserialize" && req.target_type_name == "Options");
    if already_requested {
        return;
    }

    let target_type_id = entry_module
        .type_table
        .borrow()
        .find_struct_by_name("Options", &entry)
        .unwrap_or(crate::tir::TypeTable::UNIT);

    entry_module.synthesis_requests.push(SynthesisRequest {
        trait_name: "Deserialize".to_string(),
        target_type_name: "Options".to_string(),
        target_type_id,
        type_params: Vec::new(),
        span: options.span,
    });
}
