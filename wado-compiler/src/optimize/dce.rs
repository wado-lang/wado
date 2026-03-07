//! Dead Code Elimination (DCE) for Wado TIR
//!
//! This module provides dead code elimination at two levels:
//!
//! 1. **Function-level DCE**: Reachability analysis starting from the entry point,
//!    removing functions that are never called.
//!
//! 2. **Constant branch pruning**: When an `if` condition is a compile-time boolean
//!    literal, the dead branch is eliminated and the taken branch is inlined in place.

use indexmap::IndexSet;

use crate::name::{
    FreeFunctionName, FunctionId, MethodName, ModuleSource, mangle_generic_name,
    mangle_local_method, mangle_local_trait_method, mangle_method_generic,
};
use crate::project::Project;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirImport, TirModule, TirStmt,
    TirStmtKind, TypeId, TypeTable,
};
use indexmap::IndexMap;

/// Call graph: function ID -> set of called function IDs
type CallGraph = IndexMap<FunctionId, IndexSet<FunctionId>>;

/// Effect usage: function ID -> set of (`effect_name`, `operation_name`) pairs
type EffectUsageMap = IndexMap<FunctionId, IndexSet<(String, String)>>;

/// Canonical method usage: function ID -> set of canonical builtin names
type CanonicalMethodUsageMap = IndexMap<FunctionId, IndexSet<String>>;

/// Analysis results for a single function
#[derive(Debug, Clone, Default)]
struct FunctionAnalysis {
    /// Functions called by this function
    callees: IndexSet<FunctionId>,
    /// Effect calls: (`effect_name`, `op_name`)
    effect_calls: IndexSet<(String, String)>,
    /// Canonical resource method names from `#[canonical("...")]` attributes
    /// e.g., `stream_drop_readable`, `stream_write`
    canonical_methods: IndexSet<String>,
}

/// Analyze the project and populate its usage fields with DCE analysis results.
///
/// This performs dead code elimination analysis starting from the entry point
/// and populates the project's `reachable_functions`, `used_wasi_functions`
/// fields, and the entry module's `imports` list.
pub fn analyze_project(project: &mut Project) {
    // Build call graph and effect usage from all modules
    let (call_graph, effect_usage, canonical_method_usage) =
        build_analysis_graph(&project.tir_modules);

    // Determine entry functions from world exports.
    // For the test world, test functions are the sole entry points; world exports
    // (like `run`) are only reachable if tests transitively call them.
    let mut entry_func_names: Vec<String> = if project.is_test_world() {
        vec![]
    } else {
        project
            .world_registry
            .get(&project.target_world)
            .map(|w| w.exports.iter().map(|e| e.name.clone()).collect())
            .unwrap_or_else(|| vec!["run".to_string()])
    };

    // Include export adapter functions as additional entry points
    // (they wrap the user's export functions with CM boundary logic)
    for adapter_name in project.export_adapter_names.values() {
        entry_func_names.push(adapter_name.clone());
    }

    // Compute reachable functions from all entry points
    let mut reachable = IndexSet::new();
    for entry_name in &entry_func_names {
        let entry_func = FunctionId::Free(FreeFunctionName::from_module_source(
            &project.entry_module_source,
            entry_name,
        ));
        let entry_reachable = compute_reachable(&call_graph, &entry_func);
        reachable.extend(entry_reachable);
    }

    // Add test functions as entry points only when targeting the test world.
    // For non-test worlds (command, service, …), tests are dead code.
    if project.is_test_world()
        && let Some(entry_module) = project.tir_modules.get(&project.entry_module_source)
    {
        for test in &entry_module.tests {
            let test_func = FunctionId::Free(FreeFunctionName::from_module_source(
                &project.entry_module_source,
                &test.function_name,
            ));
            let test_reachable = compute_reachable(&call_graph, &test_func);
            reachable.extend(test_reachable);
        }
    }

    // Mark exported functions from #![wasm_module] sources as reachable.
    // These are compiled into separate wasm modules and must not be eliminated.
    for (module_source, tir_mod) in &project.tir_modules {
        if tir_mod.wasm_module.is_some() {
            for func_rc in &tir_mod.functions {
                let func = func_rc.borrow();
                if func.is_export {
                    let func_id = FunctionId::Free(FreeFunctionName::from_module_source(
                        module_source,
                        &func.name,
                    ));
                    let wasm_mod_reachable = compute_reachable(&call_graph, &func_id);
                    reachable.extend(wasm_mod_reachable);
                }
            }
        }
    }

    // Collect used WASI functions from reachable functions
    let mut used_wasi_functions: IndexSet<String> = IndexSet::new();
    for func_id in &reachable {
        if let Some(effects) = effect_usage.get(func_id) {
            for (effect_name, op_name) in effects {
                used_wasi_functions.insert(format!("{effect_name}::{op_name}"));
            }
        }
    }

    // Collect canonical resource method usage from reachable functions.
    // Used only for ensuring cm_lower_array_u8 is reachable when stream-write is used.
    let used_canonical_methods: IndexSet<String> = reachable
        .iter()
        .filter_map(|f| canonical_method_usage.get(f))
        .flatten()
        .cloned()
        .collect();

    // Helper to check if a core/internal function is reachable
    let core_internal = |name: &str| -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["core", "internal"], name))
    };

    // CM helper functions (cm_lower_string, memory_to_gc_string, etc.)
    // are called from synthesized CM adapter functions, which are part of the TIR
    // and discovered through normal call graph analysis.

    // HTTP handler exports need cm_lower_string for ErrorCode payload lowering
    // (ErrorCode variant cases can contain Option<String> payloads).
    // This is used in the component export path, not via TIR call graph.
    let has_http_handler_export_early = project
        .world_registry
        .get(&project.target_world)
        .is_some_and(crate::world_registry::WorldInfo::has_http_handler_export);
    if has_http_handler_export_early {
        let func = core_internal("cm_lower_string");
        reachable.extend(compute_reachable(&call_graph, &func));
    }

    // Check if stream intrinsics are needed by looking for:
    // 1. Stdout/Stderr effects being used
    // 2. Any builtin stream_* functions being called (for ambient logging)
    // 3. Any builtin call_indirect_* functions (ambient effect calls)
    let is_builtin_func = |f: &FreeFunctionName| {
        // Check if module_source is core/builtin
        f.module_source.is_core_builtin()
            // Legacy format: name starts with "builtin::"
            || f.name.starts_with("builtin::")
    };
    let is_builtin_call_indirect_stdout = |f: &FreeFunctionName| {
        if is_builtin_func(f) {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            name.starts_with("call_indirect_stdout")
        } else {
            false
        }
    };
    let is_builtin_call_indirect_stderr = |f: &FreeFunctionName| {
        if is_builtin_func(f) {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            name.starts_with("call_indirect_stderr")
        } else {
            false
        }
    };

    // Also mark WASI functions as used if indirect calls are present (for ambient logging)
    if reachable
        .iter()
        .any(|func_id| matches!(func_id, FunctionId::Free(f) if is_builtin_call_indirect_stdout(f)))
    {
        used_wasi_functions.insert("Stdout::write_via_stream".to_string());
    }
    if reachable
        .iter()
        .any(|func_id| matches!(func_id, FunctionId::Free(f) if is_builtin_call_indirect_stderr(f)))
    {
        used_wasi_functions.insert("Stderr::write_via_stream".to_string());
    }

    // Collect imports using registry lookup instead of hard-coded match
    let mut imports: IndexSet<TirImport> = IndexSet::new();

    // Helper to add an import from builtin registry by function name
    let add_import_by_name = |imports: &mut IndexSet<TirImport>, name: &str| {
        if let Some(info) = project.builtin_registry.get(name)
            && let Some(canonical_name) = &info.canonical_name
        {
            imports.insert(TirImport {
                namespace: info.namespace.clone(),
                canonical_name: canonical_name.clone(),
                func_name: name.to_string(),
                params: info.params.iter().map(|(_, ty)| *ty).collect(),
                return_type: info.return_type,
            });
        }
    };

    // Map reachable builtin function calls to imports via registry lookup
    for func_id in &reachable {
        if let FunctionId::Free(f) = func_id
            && is_builtin_func(f)
        {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            add_import_by_name(&mut imports, name);
        }
    }

    // realloc is always needed for memory management
    add_import_by_name(&mut imports, "realloc");

    // When stream-write canonical is used via resource methods, cm_lower_array_u8 must be
    // reachable (called at WIR level by emit_stream_write, not visible in the TIR call graph).
    if used_canonical_methods.contains("stream-write") {
        reachable.extend(compute_reachable(
            &call_graph,
            &core_internal("cm_lower_array_u8"),
        ));
    }

    // Async exports require task-return and potentially other canonical intrinsics.
    // The test world always has async exports (each test is an async component export).
    let has_async_export = project.is_test_world()
        || project
            .world_registry
            .get(&project.target_world)
            .is_some_and(crate::world_registry::WorldInfo::has_async_export);
    let has_http_handler_export = project
        .world_registry
        .get(&project.target_world)
        .is_some_and(crate::world_registry::WorldInfo::has_http_handler_export);
    if has_async_export {
        // TaskReturn is always needed for async exports.
        // For Result-returning exports (e.g., HTTP handler), synthesis::cm_adapter computes
        // the correct flattened CM ABI params. Override the builtin registry's default
        // single-i32 signature with the correct flat params.
        if let Some(flat_params) = project.task_return_flat_params.clone() {
            if let Some(info) = project.builtin_registry.get("task_return")
                && let Some(canonical_name) = &info.canonical_name
            {
                imports.insert(TirImport {
                    namespace: info.namespace.clone(),
                    canonical_name: canonical_name.clone(),
                    func_name: "task_return".to_string(),
                    params: flat_params,
                    return_type: info.return_type,
                });
            }
        } else {
            add_import_by_name(&mut imports, "task_return");
        }

        // Waitable-set builtins (waitable_set_new, waitable_join, waitable_set_wait, subtask_drop)
        // are added automatically via reachability from internal::wait_for_subtask

        // HTTP handler exports need future intrinsics for response creation
        // (trailers parameter to response.new is a future)
        if has_http_handler_export {
            add_import_by_name(&mut imports, "future_new");
            add_import_by_name(&mut imports, "future_write");
            add_import_by_name(&mut imports, "future_drop_writable");
        }
    }

    // Store imports in the entry module
    if let Some(entry_module) = project.tir_modules.get_mut(&project.entry_module_source) {
        entry_module.imports = imports.into_iter().collect();
        // Sort imports for deterministic output
        entry_module
            .imports
            .sort_by(|a, b| a.canonical_name.cmp(&b.canonical_name));
    }

    // Apply results to project
    project.reachable_functions = reachable.clone();
    project.used_wasi_functions = used_wasi_functions;

    // Filter string literals in each module to only include strings from reachable functions
    for module in project.tir_modules.values_mut() {
        let module_source = &module.module_source;
        let mut reachable_strings: IndexSet<String> = IndexSet::new();

        for (func_name, strings) in &module.function_strings {
            // Build function ID(s) to check if it's reachable
            // Note: monomorphized methods are tracked as FunctionId::Free in the call graph
            // but their names look like methods (e.g., "TreeMap<String,i32>^Index::index")
            let is_reachable =
                if let Some(Some(method_info)) = module.function_method_info.get(func_name) {
                    // Method with method_info
                    // Check as MethodName first (for non-monomorphized methods)
                    let method_id = FunctionId::Method(MethodName::new(
                        module_source.clone(),
                        method_info.struct_name.clone(),
                        method_info.trait_name.clone(),
                        method_info.method_name.clone(),
                    ));
                    if reachable.contains(&method_id) {
                        true
                    } else {
                        // For monomorphized methods, also check as FreeFunctionName
                        // Monomorphized methods have type args in the struct name (e.g., "TreeMap<String,i32>")
                        let free_id = FunctionId::Free(FreeFunctionName::from_module_source(
                            module_source,
                            func_name,
                        ));
                        reachable.contains(&free_id)
                    }
                } else {
                    // Regular function (no method_info)
                    let func_id = FunctionId::Free(FreeFunctionName::from_module_source(
                        module_source,
                        func_name,
                    ));
                    reachable.contains(&func_id)
                };

            if is_reachable {
                reachable_strings.extend(strings.iter().cloned());
            }
        }

        module.string_literals = reachable_strings.into_iter().collect();
    }
}

