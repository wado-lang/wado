//! CM Binding Synthesis: TIR binding functions for Component Model boundary
//! crossing, each lowering Wado values to the CM flat ABI and lifting flat
//! results back. Runs after `effect_check` and before monomorphize, so the
//! bindings go through monomorphization, lowering and optimization like any
//! other function. Design: `docs/wep-2026-02-15-cm-binding-synthesis.md`.

mod cm_free;
mod export_adapter;
mod import_adapter;
mod lift;
mod lower;
mod resource_rewrite;
mod task_return;
mod type_fixup;
mod types;

use std::cell::RefCell;
use std::rc::Rc;

use crate::cm_abi::CmValType;
use crate::hashmap::{IndexMap, IndexSet};

use crate::canonical::{CanonicalIntrinsic, CmPayloadType};
use crate::compiler_item::CompilerItem;
use crate::module_source::{CmNamespace, ModuleSource};
use crate::name::DeclPath;
use crate::package::Package;
use crate::tir::{ResolvedType, TirExpr, TirExprKind, TirFunction, TirModule, TypeId, TypeTable};
use crate::tir_visitor::TirRefVisitor;
use crate::world_registry::{WorldExportInfo, WorldInfo};

pub use export_adapter::export_binding_func_name;
use export_adapter::{
    ExportBindingEnv, ExportReturnStrategy, post_return_func_name, synthesize_export_binding,
    synthesize_post_return,
};
pub use import_adapter::binding_func_name;
use import_adapter::synthesize_adapter;
pub use lift::synthesize_lift;
pub use lower::synthesize_lower;
use resource_rewrite::{
    rewrite_cm_resource_methods, synthesize_future_reads, synthesize_future_writes,
    synthesize_record_stream_reads, synthesize_stream_reads, synthesize_stream_writes,
};
use task_return::{expand_task_returns_in_func, strip_task_returns_in_func};
use type_fixup::{
    collect_effect_calls_in_block, collect_local_type_updates, rewrite_calls_in_block,
};
pub use types::{
    LiftContext, cm_enum_byte_size, cm_flags_byte_size, cm_type_to_type_id, flatten_param_type,
};
use types::{cm_val_type_to_type_id, compute_export_flat_return_types};

/// Build a `(module_source, name)` set for every effect/resource declared in
/// the loaded TIR modules. The CM binding synthesizer uses this to attach the
/// owning effect to each generated binding using the same `module_source` the
/// elaborator assigns to user-written `with E` clauses.
///
/// Keying by `(module_source, name)` (rather than name alone) prevents
/// collisions when two modules declare an effect or resource with the same
/// name — `lookup_effect_owner` selects the canonical WASI module.
fn effect_owner_module_sources(
    modules: &IndexMap<ModuleSource, TirModule>,
) -> IndexSet<(ModuleSource, String)> {
    let mut out: IndexSet<(ModuleSource, String)> = IndexSet::default();
    for (module_source, module) in modules {
        for effect in &module.effects {
            out.insert((module_source.clone(), effect.name.clone()));
        }
        for resource in &module.resources {
            out.insert((module_source.clone(), resource.name.clone()));
        }
    }
    out
}

/// The canonical owning module for an effect/resource named `name` whose
/// binding targets CM `package`. A [`ModuleSource::Binding`] under
/// `"{package}/"` wins; any other owner of the name is the fallback.
fn lookup_effect_owner(
    owners: &IndexSet<(ModuleSource, String)>,
    name: &str,
    package: &str,
) -> Option<ModuleSource> {
    let mut fallback: Option<ModuleSource> = None;
    for (ms, n) in owners {
        if n != name {
            continue;
        }
        if let ModuleSource::Binding { interface, .. } = ms
            && interface
                .strip_prefix(package)
                .is_some_and(|rest| rest.starts_with('/'))
        {
            return Some(ms.clone());
        }
        if fallback.is_none() {
            fallback = Some(ms.clone());
        }
    }
    fallback
}

use record_payload_validation::{RecordPayloadsValidated, reject_unresolvable_record_payloads};

/// The record-payload scan and its witness, together in one module so the
/// witness's private field can only be minted by the scan — a caller elsewhere
/// cannot fabricate the `RecordPayloadsValidated` proof to skip the scan.
mod record_payload_validation {
    use crate::package::Package;
    use crate::tir_visitor::TirRefVisitor;

    /// Witness that [`reject_unresolvable_record_payloads`] ran while the TIR
    /// still carried the pristine `future-new`/`stream-new` call shape its scan
    /// matches. [`super::rewrite_async_primitives`] consumes it by value, so
    /// reordering the validation after the rewrites fails to compile. The
    /// private field makes the scan the only place that can mint one.
    pub(in crate::synthesis::cm_binding) struct RecordPayloadsValidated(());

