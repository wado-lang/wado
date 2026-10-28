//! The complete set of CM interfaces the component imports, resolved as
//! structured data at the WIR layer — the one place the import decision lives.
//! Codegen, the WIT producer, and the CM embedding all read this plan rather
//! than re-deriving membership, upholding `codegen.rs`'s "emit `Package` as is".
//! `tests/wit_import_plan.rs` asserts the plan matches the compiled component.

use crate::ast::Type;
use crate::canonical::{CanonicalIntrinsic, CmDeclKind, CmFuturePayload};
use crate::component_model::{
    CANONICAL_ERROR_CODE_INTERFACE, CmFunctionInfo, ERROR_CODE_WADO_NAME, cm_decl_in_interface,
};
use crate::hashmap::{IndexMap, IndexSet};
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
            entries.push(ImportEntry {
                fq,
                kind,
                provides: IndexSet::default(),
            });
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
    // interface is categorized `ResourceUsingInterface`, imported after the
    // interfaces defining those resources; otherwise it is a plain
    // `FunctionInterface`.
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
                    && project.used_wasi_functions.contains(&func.cm_identifier())
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
                    && project.used_wasi_functions.contains(&func.cm_identifier())
            })
            .collect();
        if used.is_empty() {
            continue;
        }
        let here = registry.resources_in_signatures(&used);
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

    // Phase 2: the interfaces defining every resource a signature or canonical
    // reaches, transitively, since a defining interface may reach more.
    let mut worklist: Vec<String> = needed_resources
        .iter()
        .map(|(source, _)| source.clone())
        .collect();
    for canonical in needed_canonicals {
        canonical.for_each_decl(&mut |decl, kind| {
            if kind == CmDeclKind::Resource
                && let Some((source, _)) = registry.declared_cm_type(decl)
            {
                worklist.push(source.to_string());
            }
        });
    }
    let mut seen_sources: IndexSet<String> = IndexSet::default();
    while let Some(source) = worklist.pop() {
        if !seen_sources.insert(source.clone()) {
            continue;
        }
        push(&mut entries, source.clone(), ImportKind::TypeSource);
        for info in registry.interfaces().filter(|i| i.path == source) {
            let funcs: Vec<_> = info.functions.iter().collect();
            let more = registry.resources_in_signatures(&funcs);
            worklist.extend(more.into_iter().map(|(next, _)| next));
        }
    }

    // Phase 3: resource-getter interfaces whose accessor is used. Each such
    // getter contributes the getter interface itself (http ones going through
    // the HTTP phase) and, for a resource defined elsewhere, its defining
    // interface.
    for interface_info in registry.interfaces() {
        let Some(resource) = &interface_info.resource_type else {
            continue;
        };
        // This interface's own used operations, not merely some operation of a
        // Wado name it shares: a user module may name a resource what a bundled
        // interface names one, and the shared name alone would import that
        // bundled interface into a program that never mentions it.
        let needed = interface_info
            .functions
            .iter()
            .any(|f| project.used_wasi_functions.contains(&f.cm_identifier()));
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
        if resource.source != interface_info.path {
            push(
                &mut entries,
                resource.source.clone(),
                ImportKind::TypeSource,
            );
        }
    }

    // Phase 4: world-level function imports (Phase 9). Composed away by
    // `wasm-compose`, so they drop out of `imported_cm_interface_fqs`.
    for func in registry.world_import_functions() {
        push(
            &mut entries,
            func.cm_identifier(),
            ImportKind::WorldFunction,
        );
    }

    // Phase 5: the interfaces declaring a type another import reaches, each
    // exporting the types reached and imported before its readers, so a reader
    // aliases the one type its declaring interface exports, as a WIT `use`
    // reads.
    let reached = provide_reached_types(project, &mut entries);
    emission_order(entries, &reached)
}