/// Build call graph and effect usage from all TIR modules
/// Returns (`call_graph`, `effect_usage`, `canonical_method_usage`)
fn build_analysis_graph(
    modules: &IndexMap<ModuleSource, TirModule>,
) -> (CallGraph, EffectUsageMap, CanonicalMethodUsageMap) {
    let mut call_graph: CallGraph = IndexMap::new();
    let mut effect_usage: EffectUsageMap = IndexMap::new();
    let mut canonical_method_usage: CanonicalMethodUsageMap = IndexMap::new();

    for (module_source, module) in modules {
        let type_table = &*module.type_table.borrow();

        // Analyze functions (including methods stored as functions)
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            // Use the TirFunction's is_method() to determine if this is a method
            let func_id = if let Some(ref info) = func.method_info {
                // This is a method - use MethodName or FreeFunctionName with monomorph info
                if let Some(monomorph_info) = &func.monomorph_info {
                    // Monomorphized method - use FreeFunctionName with metadata.
                    // Use the actual module_source where the function lives.
                    FunctionId::Free(FreeFunctionName::with_monomorph_info(
                        module_source.clone(),
                        func.name.clone(),
                        monomorph_info.generic_name.clone(),
                    ))
                } else {
                    // Non-monomorphized method - use method_info
                    FunctionId::Method(MethodName::new(
                        module_source.clone(),
                        info.struct_name.clone(),
                        info.trait_name.clone(),
                        info.method_name.clone(),
                    ))
                }
            } else {
                // Regular function - use FreeFunctionName
                if let Some(monomorph_info) = &func.monomorph_info {
                    // Monomorphized function - use actual module_source
                    FunctionId::Free(FreeFunctionName::with_monomorph_info(
                        module_source.clone(),
                        func.name.clone(),
                        monomorph_info.generic_name.clone(),
                    ))
                } else {
                    FunctionId::Free(FreeFunctionName::from_module_source(
                        module_source,
                        &func.name,
                    ))
                }
            };
            let analysis = analyze_function(&func, module_source, type_table);
            call_graph.insert(func_id.clone(), analysis.callees);
            if !analysis.effect_calls.is_empty() {
                effect_usage.insert(func_id.clone(), analysis.effect_calls);
            }
            if !analysis.canonical_methods.is_empty() {
                canonical_method_usage.insert(func_id, analysis.canonical_methods);
            }
        }

        // Note: impl_block.methods is empty because resolver adds methods to functions
        // with mangled names like "Point::sum". This loop is kept for future compatibility.
        for impl_block in &module.impls {
            let struct_name = match type_table.get(impl_block.target_type) {
                ResolvedType::Struct { name, .. } => name.clone(),
                _ => continue,
            };

            for method in &impl_block.methods {
                let method_id = FunctionId::Method(MethodName::new(
                    module_source.clone(),
                    struct_name.clone(),
                    None,
                    method.name.clone(),
                ));
                let analysis = analyze_function(method, module_source, type_table);
                call_graph.insert(method_id.clone(), analysis.callees);
                if !analysis.effect_calls.is_empty() {
                    effect_usage.insert(method_id.clone(), analysis.effect_calls);
                }
                if !analysis.canonical_methods.is_empty() {
                    canonical_method_usage.insert(method_id, analysis.canonical_methods);
                }
            }
        }
    }

    (call_graph, effect_usage, canonical_method_usage)
}

/// Analyze a TIR function for callees and effect usage
fn analyze_function(
    func: &TirFunction,
    current_module: &ModuleSource,
    type_table: &TypeTable,
) -> FunctionAnalysis {
    let mut analysis = FunctionAnalysis::default();

    if let Some(body) = &func.body {
        analyze_block(body, current_module, type_table, &mut analysis);
    }
    analysis
}

fn analyze_block(
    block: &TirBlock,
    current_module: &ModuleSource,
    type_table: &TypeTable,
    analysis: &mut FunctionAnalysis,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                analyze_expr(value, current_module, type_table, analysis);
            }
            TirStmtKind::Expr(expr) => {
                analyze_expr(expr, current_module, type_table, analysis);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    analyze_expr(expr, current_module, type_table, analysis);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                analyze_expr(condition, current_module, type_table, analysis);
                analyze_block(then_block, current_module, type_table, analysis);
                if let Some(else_blk) = else_block {
                    analyze_block(else_blk, current_module, type_table, analysis);
                }
            }
            TirStmtKind::Loop { body } => {
                analyze_block(body, current_module, type_table, analysis);
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                analyze_block(block, current_module, type_table, analysis);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                analyze_expr(scrutinee, current_module, type_table, analysis);
                analyze_block(then_block, current_module, type_table, analysis);
                if let Some(else_blk) = else_block {
                    analyze_block(else_blk, current_module, type_table, analysis);
                }
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    analyze_expr(v, current_module, type_table, analysis);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LetPattern { value, .. } => {
                analyze_expr(value, current_module, type_table, analysis);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }
}

