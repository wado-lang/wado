//! CM Binding Synthesis phase.
//!
//! Generates TIR binding functions for Component Model boundary crossing.
//! Each binding handles lifting Wado values to CM flat ABI (lowering params)
//! and lifting CM flat ABI values back to Wado types (lifting results).
//!
//! Pipeline position: after `effect_check`, before monomorphize.
//! This ensures binding functions go through monomorphization, lowering,
//! and optimization.
//!
//! See `docs/wep-2026-02-15-cm-binding-synthesis.md` for design details.

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

use crate::compiler_item::CompilerItem;
use crate::module_source::ModuleSource;
use crate::package::Package;
use crate::tir::{ResolvedType, TirExpr, TirExprKind, TirFunction, TirModule, TypeId, TypeTable};
use crate::tir_visitor::TirRefVisitor;
use crate::wir::CmPayloadType;
use crate::world_registry::WorldExportInfo;

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

/// Look up the canonical owning module for an effect/resource named `name`
/// whose binding targets WASI `package` (e.g. `"cli"`).
///
/// Preferred match: a `ModuleSource::Wasi { interface }` whose interface starts
/// with `"{package}/"` (e.g. `wasi:cli/stdio.wado` for package `"cli"`).
/// Falls back to any other owner with the same name if no WASI match exists.
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
        if let ModuleSource::Wasi { interface } = ms
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
    /// are not registered in the CM interface registry. Such a record has no CM
    /// type to lower against, so the lower would silently mis-treat it as an i32
    /// handle and emit an invalid component. Records are registered only for
    /// `--lib` components (under the package default interface); in any other
    /// world a user record has no CM home, so `get_struct_fields` returns `None`
    /// and the use is rejected.
    ///
    /// The scan is scoped to functions reachable from the active world's export
    /// bindings — the resolvability condition, not the world, decides — so a
    /// record future in code the world drops (e.g. the non-`test` exports of a
    /// library-shaped source like `cm_catalog.wado` compiled for the test world)
    /// is never reached and never flagged.
    pub(in crate::synthesis::cm_binding) fn reject_unresolvable_record_payloads(
        project: &Package,
    ) -> Result<RecordPayloadsValidated, String> {
        let reachable = super::reachable_from_export_bindings(project);
        for (module_source, module) in &project.tir_modules {
            let tt = module.type_table.borrow();
            for func_rc in &module.functions {
                let func = func_rc.borrow();
                if !reachable.contains(&(module_source.clone(), func.name.clone())) {
                    continue;
                }
                let Some(body) = &func.body else { continue };
                let mut finder = super::NamedPayloadFinder {
                    tt: &tt,
                    registry: project.cm_interface_registry.as_ref(),
                    found: None,
                };
                finder.visit_block(body);
                if let Some(name) = finder.found {
                    return Err(format!(
                        "record type `{name}` is used as a `future` / `stream` payload, \
                         which is only supported in library (`--lib`) components"
                    ));
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
        if let TirExprKind::Call { func, .. } | TirExprKind::MethodCall { func, .. } = &expr.kind {
            self.callees
                .push((func.module_source.clone(), func.name.clone()));
        }
        self.walk_expr(expr);
    }
}

struct NamedPayloadFinder<'a> {
    tt: &'a TypeTable,
    registry: &'a crate::component_model::CmInterfaceRegistry,
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
            self.found = unresolvable_future_stream_payload(self.tt, self.registry, expr);
        }
        self.walk_expr(expr);
    }
}

/// For a `Future::<T>::new()` / `Stream::<T>::new()` static call whose payload
/// `T` contains a named record with no CM type to lower against, return that
/// record's Wado name. `new` is the only way to obtain a `Future<T>` /
/// `Stream<T>` outside `--lib` (non-lib world exports have fixed signatures), so
/// checking it covers the creation sites.
fn unresolvable_future_stream_payload(
    tt: &TypeTable,
    registry: &crate::component_model::CmInterfaceRegistry,
    expr: &TirExpr,
) -> Option<String> {
    let TirExprKind::Call { func, .. } = &expr.kind else {
        return None;
    };
    let cm = func
        .method_info
        .as_ref()
        .and_then(|m| m.cm_name.as_deref())?;
    if cm != "future-new" && cm != "stream-new" {
        return None;
    }
    let payload = func
        .monomorph_info
        .as_ref()?
        .impl_type_args
        .first()
        .copied()?;
    unresolvable_record_in_payload(tt, registry, payload)
}

