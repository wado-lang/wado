//! Dead Code Elimination (DCE) for Wado TIR
//!
//! This module provides function-level dead code elimination through reachability analysis.
//! It starts from the entry point and traces all reachable functions via the call graph.

use crate::ast::Type;
use crate::component_model::WasiRegistry;
use crate::name::{FreeFunctionName, FunctionId, LocalMethodName, MethodName, ModuleSource};
use crate::optimize::CanonBuiltin;
use crate::project::Project;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirModule,
    TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

/// Call graph: function ID -> set of called function IDs
type CallGraph = HashMap<FunctionId, HashSet<FunctionId>>;

/// Effect usage: function ID -> set of (`effect_name`, `operation_name`) pairs
type EffectUsageMap = HashMap<FunctionId, HashSet<(String, String)>>;

/// Per-function box primitives usage
type BoxPrimitivesMap = HashMap<FunctionId, HashSet<PrimitiveType>>;

/// Analysis results for a single function
#[derive(Debug, Clone, Default)]
struct FunctionAnalysis {
    /// Functions called by this function
    callees: HashSet<FunctionId>,
    /// Effect calls: (`effect_name`, `op_name`)
    effect_calls: HashSet<(String, String)>,
    /// Primitive types that need box types (for references like &i32, &mut f64)
    used_box_primitives: HashSet<PrimitiveType>,
}

