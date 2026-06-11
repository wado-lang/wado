//! Post-DCE resolution of the complete set of CM interface FQs the component
//! imports, as structured data at the WIR layer.
//!
//! Today the import decision lives in `codegen/component.rs` (the four-phase
//! `generate_cm_imports` + resource phases + HTTP phase), which violates the
//! `codegen.rs` principle ("emit `Package` as is, without knowledge of earlier
//! phases"). WEP `wep-2026-05-02-wit-interoperability.md` §"Faithful imports"
//! moves the decision here so codegen, the WIT producer, and CM embedding all
//! read one source of truth.
//!
//! This is R1: the plan is built additively and validated against the real
//! component (see `tests/wit_import_plan.rs`); codegen still computes its own
//! imports until R2 rewires it to consume the plan.

use crate::ast::Type;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::IndexSet;
use crate::nir_package::NirPackage;

/// Compute the imported CM interface FQs for `project`, mirroring the decision
/// codegen's import phases make from `used_wasi_functions` + the registry.
/// Returns a sorted, de-duplicated list.
#[must_use]
pub fn resolve_imported_cm_interfaces(project: &NirPackage) -> Vec<String> {
    let registry = project.cm_interface_registry;
    let mut imports: IndexSet<String> = IndexSet::default();

    // The kiln-generator-shaped worlds forbid every WASI interface, matching
    // the suppression in `optimize/dce.rs`.
    let wasi_allowed = !project.world_imports_interface("KilnHost");
    if !wasi_allowed {
        return Vec::new();
    }

    // Phase 0: `wasi:cli/types` provides the shared `error-code` enum and is
    // imported unconditionally by codegen's `generate_cm_imports` (the canonical
    // fallback for `result<_, error-code>` bindings), so the plan must too —
    // even for a pure-compute program that references no WASI function. Trimming
    // this to "only when error-code is actually referenced" is an R2
    // optimization, made once codegen reads the plan.
    if let Some(version) = registry.get_cli_version() {
        imports.insert(format!("wasi:cli/types@{version}"));
    }

    // Phase 1: every interface with a function in `used_wasi_functions`, plus
    // the resource-defining interfaces for resources those signatures touch.
    let mut needed_resources: IndexSet<String> = IndexSet::default();
    for interface_info in registry.interfaces() {
        if interface_info.interface == "run"
            || interface_info.resource_type.is_some()
            || interface_info.package == "http"
        {
            continue;
        }
        let used: Vec<_> = interface_info
            .functions
            .iter()
            .filter(|func| {
                registry.is_function_supported(func)
                    && project
                        .used_wasi_functions
                        .contains(&format!("{}::{}", func.interface_name, func.method_name))
            })
            .collect();
        if used.is_empty() {
            continue;
        }
        imports.insert(interface_info.path.clone());
        for func in used {
            if let Some(ret) = &func.return_type {
                collect_resources_in_type(ret, registry, &mut needed_resources);
            }
            for (_, _, ty) in &func.params {
                collect_resources_in_type(ty, registry, &mut needed_resources);
            }
        }
    }

    // Resource-defining interfaces for every referenced resource (transitive:
    // a resource's defining interface may itself reference further resources).
    let mut worklist: Vec<String> = needed_resources.iter().cloned().collect();
    let mut seen_resources: IndexSet<String> = IndexSet::default();
    while let Some(resource) = worklist.pop() {
        if !seen_resources.insert(resource.clone()) {
            continue;
        }
        let Some(source) = registry.get_resource_source_interface(&resource) else {
            continue;
        };
        let source = source.to_string();
        imports.insert(source.clone());
        // The defining interface's own signatures may reference further
        // resources; queue those too.
        let mut more: IndexSet<String> = IndexSet::default();
        for info in registry.interfaces().filter(|i| i.path == source) {
            for func in &info.functions {
                if let Some(ret) = &func.return_type {
                    collect_resources_in_type(ret, registry, &mut more);
                }
                for (_, _, ty) in &func.params {
                    collect_resources_in_type(ty, registry, &mut more);
                }
            }
        }
        worklist.extend(more);
    }

    // Phase 4: HTTP interfaces are imported under the handler / Client
    // conditions codegen uses.
    if (project.has_http_handler_export || project.has_interface("Client"))
        && let Some(version) = registry.get_package_version("http")
    {
        imports.insert(format!("wasi:http/types@{version}"));
    }
    if project.has_interface("Client")
        && let Some(version) = registry.get_package_version("http")
    {
        imports.insert(format!("wasi:http/client@{version}"));
    }

    let mut out: Vec<String> = imports.into_iter().collect();
    out.sort();
    out
}

/// Collect resource type names referenced anywhere in `ty` (recursing through
/// generics, tuples, and references). Mirrors `codegen::component`'s helper.
fn collect_resources_in_type(
    ty: &Type,
    registry: &CmInterfaceRegistry,
    out: &mut IndexSet<String>,
) {
    match ty {
        Type::Named(named) => {
            if registry
                .get_resource_source_interface(&named.name)
                .is_some()
            {
                out.insert(named.name.clone());
            }
        }
        Type::Generic(generic) => {
            for arg in &generic.args {
                collect_resources_in_type(arg, registry, out);
            }
        }
        Type::NamespacedGeneric(generic) => {
            for arg in &generic.args {
                collect_resources_in_type(arg, registry, out);
            }
        }
        Type::Tuple(elems) => {
            for elem in elems {
                collect_resources_in_type(elem, registry, out);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            collect_resources_in_type(inner, registry, out);
        }
        Type::Function(_) | Type::TypePackSpread(_, _) | Type::Error(_) => {}
    }
}