/// The Wado name of the first user record nested anywhere in a CM payload type
/// (`Future<Point>`, `Future<List<Point>>`, `Future<[Point, u32]>`, …) that is
/// not registered under its own module source — i.e. has no CM type to lower
/// against. `None` if every named record in the payload resolves.
///
/// Resolvability is keyed on the record's own `module_source`, never its bare
/// name: a user record that happens to share a name with an imported WASI/
/// dependency struct must still be rejected, since the homonym lives under a
/// different source and carries different fields.
fn unresolvable_record_in_payload(
    tt: &TypeTable,
    registry: &crate::component_model::CmInterfaceRegistry,
    type_id: TypeId,
) -> Option<String> {
    if let ResolvedType::Struct {
        name,
        module_source,
        ..
    } = tt.get(type_id)
        && matches!(
            crate::component_model::cm_payload_type_from_type_id(tt, type_id),
            Some(CmPayloadType::Named(_))
        )
        && !registry.is_struct_registered_from(module_source, name)
    {
        return Some(name.clone());
    }
    if let Some(inner) = tt.as_option(type_id).or_else(|| tt.as_list(type_id)) {
        return unresolvable_record_in_payload(tt, registry, inner);
    }
    if let Some(elems) = tt.as_tuple(type_id) {
        return elems
            .iter()
            .find_map(|&e| unresolvable_record_in_payload(tt, registry, e));
    }
    if let ResolvedType::GenericInstance {
        name, type_args, ..
    } = tt.get(type_id)
        && name == "Result"
    {
        return type_args
            .clone()
            .iter()
            .find_map(|&a| unresolvable_record_in_payload(tt, registry, a));
    }
    None
}