/// Analyze the project and populate its usage fields with DCE analysis results.
///
/// This performs dead code elimination analysis starting from the entry point
/// and populates the project's `reachable_functions`, `used_wasi_functions`,
/// `used_builtins`, etc. fields.
pub fn analyze_project(project: &mut Project) {
    // Build call graph, effect usage, and box primitives from all modules
    let (call_graph, effect_usage, box_primitives_map) = build_analysis_graph(&project.tir_modules);

    // Find entry function (run in entry module)
    let entry_func = FunctionId::Free(FreeFunctionName::from_module_source(
        &project.entry_module_source,
        "run",
    ));

    // Compute reachable functions from entry point
    let mut reachable = compute_reachable(&call_graph, &entry_func);

    // Add test functions as additional entry points
    // Test functions are also roots for reachability analysis
    if let Some(entry_module) = project.tir_modules.get(&project.entry_module_source) {
        for test in &entry_module.tests {
            let test_func = FunctionId::Free(FreeFunctionName::from_module_source(
                &project.entry_module_source,
                &test.function_name,
            ));
            let test_reachable = compute_reachable(&call_graph, &test_func);
            reachable.extend(test_reachable);
        }
    }

    // Collect used WASI functions and box primitives from reachable functions
    let mut used_wasi_functions: HashSet<String> = HashSet::new();
    let mut used_box_primitives: HashSet<PrimitiveType> = HashSet::new();
    for func_id in &reachable {
        if let Some(effects) = effect_usage.get(func_id) {
            for (effect_name, op_name) in effects {
                used_wasi_functions.insert(format!("{effect_name}::{op_name}"));
            }
        }
        if let Some(prims) = box_primitives_map.get(func_id) {
            used_box_primitives.extend(prims.iter().copied());
        }
    }

    // Helper to check if a core/internal function is reachable
    let core_internal = |name: &str| -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["core", "internal"], name))
    };

    // Helper to check if a core/builtin function is reachable
    let core_builtin = |name: &str| -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["core", "builtin"], name))
    };

    // Derive builtin usage from reachable internal functions
    // f64_to_string/f32_to_string call the bundled f64_to_buffer/f32_to_buffer
    let needs_f64_to_buffer = reachable.contains(&core_internal("f64_to_string"));
    let needs_f32_to_buffer = reachable.contains(&core_internal("f32_to_string"));

    // Add CM converter functions based on WASI function return types
    // These conversion functions are called from codegen, not Wado code
    // We need to compute transitive closure to include all functions they call
    let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
    let mut needs_list_string_converter = false;
    let mut needs_list_tuple_string_converter = false;
    let mut needs_option_string_converter = false;

    for func_name in &used_wasi_functions {
        if let Some(func_info) = wasi_registry.get_function(func_name)
            && let Some(return_type) = &func_info.return_type
        {
            match return_type {
                // Array<String> -> cm_list_string_to_array
                Type::Generic(g) if g.name == "Array" && g.args.len() == 1 => {
                    if matches!(&g.args[0], Type::Named(n) if n.name == "String") {
                        needs_list_string_converter = true;
                    }
                    // Array<[String, String]> -> cm_list_tuple_string_string_to_array
                    if let Type::Tuple(tuple_types) = &g.args[0]
                        && tuple_types.len() == 2
                        && matches!(&tuple_types[0], Type::Named(n) if n.name == "String")
                        && matches!(&tuple_types[1], Type::Named(n) if n.name == "String")
                    {
                        needs_list_tuple_string_converter = true;
                    }
                }
                // Option<String> -> cm_option_string_to_option + copy_string_from_linear
                Type::Generic(g) if g.name == "Option" && g.args.len() == 1 => {
                    if matches!(&g.args[0], Type::Named(n) if n.name == "String") {
                        needs_option_string_converter = true;
                    }
                }
                _ => {}
            }
        }
    }

    // Add converter functions and their transitive dependencies
    if needs_list_string_converter {
        let cm_list_func = core_internal("cm_list_string_to_array");
        reachable.extend(compute_reachable(&call_graph, &cm_list_func));
    }
    if needs_list_tuple_string_converter {
        let cm_list_func = core_internal("cm_list_tuple_string_string_to_array");
        reachable.extend(compute_reachable(&call_graph, &cm_list_func));
    }
    if needs_option_string_converter {
        let cm_option_func = core_internal("cm_option_string_to_option");
        let copy_string_func = core_internal("copy_string_from_linear");
        reachable.extend(compute_reachable(&call_graph, &cm_option_func));
        reachable.extend(compute_reachable(&call_graph, &copy_string_func));
    }

    // Note: array_copy_string is tracked via call graph analysis
    // It will be included if called from reachable user code

    // Check if stream intrinsics are needed by looking for:
    // 1. Stdout/Stderr effects being used
    // 2. Any builtin stream_* functions being called (for ambient logging)
    // 3. Any builtin call_indirect_* functions (ambient effect calls)
    let is_builtin_func = |f: &FreeFunctionName| {
        // New format: module_path == ["core", "builtin"]
        (f.module_path.len() == 2 && f.module_path[0] == "core" && f.module_path[1] == "builtin")
            // Legacy format: name starts with "builtin::"
            || f.name.starts_with("builtin::")
    };
    let is_builtin_stream = |f: &FreeFunctionName| {
        if is_builtin_func(f) {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            name.starts_with("stream_")
        } else {
            false
        }
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

    let uses_stream_builtins = reachable.iter().any(|func_id| {
        if let FunctionId::Free(f) = func_id {
            is_builtin_stream(f)
                || is_builtin_call_indirect_stdout(f)
                || is_builtin_call_indirect_stderr(f)
        } else {
            false
        }
    });

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

    // Compute precise builtin usage based on reachable builtin function calls
    let mut used_builtins: HashSet<CanonBuiltin> = HashSet::new();

    // Map builtin function names to canonical CanonBuiltin variants
    for func_id in &reachable {
        if let FunctionId::Free(f) = func_id
            && is_builtin_func(f)
        {
            let name = f.name.strip_prefix("builtin::").unwrap_or(&f.name);
            match name {
                "stream_new" => {
                    used_builtins.insert(CanonBuiltin::StreamNew);
                }
                "stream_write" => {
                    used_builtins.insert(CanonBuiltin::StreamWrite);
                }
                "stream_drop_writable" => {
                    used_builtins.insert(CanonBuiltin::StreamDropWritable);
                }
                "stream_drop_readable" => {
                    used_builtins.insert(CanonBuiltin::StreamDropReadable);
                }
                // Ambient logging builtins also need stream intrinsics
                n if n.starts_with("call_indirect_stdout")
                    || n.starts_with("call_indirect_stderr") =>
                {
                    used_builtins.insert(CanonBuiltin::StreamNew);
                    used_builtins.insert(CanonBuiltin::StreamWrite);
                    used_builtins.insert(CanonBuiltin::StreamDropWritable);
                }
                _ => {}
            }
        }
    }

    // realloc is always needed for memory management
    used_builtins.insert(CanonBuiltin::Realloc);

    // Float-to-string builtins if their internal wrappers are used
    if needs_f64_to_buffer {
        used_builtins.insert(CanonBuiltin::F64ToBuffer);
    }
    if needs_f32_to_buffer {
        used_builtins.insert(CanonBuiltin::F32ToBuffer);
    }

    // Effect usage requires TaskReturn for async entry point
    // But waitable-set builtins are only needed when effect_wait is actually called
    if !used_wasi_functions.is_empty() || uses_stream_builtins {
        // TaskReturn is always needed for async exports
        used_builtins.insert(CanonBuiltin::TaskReturn);

        // Waitable-set builtins only needed when effect_wait is called
        // effect_wait is used by ambient logging functions (log_stdout, log_stderr)
        // but NOT by regular println/eprintln which don't wait for completion
        if reachable.contains(&core_builtin("effect_wait")) {
            for builtin in CanonBuiltin::WAITABLE_SET {
                used_builtins.insert(*builtin);
            }
        }
    }

    // Apply results to project
    project.reachable_functions = reachable.clone();
    project.all_reachable = false;
    project.used_wasi_functions = used_wasi_functions;
    project.used_builtins = used_builtins;
    project.used_box_primitives = used_box_primitives;

    // Filter string literals in each module to only include strings from reachable functions
    for module in project.tir_modules.values_mut() {
        let module_path = module.module_source.to_path();
        let mut reachable_strings: Vec<String> = Vec::new();

        for (func_name, strings) in &module.function_strings {
            // Build function ID(s) to check if it's reachable
            // Note: monomorphized methods are tracked as FunctionId::Free in the call graph
            // but their names look like methods (e.g., "TreeMap<String,i32>^Index::index")
            let is_reachable = if let Some(parsed) = LocalMethodName::parse(func_name) {
                // Method name like "Point::sum" or "Point^Trait::method"
                // Check as MethodName first (for non-monomorphized methods)
                let method_id = FunctionId::Method(MethodName::new(
                    module_path.join("/"),
                    parsed.struct_name.clone(),
                    parsed.trait_name.clone(),
                    parsed.method_name.clone(),
                ));
                if reachable.contains(&method_id) {
                    true
                } else {
                    // For monomorphized methods, also check as FreeFunctionName
                    // Monomorphized methods have type args in the struct name (e.g., "TreeMap<String,i32>")
                    let free_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                        &module_path,
                        func_name,
                    ));
                    reachable.contains(&free_id)
                }
            } else {
                // Regular function
                let func_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                    &module_path,
                    func_name,
                ));
                reachable.contains(&func_id)
            };

            if is_reachable {
                for s in strings {
                    if !reachable_strings.contains(s) {
                        reachable_strings.push(s.clone());
                    }
                }
            }
        }

        module.string_literals = reachable_strings;
    }
}