    /// Reject a user record used as a `future`/`stream` payload when its fields
    /// are not registered in the CM interface registry: with no CM type to lower
    /// against, the lower would mis-treat it as an i32 handle and emit an invalid
    /// component. Records are registered only for `--lib` components. Scoped to
    /// functions reachable from the active world's export bindings.
    pub(in crate::synthesis::cm_binding) fn reject_unresolvable_record_payloads(
        project: &Package,
    ) -> Result<RecordPayloadsValidated, String> {
        let reachable = super::reachable_from_export_bindings(project);
        for (module_source, module) in &project.tir_modules {
            let tt = module.type_table.borrow();
            for func_rc in &module.functions {
                let func = func_rc.borrow();
                let Some(body) = &func.body else { continue };
                let mut finder = super::NamedPayloadFinder {
                    tt: &tt,
                    registry: project.cm_interface_registry.as_ref(),
                    check_records: reachable.contains(&(module_source.clone(), func.name.clone())),
                    found: None,
                };
                finder.visit_block(body);
                if let Some(reason) = finder.found {
                    return Err(reason);
                }
            }
        }
        Ok(RecordPayloadsValidated(()))
    }
}

/// Module-qualified identity of a TIR function before link: the owning
/// `TirModule`'s key in `tir_modules` paired with the function name. Call
/// edges carry the same identity via `FunctionRef::module_source`, which the
/// elaborator resolves to the callee's defining module, so same-named
/// functions in different modules never conflate.
type FunctionKey = (ModuleSource, String);

/// Every function reachable from the active world's export bindings,
/// following free-function and method `Call` edges by `(module, name)`. The
/// roots are the synthesized export bindings in the entry module — one per
/// function the world actually exports (world exports for CLI/HTTP/`--lib`,
/// `test` functions for the test world), each calling its user function with
/// a module-qualified `FunctionRef` — so any function the world drops is
/// excluded.
fn reachable_from_export_bindings(project: &Package) -> IndexSet<FunctionKey> {
    let mut by_key: IndexMap<FunctionKey, Vec<Rc<RefCell<TirFunction>>>> = IndexMap::default();
    for (module_source, module) in &project.tir_modules {
        for func_rc in &module.functions {
            let key = (module_source.clone(), func_rc.borrow().name.clone());
            by_key.entry(key).or_default().push(func_rc.clone());
        }
    }

    let mut visited: IndexSet<FunctionKey> = IndexSet::default();
    let mut work: Vec<FunctionKey> = project
        .export_binding_names
        .values()
        .map(|binding_name| (project.entry_module_source.clone(), binding_name.clone()))
        .collect();
    while let Some(key) = work.pop() {
        if !visited.insert(key.clone()) {
            continue;
        }
        let Some(funcs) = by_key.get(&key) else {
            continue;
        };
        for func_rc in funcs {
            let func = func_rc.borrow();
            let Some(body) = &func.body else { continue };
            let mut collector = CalleeCollector {
                callees: Vec::new(),
            };
            collector.visit_block(body);
            for callee in collector.callees {
                if !visited.contains(&callee) {
                    work.push(callee);
                }
            }
        }
    }
    visited
}

struct CalleeCollector {
    callees: Vec<FunctionKey>,
}

impl TirRefVisitor for CalleeCollector {
    fn visit_stmt(&mut self, stmt: &crate::tir::TirStmt) {
        // This pass runs before `task return` is stripped; descend into its
        // value rather than tripping the default walker's guard.
        if let crate::tir::TirStmtKind::TaskReturn { value } = &stmt.kind {
            self.visit_expr(value);
        } else {
            self.walk_stmt(stmt);
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Call { func, .. } = &expr.kind {
            self.callees
                .push((func.module_source.clone(), func.name.clone()));
        }
        self.walk_expr(expr);
    }
}

struct NamedPayloadFinder<'a> {
    tt: &'a TypeTable,
    registry: &'a crate::component_model::CmInterfaceRegistry,
    /// Only where the world keeps the code: a record's resolvability depends on
    /// the world, unlike classifiability, which is always checked.
    check_records: bool,
    found: Option<String>,
}

impl TirRefVisitor for NamedPayloadFinder<'_> {
    fn visit_stmt(&mut self, stmt: &crate::tir::TirStmt) {
        // This pass runs before `task return` is stripped; descend into its
        // value rather than tripping the default walker's guard.
        if let crate::tir::TirStmtKind::TaskReturn { value } = &stmt.kind {
            self.visit_expr(value);
        } else {
            self.walk_stmt(stmt);
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        if self.found.is_none() {
            self.found = unresolvable_future_stream_payload(
                self.tt,
                self.registry,
                expr,
                self.check_records,
            );
        }
        self.walk_expr(expr);
    }
}

fn unresolvable_future_stream_payload(
    tt: &TypeTable,
    registry: &crate::component_model::CmInterfaceRegistry,
    expr: &TirExpr,
    check_records: bool,
) -> Option<String> {
    let (payload, is_future) = future_stream_payload_site(tt, expr)?;
    if check_records && let Some(name) = unresolvable_record_in_payload(tt, registry, payload) {
        return Some(format!(
            "record type `{name}` is used as a `future` / `stream` payload, \
             which is only supported in library (`--lib`) components"
        ));
    }
    if is_future {
        return crate::component_model::future_payload_rejection(tt, payload);
    }
    if crate::component_model::is_cm_record_stream_element(tt, payload) {
        return None;
    }
    crate::component_model::stream_payload_rejection(tt, payload)
}

