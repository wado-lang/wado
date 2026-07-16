//! WIR build — translates linked TIR (`NirPackage`) into a `WirPackage`.
//!
//! Pipeline: `NirPackage` → `build_wir_package` → `WirPackage`
//!
//! Emission (`WirPackage` → Wasm bytes) is handled by `codegen`.

use crate::nir_package::NirPackage;
use crate::wir::WirPackage;

mod calls;
pub mod component_imports;
pub mod component_plan;
mod context;
mod functions;
mod pattern_match;
mod primitive_ops;
mod translate;
mod types;

pub use context::DEFINED_FUNC_BASE;

/// Build a `WirPackage` from a linked `NirPackage`.
pub fn build_wir_package(package: &NirPackage) -> WirPackage {
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

    // Step 3: Translate function bodies. Auto-derived
    // `Fn<arity, ret>^Inspect / InspectAlt` dispatch stubs (kind
    // `FnCanonicalDispatch`) get their indirect-call body supplied
    // here directly — see `translate_function_bodies`.
    translate::translate_function_bodies(&mut ctx);

    // Step 4: Build the final WirPackage
    let mut wir = ctx.into_wir_package();

    // Step 5: Resolve the categorized CM import plan now that `needed_canonicals`
    // is final — both the NIR facts (`used_wasi_functions`, the registry) and
    // the WIR canonical intrinsics (transmission sources) are available here, so
    // this is the single place with the full picture. Codegen reads the plan to
    // decide membership per phase; the WIT producer reads the flat list.
    wir.import_plan = component_imports::resolve_import_plan(package, &wir.needed_canonicals);
    wir.imported_cm_interfaces =
        component_imports::imported_cm_interface_fqs(package, &wir.import_plan);
    wir
}