/// Populate project with all features enabled (no DCE, for O0 mode).
pub fn populate_all_features(project: &mut Project) {
    use PrimitiveType::{F32, F64, I32, I64};

    project.reachable_functions = HashSet::new();
    project.all_reachable = true;
    // Standard WASI functions from the stdlib registry
    let (wasi_registry, _world_registry) = WasiRegistry::build_from_stdlib();
    project.used_wasi_functions = wasi_registry
        .standard_function_names()
        .map(std::string::ToString::to_string)
        .collect();
    // All importable builtins when DCE is disabled
    project.used_builtins = CanonBuiltin::ALL.iter().copied().collect();
    // All primitives that map to box types when DCE is disabled
    project.used_box_primitives = HashSet::from([I32, I64, F32, F64]);
}

/// Build call graph and effect usage from all TIR modules
/// Returns (`call_graph`, `effect_usage`, `box_primitives_map`)
fn build_analysis_graph(
    modules: &IndexMap<ModuleSource, TirModule>,
) -> (CallGraph, EffectUsageMap, BoxPrimitivesMap) {
    let mut call_graph: CallGraph = HashMap::new();
    let mut effect_usage: EffectUsageMap = HashMap::new();
    let mut box_primitives_map: BoxPrimitivesMap = HashMap::new();

    for (module_source, module) in modules {
        let type_table = &*module.type_table.borrow();
        let path = module_source.to_path();

        // Analyze functions (including methods stored as functions)
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            // Use the TirFunction's is_method() to determine if this is a method
            let func_id = if let Some(ref info) = func.method_info {
                // This is a method - use MethodName or FreeFunctionName with monomorph info
                if let Some(monomorph_info) = &func.monomorph_info {
                    // Monomorphized method - use FreeFunctionName with metadata
                    FunctionId::Free(FreeFunctionName::with_monomorph_info(
                        path.clone(),
                        func.name.clone(),
                        monomorph_info.generic_name.clone(),
                    ))
                } else {
                    // Non-monomorphized method - use method_info
                    FunctionId::Method(MethodName::new(
                        path.join("/"),
                        info.struct_name.clone(),
                        info.trait_name.clone(),
                        info.method_name.clone(),
                    ))
                }
            } else {
                // Regular function - use FreeFunctionName
                if let Some(monomorph_info) = &func.monomorph_info {
                    FunctionId::Free(FreeFunctionName::with_monomorph_info(
                        path.clone(),
                        func.name.clone(),
                        monomorph_info.generic_name.clone(),
                    ))
                } else {
                    FunctionId::Free(FreeFunctionName::from_path_and_name(&path, &func.name))
                }
            };
            let analysis = analyze_function(&func, &path, type_table);
            call_graph.insert(func_id.clone(), analysis.callees);
            if !analysis.effect_calls.is_empty() {
                effect_usage.insert(func_id.clone(), analysis.effect_calls);
            }
            if !analysis.used_box_primitives.is_empty() {
                box_primitives_map.insert(func_id, analysis.used_box_primitives);
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
                    path.join("/"),
                    struct_name.clone(),
                    None,
                    method.name.clone(),
                ));
                let analysis = analyze_function(method, &path, type_table);
                call_graph.insert(method_id.clone(), analysis.callees);
                if !analysis.effect_calls.is_empty() {
                    effect_usage.insert(method_id.clone(), analysis.effect_calls);
                }
                if !analysis.used_box_primitives.is_empty() {
                    box_primitives_map.insert(method_id, analysis.used_box_primitives);
                }
            }
        }
    }

    (call_graph, effect_usage, box_primitives_map)
}