/// Two shapes name a payload: a `new()` static call, and a CM method on a
/// handle. The bool is whether it is a future's.
fn future_stream_payload_site(tt: &TypeTable, expr: &TirExpr) -> Option<(TypeId, bool)> {
    let TirExprKind::Call { func, .. } = &expr.kind else {
        return None;
    };
    let cm = func
        .method_info
        .as_ref()
        .and_then(|m| m.cm_name.as_deref())?;
    if let Some(is_future) = match cm {
        "future-new" => Some(true),
        "stream-new" => Some(false),
        _ => None,
    } {
        let payload = func
            .monomorph_info
            .as_ref()?
            .impl_type_args
            .first()
            .copied()?;
        return Some((payload, is_future));
    }
    let is_future = if cm.starts_with("future-") {
        true
    } else if cm.starts_with("stream-") {
        false
    } else {
        return None;
    };
    let (receiver, _, _) = expr.kind.as_method_call()?;
    let mut type_id = receiver.type_id;
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(type_id) {
        type_id = *inner;
    }
    Some((*tt.generic_type_args(type_id)?.first()?, is_future))
}

/// Keyed on `module_source`, never the bare name: a homonym of an imported
/// WASI or dependency declaration lives under a different source and carries a
/// different shape.
fn unresolvable_record_in_payload(
    tt: &TypeTable,
    registry: &crate::component_model::CmInterfaceRegistry,
    type_id: TypeId,
) -> Option<String> {
    if let Some((name, module_source)) = named_decl_of(tt, tt.get(type_id))
        && matches!(
            crate::component_model::cm_payload_type_from_type_id(tt, type_id),
            Some(CmPayloadType::Named(_))
        )
        && !registry.is_named_type_registered_from(module_source, name)
    {
        return Some(name.to_string());
    }
    // Codegen peels aliases, so check through them here too.
    if let ResolvedType::Newtype { base_type, .. } = tt.get(type_id) {
        return unresolvable_record_in_payload(tt, registry, *base_type);
    }
    if let Some(inner) = tt.as_option(type_id).or_else(|| tt.as_list(type_id)) {
        return unresolvable_record_in_payload(tt, registry, inner);
    }
    if let Some(elems) = tt.as_tuple(type_id) {
        return elems
            .iter()
            .find_map(|&e| unresolvable_record_in_payload(tt, registry, e));
    }
    if let ResolvedType::GenericInstance { def, type_args } = tt.get(type_id)
        && tt.def_name(*def) == "Result"
    {
        return type_args
            .clone()
            .iter()
            .find_map(|&a| unresolvable_record_in_payload(tt, registry, a));
    }
    None
}

/// The declared name and module of a nominal type, or `None` for one that
/// names no declaration.
fn named_decl_of<'a>(tt: &'a TypeTable, ty: &ResolvedType) -> Option<(&'a str, &'a ModuleSource)> {
    let def = match ty {
        ResolvedType::Struct { def, .. } => def.decl()?,
        ResolvedType::Enum { def }
        | ResolvedType::Variant { def }
        | ResolvedType::Flags { def } => *def,
        _ => return None,
    };
    Some((tt.def_name(def), tt.def_module(def)))
}

/// Phase entry point: generate CM binding functions and rewrite call sites.
///
/// Ordered pipeline: import adapters, export adapters, the shared task-return
/// signature, test-world bindings, payload validation (producing the
/// `RecordPayloadsValidated` witness), task-return stripping, and finally
/// the async/resource primitive rewrites (consuming the witness).
///
/// Adapter functions flow through monomorphize → lower → optimize → codegen
/// like any other function.
pub fn generate_adapters(mut project: Package) -> Result<Package, String> {
    generate_import_adapters(&mut project);
    synthesize_export_adapters(&mut project)?;
    record_task_return_flat_params(&mut project);
    generate_test_world_bindings(&mut project);
    let validated = reject_unresolvable_record_payloads(&project)?;
    strip_unexpanded_task_returns(&project);
    rewrite_async_primitives(&mut project, validated);
    Ok(project)
}

/// The entry module's shared `TypeTable`. A missing entry module is an
/// invariant violation.
fn entry_type_table(project: &Package) -> Rc<RefCell<TypeTable>> {
    project
        .tir_modules
        .get(&project.entry_module_source)
        .expect("entry module should exist")
        .type_table
        .clone()
}

