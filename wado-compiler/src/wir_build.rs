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
mod translate_calls;
mod translate_canonical;
mod translate_match;
mod translate_ops;
mod translate_value_copy;
mod types;

pub use context::DEFINED_FUNC_BASE;

/// Build a `WirPackage` from a linked `FlatPackage`.
pub fn build_wir_package(package: &FlatPackage) -> WirPackage {
    let mut ctx = context::WirContext::new(package);

    // Copy wasm_module attributes (already collected during link)
    ctx.wasm_module_sources
        .clone_from(&package.wasm_module_sources);

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