/// Analyze a TIR function for callees and effect usage
fn analyze_function(
    func: &TirFunction,
    current_module: &[String],
    type_table: &TypeTable,
) -> FunctionAnalysis {
    let mut analysis = FunctionAnalysis::default();

    // Check parameters for references to primitives (e.g., &i32, &mut f64)
    for param in &func.params {
        if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
            type_table.get(param.type_id)
            && let ResolvedType::Primitive(prim) = type_table.get(*inner)
        {
            analysis.used_box_primitives.insert(*prim);
        }
    }

    if let Some(body) = &func.body {
        analyze_block(body, current_module, type_table, &mut analysis);
    }
    analysis
}

fn analyze_block(
    block: &TirBlock,
    current_module: &[String],
    type_table: &TypeTable,
    analysis: &mut FunctionAnalysis,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let { value, type_id, .. } => {
                analyze_expr(value, current_module, type_table, analysis);
                // Check locals for references to primitives (e.g., let r: &i32 = ...)
                if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    type_table.get(*type_id)
                    && let ResolvedType::Primitive(prim) = type_table.get(*inner)
                {
                    analysis.used_box_primitives.insert(*prim);
                }
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
            TirStmtKind::While { condition, body } => {
                analyze_expr(condition, current_module, type_table, analysis);
                analyze_block(body, current_module, type_table, analysis);
            }
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for init_stmt in init {
                    if let TirStmtKind::Let { value, .. } = &init_stmt.kind {
                        analyze_expr(value, current_module, type_table, analysis);
                    }
                }
                if let Some(cond) = condition {
                    analyze_expr(cond, current_module, type_table, analysis);
                }
                analyze_block(body, current_module, type_table, analysis);
                if let Some(upd) = update {
                    analyze_expr(upd, current_module, type_table, analysis);
                }
            }
            TirStmtKind::Loop { body } => {
                analyze_block(body, current_module, type_table, analysis);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                analyze_expr(iterable, current_module, type_table, analysis);
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
            TirStmtKind::WhilePattern {
                scrutinee, body, ..
            } => {
                analyze_expr(scrutinee, current_module, type_table, analysis);
                analyze_block(body, current_module, type_table, analysis);
            }
            TirStmtKind::ForPattern {
                init,
                scrutinee,
                body,
                update,
                ..
            } => {
                for init_stmt in init {
                    if let TirStmtKind::Let { value, .. } = &init_stmt.kind {
                        analyze_expr(value, current_module, type_table, analysis);
                    }
                }
                analyze_expr(scrutinee, current_module, type_table, analysis);
                analyze_block(body, current_module, type_table, analysis);
                if let Some(upd) = update {
                    analyze_expr(upd, current_module, type_table, analysis);
                }
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    analyze_expr(v, current_module, type_table, analysis);
                }
            }
            TirStmtKind::Continue => {}
        }
    }
}