/// Synthesize a binding function for each used WASI effect call and resource
/// method call, add them to the entry module, and rewrite effect-like call
/// sites to target them.
fn generate_import_adapters(project: &mut Package) {
    let entry_source = project.entry_module_source.clone();

    let mut seen_effects: IndexSet<DeclPath> = IndexSet::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_effect_calls_in_block(
                    body,
                    &mut seen_effects,
                    &project.cm_interface_registry,
                    &module.type_table,
                );
            }
        }
    }
    if seen_effects.is_empty() {
        return;
    }

    let entry_type_table = entry_type_table(project);
    // Map effect/resource name → defining module source. Used to attach the
    // canonical owner as an effect on each generated binding so the
    // checker's `(module_source, name)` identity matches user-written
    // `with E` clauses (which the elaborator also canonicalises to the
    // defining module).
    let owner_sources = effect_owner_module_sources(&project.tir_modules);
    // Keyed by the qualified `interface::method` effect name — the same key
    // call sites are rewritten against.
    let mut adapters: IndexMap<DeclPath, Rc<RefCell<TirFunction>>> = IndexMap::default();
    // Auxiliary functions returned alongside an adapter (e.g. the
    // per-import `__cm_lift__*` for async imports). Not used for
    // call-site rewriting, but added to the entry module so they
    // participate in monomorphize / lower / DCE like normal functions.
    let mut auxiliary_functions: Vec<Rc<RefCell<TirFunction>>> = Vec::new();
    for qualified_name in &seen_effects {
        if let Some(func_info) = project.cm_interface_registry.get_function(qualified_name) {
            let func_info = func_info.clone();
            let owner_module = lookup_effect_owner(
                &owner_sources,
                &func_info.interface_name,
                &func_info.package,
            )
            // No declaring module: a placeholder owner in the function's own
            // namespace. A world-level import carries none, and falls back to
            // `Wasi` as it did when that was the only bundled namespace.
            .unwrap_or_else(|| {
                let namespace =
                    CmNamespace::from_prefix(&func_info.namespace).unwrap_or(CmNamespace::Wasi);
                project
                    .interner
                    .borrow_mut()
                    .binding(namespace, &func_info.package)
            });
            let produced = synthesize_adapter(
                &func_info,
                &project.cm_interface_registry,
                &entry_type_table,
                &project.interner,
                &owner_module,
                &entry_source,
            );
            // A world function (Phase 9) has no interface, so it needs no
            // capability effect. The shared synthesizer pushed its empty
            // interface name as one; drop it so the import stays pure.
            if project
                .cm_interface_registry
                .is_world_import_function(qualified_name)
            {
                produced.adapter.borrow_mut().effects.clear();
            }
            auxiliary_functions.extend(produced.auxiliary);
            adapters.insert(qualified_name.clone(), produced.adapter);
        }
    }

    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module should exist");
    for adapter_rc in adapters.values() {
        entry_module.functions.push(adapter_rc.clone());
    }
    for aux in auxiliary_functions {
        entry_module.functions.push(aux);
    }

    // Rewrite effect-like call nodes to target adapters. Call sites
    // are keyed by qualified `interface::method` name, exactly how
    // `adapters` is keyed. `applied_returns` spans all modules so call
    // sites that disagree on a shared adapter's return type are caught.
    let mut applied_returns: IndexMap<usize, TypeId> = IndexMap::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                rewrite_calls_in_block(
                    body,
                    &adapters,
                    &entry_source,
                    &project.cm_interface_registry,
                    &entry_type_table,
                    &mut applied_returns,
                );
            }
            // Sync locals with any Let stmts that were updated by the rewrite
            // (e.g., streaming binding calls changing the let binding type to i32).
            if !func.locals.is_empty() {
                let mut updates = Vec::new();
                if let Some(body) = &func.body {
                    collect_local_type_updates(body, &func.locals, &mut updates);
                }
                for (idx, type_id) in updates {
                    func.locals[idx].type_id = type_id;
                }
            }
        }
    }
}