/// Record on each interface the value types other imports reach from it,
/// adding a `TypeSource` for an interface no entry imports yet. Answers, by
/// FQ, the interfaces each import reaches a type of. An interface a nested
/// dependency provides is never imported, so it reaches and provides nothing.
fn provide_reached_types(
    project: &NirPackage,
    entries: &mut Vec<ImportEntry>,
) -> IndexMap<String, IndexSet<String>> {
    let registry = &project.cm_interface_registry;
    let nested_dependencies = project.nested_dependencies();
    let nested = NirPackage::nested_imports(&nested_dependencies);
    let mut reached: IndexMap<String, IndexSet<String>> = IndexMap::default();
    let mut worklist: Vec<(String, Vec<Type>)> = entries
        .iter()
        .filter(|entry| !nested.contains(entry.fq.as_str()))
        .map(|entry| (entry.fq.clone(), instance_types(project, entry)))
        .collect();
    loop {
        while let Some((reader, types)) = worklist.pop() {
            for (source, name) in registry.foreign_types_in(&reader, &types) {
                if nested.contains(source.as_str()) {
                    continue;
                }
                reached
                    .entry(reader.clone())
                    .or_default()
                    .insert(source.clone());
                if registry.value_type_kind(&source, &name).is_some() {
                    provide(project, entries, &mut worklist, &source, &name);
                }
            }
        }
        // A nested dependency is handed the program's own import of a
        // types-only interface it imports too, which must then carry every
        // type the dependency's import names.
        let shared: Vec<(String, String)> = nested_dependencies
            .iter()
            .flat_map(|dependency| dependency.imports)
            .filter(|fq| {
                !registry.declares_functions(fq) && entries.iter().any(|entry| &entry.fq == *fq)
            })
            .flat_map(|fq| {
                registry
                    .declared_type_names(fq)
                    .filter(|name| {
                        registry.value_type_kind(fq, name).is_some()
                            || registry.newtype(fq, name).is_some()
                    })
                    .map(|name| (fq.clone(), name.to_string()))
            })
            .collect();
        for (fq, name) in &shared {
            provide(project, entries, &mut worklist, fq, name);
        }
        if worklist.is_empty() {
            return reached;
        }
    }
}

/// Have `source` provide its type `name`, adding a `TypeSource` for it where
/// no entry imports it yet, and queue what that type reaches in turn.
fn provide(
    project: &NirPackage,
    entries: &mut Vec<ImportEntry>,
    worklist: &mut Vec<(String, Vec<Type>)>,
    source: &str,
    name: &str,
) {
    let index = entries
        .iter()
        .position(|entry| entry.fq == source)
        .unwrap_or_else(|| {
            let entry = ImportEntry {
                fq: source.to_string(),
                kind: ImportKind::TypeSource,
                provides: IndexSet::default(),
            };
            worklist.push((source.to_string(), instance_types(project, &entry)));
            entries.push(entry);
            entries.len() - 1
        });
    if entries[index].provides.insert(name.to_string()) {
        let site = project
            .cm_interface_registry
            .declared_type(source, name)
            .expect("a provided type has a declaration");
        worklist.push((source.to_string(), vec![site]));
    }
}

/// The types `entry`'s instance type spells before any it provides: its
/// emitted signatures, or a `TypeSource`'s own `error-code`.
fn instance_types(project: &NirPackage, entry: &ImportEntry) -> Vec<Type> {
    let registry = &project.cm_interface_registry;
    let signature_types = |funcs: Vec<&CmFunctionInfo>| -> Vec<Type> {
        funcs
            .into_iter()
            .flat_map(|func| {
                func.params
                    .iter()
                    .map(|(_, _, ty)| ty)
                    .chain(&func.return_type)
                    .cloned()
            })
            .collect()
    };
    let interface = || {
        registry
            .interfaces()
            .find(|interface| interface.path == entry.fq)
            .unwrap_or_else(|| panic!("`{}` is planned, but not registered", entry.fq))
    };
    match entry.kind {
        ImportKind::FunctionInterface
        | ImportKind::Component
        | ImportKind::ResourceUsingInterface
        | ImportKind::ResourceGetter => {
            let interface = interface();
            signature_types(
                interface
                    .functions
                    .iter()
                    .filter(|func| project.emits_cm_function(func))
                    .collect(),
            )
        }
        ImportKind::ResourceDefiningInterface => {
            let interface = interface();
            signature_types(project.resource_defining_functions(&interface))
        }
        ImportKind::TypeSource => registry
            .declared_type(&entry.fq, ERROR_CODE_WADO_NAME)
            .into_iter()
            .collect(),
        ImportKind::SharedTypes | ImportKind::WorldFunction => Vec::new(),
    }
}