fn analyze_expr(
    expr: &TirExpr,
    current_module: &[String],
    type_table: &TypeTable,
    analysis: &mut FunctionAnalysis,
) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            let module_path = func.module_path();
            let func_name = func.name();

            // Invariant: TirExprKind::Call should never have method names (containing "::")
            // Methods use TirExprKind::MethodCall instead. The only exception is "builtin::*".
            debug_assert!(
                !func_name.contains("::") || func_name.starts_with("builtin::"),
                "TirExprKind::Call should not have method-style names: {func_name}"
            );

            // Build function ID for the called function
            let callee_path = if module_path.is_empty() {
                current_module
            } else {
                module_path.as_slice()
            };
            let callee_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                callee_path,
                &func_name,
            ));
            analysis.callees.insert(callee_id);

            // Detect effect calls: single-element module_path with PascalCase name
            // (e.g., ["Stdout"], ["Stderr"], ["MonotonicClock"])
            // Effect calls are represented as Call in TIR, not as EffectCall
            if module_path.len() == 1 {
                let potential_effect = &module_path[0];
                // Check if it looks like an effect name (starts with uppercase, no file path chars)
                if potential_effect
                    .chars()
                    .next()
                    .is_some_and(|c: char| c.is_ascii_uppercase())
                    && !potential_effect.contains('/')
                    && !potential_effect.contains('.')
                {
                    analysis
                        .effect_calls
                        .insert((potential_effect.clone(), func_name.clone()));

                    // Terminal effects return Option<i32> which needs box_i32 for Some case
                    if (potential_effect == "TerminalStdin"
                        || potential_effect == "TerminalStdout"
                        || potential_effect == "TerminalStderr")
                        && matches!(
                            func_name.as_str(),
                            "get_terminal_stdin" | "get_terminal_stdout" | "get_terminal_stderr"
                        )
                    {
                        analysis.used_box_primitives.insert(PrimitiveType::I32);
                    }
                }
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

                // Use empty path because monomorphized functions are added
                // to the entry module, not the original struct's module
                let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    vec![],
                    func_name.clone(),
                    base_name,
                ));
                analysis.callees.insert(callee_id);
            } else {
                // Non-monomorphized method - determine target from receiver type
                // First strip any reference wrappers to get the base type
                let mut current_type = type_table.get(receiver.type_id);
                while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = current_type {
                    current_type = type_table.get(*inner);
                }
                let base_receiver_type = current_type.clone();

                // Extract method name and trait name from method_info
                let (method_name, trait_name) = if let Some(info) = func.method_info() {
                    (info.method_name.clone(), info.trait_name.clone())
                } else {
                    (func_name.clone(), None)
                };

                match base_receiver_type {
                    ResolvedType::Struct {
                        name,
                        is_monomorphized: true,
                        base_name: Some(base_struct),
                        ..
                    } => {
                        // Monomorphized struct method call - use FunctionId::Free
                        // Include trait name for trait methods
                        let mangled_func_name = if let Some(ref trait_n) = trait_name {
                            format!("{name}^{trait_n}::{method_name}")
                        } else {
                            format!("{name}::{method_name}")
                        };
                        // Build base method name using the original generic struct name
                        let base_method_name = if let Some(ref trait_n) = trait_name {
                            format!("{base_struct}^{trait_n}::{method_name}")
                        } else {
                            format!("{base_struct}::{method_name}")
                        };
                        // Use empty path because monomorphized functions are added
                        // to the entry module, not the original struct's module
                        let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                            vec![],
                            mangled_func_name,
                            base_method_name,
                        ));
                        analysis.callees.insert(callee_id);
                    }
                    ResolvedType::Struct {
                        name,
                        module_source,
                        is_monomorphized: false,
                        ..
                    } => {
                        // Regular struct method call - use FunctionId::Method
                        let method_id = FunctionId::Method(MethodName::new(
                            module_source.to_path().join("/"),
                            name.clone(),
                            trait_name.clone(),
                            method_name,
                        ));
                        analysis.callees.insert(method_id);
                    }
                    ResolvedType::Primitive(_) => {
                        // Primitive method call (e.g., i32.to_string())
                        if method_name == "to_string" {
                            add_to_string_callee(receiver.type_id, type_table, analysis);
                        }
                        // Other primitive methods are inline (no function call)
                    }
                    ResolvedType::GenericInstance {
                        name,
                        type_args,
                        module_source: _,
                    } => {
                        // Generic instance method call (e.g., Box<i32>.get())
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| mangle_type_for_name(*t, type_table))
                            .collect();
                        // Include trait name for trait methods (e.g., TreeMap<String,i32>^Index::index)
                        let (mangled_func_name, base_name) = if let Some(ref trait_n) = trait_name {
                            let mangled = format!(
                                "{}<{}>^{trait_n}::{method_name}",
                                name,
                                type_arg_names.join(",")
                            );
                            let base = format!("{name}^{trait_n}::{method_name}");
                            (mangled, base)
                        } else {
                            let mangled =
                                format!("{}<{}>::{method_name}", name, type_arg_names.join(","));
                            let base = format!("{name}::{method_name}");
                            (mangled, base)
                        };
                        let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                            vec![],
                            mangled_func_name,
                            base_name,
                        ));
                        analysis.callees.insert(callee_id);
                    }
                    ResolvedType::BuiltinArray(elem_type) => {
                        // Array<T> method call (e.g., arr.len(), arr.append())
                        let elem_name = mangle_type_for_name(elem_type, type_table);
                        // Include trait name for trait methods (e.g., Array<i32>^Index::index)
                        let (mangled_func_name, base_name) = if let Some(ref trait_n) = trait_name {
                            let mangled = format!("Array<{elem_name}>^{trait_n}::{method_name}");
                            let base = format!("Array^{trait_n}::{method_name}");
                            (mangled, base)
                        } else {
                            let mangled = format!("Array<{elem_name}>::{method_name}");
                            let base = format!("Array::{method_name}");
                            (mangled, base)
                        };
                        let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                            vec![],
                            mangled_func_name,
                            base_name,
                        ));
                        analysis.callees.insert(callee_id);
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
        TirExprKind::Unary { op, expr } => {
            analyze_expr(expr, current_module, type_table, analysis);
            // Track box types needed for references to primitives
            if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                && let ResolvedType::Primitive(prim) = type_table.get(expr.type_id)
            {
                analysis.used_box_primitives.insert(*prim);
            }
        }
        TirExprKind::Assign { target, value } => {
            analyze_expr(target, current_module, type_table, analysis);
            analyze_expr(value, current_module, type_table, analysis);
        }
        TirExprKind::Cast { expr, .. } => {
            analyze_expr(expr, current_module, type_table, analysis);
        }
        TirExprKind::EffectCall {
            effect_name,
            op_name,
            args,
            ..
        } => {
            // Track effect usage for WASI import DCE
            analysis
                .effect_calls
                .insert((effect_name.clone(), op_name.clone()));

            for arg in args {
                analyze_expr(arg, current_module, type_table, analysis);
            }
        }
        TirExprKind::StaticCall { func, args } => {
            let func_name = func.name();
            // Static method call - func_name already contains "StructName::method_name"
            // The function is registered as a free function with mangled name
            let callee_id = if func.is_monomorphized() {
                // Monomorphized functions are generated in the entry module (empty path)
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
                FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    vec![],
                    func_name.clone(),
                    base_name,
                ))
            } else {
                let module_path = func.module_path();
                let callee_path = if module_path.is_empty() {
                    current_module
                } else {
                    module_path.as_slice()
                };
                FunctionId::Free(FreeFunctionName::from_path_and_name(
                    callee_path,
                    &func_name,
                ))
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
                analyze_expr(&arm.body, current_module, type_table, analysis);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_expr(&field.value, current_module, type_table, analysis);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
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
        TirExprKind::OptionSome { value } => {
            analyze_expr(value, current_module, type_table, analysis);
            // Option<primitive> now uses box types for Some variant
            if let ResolvedType::Primitive(prim) = type_table.get(value.type_id) {
                analysis.used_box_primitives.insert(*prim);
            }
        }
        TirExprKind::VariantConstruct {
            variant_type,
            case_name,
            fields,
            ..
        } => {
            for field in fields {
                analyze_expr(field, current_module, type_table, analysis);
            }
            // Option::Some with primitive value needs box type
            if case_name == "Some"
                && fields.len() == 1
                && let ResolvedType::Primitive(prim) = type_table.get(fields[0].type_id)
            {
                analysis.used_box_primitives.insert(*prim);
            }
            // Generic variants (like Result<i32, String>) may need boxing for primitives
            // if the variant has heterogeneous field types (uses eqref).
            // To be safe, mark all primitive fields in generic variants as needing boxing.
            if let ResolvedType::GenericInstance { .. } = type_table.get(*variant_type) {
                for field in fields {
                    if let ResolvedType::Primitive(prim) = type_table.get(field.type_id) {
                        analysis.used_box_primitives.insert(*prim);
                    }
                }
            }
        }
        TirExprKind::Move { value } => {
            analyze_expr(value, current_module, type_table, analysis);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            analyze_block(block, current_module, type_table, analysis);
        }
        // Leaf nodes - no calls
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Add the appropriate `to_string` function call for a type
fn add_to_string_callee(type_id: TypeId, type_table: &TypeTable, analysis: &mut FunctionAnalysis) {
    let core_internal: &[&str] = &["core", "internal"];
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => {
            let func_name = match prim {
                PrimitiveType::I32 | PrimitiveType::I8 | PrimitiveType::I16 => "i32_to_string",
                PrimitiveType::U32 | PrimitiveType::U8 | PrimitiveType::U16 => "u32_to_string",
                PrimitiveType::I64 => "i64_to_string",
                PrimitiveType::U64 => "u64_to_string",
                PrimitiveType::F32 => "f32_to_string",
                PrimitiveType::F64 => "f64_to_string",
                PrimitiveType::Bool => "bool_to_string",
                PrimitiveType::Char => "char_to_string",
                _ => return,
            };
            analysis
                .callees
                .insert(FunctionId::Free(FreeFunctionName::from_strs(
                    core_internal,
                    func_name,
                )));
        }
        ResolvedType::Struct { name, .. } if name == "String" => {
            // String.to_string() is a no-op, no function call needed
        }
        _ => {}
    }
}