/// Synthesize an export binding for each world export (signature-driven) and
/// record it in `export_binding_names`. Prefers the synthesized library world
/// (`--lib`) over the static registry.
fn synthesize_export_adapters(project: &mut Package) -> Result<(), String> {
    let Some(world_info) = project.active_world_info().cloned() else {
        return Ok(());
    };
    let entry_source = project.entry_module_source.clone();
    // Library world exports use a synchronous lift (the core function returns
    // the value directly), unlike the async/task-return WASI worlds.
    let is_lib_world = project.is_lib_world();
    // The kiln generator uses the lib path only for its typed params; its
    // `generate` returns `Result<_, _>` over nested records/lists that the
    // synchronous lower path cannot handle, so it routes through the async
    // task-return result binding instead (see `sync_wasi_export_strategy`).
    let is_kiln_generator = project
        .world_registry
        .is_generator_world(&project.target_world);
    let entry_type_table = entry_type_table(project);

    // Collect adapters in a read-only pass (synthesize_export_binding needs &tir_modules)
    let mut export_adapters: Vec<(String, String, Rc<RefCell<TirFunction>>)> = Vec::new();
    let mut post_returns: Vec<(String, String, Rc<RefCell<TirFunction>>)> = Vec::new();
    {
        let entry_module = project
            .tir_modules
            .get(&entry_source)
            .expect("entry module should exist");

        // Package hint for CM name resolution inside export adapters.
        // For `wasi:http/service` this is `"http"`; for
        // `core:kiln/generator` it is `"kiln"`. The hint biases bare-name
        // resolution towards the binding's owning package (e.g.
        // `ErrorCode` in `wasi:http` bindings) and feeds
        // `resolve_cm_source_for` as a fallback anchor. Derived from
        // the world's `fq_name` — the attribute-sourced identity is
        // the single source of truth.
        let binding_cm_package = world_info.package().to_string();

        for export in &world_info.exports {
            // A library spreads its `export fn`s across submodules; an
            // export defined outside the entry module carries its origin
            // module, and the adapter calls it there. The callee's module
            // is the `tir_modules` key (a function's own `module_source` is
            // not set until link, which runs after this synthesis).
            let callee_module = export
                .reexport_origin
                .as_ref()
                .map(|(m, _)| m.clone())
                .unwrap_or_else(|| entry_source.clone());
            let user_func_rc = find_export_user_func(&project.tir_modules, entry_module, export)?;
            {
                let user_func = user_func_rc.borrow();
                let tt = entry_type_table.borrow();
                validate_export_param_count(&user_func, export)?;
                validate_boundary_representable(
                    &user_func,
                    &export.name,
                    &tt,
                    &project.tir_modules,
                )?;
                validate_world_return_compatibility(&user_func, export, &tt)?;
            }

            let is_async_export = user_func_rc.borrow().is_async;
            let strategy = if is_async_export {
                // Async export: the user function calls task-return internally via
                // `task return expr` stmts. Expand those stmts into CM task-return
                // calls; the binding only lifts params and calls.
                if let Some(return_type) = &export.return_type {
                    let flat_types = {
                        let tt = entry_type_table.borrow();
                        compute_export_flat_return_types(return_type, &project.tir_modules, &tt)
                    };
                    // Per-export `task.return` import, so codegen can type
                    // the canon to this export's own result (a `--lib`
                    // world may have several async exports of distinct
                    // result types).
                    let task_return = CanonicalIntrinsic::TaskReturn(export.name.clone());
                    expand_task_returns_in_func(
                        &user_func_rc,
                        return_type,
                        &flat_types,
                        &task_return,
                        &project.tir_modules,
                        &entry_type_table,
                        &project.cm_interface_registry,
                        &binding_cm_package,
                        &project.interner,
                    );
                }
                ExportReturnStrategy::AsyncTaskReturn
            } else if is_lib_world && !is_kiln_generator {
                // Library exports: synchronous lift. The core function
                // returns the lowered value directly.
                ExportReturnStrategy::SyncReturn
            } else {
                sync_wasi_export_strategy(
                    &user_func_rc.borrow(),
                    export,
                    &entry_type_table.borrow(),
                )
            };

            let env = ExportBindingEnv {
                tir_modules: &project.tir_modules,
                type_table: &entry_type_table,
                world_params: &export.params,
                world_return: export.return_type.as_ref(),
                cm_interface_registry: &project.cm_interface_registry,
                cm_package: &binding_cm_package,
                interner: &project.interner,
            };
            let adapter = synthesize_export_binding(
                &export.name,
                &user_func_rc,
                &callee_module,
                &env,
                strategy,
            );
            export_adapters.push((
                export.name.clone(),
                export_binding_func_name(&export.name),
                adapter,
            ));

            // `post-return` is illegal alongside `async`, so only a synchronous
            // lift can reclaim its return area this way.
            if matches!(strategy, ExportReturnStrategy::SyncReturn)
                && let Some(post_return) = synthesize_post_return(&export.name, &env)
            {
                post_returns.push((
                    export.name.clone(),
                    post_return_func_name(&export.name),
                    post_return,
                ));
            }
        }
    }

    // Must follow the loop above: the name check walks signatures through the CM
    // type engine, which recurses without a depth guard, so a recursive type has
    // to be rejected by `validate_boundary_representable` first or it overflows
    // the stack instead of getting that diagnostic.
    if is_lib_world {
        validate_lib_interface_names(&world_info, &project.cm_interface_registry)?;
    }

    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module should exist");
    for (export_name, binding_name, adapter) in export_adapters {
        project
            .export_binding_names
            .insert(export_name, binding_name);
        entry_module.functions.push(adapter);
    }
    for (export_name, func_name, post_return) in post_returns {
        project
            .post_return_binding_names
            .insert(export_name, func_name);
        entry_module.functions.push(post_return);
    }
    Ok(())
}