/// `entries` in the order codegen emits them: by phase, and each after the
/// interfaces declaring the types it reaches.
fn emission_order(
    mut entries: Vec<ImportEntry>,
    reached: &IndexMap<String, IndexSet<String>>,
) -> Vec<ImportEntry> {
    entries.sort_by_key(|entry| emission_phase(entry.kind));
    let position: IndexMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.fq.as_str(), index))
        .collect();
    let mut order: IndexSet<usize> = IndexSet::default();
    let mut visiting: IndexSet<usize> = IndexSet::default();
    for index in 0..entries.len() {
        place(
            index,
            &entries,
            &position,
            reached,
            &mut visiting,
            &mut order,
        );
    }
    let mut slots: Vec<Option<ImportEntry>> = entries.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|index| slots[index].take().expect("each entry is placed once"))
        .collect()
}

fn place(
    index: usize,
    entries: &[ImportEntry],
    position: &IndexMap<&str, usize>,
    reached: &IndexMap<String, IndexSet<String>>,
    visiting: &mut IndexSet<usize>,
    order: &mut IndexSet<usize>,
) {
    if order.contains(&index) {
        return;
    }
    assert!(
        visiting.insert(index),
        "`{}` reaches its own types through another interface; a WIT `use` graph is acyclic",
        entries[index].fq
    );
    for source in reached.get(&entries[index].fq).into_iter().flatten() {
        if let Some(&source_index) = position.get(source.as_str()) {
            place(source_index, entries, position, reached, visiting, order);
        }
    }
    order.insert(index);
}

/// Where codegen's phases put each kind when no reached type says otherwise:
/// a resource's interface before the ones handing it out or taking it.
fn emission_phase(kind: ImportKind) -> u8 {
    match kind {
        ImportKind::SharedTypes => 0,
        ImportKind::FunctionInterface | ImportKind::Component => 1,
        ImportKind::ResourceDefiningInterface => 2,
        ImportKind::TypeSource => 3,
        ImportKind::ResourceGetter => 4,
        ImportKind::ResourceUsingInterface => 5,
        ImportKind::WorldFunction => 6,
    }
}

/// Collect the CM interface FQs a world export's boundary type references, so a
/// resource-defining interface is imported when an export's signature needs its
/// types even if no function of it is called.
fn collect_export_interface_fqs(ty: &CmExportType, out: &mut IndexSet<String>) {
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
/// dependency), less those the program's handlers answer; the list then
/// mirrors the composed binary, not the pre-composition core.
#[must_use]
pub fn imported_cm_interface_fqs(project: &NirPackage, plan: &[ImportEntry]) -> Vec<String> {
    let registry = &project.cm_interface_registry;
    let answered = project.component_plan.answered_interfaces();
    let nested_dependencies = project.nested_dependencies();
    let nested = NirPackage::nested_imports(&nested_dependencies);
    let mut out: IndexSet<String> = IndexSet::default();
    for entry in plan {
        match entry.kind {
            ImportKind::Component => {
                let is_nested = nested.contains(entry.fq.as_str());
                for fq in registry.host_leaf_imports_for(&entry.fq) {
                    let defined = answered.contains(fq.as_str())
                        || (is_nested && !registry.declares_functions(fq));
                    if !defined {
                        out.insert(fq.clone());
                    }
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
    let registry = &project.cm_interface_registry;
    let type_table = project.type_table.borrow();
    let Some(canonical_error_code) = cm_decl_in_interface(
        &type_table,
        registry,
        CANONICAL_ERROR_CODE_INTERFACE,
        ERROR_CODE_WADO_NAME,
    ) else {
        return false;
    };
    let references = |ty: &Type| {
        ty.any(&mut |ty| {
            matches!(ty, Type::Named(named)
                if registry.declared_def(named, type_table.defs()) == Some(canonical_error_code))
        })
    };

    // Import side: a used interface's signature references the cli error-code.
    let import_side = registry.interfaces().any(|interface| {
        interface.functions.iter().any(|func| {
            project.used_wasi_functions.contains(&func.cm_identifier())
                && (func.return_type.as_ref().is_some_and(references)
                    || func.params.iter().any(|(_, _, ty)| references(ty)))
        })
    });
    if import_side {
        return true;
    }

    // Transmission side: an async-export transmission future carrying the
    // canonical error-code itself, e.g. a kiln generator's
    // `task return result<_, error-code>`.
    needed_canonicals.iter().any(|canonical| {
        matches!(
            canonical.future_payload(),
            Some(CmFuturePayload::Transmission(decl)) if decl.def() == canonical_error_code
        )
    })
}