/// Mangle a type ID into a string suitable for struct/function names.
/// Used for creating monomorphized function names like Array<i32>`::len`.
fn mangle_type_for_name(type_id: TypeId, type_table: &TypeTable) -> String {
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => match prim {
            PrimitiveType::I8 => "i8".to_string(),
            PrimitiveType::I16 => "i16".to_string(),
            PrimitiveType::I32 => "i32".to_string(),
            PrimitiveType::I64 => "i64".to_string(),
            PrimitiveType::I128 => "i128".to_string(),
            PrimitiveType::U8 => "u8".to_string(),
            PrimitiveType::U16 => "u16".to_string(),
            PrimitiveType::U32 => "u32".to_string(),
            PrimitiveType::U64 => "u64".to_string(),
            PrimitiveType::U128 => "u128".to_string(),
            PrimitiveType::F32 => "f32".to_string(),
            PrimitiveType::F64 => "f64".to_string(),
            PrimitiveType::Bool => "bool".to_string(),
            PrimitiveType::Char => "char".to_string(),
        },
        ResolvedType::Unit => "unit".to_string(),
        ResolvedType::Struct { name, .. } => name.clone(),
        ResolvedType::GenericInstance {
            name, type_args, ..
        } => {
            let args: Vec<String> = type_args
                .iter()
                .map(|t| mangle_type_for_name(*t, type_table))
                .collect();
            format!("{}<{}>", name, args.join(","))
        }
        ResolvedType::Function {
            params,
            return_type,
            ..
        } => {
            let ret_name = mangle_type_for_name(*return_type, type_table);
            format!("Fn<{},{}>", params.len(), ret_name)
        }
        ResolvedType::Tuple(elems) => {
            let elem_names: Vec<String> = elems
                .iter()
                .map(|t| mangle_type_for_name(*t, type_table))
                .collect();
            format!("Tuple<{}>", elem_names.join(","))
        }
        ResolvedType::Option(inner) => {
            let inner_name = mangle_type_for_name(*inner, type_table);
            format!("Option<{inner_name}>")
        }
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            mangle_type_for_name(*inner, type_table)
        }
        _ => "unknown".to_string(),
    }
}

