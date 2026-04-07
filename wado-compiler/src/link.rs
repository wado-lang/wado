//! Link phase — transforms a per-module `Package` into a linked `FlatPackage`.
//!
//! The link phase sits between optimization and WIR building in the pipeline:
//!
//! ```text
//! Package (per-module TIR)
//!   → link
//! FlatPackage (linked)
//!   → wir_build
//! WirPackage (Wasm IR)
//! ```
//!
//! Currently, linking performs component-model planning (world exports, test
//! exports, bundled functions) and transfers ownership of TIR modules from
//! `Package` to `FlatPackage`.
//!
//! Future work will flatten `tir_modules` into module-independent lists,
//! removing per-module iteration from `wir_build`.

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::package::Package;
use crate::tir::TypeTable;
use crate::wir_build::component_plan;

/// Link a `Package` into a `FlatPackage`.
///
/// Consumes the per-module package and produces a linked representation
/// suitable for WIR building and code generation.
pub fn link(package: Package) -> FlatPackage {
    // Extract the shared type table from the first module.
    let type_table: Rc<RefCell<TypeTable>> = package
        .tir_modules
        .values()
        .next()
        .expect("package must have at least one module")
        .type_table
        .clone();

    // Determine HTTP handler export from world registry.
    let has_http_handler_export = package
        .world_registry
        .get(&package.target_world)
        .map(super::world_registry::WorldInfo::has_http_handler_export)
        .unwrap_or(false);

    // Build the component plan (world exports, test exports, bundled functions).
    // This was previously done by `wir_build::plan_project`.
    let component_plan = component_plan::build_component_plan(&package);

    FlatPackage {
        entry_module_source: package.entry_module_source,
        tir_modules: package.tir_modules,
        type_table,
        module_name: package.module_name,
        wasi_registry: package.wasi_registry,
        world_registry: package.world_registry,
        reachable_functions: package.reachable_functions,
        used_wasi_functions: package.used_wasi_functions,
        strip_names: package.strip_names,
        skip_validation: package.skip_validation,
        target_world: package.target_world,
        has_http_handler_export,
        export_binding_names: package.export_binding_names,
        component_plan,
    }
}
