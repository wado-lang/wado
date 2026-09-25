//! The complete set of CM interfaces the component imports, resolved as
//! structured data at the WIR layer — the one place the import decision lives.
//! Codegen, the WIT producer, and the CM embedding all read this plan rather
//! than re-deriving membership, upholding `codegen.rs`'s "emit `Package` as is".
//! `tests/wit_import_plan.rs` asserts the plan matches the compiled component.

use crate::ast::Type;
use crate::canonical::{CanonicalIntrinsic, CmFuturePayload};
use crate::component_model::{
    CANONICAL_ERROR_CODE_INTERFACE, CmInterfaceRegistry, ERROR_CODE_WADO_NAME, cm_decl_in_interface,
};
use crate::hashmap::IndexSet;
use crate::nir_package::NirPackage;
use crate::wir::{ImportEntry, ImportKind};
use crate::wir_build::component_plan::CmExportType;

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
        for cm_ty in export
            .cm_params
            .iter()
            .map(|(_, ty)| ty)
            .chain(&export.cm_result)
        {
            collect_export_interface_fqs(cm_ty, &mut export_referenced_interfaces);
        }
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
    let mut needed_resources: IndexSet<(String, String)> = IndexSet::default();
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
                    && project.used_wasi_functions.contains(&func.used_key())
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
                    && project.used_wasi_functions.contains(&func.used_key())
            })
            .collect();
        if used.is_empty() {
            continue;
        }
        let here = registry.resources_in_signatures(&used, Some(&interface_info.path));
        let uses_external_resources = here
            .iter()
            .any(|(source, _)| source != &interface_info.path);
        let kind = if registry.is_component_interface(&interface_info.path) {
            // Imported like a host interface, but the dependency is composed in
            // at codegen (wasm-compose) rather than provided by the host.
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
    let mut worklist: Vec<String> = needed_resources
        .iter()
        .map(|(source, _)| source.clone())
        .collect();
    let mut seen_sources: IndexSet<String> = IndexSet::default();
    while let Some(source) = worklist.pop() {
        if !seen_sources.insert(source.clone()) {
            continue;
        }
        push(&mut entries, source.clone(), ImportKind::ResourceSource);
        for info in registry.interfaces().filter(|i| i.path == source) {
            let funcs: Vec<_> = info.functions.iter().collect();
            let more = registry.resources_in_signatures(&funcs, Some(&source));
            worklist.extend(more.into_iter().map(|(next, _)| next));
        }
    }

    // Phase 3: resource-getter interfaces whose accessor is used. Each such
    // getter contributes the getter interface itself (http ones going through
    // the HTTP phase) and, for a resource defined elsewhere, its defining
    // interface.
    for interface_info in registry.interfaces() {
        let Some((resource_wado_name, _)) = &interface_info.resource_type else {
            continue;
        };
        // This interface's own used operations, not merely some operation of a
        // Wado name it shares: a user module may name a resource what a bundled
        // interface names one, and the shared name alone would import that
        // bundled interface into a program that never mentions it.
        let needed = interface_info
            .functions
            .iter()
            .any(|f| project.used_wasi_functions.contains(&f.used_key()));
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
        if let Some(source) =
            registry.resource_source_in(Some(&interface_info.path), resource_wado_name)
            && source != interface_info.path
        {
            push(&mut entries, source.to_string(), ImportKind::ResourceSource);
        }
    }

    // Phase 4: world-level function imports (Phase 9). Composed away by
    // `wasm-compose`, so they drop out of `imported_cm_interface_fqs`.
    for (name, _) in registry.world_import_functions() {
        push(&mut entries, name.to_string(), ImportKind::WorldFunction);
    }

    entries
}

/// Collect the CM interface FQs a world export's boundary type references, so a
/// resource-defining interface is imported when an export's signature needs its
/// types even if no function of it is called.
fn collect_export_interface_fqs(ty: &CmExportType, out: &mut IndexSet<String>) {
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

/// The flat sorted FQ list of interfaces the *composed* artifact imports, for
/// the WIT producer's world import refs and the import-plan faithfulness check.
///
/// An `ImportKind::Component` entry's interface is composed away, so it is
/// substituted with the dependency's own host-leaf import FQs (empty for a pure
/// dependency); the list then mirrors the composed binary, not the
/// pre-composition core.
#[must_use]
pub fn imported_cm_interface_fqs(project: &NirPackage, plan: &[ImportEntry]) -> Vec<String> {
    let registry = &project.cm_interface_registry;
    let mut out: IndexSet<String> = IndexSet::default();
    for entry in plan {
        match entry.kind {
            ImportKind::Component => {
                for fq in registry.host_leaf_imports_for(&entry.fq) {
                    out.insert(fq.clone());
                }
            }
            // Composed away by `wasm-compose`; a pure dependency function
            // contributes no surviving import.
            ImportKind::WorldFunction => {}
            _ => {
                out.insert(entry.fq.clone());
            }
        }
    }
    let mut list: Vec<String> = out.into_iter().collect();
    list.sort();
    list
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
            project.used_wasi_functions.contains(&func.used_key())
                && (func.return_type.as_ref().is_some_and(|ty| {
                    references_cli_error_code(ty, &project.cm_interface_registry)
                }) || func.params.iter().any(|(_, _, ty)| {
                    references_cli_error_code(ty, &project.cm_interface_registry)
                }))
        })
    });
    if import_side {
        return true;
    }

    // Transmission side: an async-export transmission future carrying the
    // canonical error-code itself, e.g. a kiln generator's
    // `task return result<_, error-code>`.
    let Some(canonical_error_code) = cm_decl_in_interface(
        &project.type_table.borrow(),
        &project.cm_interface_registry,
        CANONICAL_ERROR_CODE_INTERFACE,
        ERROR_CODE_WADO_NAME,
    ) else {
        return false;
    };
    needed_canonicals.iter().any(|canonical| {
        matches!(
            canonical.future_payload(),
            Some(CmFuturePayload::Transmission(decl)) if decl.def() == canonical_error_code
        )
    })
}

/// Whether `ty` references the canonical `wasi:cli/types` `ErrorCode`.
fn references_cli_error_code(ty: &Type, registry: &CmInterfaceRegistry) -> bool {
    let any = |tys: &[Type]| tys.iter().any(|ty| references_cli_error_code(ty, registry));
    match ty {
        Type::Named(named) => {
            named.name == ERROR_CODE_WADO_NAME
                && registry
                    .source_interface(named)
                    .is_some_and(|s| s.starts_with("wasi:cli/types"))
        }
        Type::Generic(generic) => any(&generic.args),
        Type::NamespacedGeneric(generic) => any(&generic.args),
        Type::Tuple(elems) => any(elems),
        Type::Reference(inner) | Type::MutReference(inner) => {
            references_cli_error_code(inner, registry)
        }
        Type::Function(_) | Type::TypePackSpread(_, _) | Type::Infer(_) | Type::Error(_) => false,
    }
}
