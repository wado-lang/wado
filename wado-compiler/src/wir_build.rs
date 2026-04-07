//! WIR build — translates linked TIR (`FlatPackage`) into a `WirPackage`.
//!
//! Pipeline: `FlatPackage` → `build_wir_package` → `WirPackage`
//!
//! Emission (`WirPackage` → Wasm bytes) is handled by `codegen`.

use crate::flat_package::FlatPackage;
use crate::wir::WirPackage;

pub mod component_plan;
mod context;
mod functions;
mod translate;
mod types;

pub use context::DEFINED_FUNC_BASE;

/// Build a `WirPackage` from a linked `FlatPackage`.
pub fn build_wir_package(package: &FlatPackage) -> WirPackage {
    let mut ctx = context::WirContext::new(package);

    // Collect wasm_module attributes from TIR modules
    for (module_source, tir_mod) in &package.tir_modules {
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
