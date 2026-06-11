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
use crate::wir::{CanonicalIntrinsic, CmFuturePayload, ImportEntry, ImportKind};

/// Resolve the categorized import plan for `project`, mirroring the decision
/// codegen's import phases make from `used_wasi_functions`, the registry, and
/// the WIR-level canonical intrinsics. Entries are in codegen emission order.
#[must_use]
pub fn resolve_import_plan(
    project: &NirPackage,
    needed_canonicals: &IndexSet<CanonicalIntrinsic>,
) -> Vec<ImportEntry> {
    let registry = project.cm_interface_registry;
    let mut entries: Vec<ImportEntry> = Vec::new();
    let mut seen: IndexSet<String> = IndexSet::default();
    let mut push = |entries: &mut Vec<ImportEntry>, fq: String, kind: ImportKind| {
        if seen.insert(fq.clone()) {
            entries.push(ImportEntry { fq, kind });
        }
    };

    // No blanket "kiln forbids WASI" early-return: the kiln-generator world
    // forbids WASI *interfaces*, and `optimize/dce.rs` keeps `used_wasi_functions`
    // empty for it, so the interface phases below yield nothing. But its
    // `task return` transmission future still needs the canonical cli
    // `error-code`, so Phase 0 stays governed by the predicate below.

    // Phase 0: `wasi:cli/types` (shared `error-code` enum) — needed iff a used
    // interface references the cli error-code OR an async-export transmission
    // future resolves to it (`Transmission("cli")`, e.g. a kiln generator).
    if needs_canonical_cli_error_code(project, needed_canonicals)
        && let Some(version) = registry.get_cli_version()
    {
        push(
            &mut entries,
            format!("wasi:cli/types@{version}"),
            ImportKind::SharedTypes,
        );
    }

    // Phase 1: every interface with a function in `used_wasi_functions`, in
    // registry order. Records the resources those signatures touch for Phase 2.
    // An interface whose used signatures reference a resource *defined by another*
    // interface is categorized `ResourceUsingInterface` (codegen defers it to the
    // resource-using phase, after the resource-defining interfaces are imported);
    // otherwise it is a plain `FunctionInterface`.
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
        let mut here: IndexSet<String> = IndexSet::default();
        for func in &used {
            if let Some(ret) = &func.return_type {
                collect_resources_in_type(ret, registry, &mut here);
            }
            for (_, _, ty) in &func.params {
                collect_resources_in_type(ty, registry, &mut here);
            }
        }
        let uses_external_resources = here.iter().any(|resource| {
            registry
                .get_resource_source_interface(resource)
                .is_some_and(|src| src != interface_info.path)
        });
        let kind = if uses_external_resources {
            ImportKind::ResourceUsingInterface
        } else {
            ImportKind::FunctionInterface
        };
        push(&mut entries, interface_info.path.clone(), kind);
        needed_resources.extend(here);
    }

    // Phase 2: resource-defining interfaces for every referenced resource
    // (transitive: a defining interface may reference further resources).
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
        push(&mut entries, source.clone(), ImportKind::ResourceSource);
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

    // Phase 2 (continued): mirror codegen's `is_needed` — a resource-getter
    // interface whose accessor is used (`NirPackage::has_interface`, i.e. a
    // `{interface}::` entry exists in `used_wasi_functions`) pulls in the
    // interface that *defines* its resource. NOTE: `has_interface` here is the
    // `NirPackage` method (used-function membership), NOT the registry's
    // `with`-set predicate; the two are distinct and codegen uses this one.
    for interface_info in registry.interfaces() {
        let Some((resource_wado_name, _)) = &interface_info.resource_type else {
            continue;
        };
        let Some(source) = registry.get_resource_source_interface(resource_wado_name) else {
            continue;
        };
        if source == interface_info.path {
            continue;
        }
        let needed = interface_info
            .functions
            .first()
            .is_some_and(|f| project.has_interface(&f.interface_name));
        if needed {
            push(&mut entries, source.to_string(), ImportKind::ResourceSource);
        }
    }

    // Phase 3: resource-getter interfaces (returning `option<resource>`). Codegen
    // imports one iff its accessor is used — `NirPackage::has_interface`, which
    // tests whether a `{interface}::` function appears in `used_wasi_functions`
    // (not the registry's `with`-set predicate of the same name). Mirror that
    // gate here so the getter FQ lands in the plan (and the faithful world
    // import set).
    for interface_info in registry.interfaces() {
        if interface_info.resource_type.is_none() || interface_info.package == "http" {
            continue;
        }
        let needed = interface_info
            .functions
            .first()
            .is_some_and(|f| project.has_interface(&f.interface_name));
        if needed {
            push(
                &mut entries,
                interface_info.path.clone(),
                ImportKind::ResourceGetter,
            );
        }
    }

    // Phase 4: HTTP interfaces under the handler / Client conditions.
    if (project.has_http_handler_export || project.has_interface("Client"))
        && let Some(version) = registry.get_package_version("http")
    {
        push(
            &mut entries,
            format!("wasi:http/types@{version}"),
            ImportKind::HttpTypes,
        );
    }
    if project.has_interface("Client")
        && let Some(version) = registry.get_package_version("http")
    {
        push(
            &mut entries,
            format!("wasi:http/client@{version}"),
            ImportKind::HttpClient,
        );
    }

    entries
}

/// The flat sorted FQ list, for the WIT producer's world import refs.
#[must_use]
pub fn import_plan_fqs(plan: &[ImportEntry]) -> Vec<String> {
    let mut out: Vec<String> = plan.iter().map(|e| e.fq.clone()).collect();
    out.sort();
    out
}

/// Whether the component needs the canonical `wasi:cli/types#error-code`.
/// Codegen's Phase 0 import is gated on the plan, which is gated on this.
fn needs_canonical_cli_error_code(
    project: &NirPackage,
    needed_canonicals: &IndexSet<CanonicalIntrinsic>,
) -> bool {
    // Import side: a used interface's signature references the cli error-code.
    let import_side = project.cm_interface_registry.interfaces().any(|interface| {
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
    });
    if import_side {
        return true;
    }

    // Transmission side: an async-export transmission future whose error-code
    // source is `cli` (the canonical error-code), e.g. a kiln generator's
    // `task return result<_, error-code>`.
    needed_canonicals.iter().any(|canonical| {
        matches!(
            canonical.future_payload(),
            Some(CmFuturePayload::Transmission(source)) if source == "cli"
        )
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