/// Compute the set of reachable functions from an entry point
fn compute_reachable(
    call_graph: &HashMap<FunctionId, HashSet<FunctionId>>,
    entry: &FunctionId,
) -> HashSet<FunctionId> {
    let mut reachable = HashSet::new();
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
    // Skip if all functions are reachable (no DCE)
    if project.all_reachable {
        return;
    }

    for (module_source, module) in &mut project.tir_modules {
        let module_path = module_source.to_path();
        // Retain only reachable functions
        module.functions.retain(|func_rc| {
            let func = func_rc.borrow();

            // Always retain test functions (they are entry points for the test runner)
            if func.name.starts_with("__test_") {
                return true;
            }
            // Use TirFunction's method_info to check if this is a method
            if let Some(ref info) = func.method_info {
                // Could be either:
                // - Instance method tracked as FunctionId::Method
                // - Static method tracked as FunctionId::Free with mangled name
                // Use method_info to build the method ID
                // Try as instance method (FunctionId::Method)
                let method_id = FunctionId::Method(MethodName::new(
                    module_path.join("/"),
                    info.struct_name.clone(),
                    info.trait_name.clone(),
                    info.method_name.clone(),
                ));
                if project.reachable_functions.contains(&method_id) {
                    return true;
                }

                // Try as static method (FunctionId::Free with mangled name)
                let free_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                    &module_path,
                    &func.name,
                ));
                if project.reachable_functions.contains(&free_id) {
                    return true;
                }

                // For monomorphized methods, also check with empty module_path (entry module)
                // Monomorphized functions are tracked in the call graph with module_path = []
                // regardless of which module they were generated in
                if func.monomorph_info.is_some() {
                    let entry_module_free_id =
                        FunctionId::Free(FreeFunctionName::from_path_and_name(&[], &func.name));
                    if project.reachable_functions.contains(&entry_module_free_id) {
                        return true;
                    }
                }

                // For generic methods/static methods, check if any monomorphized version is reachable
                // Generic functions are named "Array::with_capacity" but calls use "Array<i32>::with_capacity"
                // Check if any function ID in reachable_functions matches this base name
                is_generic_func_reachable(&project.reachable_functions, &module_path, &func.name)
            } else {
                // Regular function
                let func_id = FunctionId::Free(FreeFunctionName::from_path_and_name(
                    &module_path,
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
    reachable: &HashSet<FunctionId>,
    module_path: &[String],
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
            // For monomorphized functions, the actual function may be in a different module
            // (e.g., entry module []) than the original definition (e.g., ["core", "prelude"]).
            // So we relax the module path check for monomorphized names using metadata.
            let module_matches = free_name.module_path.as_slice() == module_path
                || (free_name.is_monomorphized && module_path.is_empty());

            if !module_matches {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn free_fn(name: &str) -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["test"], name))
    }

    #[test]
    fn test_empty_reachable_set() {
        let call_graph = HashMap::new();
        let entry = free_fn("run");
        let reachable = compute_reachable(&call_graph, &entry);
        assert!(reachable.contains(&free_fn("run")));
        assert_eq!(reachable.len(), 1);
    }

    #[test]
    fn test_transitive_reachability() {
        let mut call_graph = HashMap::new();
        call_graph.insert(free_fn("run"), HashSet::from([free_fn("foo")]));
        call_graph.insert(free_fn("foo"), HashSet::from([free_fn("bar")]));
        call_graph.insert(free_fn("bar"), HashSet::new());
        call_graph.insert(free_fn("unused"), HashSet::from([free_fn("bar")]));

        let reachable = compute_reachable(&call_graph, &free_fn("run"));
        assert!(reachable.contains(&free_fn("run")));
        assert!(reachable.contains(&free_fn("foo")));
        assert!(reachable.contains(&free_fn("bar")));
        assert!(!reachable.contains(&free_fn("unused")));
    }
}
