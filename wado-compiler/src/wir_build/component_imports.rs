//! The complete set of CM interfaces the component imports, resolved as
//! structured data at the WIR layer.
//!
//! This is where the import decision lives. Codegen (`generate_cm_imports` + the
//! resource and HTTP phases), the WIT producer (`wit_emit`), and the CM embedding
//! all read this one plan rather than each re-deriving membership from the
//! registry / `used_wasi_functions` — upholding the `codegen.rs` principle
//! ("emit `Package` as is, without knowledge of earlier phases"). See WEP
//! `wep-2026-05-02-wit-interoperability.md` §"Faithful imports".
//!
//! `tests/wit_import_plan.rs` asserts the plan equals the compiled component's
//! actual CM imports, so the two cannot drift.

use crate::ast::Type;
use crate::component_model::CmInterfaceRegistry;
use crate::hashmap::IndexSet;
use crate::nir_package::NirPackage;
use crate::wir::{CanonicalIntrinsic, CmFuturePayload, ImportEntry, ImportKind};

/// Resolve the categorized import plan for `project` from `used_wasi_functions`,
/// the registry, and the WIR-level canonical intrinsics. This is the decision
/// codegen's import phases then read and encode. Entries are in codegen emission
/// order.
#[must_use]
pub fn resolve_import_plan(
    project: &NirPackage,
    needed_canonicals: &IndexSet<CanonicalIntrinsic>,
) -> Vec<ImportEntry> {
    let registry = &project.cm_interface_registry;
    let mut entries: Vec<ImportEntry> = Vec::new();
    // Dedup by FQ alone — one import per interface, the first kind pushed wins.
    // An interface that both defines resources and exposes a getter (e.g.
    // `wasi:http/types`) matches both Phase 1 (`ResourceDefiningInterface`) and
    // the Phase 3 getter scan; Phase 1 runs first, so the resource-defining kind
    // wins and the getter push is deduped away. The remaining shapes
    // (function-bearing, resource-source, pure getter) are mutually exclusive by
    // construction.
    let mut seen: IndexSet<String> = IndexSet::default();
    let mut push = |entries: &mut Vec<ImportEntry>, fq: String, kind: ImportKind| {
        if seen.insert(fq.clone()) {
            entries.push(ImportEntry { fq, kind });
        }
    };

    let mut export_referenced_interfaces: IndexSet<String> = IndexSet::default();
    for export in &project.component_plan.world_exports {
        for (_, cm_ty) in &export.cm_params {
            collect_export_interface_fqs(cm_ty, &mut export_referenced_interfaces);
        }
        collect_export_interface_fqs(&export.cm_result, &mut export_referenced_interfaces);
    }

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
        if interface_info.interface == "run" {
            continue;
        }
        // An interface that both defines its own resources and exposes an
        // `option<resource>` getter method (e.g. `wasi:http/types`) is encoded
        // by the dedicated resource-defining pass. A pure getter (defines no
        // resources, e.g. `wasi:cli/terminal-stdin`) is handled by Phase 3; a
        // resource-definer without a getter (`wasi:sockets/types`) flows through
        // the function-interface path below.
        let defines_resources = registry
            .resources_for_interface(&interface_info.path)
            .next()
            .is_some();
        if defines_resources && interface_info.resource_type.is_some() {
            // Import when a function of this interface is used, or when a world
            // export's signature references its types (e.g. a handler returning
            // `Result<Response, ErrorCode>` that constructs no request/response
            // itself still needs the response/error-code types).
            let has_used_function = interface_info.functions.iter().any(|func| {
                registry.is_function_supported(func)
                    && project
                        .used_wasi_functions
                        .contains(&format!("{}::{}", func.interface_name, func.method_name))
            });
            if has_used_function || export_referenced_interfaces.contains(&interface_info.path) {
                push(
                    &mut entries,
                    interface_info.path.clone(),
                    ImportKind::ResourceDefiningInterface,
                );
            }
            continue;
        }
        if interface_info.resource_type.is_some() {
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
        let kind = if registry.is_component_interface(&interface_info.path) {
            // A linked CM component is satisfied by embedding, not a host import.
            ImportKind::Component
        } else if uses_external_resources {
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

    // Phase 3: resource-getter interfaces (returning `option<resource>`) whose
    // accessor is used. The gate is `NirPackage::has_interface` — whether a
    // `{interface}::` function appears in `used_wasi_functions` — which is the
    // gate codegen applies inline, NOT the registry's same-named `with`-set
    // predicate; the two are distinct. For each used getter the plan carries two
    // imports: the getter interface itself (`ResourceGetter`; codegen's getter
    // phase — http getters go through the HTTP phase instead, so they are
    // excluded here) and, when the returned resource is *defined elsewhere*, that
    // defining interface (`ResourceSource`, so the resource type is in scope
    // before the getter is encoded).
    for interface_info in registry.interfaces() {
        let Some((resource_wado_name, _)) = &interface_info.resource_type else {
            continue;
        };
        let needed = interface_info
            .functions
            .first()
            .is_some_and(|f| project.has_interface(&f.interface_name));
        if !needed {
            continue;
        }
        // A resource-defining interface was already pushed in Phase 1 (deduped
        // by FQ); only a pure getter is newly pushed here.
        push(
            &mut entries,
            interface_info.path.clone(),
            ImportKind::ResourceGetter,
        );
        if let Some(source) = registry.get_resource_source_interface(resource_wado_name)
            && source != interface_info.path
        {
            push(&mut entries, source.to_string(), ImportKind::ResourceSource);
        }
    }

    entries
}

/// Collect the CM interface FQs a world export's boundary type references, so a
/// resource-defining interface is imported when an export's signature needs its
/// types even if no function of it is called.
fn collect_export_interface_fqs(
    ty: &crate::wir_build::component_plan::CmExportType,
    out: &mut IndexSet<String>,
) {
    use crate::wir_build::component_plan::CmExportType;
    match ty {
        CmExportType::Unit => {}
        CmExportType::Primitive(_) => {}
        CmExportType::Named { interface_fq, .. } => {
            out.insert(interface_fq.clone());
        }
        CmExportType::HandlerResult { ok, err } => {
            collect_export_interface_fqs(ok, out);
            collect_export_interface_fqs(err, out);
        }
    }
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
        Type::Function(_) | Type::TypePackSpread(_, _) | Type::Infer(_) | Type::Error(_) => false,
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
        Type::Function(_) | Type::TypePackSpread(_, _) | Type::Infer(_) | Type::Error(_) => {}
    }
}