/// Reject a library whose exports would claim one interface name twice.
///
/// A Component Model interface has one namespace covering its types *and* its
/// functions — "An interface has a single namespace which means that none of the
/// defined names can collide" — with case-insensitive uniqueness (`WIT.md`).
/// Wado keeps the two apart, so `variant Shape` beside `export fn shape` reads
/// as unambiguous until both kebab-case to `shape`. Left to codegen it surfaces
/// as a Wasm validation failure reported as a compiler bug, when it is the
/// source that has to change.
fn validate_lib_interface_names(
    world_info: &WorldInfo,
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
) -> Result<(), String> {
    let exported = exported_cm_type_names(world_info, cm_interface_registry);
    let wado_names = wado_names_by_cm_name(world_info);
    let describe = |cm_name: &str| match wado_names.get(cm_name).and_then(IndexSet::first) {
        Some(wado) => format!("type `{wado}` (exported as `{cm_name}`)"),
        None => format!("type `{cm_name}`"),
    };

    let mut claimed: IndexMap<String, String> = IndexMap::default();
    for cm_name in &exported {
        // Two Wado types kebab-casing onto one CM name never reach `exported`
        // twice: `CmTypeGen` caches by CM name, so the second silently reuses
        // the first one's type and the two merge. Catch it from the signatures,
        // where both names are still distinct.
        if let Some(wado) = wado_names.get(cm_name)
            && wado.len() > 1
        {
            let names: Vec<String> = wado.iter().map(|n| format!("`{n}`")).collect();
            return Err(format!(
                "types {} share the Component Model name `{cm_name}` in this \
                 library's interface, which can name each type only once. \
                 Rename all but one of them.",
                names.join(" and "),
            ));
        }
        let key = cm_name.to_ascii_lowercase();
        if let Some(previous) = claimed.get(&key) {
            return Err(format!(
                "{} and {} both claim the name `{cm_name}` in this library's \
                 Component Model interface, where types and functions share one \
                 namespace. Rename one of them.",
                describe(cm_name),
                previous,
            ));
        }
        claimed.insert(key, describe(cm_name));
    }
    for export in &world_info.exports {
        let cm_name = crate::name::kebab_export_name(&export.name);
        let key = cm_name.to_ascii_lowercase();
        if let Some(previous) = claimed.get(&key) {
            return Err(format!(
                "export `{}` becomes `{cm_name}` in this library's Component \
                 Model interface, where {} already claims that name — an \
                 interface has a single namespace covering both types and \
                 functions. Rename one of them.",
                export.name, previous,
            ));
        }
        claimed.insert(key, format!("function `{}`", export.name));
    }
    Ok(())
}

/// The CM type names the export signatures put into the interface, taken from
/// the same walk codegen uses to emit them.
fn exported_cm_type_names(
    world_info: &WorldInfo,
    cm_interface_registry: &crate::component_model::CmInterfaceRegistry,
) -> Vec<String> {
    let mut type_gen = match world_info
        .exports
        .first()
        .and_then(|e| e.from_interface_fq.as_deref())
    {
        Some(fq) => crate::component_model::CmTypeGen::with_interface_hint(fq),
        None => crate::component_model::CmTypeGen::new(),
    };
    let mut sink = crate::component_model::CmNameSink::default();
    let no_resources = IndexMap::default();
    for ty in export_signature_types(world_info) {
        let resolved = cm_interface_registry.resolve_type_preserving_local_newtypes(ty);
        type_gen.ast_type_to_cm(&mut sink, &resolved, cm_interface_registry, &no_resources);
    }
    sink.names().to_vec()
}

/// The Wado types behind each CM name the signatures mention. A user type's CM
/// name is `to_kebab` of its Wado name, applied by the registry when it records
/// the type, so this inverts that exactly.
fn wado_names_by_cm_name(world_info: &WorldInfo) -> IndexMap<String, IndexSet<String>> {
    let mut out = IndexMap::default();
    for ty in export_signature_types(world_info) {
        collect_named_types(ty, &mut out);
    }
    out
}

fn export_signature_types(world_info: &WorldInfo) -> impl Iterator<Item = &crate::ast::Type> {
    world_info.exports.iter().flat_map(|export| {
        export
            .params
            .iter()
            .map(|(_, ty)| ty)
            .chain(export.return_type.as_ref())
    })
}

