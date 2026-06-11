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

    // Phase 0: `wasi:cli/types` provides the shared `error-code` enum, the
    // canonical fallback for `result<_, error-code>` bindings. It is needed iff
    // a used interface's signature actually references that error-code (the cli
    // stdin/stdout/stderr interfaces); a pure-compute or clock-only program
    // does not pull it in. Codegen gates its Phase 0 on the same predicate.
    if needs_canonical_cli_error_code(project)
        && let Some(version) = registry.get_cli_version()
    {
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

/// Whether the component needs the canonical `wasi:cli/types#error-code` —
/// i.e. a used WASI function's signature references it. Codegen's Phase 0
/// `wasi:cli/types` import and this plan are both gated on this predicate, so
/// the dead import is dropped for programs that never touch that error-code.
#[must_use]
pub fn needs_canonical_cli_error_code(project: &NirPackage) -> bool {
    project.cm_interface_registry.interfaces().any(|interface| {
        interface.functions.iter().any(|func| {
            let key = format!("{}::{}", func.interface_name, func.method_name);
            project.used_wasi_functions.contains(&key)
                && (func
                    .return_type
                    .as_ref()
                    .is_some_and(references_cli_error_code)
                    || func
                        .params
                        .iter()
                        .any(|(_, _, ty)| references_cli_error_code(ty)))
        })
    })
}

/// Whether `ty` references the canonical `wasi:cli/types` `ErrorCode`.
fn references_cli_error_code(ty: &Type) -> bool {
    match ty {
        Type::Named(named) => {
            named.name == "ErrorCode"
                && named
                    .source_interface
                    .as_deref()
                    .is_some_and(|s| s.starts_with("wasi:cli/types"))
        }
        Type::Generic(generic) => generic.args.iter().any(references_cli_error_code),
        Type::NamespacedGeneric(generic) => generic.args.iter().any(references_cli_error_code),
        Type::Tuple(elems) => elems.iter().any(references_cli_error_code),
        Type::Reference(inner) | Type::MutReference(inner) => references_cli_error_code(inner),
        Type::Function(_) | Type::TypePackSpread(_, _) | Type::Error(_) => false,
    }
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