fn analyze_expr(
    expr: &TirExpr,
    current_module: &ModuleSource,
    type_table: &TypeTable,
    analysis: &mut FunctionAnalysis,
) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let original_callee_module = func.module_source();
            let func_name = func.name();

            // Invariant: TirExprKind::Call should never have method names (containing "::")
            // Methods use TirExprKind::MethodCall instead. The only exception is "builtin::*".
            debug_assert!(
                !func_name.contains("::") || func_name.starts_with("builtin::"),
                "TirExprKind::Call should not have method-style names: {func_name}"
            );

            // Build function ID for the called function
            // If the callee has an entry point module source (local call), use current module.
            // Exception: CM adapter functions are genuinely in the entry module
            // and should NOT be remapped to the caller's module.
            let callee_module = if original_callee_module.is_entry_point() && !func.is_cm_adapter()
            {
                current_module.clone()
            } else {
                original_callee_module.clone()
            };
            let callee_id = FunctionId::Free(FreeFunctionName::from_module_source(
                &callee_module,
                &func_name,
            ));
            analysis.callees.insert(callee_id);

            // Detect effect calls: Effects have a single-element path with PascalCase name
            // (e.g., "Stdout", "Stderr", "MonotonicClock")
            if let Some(effect_name) = original_callee_module.effect_name() {
                analysis
                    .effect_calls
                    .insert((effect_name, func_name.clone()));
            }

            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            // Collect canonical resource method names for builtin import injection.
            if let Some(info) = func.method_info()
                && let Some(canonical_name) = &info.canonical_name
            {
                analysis.canonical_methods.insert(canonical_name.clone());
            }

            // Use the func reference directly - it already has the correct mangled name
            // and monomorph_info from lowering phase
            let func_name = func.name();

            // Check if this is a monomorphized method using FunctionRef metadata
            if func.is_monomorphized() {
                // Monomorphized method (e.g., Array<i32>::len, Box<i32>::get)
                // Use the func reference's information directly
                let base_name = func
                    .base_struct_name()
                    .map(|base| {
                        // Extract method name from "Array<i32>::len" -> "len"
                        func_name
                            .find("::")
                            .map(|pos| format!("{}::{}", base, &func_name[pos + 2..]))
                            .unwrap_or_else(|| base)
                    })
                    .unwrap_or_else(|| func_name.clone());

                // Use the func's actual module_source — monomorphized functions
                // are placed in the module that uses them.
                let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    func.module_source(),
                    func_name.clone(),
                    base_name,
                ));
                analysis.callees.insert(callee_id);
            } else {
                // Non-monomorphized method - determine target from receiver type
                // First strip any reference wrappers and newtypes to get the base type
                let mut current_type = type_table.get(receiver.type_id);
                let mut newtype_info: Option<(String, ModuleSource)> = None;
                loop {
                    match current_type {
                        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                            current_type = type_table.get(*inner);
                        }
                        ResolvedType::Newtype {
                            name,
                            module_source,
                            base_type,
                        } => {
                            // Remember the outermost newtype for its own trait impls
                            if newtype_info.is_none() {
                                newtype_info = Some((name.clone(), module_source.clone()));
                            }
                            current_type = type_table.get(*base_type);
                        }
                        _ => break,
                    }
                }
                let base_receiver_type = current_type.clone();

                // Extract method name and trait name from method_info
                let (method_name, trait_name) = if let Some(info) = func.method_info() {
                    (info.method_name.clone(), info.trait_name.clone())
                } else {
                    (func_name.clone(), None)
                };

                // If the receiver was a newtype (e.g., flags type), also mark
                // the newtype's own methods as reachable (e.g., Perms^Inspect::inspect).
                if let Some((newtype_name, newtype_module)) = newtype_info {
                    let method_id = FunctionId::Method(MethodName::new(
                        newtype_module,
                        newtype_name,
                        trait_name.clone(),
                        method_name.clone(),
                    ));
                    analysis.callees.insert(method_id);
                }

                match base_receiver_type {
                    ResolvedType::Struct {
                        ref name,
                        is_monomorphized: true,
                        base_name: Some(ref base_struct),
                        ..
                    } => {
                        // Monomorphized struct method call - use FunctionId::Free
                        let mangled_func_name =
                            MethodName::format_local(name, trait_name.as_deref(), &method_name);
                        // Build base method name using the original generic struct name
                        let base_method_name = MethodName::format_local(
                            base_struct,
                            trait_name.as_deref(),
                            &method_name,
                        );
                        // Use current module — monomorphized functions live in the
                        // module that uses them.
                        let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                            current_module.clone(),
                            mangled_func_name,
                            base_method_name,
                        ));
                        analysis.callees.insert(callee_id);

                        // For internal Box<T> types (primitive boxing), the method is
                        // actually defined on the inner type (e.g., i32^Ord::cmp, not
                        // Box<i32>^Ord::cmp). Also mark the FunctionRef's original
                        // method target as reachable.
                        if base_struct == "Box"
                            && let Some(info) = func.method_info()
                        {
                            let original_method_id = FunctionId::Method(MethodName::new(
                                func.module_source(),
                                info.struct_name.clone(),
                                info.trait_name.clone(),
                                info.method_name.clone(),
                            ));
                            analysis.callees.insert(original_method_id);
                        }
                    }
                    ResolvedType::Struct {
                        name,
                        module_source,
                        is_monomorphized: false,
                        ..
                    } => {
                        // Regular struct method call - use FunctionId::Method
                        let method_id = FunctionId::Method(MethodName::new(
                            module_source.clone(),
                            name.clone(),
                            trait_name.clone(),
                            method_name.clone(),
                        ));
                        analysis.callees.insert(method_id);

                        // Also mark reachable using the FunctionRef's module source,
                        // since trait impls may live in a different module than the type
                        // (e.g., `impl Display for String` is in format.wado, not string.wado)
                        let func_module = func.module_source();
                        if func_module != module_source.clone()
                            && let Some(info) = func.method_info()
                        {
                            let alt_method_id = FunctionId::Method(MethodName::new(
                                func_module,
                                info.struct_name.clone(),
                                info.trait_name.clone(),
                                info.method_name.clone(),
                            ));
                            analysis.callees.insert(alt_method_id);
                        }
                    }
                    ResolvedType::Primitive(prim) => {
                        // Primitive method call (e.g., i32.to_string())
                        if method_name == "to_string" {
                            add_to_string_callee(receiver.type_id, type_table, analysis);
                        }
                        // Trait and inherent methods on primitives
                        // (e.g., i32^Ord::cmp, char::is_ascii_space)
                        let prim_name = prim.as_str().to_string();
                        let method_id = FunctionId::Method(MethodName::new(
                            ModuleSource::primitives(),
                            prim_name,
                            trait_name.clone(),
                            method_name.clone(),
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::GenericInstance {
                        name,
                        type_args,
                        module_source: _,
                    } => {
                        // Generic instance method call (e.g., Box<i32>.get())
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        // Include trait name for trait methods (e.g., TreeMap<String,i32>^Index::index)
                        let (mangled_func_name, base_name) = if let Some(ref trait_n) = trait_name {
                            let generic_name = mangle_generic_name(&name, &type_arg_names);
                            let mangled =
                                mangle_local_trait_method(&generic_name, trait_n, &method_name);
                            let base = mangle_local_trait_method(&name, trait_n, &method_name);
                            (mangled, base)
                        } else {
                            let mangled =
                                mangle_method_generic(&name, &type_arg_names, &method_name);
                            let base = mangle_local_method(&name, &method_name);
                            (mangled, base)
                        };
                        let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                            current_module.clone(),
                            mangled_func_name,
                            base_name,
                        ));
                        analysis.callees.insert(callee_id);
                    }
                    ResolvedType::Enum {
                        name,
                        module_source,
                    } => {
                        // Enum method call (user-defined or auto-derived trait impls)
                        let method_id = FunctionId::Method(MethodName::new(
                            module_source.clone(),
                            name.clone(),
                            trait_name.clone(),
                            method_name.clone(),
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::Resource { name, .. } => {
                        // Resource instance method call (e.g., fields.has(), fields.append())
                        // Record as effect call so it's tracked in used_wasi_functions
                        analysis
                            .effect_calls
                            .insert((name.clone(), method_name.clone()));
                    }
                    ResolvedType::Variant {
                        name,
                        module_source,
                        ..
                    } => {
                        // Variant method call (e.g., Shape^Inspect::inspect)
                        let method_id = FunctionId::Method(MethodName::new(
                            module_source.clone(),
                            name.clone(),
                            trait_name.clone(),
                            method_name.clone(),
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::Tuple(elems) => {
                        // Tuple method call (e.g., Tuple<f64,f64>^Inspect::inspect)
                        // Synthesized as non-monomorphized methods with struct_name "Tuple<f64,f64>"
                        let type_arg_names: Vec<String> = elems
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        let mangled_struct = mangle_generic_name("Tuple", &type_arg_names);
                        let method_id = FunctionId::Method(MethodName::new(
                            current_module.clone(),
                            mangled_struct,
                            trait_name.clone(),
                            method_name.clone(),
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::Function {
                        params,
                        return_type,
                        ..
                    } => {
                        // Function type method call (e.g., Fn<2,i32>^Inspect::inspect)
                        let type_arg_names = vec![
                            params.len().to_string(),
                            type_table.mangle_type_name(return_type),
                        ];
                        let mangled_struct = mangle_generic_name("Fn", &type_arg_names);
                        let method_id = FunctionId::Method(MethodName::new(
                            current_module.clone(),
                            mangled_struct,
                            trait_name.clone(),
                            method_name.clone(),
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::GenericResource {
                        name, type_args, ..
                    } => {
                        // Generic resource method call (e.g., Future<T>^Inspect::inspect)
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        let mangled_struct = mangle_generic_name(name.as_str(), &type_arg_names);
                        let method_id = FunctionId::Method(MethodName::new(
                            current_module.clone(),
                            mangled_struct,
                            trait_name.clone(),
                            method_name.clone(),
                        ));
                        analysis.callees.insert(method_id);
                    }
                    _ => {}
                }
            }

            analyze_expr(receiver, current_module, type_table, analysis);
            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            analyze_expr(left, current_module, type_table, analysis);
            analyze_expr(right, current_module, type_table, analysis);
        }
        TirExprKind::Unary { expr, .. } => {
            analyze_expr(expr, current_module, type_table, analysis);
        }
        TirExprKind::Assign { target, value } => {
            analyze_expr(target, current_module, type_table, analysis);
            analyze_expr(value, current_module, type_table, analysis);
        }
        TirExprKind::Cast { expr, .. } => {
            analyze_expr(expr, current_module, type_table, analysis);
        }
        TirExprKind::CmRawCall { local_name, args } => {
            // CmRawCall references a lowered WASI import function.
            // Parse the local_name (e.g., "wasi:cli/Stdout::write_via_stream")
            // to extract the effect_name and op_name for WASI import tracking.
            if let Some((effect_name, op_name)) = local_name.split_once("::").map(|(prefix, op)| {
                // prefix is like "wasi:cli/Stdout" → extract "Stdout"
                let effect = prefix.rsplit('/').next().unwrap_or(prefix);
                (effect.to_string(), op.to_string())
            }) {
                analysis.effect_calls.insert((effect_name, op_name));
            }
            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::StaticCall { func, args } => {
            let func_name = func.name();
            // Static method call - func_name already contains "StructName::method_name"
            // The function is registered as a free function with mangled name
            let callee_id = if func.is_monomorphized() {
                // Get base name from the function's monomorph_info
                let base_name = func
                    .base_struct_name()
                    .map(|base| {
                        // Extract method name from "Box<i32>::get" -> "get"
                        func_name
                            .find("::")
                            .map(|pos| format!("{}::{}", base, &func_name[pos + 2..]))
                            .unwrap_or_else(|| base)
                    })
                    .unwrap_or_else(|| func_name.clone());
                // Use the func's actual module_source
                FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    func.module_source(),
                    func_name.clone(),
                    base_name,
                ))
            } else {
                let callee_module = func.module_source();
                // Use current module for local calls (entry point source)
                let callee_module = if callee_module.is_entry_point() {
                    current_module.clone()
                } else {
                    callee_module
                };
                // Check if this is a method call (contains "::") or a regular function
                if let Some(sep_pos) = func_name.find("::") {
                    // This is a static method call (e.g., "Uint128::from_u64")
                    // Track as FunctionId::Method to match WIR registration
                    let prefix = &func_name[..sep_pos];
                    let method_name = &func_name[sep_pos + 2..];
                    // Parse struct name and optional trait name from prefix
                    // Format: "StructName" or "StructName^TraitName"
                    let (struct_name, trait_name): (&str, Option<&str>) =
                        if let Some(caret_pos) = prefix.find('^') {
                            (&prefix[..caret_pos], Some(&prefix[caret_pos + 1..]))
                        } else {
                            (prefix, None)
                        };
                    FunctionId::Method(MethodName::new(
                        callee_module.clone(),
                        struct_name.to_string(),
                        trait_name.map(String::from),
                        method_name.to_string(),
                    ))
                } else {
                    FunctionId::Free(FreeFunctionName::from_module_source(
                        &callee_module,
                        &func_name,
                    ))
                }
            };
            analysis.callees.insert(callee_id);

            // Detect resource method calls from WASI modules
            // Static methods on resources (e.g., TcpSocket::static_tcp_socket_create)
            // need to be tracked as effect calls for proper import generation
            let module_path = func.module_path();
            if module_path.len() >= 2 && module_path[0] == "wasi" {
                // This is a WASI module - check for "Type::method" pattern
                if let Some(pos) = func_name.find("::") {
                    let resource_name = &func_name[..pos];
                    let method_name = &func_name[pos + 2..];
                    // Record as effect call (effect_name = resource name, op_name = method name)
                    analysis
                        .effect_calls
                        .insert((resource_name.to_string(), method_name.to_string()));
                }
            }

            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::FieldAccess { expr, .. } => {
            analyze_expr(expr, current_module, type_table, analysis);
        }
        TirExprKind::Index { expr, index } => {
            analyze_expr(expr, current_module, type_table, analysis);
            analyze_expr(index, current_module, type_table, analysis);
        }
        TirExprKind::Block(block) => {
            analyze_block(block, current_module, type_table, analysis);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_expr(condition, current_module, type_table, analysis);
            analyze_block(then_branch, current_module, type_table, analysis);
            if let Some(else_blk) = else_branch {
                analyze_block(else_blk, current_module, type_table, analysis);
            }
        }
        TirExprKind::Match { expr, arms } => {
            analyze_expr(expr, current_module, type_table, analysis);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    analyze_expr(guard, current_module, type_table, analysis);
                }
                analyze_expr(&arm.body, current_module, type_table, analysis);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_expr(&field.value, current_module, type_table, analysis);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                analyze_expr(elem, current_module, type_table, analysis);
            }
        }
        TirExprKind::Closure { body, .. } => {
            analyze_expr(body, current_module, type_table, analysis);
        }
        TirExprKind::IndirectCall { callee, args } => {
            analyze_expr(callee, current_module, type_table, analysis);
            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::ClosureToCanonical {
            functor,
            functor_id,
            ..
        } => {
            analyze_expr(functor, current_module, type_table, analysis);
            // Mark the __call method as reachable (it's referenced via ref.func)
            let method_name = MethodName::new(
                current_module.clone(),
                format!("__Closure_{functor_id}"),
                None,
                "__call".to_string(),
            );
            analysis.callees.insert(FunctionId::Method(method_name));
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                analyze_expr(payload_expr, current_module, type_table, analysis);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            analyze_block(block, current_module, type_table, analysis);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            analyze_expr(value, current_module, type_table, analysis);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            analyze_expr(expr, current_module, type_table, analysis);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            analyze_expr(expr, current_module, type_table, analysis);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            analyze_expr(scrutinee, current_module, type_table, analysis);
            for arm in arms {
                analyze_block(arm, current_module, type_table, analysis);
            }
            analyze_block(default, current_module, type_table, analysis);
        }
        // Leaf nodes - no calls
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Add the appropriate `to_string` function call for a type
fn add_to_string_callee(type_id: TypeId, type_table: &TypeTable, analysis: &mut FunctionAnalysis) {
    // Follow newtype chain to get the ultimate base type
    let base_type_id = type_table.get_ultimate_base_type(type_id);
    match type_table.get(base_type_id) {
        ResolvedType::Primitive(prim) => {
            // Primitive to_string methods are defined in core:prelude/primitives as impl blocks
            // e.g., impl i32 { fn to_string(&self) -> String { ... } }
            let prim_name = prim.as_str();
            // Method format: module_source/StructName::method_name
            let method_id = FunctionId::Method(MethodName::new(
                ModuleSource::primitives(),
                prim_name.to_string(),
                None,
                "to_string".to_string(),
            ));
            analysis.callees.insert(method_id);
        }
        ResolvedType::Struct { name, .. } if name == "String" => {
            // String.to_string() is a no-op, no function call needed
        }
        _ => {}
    }
}

/// Mangle a type ID into a string suitable for struct/function names.
/// Compute the set of reachable functions from an entry point
fn compute_reachable(
    call_graph: &IndexMap<FunctionId, IndexSet<FunctionId>>,
    entry: &FunctionId,
) -> IndexSet<FunctionId> {
    let mut reachable = IndexSet::new();
    let mut worklist = vec![entry.clone()];

    while let Some(func) = worklist.pop() {
        if reachable.contains(&func) {
            continue;
        }
        reachable.insert(func.clone());

        // Add all callees to worklist
        if let Some(callees) = call_graph.get(&func) {
            for callee in callees {
                if !reachable.contains(callee) {
                    worklist.push(callee.clone());
                }
            }
        }
    }

    reachable
}

/// Remove unreachable functions from the project's TIR modules.
///
/// This physically removes functions that are not in `reachable_functions`
/// from the TIR, so codegen doesn't need to filter them.
pub fn remove_unreachable_functions(project: &mut Project) {
    for (module_source, module) in &mut project.tir_modules {
        // Retain only reachable functions
        module.functions.retain(|func_rc| {
            let func = func_rc.borrow();

            // Use TirFunction's method_info to check if this is a method
            if let Some(ref info) = func.method_info {
                // Could be either:
                // - Instance method tracked as FunctionId::Method
                // - Static method tracked as FunctionId::Free with mangled name
                // Use method_info to build the method ID
                // Try as instance method (FunctionId::Method)
                let method_id = FunctionId::Method(MethodName::new(
                    module_source.clone(),
                    info.struct_name.clone(),
                    info.trait_name.clone(),
                    info.method_name.clone(),
                ));
                if project.reachable_functions.contains(&method_id) {
                    return true;
                }

                // Try as static method (FunctionId::Free with mangled name)
                let free_id = FunctionId::Free(FreeFunctionName::from_module_source(
                    module_source,
                    &func.name,
                ));
                if project.reachable_functions.contains(&free_id) {
                    return true;
                }

                // For generic methods/static methods, check if any monomorphized version is reachable
                // Generic functions are named "Array::with_capacity" but calls use "Array<i32>::with_capacity"
                // Check if any function ID in reachable_functions matches this base name
                is_generic_func_reachable(&project.reachable_functions, module_source, &func.name)
            } else {
                // Regular function
                let func_id = FunctionId::Free(FreeFunctionName::from_module_source(
                    module_source,
                    &func.name,
                ));
                project.reachable_functions.contains(&func_id)
            }
        });
    }
}

/// Check if a generic function has any monomorphized version that is reachable.
/// For example, "`Array::with_capacity`" should be kept if "Array<i32>`::with_capacity`" is reachable.
fn is_generic_func_reachable(
    reachable: &IndexSet<FunctionId>,
    module_source: &ModuleSource,
    func_name: &str,
) -> bool {
    // func_name is like "Array::with_capacity"
    // We need to find any "Array<..>::with_capacity" in reachable set
    let Some(sep_pos) = func_name.find("::") else {
        return false;
    };
    let base_struct = &func_name[..sep_pos];
    let method_name = &func_name[sep_pos + 2..];

    for id in reachable {
        if let FunctionId::Free(free_name) = id {
            if free_name.module_source != *module_source {
                continue;
            }

            // Check if name matches pattern "BaseStruct<..>::method_name"
            if let Some(call_sep_pos) = free_name.name.find("::") {
                let call_method = &free_name.name[call_sep_pos + 2..];

                // Check if method name matches
                if call_method != method_name {
                    continue;
                }

                // Check if struct name matches using base_name metadata
                // For monomorphized: "Array<i32>::len" has base_name "Array::len" -> extract "Array"
                if free_name.is_monomorphized {
                    if let Some(ref base_name) = free_name.base_name {
                        // base_name is the generic name like "Array::len" - extract struct part
                        let base_struct_from_meta = base_name
                            .find("::")
                            .map(|pos| &base_name[..pos])
                            .unwrap_or(base_name);
                        if base_struct_from_meta == base_struct {
                            return true;
                        }
                    }
                } else {
                    // Non-monomorphized: direct struct name match
                    let call_struct = &free_name.name[..call_sep_pos];
                    if call_struct == base_struct {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Compute the set of reachable types from reachable functions.
/// A type is reachable if it's used in any reachable function's signature,
/// locals, or expressions.
fn compute_reachable_types(project: &Project) -> IndexSet<TypeId> {
    let mut reachable_types: IndexSet<TypeId> = IndexSet::new();

    // Always include primitive types (TypeId 0-17)
    for i in 0..18 {
        reachable_types.insert(TypeId(i));
    }

    // Always include BuiltinArray(U8) as it's fundamental for String operations
    // and used by codegen for internal operations (assert statements, etc.)
    // Find the TypeId for BuiltinArray(U8) in the type table
    if let Some(module) = project.tir_modules.values().next() {
        let type_table = module.type_table.borrow();
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::BuiltinArray(elem) = type_table.get(type_id)
                && *elem == TypeTable::U8
            {
                reachable_types.insert(type_id);
                break;
            }
        }
    }

    // Phase 1: Collect types from all remaining functions
    // Note: We collect from ALL functions that exist after function DCE,
    // because function DCE has already removed unreachable functions.
    // This is more conservative but ensures we don't miss any types.
    for (_module_source, module) in &project.tir_modules {
        let type_table = module.type_table.borrow();

        for func_rc in &module.functions {
            let func = func_rc.borrow();
            collect_types_from_function(&func, &type_table, &mut reachable_types);
        }

        // Also collect types from impl blocks
        for impl_block in &module.impls {
            reachable_types.insert(impl_block.target_type);
            for method in &impl_block.methods {
                collect_types_from_function(method, &type_table, &mut reachable_types);
            }
        }

        // Collect types from global variables
        for global in &module.globals {
            collect_types_from_expr(&global.initializer, &type_table, &mut reachable_types);
        }

        // Collect types from closure functors' __call methods
        for functor in &module.closure_functors {
            reachable_types.insert(functor.struct_type_id);
            reachable_types.insert(functor.ref_type_id);
            let call_method = functor.call_method.borrow();
            collect_types_from_function(&call_method, &type_table, &mut reachable_types);
            for capture in &functor.captures {
                collect_type_transitive(capture.type_id, &type_table, &mut reachable_types);
            }
        }
    }

    // Phase 2: Transitive closure - include struct fields, variant payloads, and type dependencies
    let mut changed = true;
    while changed {
        changed = false;
        let before_len = reachable_types.len();

        for (module_source, module) in &project.tir_modules {
            let type_table = module.type_table.borrow();

            // Collect struct field types for reachable structs
            // A struct's fields should be collected if:
            // 1. The Struct type itself is reachable, OR
            // 2. Any GenericInstance with this struct name is reachable, OR
            // 3. Any monomorphized version with this base name is reachable
            for tir_struct in &module.structs {
                let struct_reachable = if tir_struct.monomorph_info.is_none() {
                    // Non-monomorphized struct
                    let direct_reachable = type_table
                        .find_struct_type(&tir_struct.name, module_source)
                        .map(|id| reachable_types.contains(&id))
                        .unwrap_or(false);

                    let instance_reachable = reachable_types.iter().any(|&id| {
                        matches!(
                            type_table.get(id),
                            ResolvedType::GenericInstance { name, .. } if name == &tir_struct.name
                        )
                    });

                    let monomorph_reachable = reachable_types.iter().any(|&id| {
                        matches!(
                            type_table.get(id),
                            ResolvedType::Struct { base_name: Some(base), is_monomorphized: true, .. } if base == &tir_struct.name
                        )
                    });

                    direct_reachable || instance_reachable || monomorph_reachable
                } else {
                    // Monomorphized struct - check by exact name match
                    reachable_types.iter().any(|&id| {
                        matches!(
                            type_table.get(id),
                            ResolvedType::Struct { name, is_monomorphized: true, .. } if name == &tir_struct.name
                        )
                    })
                };

                if struct_reachable {
                    for field in &tir_struct.fields {
                        collect_type_transitive(field.type_id, &type_table, &mut reachable_types);
                    }
                }
            }

            // Collect variant payload types for reachable variants
            // A variant's payloads should be collected if:
            // 1. The base Variant type is reachable, OR
            // 2. Any GenericInstance with this variant name is reachable
            for variant in &module.variants {
                let base_reachable = type_table
                    .iter_type_ids()
                    .find(|&id| matches!(type_table.get(id), ResolvedType::Variant { name, .. } if name == &variant.name))
                    .map(|id| reachable_types.contains(&id))
                    .unwrap_or(false);

                let instance_reachable = reachable_types.iter().any(|&id| {
                    matches!(
                        type_table.get(id),
                        ResolvedType::GenericInstance { name, .. } if name == &variant.name
                    )
                });

                if base_reachable || instance_reachable {
                    for case in &variant.cases {
                        collect_type_transitive(case.payload, &type_table, &mut reachable_types);
                    }
                }
            }

            // Collect type dependencies (array elements, option inner, etc.)
            let current_types: Vec<TypeId> = reachable_types.iter().copied().collect();
            for type_id in current_types {
                collect_type_dependencies(type_id, &type_table, &mut reachable_types);
            }
        }

        if reachable_types.len() > before_len {
            changed = true;
        }
    }

    reachable_types
}

/// Collect all types used in a function
fn collect_types_from_function(
    func: &TirFunction,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    // Collect parameter types
    for param in &func.params {
        collect_type_transitive(param.type_id, type_table, reachable);
    }

    // Collect return type
    collect_type_transitive(func.return_type, type_table, reachable);

    // Collect local variable types (includes types from inlined functions)
    for &local_type_id in &func.local_types {
        collect_type_transitive(local_type_id, type_table, reachable);
    }

    // Collect types from body
    if let Some(body) = &func.body {
        collect_types_from_block(body, type_table, reachable);
    }
}

/// Collect types from a block
fn collect_types_from_block(
    block: &TirBlock,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                collect_type_transitive(*type_id, type_table, reachable);
                collect_types_from_expr(value, type_table, reachable);
            }
            TirStmtKind::Expr(expr) => {
                collect_types_from_expr(expr, type_table, reachable);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    collect_types_from_expr(expr, type_table, reachable);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_types_from_expr(condition, type_table, reachable);
                collect_types_from_block(then_block, type_table, reachable);
                if let Some(else_blk) = else_block {
                    collect_types_from_block(else_blk, type_table, reachable);
                }
            }
            TirStmtKind::Loop { body } => {
                collect_types_from_block(body, type_table, reachable);
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                collect_types_from_block(block, type_table, reachable);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                collect_types_from_expr(scrutinee, type_table, reachable);
                collect_types_from_pattern(pattern, type_table, reachable);
                collect_types_from_block(then_block, type_table, reachable);
                if let Some(else_blk) = else_block {
                    collect_types_from_block(else_blk, type_table, reachable);
                }
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    collect_types_from_expr(v, type_table, reachable);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LetPattern { pattern, value, .. } => {
                collect_types_from_pattern(pattern, type_table, reachable);
                collect_types_from_expr(value, type_table, reachable);
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
        }
    }
}

/// Collect types from an expression
fn collect_types_from_expr(
    expr: &TirExpr,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    // Always collect the expression's type
    collect_type_transitive(expr.type_id, type_table, reachable);

    match &expr.kind {
        TirExprKind::Call { args, .. } => {
            for arg in args {
                collect_types_from_expr(arg, type_table, reachable);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_types_from_expr(receiver, type_table, reachable);
            for arg in args {
                collect_types_from_expr(arg, type_table, reachable);
            }
        }
        TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                collect_types_from_expr(arg, type_table, reachable);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_types_from_expr(left, type_table, reachable);
            collect_types_from_expr(right, type_table, reachable);
        }
        TirExprKind::Unary { expr, .. } => {
            collect_types_from_expr(expr, type_table, reachable);
        }
        TirExprKind::Assign { target, value } => {
            collect_types_from_expr(target, type_table, reachable);
            collect_types_from_expr(value, type_table, reachable);
        }
        TirExprKind::Cast { expr, target_type } => {
            collect_types_from_expr(expr, type_table, reachable);
            collect_type_transitive(*target_type, type_table, reachable);
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_types_from_expr(arg, type_table, reachable);
            }
        }
        TirExprKind::FieldAccess { expr, .. } => {
            collect_types_from_expr(expr, type_table, reachable);
        }
        TirExprKind::Index { expr, index } => {
            collect_types_from_expr(expr, type_table, reachable);
            collect_types_from_expr(index, type_table, reachable);
        }
        TirExprKind::Block(block) => {
            collect_types_from_block(block, type_table, reachable);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_types_from_expr(condition, type_table, reachable);
            collect_types_from_block(then_branch, type_table, reachable);
            if let Some(else_blk) = else_branch {
                collect_types_from_block(else_blk, type_table, reachable);
            }
        }
        TirExprKind::Match { expr, arms } => {
            collect_types_from_expr(expr, type_table, reachable);
            for arm in arms {
                collect_types_from_pattern(&arm.pattern, type_table, reachable);
                if let Some(guard) = &arm.guard {
                    collect_types_from_expr(guard, type_table, reachable);
                }
                collect_types_from_expr(&arm.body, type_table, reachable);
            }
        }
        TirExprKind::StructLiteral {
            struct_type,
            fields,
            ..
        } => {
            collect_type_transitive(*struct_type, type_table, reachable);
            for field in fields {
                collect_types_from_expr(&field.value, type_table, reachable);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_types_from_expr(elem, type_table, reachable);
            }
        }
        TirExprKind::Closure {
            params,
            body,
            captures,
            ..
        } => {
            // Collect parameter types
            for (_name, type_id) in params {
                collect_type_transitive(*type_id, type_table, reachable);
            }
            // Collect capture types
            for capture in captures {
                collect_type_transitive(capture.type_id, type_table, reachable);
            }
            collect_types_from_expr(body, type_table, reachable);
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_types_from_expr(callee, type_table, reachable);
            for arg in args {
                collect_types_from_expr(arg, type_table, reachable);
            }
        }
        TirExprKind::ClosureToCanonical {
            functor,
            target_fn_type,
            ..
        } => {
            collect_types_from_expr(functor, type_table, reachable);
            collect_type_transitive(*target_fn_type, type_table, reachable);
        }
        TirExprKind::VariantConstruct {
            variant_type,
            payload,
            ..
        } => {
            collect_type_transitive(*variant_type, type_table, reachable);
            if let Some(payload_expr) = payload {
                collect_types_from_expr(payload_expr, type_table, reachable);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_types_from_block(block, type_table, reachable);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_types_from_expr(value, type_table, reachable);
        }
        TirExprKind::VariantTag { expr } | TirExprKind::VariantTest { expr, .. } => {
            collect_types_from_expr(expr, type_table, reachable);
        }
        TirExprKind::VariantPayload {
            expr, payload_type, ..
        } => {
            collect_types_from_expr(expr, type_table, reachable);
            collect_type_transitive(*payload_type, type_table, reachable);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_types_from_expr(scrutinee, type_table, reachable);
            for arm in arms {
                collect_types_from_block(arm, type_table, reachable);
            }
            collect_types_from_block(default, type_table, reachable);
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Collect types from a pattern
fn collect_types_from_pattern(
    pattern: &crate::tir::TirPattern,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    use crate::tir::TirPattern;

    match pattern {
        TirPattern::Wildcard => {}
        TirPattern::Binding { type_id, .. } => {
            collect_type_transitive(*type_id, type_table, reachable);
        }
        TirPattern::Literal(_) => {}
        TirPattern::Tuple(patterns) => {
            for p in patterns {
                collect_types_from_pattern(p, type_table, reachable);
            }
        }
        TirPattern::Variant {
            enum_type,
            bindings,
            payload_type,
            ..
        } => {
            collect_type_transitive(*enum_type, type_table, reachable);
            collect_type_transitive(*payload_type, type_table, reachable);
            for binding in bindings {
                collect_types_from_pattern(binding, type_table, reachable);
            }
        }
        TirPattern::Enum { enum_type, .. } => {
            collect_type_transitive(*enum_type, type_table, reachable);
        }
        TirPattern::Struct {
            struct_type,
            fields,
            ..
        } => {
            collect_type_transitive(*struct_type, type_table, reachable);
            for field in fields {
                collect_types_from_pattern(&field.pattern, type_table, reachable);
            }
        }
    }
}

/// Add a type and its dependencies to the reachable set
fn collect_type_transitive(
    type_id: TypeId,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    if reachable.contains(&type_id) {
        return;
    }
    reachable.insert(type_id);
    collect_type_dependencies(type_id, type_table, reachable);
}

/// Collect direct type dependencies (struct fields, array elements, etc.)
fn collect_type_dependencies(
    type_id: TypeId,
    type_table: &TypeTable,
    reachable: &mut IndexSet<TypeId>,
) {
    match type_table.get(type_id) {
        ResolvedType::BuiltinArray(inner)
        | ResolvedType::Ref(inner)
        | ResolvedType::MutRef(inner)
        | ResolvedType::Reactive(inner) => {
            collect_type_transitive(*inner, type_table, reachable);
        }
        ResolvedType::GenericResource { type_args, .. } => {
            for &arg in type_args {
                collect_type_transitive(arg, type_table, reachable);
            }
        }
        ResolvedType::Tuple(elements) => {
            for elem in elements {
                collect_type_transitive(*elem, type_table, reachable);
            }
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            for param in params {
                collect_type_transitive(*param, type_table, reachable);
            }
            collect_type_transitive(*return_type, type_table, reachable);
        }
        ResolvedType::GenericInstance { type_args, .. } => {
            for arg in type_args {
                collect_type_transitive(*arg, type_table, reachable);
            }
        }
        // Leaf types - no dependencies
        ResolvedType::Primitive(_)
        | ResolvedType::Unit
        | ResolvedType::Never
        | ResolvedType::Unknown
        | ResolvedType::Error
        | ResolvedType::Struct { .. }
        | ResolvedType::Enum { .. }
        | ResolvedType::Variant { .. }
        | ResolvedType::Resource { .. }
        | ResolvedType::TypeParam { .. }
        | ResolvedType::AssocTypeProjection { .. } => {}

        // Newtype: collect dependency on base type
        ResolvedType::Newtype { base_type, .. } => {
            collect_type_transitive(*base_type, type_table, reachable);
        }
    }
}

/// Remove unreachable types from the project's `TypeTable` and module definitions.
/// This should be called after function DCE.
pub fn remove_unreachable_types(project: &mut Project) {
    let reachable_types = compute_reachable_types(project);

    // Remove unreachable struct/variant/enum definitions from each module
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        let module_source = module.module_source.clone();

        // Collect names of structs to keep
        // A struct is kept if:
        // 1. Its Struct type is reachable, OR
        // 2. Any GenericInstance with its base name is reachable (e.g., Box<i32> for Box)
        // 3. Any monomorphized Struct with its base name is reachable
        let keep_structs: IndexSet<String> = module
            .structs
            .iter()
            .filter(|s| {
                // For non-monomorphized structs
                if s.monomorph_info.is_none() {
                    // Check if the struct type itself is reachable
                    let struct_reachable = type_table
                        .find_struct_type(&s.name, &module_source)
                        .map(|id| reachable_types.contains(&id))
                        .unwrap_or(false);

                    // Check if any GenericInstance with this struct name is reachable
                    let instance_reachable = reachable_types.iter().any(|&id| {
                        matches!(
                            type_table.get(id),
                            ResolvedType::GenericInstance { name, .. } if name == &s.name
                        )
                    });

                    // Check if any monomorphized version is reachable
                    let monomorph_reachable = reachable_types.iter().any(|&id| {
                        matches!(
                            type_table.get(id),
                            ResolvedType::Struct { base_name: Some(base), is_monomorphized: true, .. } if base == &s.name
                        )
                    });

                    struct_reachable || instance_reachable || monomorph_reachable
                } else {
                    // For monomorphized structs, check by exact name match
                    reachable_types.iter().any(|&id| {
                        matches!(
                            type_table.get(id),
                            ResolvedType::Struct { name, is_monomorphized: true, .. } if name == &s.name
                        )
                    })
                }
            })
            .map(|s| s.name.clone())
            .collect();

        // Collect names of variants to keep
        // A variant is kept if:
        // 1. Its base Variant type is reachable, OR
        // 2. Any GenericInstance with its name is reachable (e.g., Result<i32, String>)
        let keep_variants: IndexSet<String> = module
            .variants
            .iter()
            .filter(|v| {
                // Check if base Variant type is reachable
                let base_reachable = type_table
                    .iter_type_ids()
                    .find(|&id| {
                        matches!(type_table.get(id), ResolvedType::Variant { name, .. } if name == &v.name)
                    })
                    .map(|id| reachable_types.contains(&id))
                    .unwrap_or(false);

                // Check if any GenericInstance with this variant name is reachable
                let instance_reachable = reachable_types.iter().any(|&id| {
                    matches!(
                        type_table.get(id),
                        ResolvedType::GenericInstance { name, .. } if name == &v.name
                    )
                });

                base_reachable || instance_reachable
            })
            .map(|v| v.name.clone())
            .collect();

        // Collect names of enums to keep
        let keep_enums: IndexSet<String> = module
            .enums
            .iter()
            .filter(|e| {
                type_table
                    .iter_type_ids()
                    .find(|&id| {
                        matches!(type_table.get(id), ResolvedType::Enum { name, .. } if name == &e.name)
                    })
                    .map(|id| reachable_types.contains(&id))
                    .unwrap_or(false)
            })
            .map(|e| e.name.clone())
            .collect();

        drop(type_table);

        // Remove unreachable definitions
        module.structs.retain(|s| keep_structs.contains(&s.name));
        module.variants.retain(|v| keep_variants.contains(&v.name));
        module.enums.retain(|e| keep_enums.contains(&e.name));
    }

    // Remove unreachable types from the shared TypeTable
    // Since all modules share the same TypeTable via Rc<RefCell<>>,
    // we only need to modify it once through any module
    if let Some(module) = project.tir_modules.values().next() {
        let mut type_table = module.type_table.borrow_mut();
        type_table.retain(|type_id, _| reachable_types.contains(&type_id));
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Global variable DCE
// ──────────────────────────────────────────────────────────────────────────────

/// Remove unreachable global variables from the project's TIR modules.
///
/// A global is considered "used" if any surviving function references it via
/// `GlobalVarGet`. Globals only referenced by `GlobalVarSet` (e.g., their
/// lazy initializer in `__initialize_module`) are dead.
///
/// When a global is removed:
/// 1. Its declaration is removed from `module.globals`
/// 2. Any `GlobalVarSet` statements for it are removed from function bodies
///    (this covers both the original `__initialize_module` and inlined copies)
pub fn remove_unreachable_globals(project: &mut Project) {
    // Phase 1: Collect all GlobalVarGet references from surviving functions.
    // Key: (module_source path as string, global name)
    let mut used_globals: IndexSet<(String, String)> = IndexSet::new();

    for module in project.tir_modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                collect_global_reads_block(body, &mut used_globals);
            }
        }
    }

    // Phase 2: Remove unused globals from module.globals
    for module in project.tir_modules.values_mut() {
        let module_key = module.module_source.to_path().join("::");
        module.globals.retain(|global| {
            let global_module_key = global.module_source.to_path().join("::");
            used_globals.contains(&(global_module_key, global.name.clone()))
                || used_globals.contains(&(module_key.clone(), global.name.clone()))
        });
    }

    // Phase 3: Remove GlobalVarSet statements for dead globals from function bodies
    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(body) = &mut func.body {
                remove_dead_global_sets_block(body, &used_globals);
            }
        }
    }
}

/// Collect all `GlobalVarGet` references from a block.
fn collect_global_reads_block(block: &TirBlock, used: &mut IndexSet<(String, String)>) {
    for stmt in &block.stmts {
        collect_global_reads_stmt(stmt, used);
    }
}

fn collect_global_reads_stmt(stmt: &TirStmt, used: &mut IndexSet<(String, String)>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            collect_global_reads_expr(value, used);
        }
        TirStmtKind::Return { value } => {
            if let Some(expr) = value {
                collect_global_reads_expr(expr, used);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_global_reads_expr(condition, used);
            collect_global_reads_block(then_block, used);
            if let Some(else_blk) = else_block {
                collect_global_reads_block(else_blk, used);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_global_reads_block(body, used);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_global_reads_expr(scrutinee, used);
            collect_global_reads_block(then_block, used);
            if let Some(else_blk) = else_block {
                collect_global_reads_block(else_blk, used);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_global_reads_expr(v, used);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            collect_global_reads_expr(value, used);
        }
        TirStmtKind::TaskReturn { .. } => {}
    }
}

fn collect_global_reads_expr(expr: &TirExpr, used: &mut IndexSet<(String, String)>) {
    match &expr.kind {
        TirExprKind::GlobalVarGet {
            module_source,
            name,
        } => {
            used.insert((module_source.to_path().join("::"), name.clone()));
        }
        // Recurse into sub-expressions — mirrors analyze_expr structure
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_global_reads_expr(arg, used);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_global_reads_expr(receiver, used);
            for arg in args {
                collect_global_reads_expr(arg, used);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_global_reads_expr(left, used);
            collect_global_reads_expr(right, used);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            collect_global_reads_expr(inner, used);
        }
        TirExprKind::Assign { target, value } => {
            collect_global_reads_expr(target, used);
            collect_global_reads_expr(value, used);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_global_reads_expr(condition, used);
            collect_global_reads_block(then_branch, used);
            if let Some(else_blk) = else_branch {
                collect_global_reads_block(else_blk, used);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_global_reads_block(block, used);
        }
        TirExprKind::Index { expr, index } => {
            collect_global_reads_expr(expr, used);
            collect_global_reads_expr(index, used);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_global_reads_expr(&field.value, used);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_global_reads_expr(elem, used);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_global_reads_expr(body, used);
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_global_reads_expr(callee, used);
            for arg in args {
                collect_global_reads_expr(arg, used);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_global_reads_expr(functor, used);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_global_reads_expr(payload_expr, used);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_global_reads_expr(value, used);
        }
        TirExprKind::Match { expr, arms } => {
            collect_global_reads_expr(expr, used);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_global_reads_expr(guard, used);
                }
                collect_global_reads_expr(&arm.body, used);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_global_reads_expr(scrutinee, used);
            for arm in arms {
                collect_global_reads_block(arm, used);
            }
            collect_global_reads_block(default, used);
        }
        // Leaf nodes — no GlobalVarGet possible
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Remove `GlobalVarSet` statements for dead globals from a block.
///
/// For dead globals whose initializer contains function calls (potential side
/// effects), the `GlobalVarSet` is replaced with the value expression to
/// preserve the side effects. For pure initializers (constants, struct/array
/// literals without calls), the entire statement is removed.
fn remove_dead_global_sets_block(block: &mut TirBlock, used: &IndexSet<(String, String)>) {
    // Recurse into sub-statements first
    for stmt in &mut block.stmts {
        remove_dead_global_sets_stmt(stmt, used);
    }

    // Process GlobalVarSet statements for dead globals
    let mut new_stmts: Vec<TirStmt> = Vec::with_capacity(block.stmts.len());
    for stmt in std::mem::take(&mut block.stmts) {
        if let TirStmtKind::Expr(ref expr) = stmt.kind
            && let TirExprKind::GlobalVarSet {
                ref module_source,
                ref name,
                ref value,
                ..
            } = expr.kind
        {
            let key = (module_source.to_path().join("::"), name.clone());
            if !used.contains(&key) {
                // Dead global: keep the value expression only if it has side effects
                // (e.g., panic() / unreachable — detected via never type)
                if expr_has_side_effects(value) {
                    new_stmts.push(TirStmt::new(TirStmtKind::Expr(*value.clone()), stmt.span));
                }
                continue;
            }
        }
        new_stmts.push(stmt);
    }
    block.stmts = new_stmts;
}

/// Check whether an expression tree contains observable side effects.
///
/// Only diverging expressions (type `never` — e.g. `panic()`, `unreachable()`) are
/// considered side effects. Pure function calls like array construction are not.
fn expr_has_side_effects(expr: &TirExpr) -> bool {
    if expr.type_id == TypeTable::NEVER {
        return true;
    }
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            block_has_side_effects(block)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_side_effects(condition)
                || block_has_side_effects(then_branch)
                || else_branch.as_ref().is_some_and(block_has_side_effects)
        }
        TirExprKind::Match { expr, arms } => {
            expr_has_side_effects(expr)
                || arms.iter().any(|a| {
                    a.guard.as_ref().is_some_and(expr_has_side_effects)
                        || expr_has_side_effects(&a.body)
                })
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_side_effects(scrutinee)
                || arms.iter().any(block_has_side_effects)
                || block_has_side_effects(default)
        }
        _ => false,
    }
}

fn block_has_side_effects(block: &TirBlock) -> bool {
    block.stmts.iter().any(|stmt| match &stmt.kind {
        TirStmtKind::Expr(e) | TirStmtKind::Let { value: e, .. } => expr_has_side_effects(e),
        TirStmtKind::Return { value } => value.as_ref().is_some_and(expr_has_side_effects),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_has_side_effects(condition)
                || block_has_side_effects(then_block)
                || else_block.as_ref().is_some_and(block_has_side_effects)
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            block_has_side_effects(body)
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            expr_has_side_effects(scrutinee)
                || block_has_side_effects(then_block)
                || else_block.as_ref().is_some_and(block_has_side_effects)
        }
        TirStmtKind::Break { value, .. } => value.as_ref().is_some_and(expr_has_side_effects),
        TirStmtKind::Continue | TirStmtKind::TaskReturn { .. } => false,
        TirStmtKind::LetPattern { value, .. } => expr_has_side_effects(value),
    })
}

fn remove_dead_global_sets_stmt(stmt: &mut TirStmt, used: &IndexSet<(String, String)>) {
    match &mut stmt.kind {
        TirStmtKind::Expr(expr) | TirStmtKind::Let { value: expr, .. } => {
            remove_dead_global_sets_expr(expr, used);
        }
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            remove_dead_global_sets_block(then_block, used);
            if let Some(else_blk) = else_block {
                remove_dead_global_sets_block(else_blk, used);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            remove_dead_global_sets_block(body, used);
        }
        TirStmtKind::IfPattern {
            then_block,
            else_block,
            ..
        } => {
            remove_dead_global_sets_block(then_block, used);
            if let Some(else_blk) = else_block {
                remove_dead_global_sets_block(else_blk, used);
            }
        }
        TirStmtKind::Return { value } => {
            if let Some(expr) = value {
                remove_dead_global_sets_expr(expr, used);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(expr) = value {
                remove_dead_global_sets_expr(expr, used);
            }
        }
        TirStmtKind::Continue | TirStmtKind::TaskReturn { .. } | TirStmtKind::LetPattern { .. } => {
        }
    }
}

/// Recursively remove dead `GlobalVarSet` from expressions that contain blocks.
fn remove_dead_global_sets_expr(expr: &mut TirExpr, used: &IndexSet<(String, String)>) {
    match &mut expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            remove_dead_global_sets_block(block, used);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            remove_dead_global_sets_expr(condition, used);
            remove_dead_global_sets_block(then_branch, used);
            if let Some(else_blk) = else_branch {
                remove_dead_global_sets_block(else_blk, used);
            }
        }
        TirExprKind::Closure { body, .. } => {
            remove_dead_global_sets_expr(body, used);
        }
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            remove_dead_global_sets_expr(scrutinee, used);
            for arm in arms {
                remove_dead_global_sets_expr(&mut arm.body, used);
            }
        }
        TirExprKind::Switch { arms, default, .. } => {
            for arm in arms {
                remove_dead_global_sets_block(arm, used);
            }
            remove_dead_global_sets_block(default, used);
        }
        _ => {}
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Constant branch pruning
// ──────────────────────────────────────────────────────────────────────────────

/// Eliminate dead branches where the `if` condition is a compile-time boolean literal.
///
/// **Statement-level (`TirStmtKind::If`):**
/// - `if true  { A } [else { B }]` → inline A's statements
/// - `if false { A }`              → remove entirely
/// - `if false { A } else { B }`   → inline B's statements
///
/// **Expression-level (`TirExprKind::If`):**
/// - `if true  { A } [else { B }]` → `Block(A)`
/// - `if false { A }`              → `Unit`
/// - `if false { A } else { B }`   → `Block(B)`
pub fn prune_constant_branches(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= prune_branches_in_function(&mut func);
        }
    }
    changed
}

fn prune_branches_in_function(func: &mut TirFunction) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };
    prune_branches_in_block(body)
}

fn prune_branches_in_block(block: &mut TirBlock) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= prune_branches_in_stmt(stmt);
    }
    // Eliminate dead statements: constant `if`, empty labeled blocks.
    changed |= eliminate_dead_stmts(block);
    changed
}

fn prune_branches_in_stmt(stmt: &mut TirStmt) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => prune_branches_in_expr(value),
        TirStmtKind::Expr(expr) => prune_branches_in_expr(expr),
        TirStmtKind::Return { value } => value.as_mut().is_some_and(prune_branches_in_expr),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = prune_branches_in_expr(condition);
            changed |= prune_branches_in_block(then_block);
            if let Some(eb) = else_block {
                changed |= prune_branches_in_block(eb);
            }
            changed
        }
        TirStmtKind::Loop { body } => prune_branches_in_block(body),
        TirStmtKind::LabeledBlock { block, .. } => prune_branches_in_block(block),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = prune_branches_in_expr(scrutinee);
            changed |= prune_branches_in_block(then_block);
            if let Some(eb) = else_block {
                changed |= prune_branches_in_block(eb);
            }
            changed
        }
        TirStmtKind::Break { value, .. } => value.as_mut().is_some_and(prune_branches_in_expr),
        TirStmtKind::Continue => false,
        TirStmtKind::LetPattern { value, .. } => prune_branches_in_expr(value),
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn prune_branches_in_expr(expr: &mut TirExpr) -> bool {
    let mut changed = false;

    // Recurse into sub-expressions first (bottom-up)
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= prune_branches_in_expr(left);
            changed |= prune_branches_in_expr(right);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            changed |= prune_branches_in_expr(inner);
        }
        TirExprKind::Assign { target, value } => {
            changed |= prune_branches_in_expr(target);
            changed |= prune_branches_in_expr(value);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            changed |= prune_branches_in_expr(inner);
            changed |= prune_branches_in_expr(index);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= prune_branches_in_expr(arg);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= prune_branches_in_expr(receiver);
            for arg in args {
                changed |= prune_branches_in_expr(arg);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            changed |= prune_branches_in_expr(callee);
            for arg in args {
                changed |= prune_branches_in_expr(arg);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= prune_branches_in_expr(functor);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            changed |= prune_branches_in_block(block);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= prune_branches_in_expr(condition);
            changed |= prune_branches_in_block(then_branch);
            if let Some(eb) = else_branch {
                changed |= prune_branches_in_block(eb);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= prune_branches_in_expr(inner);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= prune_branches_in_expr(guard);
                }
                changed |= prune_branches_in_expr(&mut arm.body);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= prune_branches_in_expr(&mut field.value);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                changed |= prune_branches_in_expr(elem);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= prune_branches_in_expr(p);
            }
        }
        TirExprKind::Closure { body, .. } => {
            changed |= prune_branches_in_expr(body);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= prune_branches_in_expr(value);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= prune_branches_in_expr(scrutinee);
            for arm in arms {
                changed |= prune_branches_in_block(arm);
            }
            changed |= prune_branches_in_block(default);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }

    // Prune expression-level `if` with constant boolean condition.
    if let TirExprKind::If { condition, .. } = &expr.kind
        && let TirExprKind::BoolLiteral(value) = condition.kind
    {
        let TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } = std::mem::replace(&mut expr.kind, TirExprKind::Unit)
        else {
            unreachable!();
        };
        if value {
            expr.kind = TirExprKind::Block(then_branch);
        } else if let Some(else_blk) = else_branch {
            expr.kind = TirExprKind::Block(else_blk);
        }
        // false without else: type is Unit, TirExprKind::Unit is already set
        changed = true;
    }

    // Simplify `{ expr; }` → `expr` (single-expression unlabeled block)
    if let TirExprKind::Block(block) = &expr.kind
        && block.stmts.len() == 1
        && let TirStmtKind::Expr(_) = &block.stmts[0].kind
    {
        let TirExprKind::Block(block) = std::mem::replace(&mut expr.kind, TirExprKind::Unit) else {
            unreachable!();
        };
        let mut stmts = block.stmts;
        let TirStmtKind::Expr(inner) = stmts.remove(0).kind else {
            unreachable!();
        };
        *expr = inner;
        changed = true;
    }

    // Simplify `label: { break label: val; }` → `val`
    // This pattern arises from inlining `return expr;` → `break label: expr;`
    if let TirExprKind::LabeledBlock { label, block, .. } = &expr.kind
        && block.stmts.len() == 1
        && let TirStmtKind::Break {
            label: Some(brk_label),
            ..
        } = &block.stmts[0].kind
        && brk_label == label
    {
        let TirExprKind::LabeledBlock { block, .. } =
            std::mem::replace(&mut expr.kind, TirExprKind::Unit)
        else {
            unreachable!();
        };
        let mut stmts = block.stmts;
        let TirStmtKind::Break { value, .. } = stmts.remove(0).kind else {
            unreachable!();
        };
        if let Some(inner) = value {
            *expr = inner;
        }
        // else: break without value → Unit is already set
        changed = true;
    }

    // Simplify `[label:] { }` → `()` (empty block, with or without label)
    if matches!(&expr.kind, TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } if b.stmts.is_empty())
    {
        expr.kind = TirExprKind::Unit;
        changed = true;
    }

    changed
}

/// Eliminate dead statements from a block:
/// - `if true { A } [else { B }]` → inline A's statements
/// - `if false { A }` → remove
/// - `if false { A } else { B }` → inline B's statements
/// - `label: { }` (empty labeled block) → remove
/// - `label: { stmts }` (unused label) → flatten stmts into parent
fn eliminate_dead_stmts(block: &mut TirBlock) -> bool {
    let dominated = |s: &TirStmt| {
        matches!(
            &s.kind,
            TirStmtKind::If { condition, .. }
                if matches!(condition.kind, TirExprKind::BoolLiteral(_))
        ) || matches!(
            &s.kind,
            TirStmtKind::LabeledBlock { label, block }
                if block.stmts.is_empty() || !block_has_break_to(label, block)
        ) || matches!(
            &s.kind,
            TirStmtKind::Expr(e) if matches!(e.kind, TirExprKind::Unit | TirExprKind::Block(_))
        ) || matches!(
            &s.kind,
            TirStmtKind::Expr(e) if matches!(&e.kind, TirExprKind::LabeledBlock { label, block, .. } if !block_has_break_to(label, block))
        )
    };
    if !block.stmts.iter().any(dominated) {
        return false;
    }

    let old_stmts = std::mem::take(&mut block.stmts);
    for stmt in old_stmts {
        // Constant `if` → inline taken branch or drop
        if let TirStmtKind::If { ref condition, .. } = stmt.kind
            && let TirExprKind::BoolLiteral(value) = condition.kind
        {
            let TirStmtKind::If {
                then_block,
                else_block,
                ..
            } = stmt.kind
            else {
                unreachable!();
            };
            if value {
                block.stmts.extend(then_block.stmts);
            } else if let Some(else_blk) = else_block {
                block.stmts.extend(else_blk.stmts);
            }
            continue;
        }
        // Labeled block with unused label → flatten stmts into parent
        if let TirStmtKind::LabeledBlock {
            ref label,
            block: ref inner,
        } = stmt.kind
            && !block_has_break_to(label, inner)
        {
            let TirStmtKind::LabeledBlock { block: inner, .. } = stmt.kind else {
                unreachable!();
            };
            block.stmts.extend(inner.stmts);
            continue;
        }
        // Unit expression → drop (side-effect free)
        if let TirStmtKind::Expr(e) = &stmt.kind
            && matches!(e.kind, TirExprKind::Unit)
        {
            continue;
        }
        // Void block expression → flatten stmts into parent.
        // Handles both `Expr(Block { ... })` and `Expr(LabeledBlock { unused label, ... })`.
        // The latter arises from inlining void functions whose label has no break.
        if let TirStmtKind::Expr(e) = &stmt.kind
            && matches!(e.kind, TirExprKind::Block(_))
        {
            let TirStmtKind::Expr(e) = stmt.kind else {
                unreachable!();
            };
            let TirExprKind::Block(inner) = e.kind else {
                unreachable!();
            };
            block.stmts.extend(inner.stmts);
            continue;
        }
        if let TirStmtKind::Expr(e) = &stmt.kind
            && matches!(&e.kind, TirExprKind::LabeledBlock { label, block, .. } if !block_has_break_to(label, block))
        {
            let TirStmtKind::Expr(e) = stmt.kind else {
                unreachable!();
            };
            let TirExprKind::LabeledBlock { block: inner, .. } = e.kind else {
                unreachable!();
            };
            block.stmts.extend(inner.stmts);
            continue;
        }
        block.stmts.push(stmt);
    }
    true
}

/// Check if any `break` statement in the block targets the given label.
fn block_has_break_to(label: &str, block: &TirBlock) -> bool {
    block.stmts.iter().any(|s| stmt_has_break_to(label, s))
}

fn stmt_has_break_to(label: &str, stmt: &TirStmt) -> bool {
    match &stmt.kind {
        TirStmtKind::Break { label: Some(l), .. } => l == label,
        TirStmtKind::Let { value, .. } | TirStmtKind::LetPattern { value, .. } => {
            expr_has_break_to(label, value)
        }
        TirStmtKind::Expr(expr) => expr_has_break_to(label, expr),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_has_break_to(label, condition)
                || block_has_break_to(label, then_block)
                || else_block
                    .as_ref()
                    .is_some_and(|b| block_has_break_to(label, b))
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            block_has_break_to(label, body)
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            expr_has_break_to(label, scrutinee)
                || block_has_break_to(label, then_block)
                || else_block
                    .as_ref()
                    .is_some_and(|b| block_has_break_to(label, b))
        }
        TirStmtKind::Return { value } => {
            value.as_ref().is_some_and(|v| expr_has_break_to(label, v))
        }
        _ => false,
    }
}

fn expr_has_break_to(label: &str, expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            block_has_break_to(label, block)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_break_to(label, condition)
                || block_has_break_to(label, then_branch)
                || else_branch
                    .as_ref()
                    .is_some_and(|b| block_has_break_to(label, b))
        }
        TirExprKind::Match { expr, arms } => {
            expr_has_break_to(label, expr)
                || arms.iter().any(|arm| expr_has_break_to(label, &arm.body))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_fn(name: &str) -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["test"], name))
    }

    #[test]
    fn test_empty_reachable_set() {
        let call_graph = IndexMap::new();
        let entry = free_fn("run");
        let reachable = compute_reachable(&call_graph, &entry);
        assert!(reachable.contains(&free_fn("run")));
        assert_eq!(reachable.len(), 1);
    }

    #[test]
    fn test_transitive_reachability() {
        let mut call_graph = IndexMap::new();
        call_graph.insert(free_fn("run"), IndexSet::from([free_fn("foo")]));
        call_graph.insert(free_fn("foo"), IndexSet::from([free_fn("bar")]));
        call_graph.insert(free_fn("bar"), IndexSet::new());
        call_graph.insert(free_fn("unused"), IndexSet::from([free_fn("bar")]));

        let reachable = compute_reachable(&call_graph, &free_fn("run"));
        assert!(reachable.contains(&free_fn("run")));
        assert!(reachable.contains(&free_fn("foo")));
        assert!(reachable.contains(&free_fn("bar")));
        assert!(!reachable.contains(&free_fn("unused")));
    }
}