fn collect_named_types(ty: &crate::ast::Type, out: &mut IndexMap<String, IndexSet<String>>) {
    use crate::ast::Type;
    match ty {
        Type::Named(named) => {
            out.entry(crate::name::to_kebab(&named.name))
                .or_default()
                .insert(named.name.clone());
        }
        Type::Generic(generic) => {
            for arg in &generic.args {
                collect_named_types(arg, out);
            }
        }
        Type::Tuple(elems) => {
            for elem in elems {
                collect_named_types(elem, out);
            }
        }
        Type::NamespacedGeneric(generic) => {
            for arg in &generic.args {
                collect_named_types(arg, out);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => collect_named_types(inner, out),
        // Not representable at the CM boundary; a signature carrying one is
        // rejected by `validate_boundary_representable` before this runs.
        Type::Function(_) | Type::TypePackSpread(_, _) | Type::Infer(_) | Type::Error(_) => {}
    }
}

/// The user function backing a world export: the origin `pub fn` for an
/// `export use` re-export, otherwise the `export fn` in the entry module.
///
/// A world export with no function at all is a missing entry point. The test
/// world handles `test` blocks separately and never reaches this lookup, so
/// in CLI / HTTP / other worlds the entry must be defined — never silently
/// stubbed.
fn find_export_user_func(
    tir_modules: &IndexMap<ModuleSource, TirModule>,
    entry_module: &TirModule,
    export: &WorldExportInfo,
) -> Result<Rc<RefCell<TirFunction>>, String> {
    if let Some((origin_module, origin_name)) = &export.reexport_origin {
        return tir_modules
            .get(origin_module)
            .and_then(|m| {
                m.functions
                    .iter()
                    .find(|f| f.borrow().name == *origin_name)
                    .cloned()
            })
            .ok_or_else(|| {
                format!(
                    "re-exported function `{}` (origin `{}`) is not defined",
                    export.name, origin_name
                )
            });
    }

    let mut found_exported = None;
    let mut found_without_export = false;
    for f in &entry_module.functions {
        let func = f.borrow();
        if func.name == export.name {
            if func.is_export {
                found_exported = Some(f.clone());
            } else {
                found_without_export = true;
            }
        }
    }
    match found_exported {
        Some(f) => Ok(f),
        None if found_without_export => Err(format!(
            "function `{}` exists but is not marked with `export` keyword. \
             Add `export` to make it a world entry point: `export fn {}(...)`",
            export.name, export.name
        )),
        None => Err(format!(
            "function `{}` is required as a world entry point but is not defined. \
             Define it with: `export fn {}(...)`",
            export.name, export.name
        )),
    }
}

/// Validate that the export function's parameter count matches the world
/// declaration.
fn validate_export_param_count(
    user_func: &TirFunction,
    export: &WorldExportInfo,
) -> Result<(), String> {
    if user_func.params.len() == export.params.len() {
        return Ok(());
    }
    Err(format!(
        "export function `{}` has {} parameter(s), \
         but the world expects {} parameter(s)",
        export.name,
        user_func.params.len(),
        export.params.len()
    ))
}

/// Reject any param/return type with no Component Model value representation
/// in any world (empty records, 128-bit/v128 scalars) with a proper compile
/// error rather than emitting an invalid component or panicking in codegen.
/// Handle/async types pass — they lower to i32 handles in every world — so
/// this needs no `--lib`-vs-WASI branch.
fn validate_boundary_representable(
    user_func: &TirFunction,
    export_name: &str,
    tt: &TypeTable,
    tir_modules: &IndexMap<ModuleSource, TirModule>,
) -> Result<(), String> {
    let mut to_check: Vec<TypeId> = user_func.params.iter().map(|p| p.type_id).collect();
    to_check.push(user_func.return_type);
    for tid in to_check {
        if let Err(reason) =
            types::check_cm_boundary_representable(tid, tt, tir_modules, &mut Vec::new())
        {
            return Err(format!("export function `{export_name}`: {reason}"));
        }
    }
    Ok(())
}

/// Validate return-type compatibility with the world. Strategy dispatch
/// routes an async export to the task-return adapters (async / Result / `()`
/// shapes) and a sync export to the synchronous lift. When an async world
/// expects only a discriminant (`Result<(), ()>`, the wasi:cli/command shape)
/// but the user supplies, say, `i32`, the async adapter would emit an extra
/// flat value beyond what the runtime declares for task-return — surfacing as
/// an opaque "values remaining on stack" wasm-validation panic at codegen.
/// Catch the mismatch here with a readable diagnostic instead.
fn validate_world_return_compatibility(
    user_func: &TirFunction,
    export: &WorldExportInfo,
    tt: &TypeTable,
) -> Result<(), String> {
    let result_name = tt.compiler_variant_name(CompilerItem::Result);
    let world_expects_result = matches!(
        &export.return_type,
        Some(crate::ast::Type::Generic(g)) if g.name == result_name
    );
    let user_is_unit = matches!(tt.get(user_func.return_type), ResolvedType::Unit);
    if !world_expects_result
        || user_func.is_async
        || user_is_unit
        || tt.is_result(user_func.return_type)
    {
        return Ok(());
    }
    let user_return_name = tt.type_name(user_func.return_type);
    Err(format!(
        "export function `{}` has return type `{user_return_name}`, \
         but the world expects a `{result_name}<_, _>` (or unit, \
         which is automatically wrapped as `{result_name}<(), _>`). \
         Change the signature to return a `{result_name}` or remove \
         the explicit return type.",
        export.name
    ))
}

/// Return strategy for a sync export in a task-return world, driven by the
/// user function's actual return type (signature-driven).
fn sync_wasi_export_strategy(
    user_func: &TirFunction,
    export: &WorldExportInfo,
    tt: &TypeTable,
) -> ExportReturnStrategy {
    if tt.is_result(user_func.return_type) {
        // Result<T, E> return: full lowering adapter
        return ExportReturnStrategy::ResultTaskReturn;
    }
    // The simple void adapter applies only with no params AND unit return.
    if export.params.is_empty() && matches!(tt.get(user_func.return_type), ResolvedType::Unit) {
        return ExportReturnStrategy::VoidTaskReturn;
    }
    // Sync export returning a plain value (not a `Result`), e.g. a `--lib`
    // export like `fn count() -> u32`. It uses the synchronous canon lift —
    // the core function returns the flattened result directly (an
    // out-pointer for multi-value results like a list). It must NOT go
    // through task-return: the component declares the export sync
    // (`.async_(false)`), so an async task-return lowering produces an
    // invalid core module.
    ExportReturnStrategy::SyncReturn
}

/// Record the flattened task-return params on the `Package` for `optimize_dce`
/// to type the shared `task_return` NIR import — the builtin takes one i32, but
/// a Result-returning export passes its full flattened result. Lib worlds are
/// skipped, bar the kiln generator. The import is one shared symbol, so a
/// disagreement between returning exports cannot be represented and is an ICE.
fn record_task_return_flat_params(project: &mut Package) {
    let Some(world_info) = project.active_world_info().cloned() else {
        return;
    };
    if project.is_lib_world()
        && !project
            .world_registry
            .is_generator_world(&project.target_world)
    {
        return;
    }
    let entry_type_table = entry_type_table(project);
    let tt = entry_type_table.borrow();
    let mut recorded: Option<(&str, Vec<CmValType>)> = None;
    for export in &world_info.exports {
        let Some(return_type) = &export.return_type else {
            continue;
        };
        let flat_types = compute_export_flat_return_types(return_type, &project.tir_modules, &tt);
        match &recorded {
            None => recorded = Some((&export.name, flat_types)),
            Some((first_name, first_flat)) => assert!(
                *first_flat == flat_types,
                "exports `{first_name}` and `{}` flatten to different task-return \
                 signatures ({first_flat:?} vs {flat_types:?}); the shared \
                 `task_return` import cannot represent both",
                export.name
            ),
        }
    }
    if let Some((_, flat_types)) = recorded {
        project.task_return_flat_params = Some(
            flat_types
                .iter()
                .map(|&vt| cm_val_type_to_type_id(vt))
                .collect(),
        );
    }
}

/// Synthesize export bindings for test functions (`__test_*`). Only when
/// targeting the test world — in other worlds, tests are dead code.
fn generate_test_world_bindings(project: &mut Package) {
    if !project.is_test_world() {
        return;
    }
    let entry_source = project.entry_module_source.clone();
    let test_name_filters = project.test_name_filters.clone();
    let entry_type_table = entry_type_table(project);

    // Test functions have is_export=false (they're not world exports),
    // but they need adapters for task-return when called via `wado test`.
    // Only selected tests get an adapter: an unselected test then has no
    // `is_cm_export` root, so early DCE drops its body — that is what makes
    // `--test-name` speed up compilation.
    let test_funcs: Vec<(String, Rc<RefCell<TirFunction>>)> = {
        let entry_module = project
            .tir_modules
            .get(&entry_source)
            .expect("entry module should exist");

        // Map each test's mangled function name → its original (lossless)
        // name so `--test-name` matches against what the user wrote, not
        // the ASCII-folded export name.
        let original_names: crate::hashmap::IndexMap<&str, Option<&str>> = entry_module
            .tests
            .iter()
            .map(|t| (t.function_name.as_str(), t.name.as_deref()))
            .collect();

        entry_module
            .functions
            .iter()
            .filter(|f| {
                let name = f.borrow().name.clone();
                name.starts_with("__test_")
                    && crate::package::test_selected(
                        original_names.get(name.as_str()).copied().flatten(),
                        &test_name_filters,
                    )
            })
            .map(|f| (f.borrow().name.clone(), f.clone()))
            .collect()
    };

    // The test world is a bare-name world, so its CM package is empty
    // (`fq_name_package`); test adapters take no params, so the package is
    // never consulted.
    let env = ExportBindingEnv {
        tir_modules: &project.tir_modules,
        type_table: &entry_type_table,
        world_params: &[],
        world_return: None,
        cm_interface_registry: &project.cm_interface_registry,
        cm_package: crate::world_registry::fq_name_package(crate::world_registry::TEST_WORLD),
        interner: &project.interner,
    };
    let adapters: Vec<(String, String, Rc<RefCell<TirFunction>>)> = test_funcs
        .into_iter()
        .map(|(test_name, user_func_rc)| {
            let binding_name = export_binding_func_name(&test_name);
            let adapter = synthesize_export_binding(
                &test_name,
                &user_func_rc,
                &entry_source,
                &env,
                ExportReturnStrategy::VoidTaskReturn,
            );
            (test_name, binding_name, adapter)
        })
        .collect();

    let entry_module = project
        .tir_modules
        .get_mut(&entry_source)
        .expect("entry module should exist");
    for (test_name, binding_name, adapter) in adapters {
        project.export_binding_names.insert(test_name, binding_name);
        entry_module.functions.push(adapter);
    }
}

/// Strip remaining `TaskReturn` stmts from all modules. `task return` is only
/// valid inside `async fn` (checked by elaborator); export synthesis expands
/// `TaskReturn` into CM calls for async exports that match the target world.
/// Any remaining async fn (unmatched exports, imported modules) will be DCE'd
/// — strip their `TaskReturn` stmts so they don't reach monomorphize. This is
/// idempotent: already-expanded functions have no `TaskReturn` stmts left.
fn strip_unexpanded_task_returns(project: &Package) {
    for module in project.tir_modules.values() {
        for f in &module.functions {
            let needs_strip = f.borrow().is_async;
            if needs_strip {
                strip_task_returns_in_func(f);
            }
        }
    }
}

/// Synthesize binding functions for the async CM primitives — the `Stream<T>` /
/// `Future<T>` read and write families — then rewrite `#[cm("...")]` resource
/// method calls onto them, which requires they exist first. Consumes the
/// [`RecordPayloadsValidated`] witness: the rewrites destroy the pristine
/// `future-new` / `stream-new` shape that validation scans.
fn rewrite_async_primitives(project: &mut Package, _validated: RecordPayloadsValidated) {
    synthesize_record_stream_reads(project);
    synthesize_future_reads(project);
    synthesize_future_writes(project);
    synthesize_stream_writes(project);
    synthesize_stream_reads(project);
    rewrite_cm_resource_methods(project);
}