/// Phase entry point: generate CM binding functions and rewrite call sites.
///
/// Ordered pipeline: import adapters, export adapters, the shared task-return
/// signature, test-world bindings, payload validation (producing the
/// [`RecordPayloadsValidated`] witness), task-return stripping, and finally
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

    let mut seen_effects: IndexSet<String> = IndexSet::default();
    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_effect_calls_in_block(
                    body,
                    &mut seen_effects,
                    &project.cm_interface_registry,
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
    let mut adapters: IndexMap<String, Rc<RefCell<TirFunction>>> = IndexMap::default();
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
            .unwrap_or_else(|| project.interner.borrow_mut().wasi(&func_info.package));
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
                    let task_return_name = format!("task-return:{}", export.name);
                    expand_task_returns_in_func(
                        &user_func_rc,
                        &flat_types,
                        &task_return_name,
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

            // A synchronous lift returns its value through guest-allocated
            // memory; `post-return` is the ABI's only channel for handing that
            // memory back. Async lifts return through `task.return`, where the
            // option is not even permitted.
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

/// Record the flattened task-return params on the `Package` for
/// `optimize_dce` to type the shared `task_return` NIR import (the builtin
/// `task_return` takes a single i32, but a Result-returning export passes its
/// full flattened result). Sync-lift lib exports never call task.return, so
/// lib worlds are skipped — except the kiln generator, which routes through
/// the async task-return result binding.
///
/// The NIR import is a single shared symbol, so every returning export must
/// flatten to the same signature; a disagreement cannot be represented and is
/// an ICE.
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

/// Synthesize binding functions for the async CM primitives —
/// `Stream<T>.read()` on WASI record types, `Future<T>::read()`,
/// `FutureWritable<T>::write()`, scalar / structural
/// `StreamWritable<T>::write()` and `StreamReadable<T>::read()` — then
/// rewrite `#[cm("...")]` resource method calls to target internal/builtin
/// binding functions. The synthesis calls must all run before
/// `rewrite_cm_resource_methods` so the functions it rewrites call sites to
/// already exist. Rewriting here (instead of inline WIR emission in
/// `wir_build/translate.rs`) sends the bindings through the normal
/// pre-monomorphization pipeline.
///
/// Consumes the [`RecordPayloadsValidated`] witness: these rewrites destroy
/// the pristine `future-new`/`stream-new` call shape that
/// [`reject_unresolvable_record_payloads`] scans, so the validation must
/// already have run.
fn rewrite_async_primitives(project: &mut Package, _validated: RecordPayloadsValidated) {
    synthesize_record_stream_reads(project);
    synthesize_future_reads(project);
    synthesize_future_writes(project);
    synthesize_stream_writes(project);
    synthesize_stream_reads(project);
    rewrite_cm_resource_methods(project);
}

#[cfg(test)]
mod tests {
    use crate::module_source::ModuleSourceInterner;

    use super::export_adapter::synthesize_lift_from_flat_params;
    use super::types::{
        CmStdlibNames, LowerContext, compute_export_flat_param_types, export_needs_param_lifting,
        param_needs_lifting,
    };
    use super::*;
    use crate::ast::{NamedType, Type};
    use crate::cm_abi;
    use crate::component_model::CmInterfaceRegistry;
    use crate::synthesis::common::{
        builtin_call, cm_raw_call, i32_const, i64_const, internal_call, let_stmt, synth_span,
    };
    use crate::tir::{TirExpr, TirExprKind, TirStmtKind, TypeId};

    fn named_type(name: &str) -> Type {
        Type::Named(NamedType {
            id: crate::ast::AstId::fresh(),
            name: name.to_string(),
            span: synth_span(),
            source_interface: None,
        })
    }

    /// `LowerContext` over an empty registry / type table, sufficient for the
    /// primitive `synthesize_lower` paths (which only read `names`).
    fn lower_ctx_for_tests<'a>(
        registry: &'a CmInterfaceRegistry,
        type_table: &'a RefCell<TypeTable>,
    ) -> LowerContext<'a> {
        LowerContext {
            cm_interface_registry: registry,
            type_table,
            wasi_package: "test",
            names: CmStdlibNames::for_tests(),
        }
    }

    /// Register the `Option`, `Result`, `String`, and `List` compiler
    /// items against the relevant prelude modules so `make_option` /
    /// `make_result` and the type-identity reads inside `lift` /
    /// `cm_binding` succeed in unit tests. Production resolution wires
    /// these up when the stdlib elaborator visits `core:prelude`.
    fn register_option_result_for_tests(tt: &mut TypeTable) {
        use crate::compiler_item::{CompilerItem, Resolved};
        let _ = tt.compiler_items_mut().register(
            CompilerItem::Option,
            Resolved::Variant {
                module_source: ModuleSource::prelude(),
                name: "Option".to_string(),
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::OptionSome,
            Resolved::VariantCase {
                module_source: ModuleSource::prelude(),
                parent_type: "Option".to_string(),
                name: "Some".to_string(),
                case_index: 0,
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::OptionNone,
            Resolved::VariantCase {
                module_source: ModuleSource::prelude(),
                parent_type: "Option".to_string(),
                name: "None".to_string(),
                case_index: 1,
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::Result,
            Resolved::Variant {
                module_source: ModuleSource::prelude(),
                name: "Result".to_string(),
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::ResultOk,
            Resolved::VariantCase {
                module_source: ModuleSource::prelude(),
                parent_type: "Result".to_string(),
                name: "Ok".to_string(),
                case_index: 0,
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::ResultErr,
            Resolved::VariantCase {
                module_source: ModuleSource::prelude(),
                parent_type: "Result".to_string(),
                name: "Err".to_string(),
                case_index: 1,
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::String,
            Resolved::Struct {
                module_source: ModuleSource::string(),
                name: "String".to_string(),
            },
        );
        let _ = tt.compiler_items_mut().register(
            CompilerItem::List,
            Resolved::Struct {
                module_source: ModuleSource::list(),
                name: "List".to_string(),
            },
        );
    }

    /// Test fixture: empty registry + fresh type table + empty interner.
    /// Mirrors the production `LiftContext` shape so the lift code paths
    /// run end-to-end in unit tests.
    struct LiftCtxFixture {
        registry: CmInterfaceRegistry,
        type_table: std::cell::RefCell<TypeTable>,
        interner: std::cell::RefCell<ModuleSourceInterner>,
    }

    impl LiftCtxFixture {
        fn new() -> Self {
            // Register the Option/Result compiler-items so `make_option` /
            // `make_result` succeed during unit-test lifts. Production
            // gets these registered when the stdlib elaborator visits
            // `core:prelude`.
            let mut tt = TypeTable::new();
            register_option_result_for_tests(&mut tt);
            Self {
                registry: CmInterfaceRegistry::new(),
                type_table: std::cell::RefCell::new(tt),
                interner: std::cell::RefCell::new(ModuleSourceInterner::new()),
            }
        }

        fn ctx(&self) -> LiftContext<'_> {
            LiftContext {
                cm_interface_registry: &self.registry,
                type_table: &self.type_table,
                cm_package: "",
                interner: &self.interner,
            }
        }
    }

    #[test]
    fn flatten_param_i32() {
        let reg = CmInterfaceRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("i32"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_i64() {
        let reg = CmInterfaceRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("i64"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I64]
        );
    }

    #[test]
    fn flatten_param_f64() {
        let reg = CmInterfaceRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("f64"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::F64]
        );
    }

    #[test]
    fn flatten_param_string() {
        let reg = CmInterfaceRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("String"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I32, TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_bool() {
        let reg = CmInterfaceRegistry::new();
        assert_eq!(
            flatten_param_type(&named_type("bool"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I32]
        );
    }

    #[test]
    fn flatten_param_unit() {
        let reg = CmInterfaceRegistry::new();
        assert!(
            flatten_param_type(&Type::Tuple(vec![]), &reg, &CmStdlibNames::for_tests()).is_empty()
        );
    }

    #[test]
    fn flatten_param_newtype_u64() {
        let (reg, _) = CmInterfaceRegistry::build_from_stdlib();
        // A newtype reference reaching CM flattening carries its declaring
        // interface (as bootstrap and lib registration populate it).
        let wasi_newtype = |name: &str| {
            let source = reg
                .find_wasi_newtype_source(name)
                .expect("wasi newtype source");
            Type::Named(NamedType {
                id: crate::ast::AstId::fresh(),
                name: name.to_string(),
                span: synth_span(),
                source_interface: Some(source.to_string()),
            })
        };
        assert_eq!(
            flatten_param_type(&wasi_newtype("Duration"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I64]
        );
        assert_eq!(
            flatten_param_type(&wasi_newtype("Mark"), &reg, &CmStdlibNames::for_tests()),
            vec![TypeTable::I64]
        );
    }

    #[test]
    fn binding_name() {
        assert_eq!(
            binding_func_name("Stdout", "write_via_stream"),
            "__cm_binding__Stdout_write_via_stream"
        );
    }

    #[test]
    fn lift_i32() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("i32"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
        assert!(stmts.is_empty()); // primitives need no setup
    }

    #[test]
    fn lift_bool() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("bool"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    /// Signed `i8`/`i16` lift through a sign-extending load so a foreign CM
    /// `s8`/`s16` of `-1` (`0xFF`/`0xFFFF`) lifts to `-1`, not `255`/`65535`.
    /// Unsigned `u8`/`u16` keep the zero-extending load. Regression for the
    /// sign-extension bug in `synthesize_lift_inner`.
    #[test]
    fn lift_small_int_signedness() {
        fn lifted_builtin(name: &str) -> (String, TypeId) {
            let fix = LiftCtxFixture::new();
            let mut stmts = Vec::new();
            let mut locals = Vec::new();
            let expr = synthesize_lift(
                &named_type(name),
                i32_const(100),
                &mut 0,
                &mut stmts,
                &mut locals,
                &fix.ctx(),
            );
            match expr.kind {
                TirExprKind::Call { func, .. } => (func.name, expr.type_id),
                other => panic!("expected Call for {name}, got {other:?}"),
            }
        }

        assert_eq!(
            lifted_builtin("i8"),
            ("i32_load8_s".to_string(), TypeTable::I8)
        );
        assert_eq!(
            lifted_builtin("u8"),
            ("i32_load8_u".to_string(), TypeTable::U8)
        );
        assert_eq!(
            lifted_builtin("i16"),
            ("i32_load16_s".to_string(), TypeTable::I16)
        );
        assert_eq!(
            lifted_builtin("u16"),
            ("i32_load16_u".to_string(), TypeTable::U16)
        );
    }

    #[test]
    fn lift_string() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let expr = synthesize_lift(
            &named_type("String"),
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lift_list_i32() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let list_ty = cm_abi::generic_type("List", vec![named_type("i32")]);
        let expr = synthesize_lift(
            &list_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        // Should produce setup stmts and return a local ref
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert!(next_local >= 5); // base, count, result, i, elem_addr
    }

    #[test]
    fn lift_option_i32() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let opt_ty = cm_abi::generic_type("Option", vec![named_type("i32")]);
        let expr = synthesize_lift(
            &opt_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert!(next_local >= 2); // disc, result
    }

    #[test]
    fn lift_result_unit_unit() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let result_ty =
            cm_abi::generic_type("Result", vec![Type::Tuple(vec![]), Type::Tuple(vec![])]);
        let expr = synthesize_lift(
            &result_ty,
            i32_const(100),
            &mut next_local,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(!stmts.is_empty());
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
    }

    #[test]
    fn lift_resource_handle() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let own_ty = cm_abi::generic_type("Own", vec![named_type("Fields")]);
        let expr = synthesize_lift(
            &own_ty,
            i32_const(100),
            &mut 0,
            &mut stmts,
            &mut locals,
            &fix.ctx(),
        );
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    /// `List<IpAddress>` lifts must walk the buffer at the variant's
    /// canonical-ABI stride (1 byte disc + payload), not the 4-byte
    /// i32-handle fallback. Regression for issue #997 (#1, #2).
    #[test]
    fn lift_list_named_variant_uses_registry_stride() {
        use crate::tir::TirStmt;

        // Count `IntLiteral { value: target }` occurrences via the full
        // `TirRefVisitor` walk, so no recursion site the synthesis emits can
        // silently escape the count. Built as a semantic check so it doesn't
        // depend on any `Debug` output formatting.
        fn count_int_literals(stmts: &[TirStmt], target: u64) -> usize {
            struct IntLiteralCounter {
                target: u64,
                count: usize,
            }
            impl TirRefVisitor for IntLiteralCounter {
                fn visit_expr(&mut self, expr: &TirExpr) {
                    if let TirExprKind::IntLiteral { value, .. } = &expr.kind
                        && *value == self.target
                    {
                        self.count += 1;
                    }
                    self.walk_expr(expr);
                }
            }
            let mut counter = IntLiteralCounter { target, count: 0 };
            for stmt in stmts {
                counter.visit_stmt(stmt);
            }
            counter.count
        }

        let (registry, _) = CmInterfaceRegistry::build_from_stdlib();
        let elem_ty = named_type("IpAddress");
        let expected_size = u64::from(crate::component_model::cm_size_with_registry_scoped(
            &elem_ty,
            &registry,
            Some("sockets"),
        ));
        // Sanity: registry-derived size differs from the 4-byte fallback.
        assert!(expected_size > 4, "registry size should exceed handle size");

        let mut tt = TypeTable::new();
        register_option_result_for_tests(&mut tt);
        let type_table = std::cell::RefCell::new(tt);
        let interner = std::cell::RefCell::new(ModuleSourceInterner::new());
        let ctx = LiftContext {
            cm_interface_registry: &registry,
            type_table: &type_table,
            cm_package: "sockets",
            interner: &interner,
        };
        // Register the variant TypeId the way the elaborator does in
        // production; an unregistered CM type is a loud error, not an
        // i32 fallback.
        {
            let Type::Named(elem_named) = &elem_ty else {
                unreachable!("elem_ty is a named type")
            };
            let source = registry
                .resolve_cm_source_for(elem_named, Some("sockets"))
                .expect("IpAddress resolves in the stdlib registry");
            let module_source = ctx.module_source_for(source);
            type_table
                .borrow_mut()
                .make_variant("IpAddress".to_string(), module_source);
        }

        let list_ty = cm_abi::generic_type("List", vec![elem_ty]);
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let _ = synthesize_lift(
            &list_ty,
            i32_const(0),
            &mut next_local,
            &mut stmts,
            &mut locals,
            &ctx,
        );
        // Stride appears at element-addr offset (`i * elem_size`) and in
        // the realloc free (`count * elem_size`).
        let occurrences = count_int_literals(&stmts, expected_size);
        assert!(
            occurrences >= 2,
            "expected ≥2 `IntLiteral {{ value: {expected_size} }}` occurrences, got {occurrences}"
        );
    }

    #[test]
    fn lower_i32() {
        let registry = CmInterfaceRegistry::new();
        let type_table = RefCell::new(TypeTable::new());
        let ctx = lower_ctx_for_tests(&registry, &type_table);
        let stmts = synthesize_lower(
            &named_type("i32"),
            i32_const(42),
            i32_const(100),
            &mut 0,
            &mut vec![],
            &ctx,
        );
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_bool() {
        let value = TirExpr::new(
            TirExprKind::BoolLiteral(true),
            TypeTable::BOOL,
            synth_span(),
        );
        let registry = CmInterfaceRegistry::new();
        let type_table = RefCell::new(TypeTable::new());
        let ctx = lower_ctx_for_tests(&registry, &type_table);
        let stmts = synthesize_lower(
            &named_type("bool"),
            value,
            i32_const(100),
            &mut 0,
            &mut vec![],
            &ctx,
        );
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn lower_unit() {
        let value = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, synth_span());
        let registry = CmInterfaceRegistry::new();
        let type_table = RefCell::new(TypeTable::new());
        let ctx = lower_ctx_for_tests(&registry, &type_table);
        let stmts = synthesize_lower(
            &Type::Tuple(vec![]),
            value,
            i32_const(100),
            &mut 0,
            &mut vec![],
            &ctx,
        );
        assert!(stmts.is_empty());
    }

    #[test]
    fn lower_string() {
        let value = TirExpr::new(
            TirExprKind::StringLiteral("hello".to_string()),
            TypeTable::I32, // placeholder
            synth_span(),
        );
        let mut next_local = 10_u32;
        let registry = CmInterfaceRegistry::new();
        let type_table = RefCell::new(TypeTable::new());
        let ctx = lower_ctx_for_tests(&registry, &type_table);
        let stmts = synthesize_lower(
            &named_type("String"),
            value,
            i32_const(100),
            &mut next_local,
            &mut vec![],
            &ctx,
        );
        // Should produce: let __packed = cm_lower_string(value); store ptr; store len
        assert_eq!(stmts.len(), 3);
        assert_eq!(next_local, 11); // one local allocated for __packed
    }

    #[test]
    fn helpers_i32_const() {
        let expr = i32_const(42);
        assert_eq!(expr.type_id, TypeTable::I32);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 42),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_i64_const() {
        let expr = i64_const(123);
        assert_eq!(expr.type_id, TypeTable::I64);
        match &expr.kind {
            TirExprKind::IntLiteral { value, .. } => assert_eq!(*value, 123),
            other => panic!("expected IntLiteral, got {other:?}"),
        }
    }

    #[test]
    fn helpers_builtin_call() {
        let call = builtin_call("i32_load", vec![i32_const(0)], TypeTable::I32);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name.clone(), "i32_load");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_internal_call() {
        let call = internal_call("cm_lower_string", vec![i32_const(0)], TypeTable::I64);
        match &call.kind {
            TirExprKind::Call { func, args, .. } => {
                assert_eq!(func.name.clone(), "cm_lower_string");
                assert_eq!(args.len(), 1);
            }
            other => panic!("expected Call, got {other:?}"),
        }
    }

    #[test]
    fn helpers_cm_raw_call() {
        let call = cm_raw_call(
            "wasi:cli/Stdout::write_via_stream",
            vec![i32_const(0), i32_const(1), i32_const(2)],
            TypeTable::I32,
        );
        match &call.kind {
            TirExprKind::CmRawCall { local_name, args } => {
                assert_eq!(local_name, "wasi:cli/Stdout::write_via_stream");
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected CmRawCall, got {other:?}"),
        }
    }

    #[test]
    fn helpers_let_stmt() {
        let stmt = let_stmt("x", 0, TypeTable::I32, i32_const(42));
        match &stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                type_id,
                ..
            } => {
                assert_eq!(name, "x");
                assert_eq!(*local_index, 0);
                assert_eq!(*type_id, TypeTable::I32);
            }
            other => panic!("expected Let, got {other:?}"),
        }
    }

    // ---- Parameter lifting tests ----
    //
    // These exercise the TypeTable-driven classification in
    // `param_needs_lifting`. Each test constructs the minimum TIR shape
    // needed to reach a specific `ResolvedType` arm.

    fn mk_param(type_id: TypeId) -> crate::tir::TirParam {
        crate::tir::TirParam {
            name: String::new(),
            type_id,
            local_index: 0,
            is_mut: false,
            is_mut_ref: false,
            span: crate::token::Span::new(0, 0, 0, 0),
        }
    }

    #[test]
    fn param_needs_lifting_primitives_passthrough() {
        let tt = TypeTable::new();
        // All Wasm-native primitive shapes flow through unchanged.
        assert!(!param_needs_lifting(TypeTable::I32, &tt));
        assert!(!param_needs_lifting(TypeTable::I64, &tt));
        assert!(!param_needs_lifting(TypeTable::F32, &tt));
        assert!(!param_needs_lifting(TypeTable::F64, &tt));
        assert!(!param_needs_lifting(TypeTable::U8, &tt));
        assert!(!param_needs_lifting(TypeTable::U16, &tt));
        assert!(!param_needs_lifting(TypeTable::CHAR, &tt));
    }

    #[test]
    fn param_needs_lifting_bool_lifts() {
        // `bool` needs a 0/!=0 widening at the CM boundary, so it
        // counts as needing a lift step.
        let tt = TypeTable::new();
        assert!(param_needs_lifting(TypeTable::BOOL, &tt));
    }

    #[test]
    fn param_needs_lifting_unit_lifts() {
        let tt = TypeTable::new();
        assert!(param_needs_lifting(TypeTable::UNIT, &tt));
    }

    #[test]
    fn param_needs_lifting_string() {
        let mut tt = TypeTable::new();
        let s = tt.make_struct("String".to_string(), ModuleSource::string());
        assert!(param_needs_lifting(s, &tt));
    }

    #[test]
    fn param_needs_lifting_resource() {
        // Resources are i32 handles — no lift.
        let mut tt = TypeTable::new();
        let r = tt.intern(crate::tir::ResolvedType::Resource {
            name: "Request".to_string(),
            module_source: ModuleSource::wasi_http(),
        });
        assert!(!param_needs_lifting(r, &tt));
    }

    #[test]
    fn param_needs_lifting_enum() {
        let mut tt = TypeTable::new();
        let mut interner = ModuleSourceInterner::new();
        let e = tt.intern(crate::tir::ResolvedType::Enum {
            name: "Color".to_string(),
            module_source: interner.entry_point("<test>"),
        });
        assert!(!param_needs_lifting(e, &tt));
    }

    #[test]
    fn param_needs_lifting_option() {
        // Option<T> is a GenericInstance under the hood; build it directly
        // (avoids `make_option`'s dependency on comp-feature registration,
        // which isn't present in a bare `TypeTable::new()`).
        let mut tt = TypeTable::new();
        let opt = tt.intern(crate::tir::ResolvedType::GenericInstance {
            name: "Option".to_string(),
            module_source: ModuleSource::types(),
            type_args: vec![TypeTable::I32],
        });
        assert!(param_needs_lifting(opt, &tt));
    }

    #[test]
    fn param_needs_lifting_array() {
        let mut tt = TypeTable::new();
        let arr = tt.intern(crate::tir::ResolvedType::GenericInstance {
            name: "List".to_string(),
            module_source: ModuleSource::prelude(),
            type_args: vec![TypeTable::I32],
        });
        assert!(param_needs_lifting(arr, &tt));
    }

    #[test]
    fn export_needs_lifting_empty() {
        let tt = std::cell::RefCell::new(TypeTable::new());
        assert!(!export_needs_param_lifting(&[], &tt));
    }

    #[test]
    fn export_needs_lifting_primitives_only() {
        let tt = std::cell::RefCell::new(TypeTable::new());
        let params = vec![mk_param(TypeTable::I32), mk_param(TypeTable::F64)];
        assert!(!export_needs_param_lifting(&params, &tt));
    }

    #[test]
    fn export_needs_lifting_with_string() {
        let tt_cell = std::cell::RefCell::new(TypeTable::new());
        let string_id = tt_cell
            .borrow_mut()
            .make_struct("String".to_string(), ModuleSource::string());
        let params = vec![mk_param(string_id)];
        assert!(export_needs_param_lifting(&params, &tt_cell));
    }

    #[test]
    fn compute_flat_params_empty() {
        let params: Vec<(String, Type)> = vec![];
        let mut type_table = TypeTable::new();
        register_option_result_for_tests(&mut type_table);
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert!(flat.is_empty());
    }

    #[test]
    fn compute_flat_params_primitives() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("b".to_string(), named_type("f64")),
        ];
        let mut type_table = TypeTable::new();
        register_option_result_for_tests(&mut type_table);
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(flat, vec![cm_abi::CmValType::I32, cm_abi::CmValType::F64]);
    }

    #[test]
    fn compute_flat_params_string() {
        let params = vec![("name".to_string(), named_type("String"))];
        let mut type_table = TypeTable::new();
        register_option_result_for_tests(&mut type_table);
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(flat, vec![cm_abi::CmValType::I32, cm_abi::CmValType::I32]);
    }

    #[test]
    fn compute_flat_params_mixed() {
        let params = vec![
            ("a".to_string(), named_type("i32")),
            ("name".to_string(), named_type("String")),
            ("b".to_string(), named_type("f32")),
        ];
        let mut type_table = TypeTable::new();
        register_option_result_for_tests(&mut type_table);
        let tir_modules = IndexMap::default();
        let flat = compute_export_flat_param_types(&params, &tir_modules, &type_table);
        assert_eq!(
            flat,
            vec![
                cm_abi::CmValType::I32,
                cm_abi::CmValType::I32,
                cm_abi::CmValType::I32,
                cm_abi::CmValType::F32,
            ]
        );
    }

    // ---- Lift from flat params tests ----

    #[test]
    fn lift_from_flat_i32() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("i32"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::I32,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            fix.ctx(),
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }

    #[test]
    fn lift_from_flat_string() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 2_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("String"),
            &[0, 1],
            &[cm_abi::CmValType::I32, cm_abi::CmValType::I32],
            TypeTable::I32, // placeholder
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            fix.ctx(),
        );
        assert_eq!(consumed, 2);
        // Should be a call to memory_to_gc_string
        assert!(matches!(expr.kind, TirExprKind::Call { .. }));
    }

    #[test]
    fn lift_from_flat_bool() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("bool"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::BOOL,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            fix.ctx(),
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Binary { .. }));
        assert_eq!(expr.type_id, TypeTable::BOOL);
    }

    #[test]
    fn lift_from_flat_unit() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 0_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &Type::Tuple(vec![]),
            &[],
            &[],
            TypeTable::UNIT,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            fix.ctx(),
        );
        assert_eq!(consumed, 0);
        assert!(matches!(expr.kind, TirExprKind::Unit));
    }

    #[test]
    fn lift_from_flat_resource() {
        let fix = LiftCtxFixture::new();
        let mut stmts = Vec::new();
        let mut locals = Vec::new();
        let mut next_local = 1_u32;
        let tir_modules = IndexMap::default();
        let (expr, consumed) = synthesize_lift_from_flat_params(
            &named_type("Request"),
            &[0],
            &[cm_abi::CmValType::I32],
            TypeTable::I32,
            &mut next_local,
            &mut stmts,
            &mut locals,
            &tir_modules,
            fix.ctx(),
        );
        assert_eq!(consumed, 1);
        assert!(matches!(expr.kind, TirExprKind::Local { .. }));
        assert_eq!(expr.type_id, TypeTable::I32);
    }
}
