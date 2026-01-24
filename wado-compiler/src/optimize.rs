//! Optimization pass for Wado TIR
//!
//! This module provides:
//! - Dead Code Elimination (DCE) at function level
//! - Usage analysis for conditional feature inclusion
//! - Function inlining (via `optimize_inline` module)

use crate::ast::Type;
use crate::component_model::WasiRegistry;
use crate::name::{FreeFunctionName, FunctionId, LocalMethodName, MethodName, ModuleSource};
use crate::optimize_inline::inline_functions;
use crate::project::Project;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirModule, TirStmt,
    TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

/// Canonical builtin functions imported from wasi or env namespace
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanonBuiltin {
    // Stream intrinsics (wasi namespace)
    StreamNew,
    StreamWrite,
    StreamDropWritable,
    StreamDropReadable,
    // Async/task intrinsics (wasi namespace)
    TaskReturn,
    WaitableSetNew,
    WaitableJoin,
    WaitableSetWait,
    SubtaskDrop,
    // Env intrinsics (env namespace)
    Realloc,
    F64ToBuffer,
    F32ToBuffer,
}

impl CanonBuiltin {
    /// Parse canonical name from string
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "stream-new" => Some(Self::StreamNew),
            "stream-write" => Some(Self::StreamWrite),
            "stream-drop-writable" => Some(Self::StreamDropWritable),
            "stream-drop-readable" => Some(Self::StreamDropReadable),
            "task-return" => Some(Self::TaskReturn),
            "waitable-set-new" => Some(Self::WaitableSetNew),
            "waitable-join" => Some(Self::WaitableJoin),
            "waitable-set-wait" => Some(Self::WaitableSetWait),
            "subtask-drop" => Some(Self::SubtaskDrop),
            "realloc" => Some(Self::Realloc),
            "f64_to_buffer" => Some(Self::F64ToBuffer),
            "f32_to_buffer" => Some(Self::F32ToBuffer),
            _ => None,
        }
    }

    /// Get the canonical name (for wasm imports)
    pub fn canonical_name(&self) -> &'static str {
        match self {
            Self::StreamNew => "stream-new",
            Self::StreamWrite => "stream-write",
            Self::StreamDropWritable => "stream-drop-writable",
            Self::StreamDropReadable => "stream-drop-readable",
            Self::TaskReturn => "task-return",
            Self::WaitableSetNew => "waitable-set-new",
            Self::WaitableJoin => "waitable-join",
            Self::WaitableSetWait => "waitable-set-wait",
            Self::SubtaskDrop => "subtask-drop",
            Self::Realloc => "realloc",
            Self::F64ToBuffer => "f64_to_buffer",
            Self::F32ToBuffer => "f32_to_buffer",
        }
    }

    /// Check if this is a float-to-string conversion builtin
    pub fn is_float_to_string(&self) -> bool {
        matches!(self, Self::F64ToBuffer | Self::F32ToBuffer)
    }

    /// All importable builtins
    pub const ALL: &'static [CanonBuiltin] = &[
        CanonBuiltin::StreamNew,
        CanonBuiltin::StreamWrite,
        CanonBuiltin::StreamDropWritable,
        CanonBuiltin::StreamDropReadable,
        CanonBuiltin::TaskReturn,
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
        CanonBuiltin::Realloc,
        CanonBuiltin::F64ToBuffer,
        CanonBuiltin::F32ToBuffer,
    ];

    /// Async/task-related builtins
    pub const ASYNC: &'static [CanonBuiltin] = &[
        CanonBuiltin::TaskReturn,
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
    ];

    /// Waitable-set builtins (only needed when `effect_wait` is called)
    pub const WAITABLE_SET: &'static [CanonBuiltin] = &[
        CanonBuiltin::WaitableSetNew,
        CanonBuiltin::WaitableJoin,
        CanonBuiltin::WaitableSetWait,
        CanonBuiltin::SubtaskDrop,
    ];
}

/// Call graph: function ID -> set of called function IDs
type CallGraph = HashMap<FunctionId, HashSet<FunctionId>>;

/// Effect usage: function ID -> set of (`effect_name`, `operation_name`) pairs
type EffectUsageMap = HashMap<FunctionId, HashSet<(String, String)>>;

/// Optimization level for the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    /// No optimizations. Used for debugging.
    #[default]
    None,
    /// Baseline optimizations including DCE. Intended for development.
    Basic,
    /// All optimizations including inlining, decomposition, etc. (TBD).
    /// Intended for production (server-side).
    Full,
    /// Full optimizations plus name section stripping. Intended for frontend.
    Size,
}

// =============================================================================
// Function Inlining - see optimize_inline.rs
// =============================================================================

// Function inlining has been extracted to optimize_inline.rs
// The inline_functions function is imported and used directly.

// =============================================================================
// Strength Reduction (placeholder for future enhancement)
// =============================================================================

// Strength reduction optimization transforms patterns like:
//   for i in 0..n { let val = base + i * step; }
// to:
//   let mut acc = base; for i in 0..n { let val = acc; acc += step; }
//
// This eliminates the multiplication inside the loop.
// Currently not implemented - requires complex loop analysis.
// Potential patterns to target:
// - `base + counter * step` where counter increments by 1
// - Nested loops with induction variables

// =============================================================================
// Dead Code Elimination (DCE)
// =============================================================================

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
fn analyze_project(project: &mut Project) {
    // Build call graph, effect usage, and box primitives from all modules
    let (call_graph, effect_usage, box_primitives_map) = build_analysis_graph(&project.tir_modules);

    // Find entry function (run in entry module)
    let entry_func = FunctionId::Free(FreeFunctionName::from_module_source(
        &project.entry_module_source,
        "run",
    ));

    // Compute reachable functions from entry point
    let mut reachable = compute_reachable(&call_graph, &entry_func);

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
fn populate_all_features(project: &mut Project) {
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

/// Per-function box primitives usage
type BoxPrimitivesMap = HashMap<FunctionId, HashSet<PrimitiveType>>;

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
            let module_path = func.module_path();

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

                let callee_id = FunctionId::Free(FreeFunctionName::with_monomorph_info(
                    module_path,
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
                        module_source,
                        ..
                    } => {
                        // Check if this is a monomorphized struct (name contains "<")
                        // Monomorphized structs are registered as FunctionId::Free, not Method
                        if name.contains('<') {
                            // Monomorphized struct method call - use FunctionId::Free
                            // Include trait name for trait methods
                            let mangled_func_name = if let Some(ref trait_n) = trait_name {
                                format!("{name}^{trait_n}::{method_name}")
                            } else {
                                format!("{name}::{method_name}")
                            };
                            // Extract base generic name (e.g., "TreeMap" from "TreeMap<String,i32>")
                            let base_struct = name.split('<').next().unwrap_or(&name);
                            let base_name = if let Some(ref trait_n) = trait_name {
                                format!("{base_struct}^{trait_n}::{method_name}")
                            } else {
                                format!("{base_struct}::{method_name}")
                            };
                            let callee_id =
                                FunctionId::Free(FreeFunctionName::with_monomorph_info(
                                    module_source.to_path(),
                                    mangled_func_name,
                                    base_name,
                                ));
                            analysis.callees.insert(callee_id);
                        } else {
                            // Regular struct method call - use FunctionId::Method
                            let method_id = FunctionId::Method(MethodName::new(
                                module_source.to_path().join("/"),
                                name.clone(),
                                trait_name.clone(),
                                method_name,
                            ));
                            analysis.callees.insert(method_id);
                        }
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
            case_name, fields, ..
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
        | TirExprKind::Capture { .. } => {}
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

// =============================================================================
// Project-level optimization
// =============================================================================

/// Remove unreachable functions from the project's TIR modules.
///
/// This physically removes functions that are not in `reachable_functions`
/// from the TIR, so codegen doesn't need to filter them.
fn remove_unreachable_functions(project: &mut Project) {
    // Skip if all functions are reachable (no DCE)
    if project.all_reachable {
        return;
    }

    for (module_source, module) in &mut project.tir_modules {
        let module_path = module_source.to_path();
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

// =============================================================================
// Loop-Invariant Code Motion (LICM)
// =============================================================================

/// Collect all local variable indices that are modified (assigned) in a block.
fn collect_modified_vars_in_block(block: &TirBlock, modified: &mut HashSet<u32>) {
    for stmt in &block.stmts {
        collect_modified_vars_in_stmt(stmt, modified);
    }
}

/// Mark the underlying local variable as modified, traversing through field accesses.
/// This is used when taking a mutable reference to a field, e.g., `&mut self.items`.
fn mark_local_as_modified(expr: &TirExpr, modified: &mut HashSet<u32>) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            modified.insert(*index);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            mark_local_as_modified(inner, modified);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            // Handle cases like *ptr where ptr is a reference
            mark_local_as_modified(inner, modified);
        }
        _ => {}
    }
}

fn collect_modified_vars_in_stmt(stmt: &TirStmt, modified: &mut HashSet<u32>) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            // Let statements define new variables, mark them as modified
            // (they're not invariant within the loop where they're defined)
            modified.insert(*local_index);
            // Also check the value expression for mutable references
            collect_modified_vars_in_expr(value, modified);
        }
        TirStmtKind::Expr(expr) => {
            collect_modified_vars_in_expr(expr, modified);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_modified_vars_in_expr(v, modified);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_modified_vars_in_expr(condition, modified);
            collect_modified_vars_in_block(then_block, modified);
            if let Some(eb) = else_block {
                collect_modified_vars_in_block(eb, modified);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_modified_vars_in_expr(condition, modified);
            collect_modified_vars_in_block(body, modified);
        }
        TirStmtKind::For {
            init,
            condition,
            body,
            update,
        } => {
            for s in init {
                collect_modified_vars_in_stmt(s, modified);
            }
            if let Some(c) = condition {
                collect_modified_vars_in_expr(c, modified);
            }
            collect_modified_vars_in_block(body, modified);
            if let Some(u) = update {
                collect_modified_vars_in_expr(u, modified);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_modified_vars_in_block(body, modified);
        }
        TirStmtKind::ForOf {
            binding_local,
            body,
            ..
        } => {
            // The binding variable changes each iteration, so it's modified
            modified.insert(*binding_local);
            collect_modified_vars_in_block(body, modified);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(block, modified);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_modified_vars_in_expr(scrutinee, modified);
            // Pattern bindings introduce new variables, but we handle them conservatively
            // by not tracking specific bindings (they're local to the block anyway)
            collect_modified_vars_in_block(then_block, modified);
            if let Some(eb) = else_block {
                collect_modified_vars_in_block(eb, modified);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_modified_vars_in_expr(v, modified);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn collect_modified_vars_in_expr(expr: &TirExpr, modified: &mut HashSet<u32>) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            // Mark the root local as modified (handles field accesses, dereferences, etc.)
            mark_local_as_modified(target, modified);
            collect_modified_vars_in_expr(target, modified);
            collect_modified_vars_in_expr(value, modified);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_modified_vars_in_expr(left, modified);
            collect_modified_vars_in_expr(right, modified);
        }
        TirExprKind::Unary { op, expr } => {
            // If taking a mutable reference, the target could be modified
            // Mark the underlying local as modified so LICM doesn't hoist it
            if matches!(op, crate::tir::TirUnaryOp::MutRef) {
                // Traverse through field accesses to find the root local
                mark_local_as_modified(expr, modified);
            }
            collect_modified_vars_in_expr(expr, modified);
        }
        TirExprKind::Cast { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified);
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                collect_modified_vars_in_expr(arg, modified);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_modified_vars_in_expr(receiver, modified);
            for arg in args {
                collect_modified_vars_in_expr(arg, modified);
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_modified_vars_in_expr(arg, modified);
            }
        }
        TirExprKind::FieldAccess { expr, .. } => {
            collect_modified_vars_in_expr(expr, modified);
        }
        TirExprKind::Index { expr, index } => {
            collect_modified_vars_in_expr(expr, modified);
            collect_modified_vars_in_expr(index, modified);
        }
        TirExprKind::Block(block) => {
            collect_modified_vars_in_block(block, modified);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_modified_vars_in_expr(condition, modified);
            collect_modified_vars_in_block(then_branch, modified);
            if let Some(eb) = else_branch {
                collect_modified_vars_in_block(eb, modified);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_modified_vars_in_expr(&field.value, modified);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                collect_modified_vars_in_expr(elem, modified);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_modified_vars_in_expr(elem, modified);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_modified_vars_in_expr(callee, modified);
            for arg in args {
                collect_modified_vars_in_expr(arg, modified);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_modified_vars_in_expr(body, modified);
        }
        TirExprKind::OptionSome { value } => {
            collect_modified_vars_in_expr(value, modified);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_modified_vars_in_expr(field, modified);
            }
        }
        TirExprKind::Move { value } => {
            collect_modified_vars_in_expr(value, modified);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_modified_vars_in_block(block, modified);
        }
        // Leaf nodes
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
        | TirExprKind::Match { .. } => {}
    }
}

/// Check if an expression is loop-invariant given the set of modified variables.
/// An expression is loop-invariant if:
/// 1. It's a constant/literal
/// 2. It's a local variable not in the modified set
/// 3. It's a field access on a loop-invariant expression
#[allow(dead_code)]
fn is_loop_invariant(expr: &TirExpr, modified_vars: &HashSet<u32>) -> bool {
    match &expr.kind {
        // Constants are always invariant
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. } => true,

        // Local variable is invariant if not modified in the loop
        TirExprKind::Local { index, .. } => !modified_vars.contains(index),

        // Field access is invariant if the base expression is invariant
        TirExprKind::FieldAccess { expr, .. } => is_loop_invariant(expr, modified_vars),

        // Pure binary ops are invariant if operands are invariant
        // (Assignments are handled separately via TirExprKind::Assign, not as binary ops)
        TirExprKind::Binary { left, right, .. } => {
            is_loop_invariant(left, modified_vars) && is_loop_invariant(right, modified_vars)
        }
        TirExprKind::Unary { expr, .. } => is_loop_invariant(expr, modified_vars),
        TirExprKind::Cast { expr, .. } => is_loop_invariant(expr, modified_vars),

        // Everything else is considered not invariant (conservative)
        // This includes: calls, method calls, index access (could have side effects), etc.
        _ => false,
    }
}

/// Information about an immutable reference binding: `let ref_var: &T = &source_var`
#[derive(Debug, Clone)]
struct LicmRefBinding {
    /// The source local index that this reference points to
    source_index: u32,
    /// The source local name (for creating hoist statements)
    source_name: String,
}

/// Collect immutable reference bindings in a block.
/// These are patterns like: `let self: &T = &source_var`
/// Returns a map from `ref_local_index` -> `source_local_index`
fn collect_immutable_ref_bindings(
    block: &TirBlock,
    type_table: &TypeTable,
) -> HashMap<u32, LicmRefBinding> {
    let mut bindings = HashMap::new();
    collect_licm_ref_bindings_in_block(block, type_table, &mut bindings);
    bindings
}

fn collect_licm_ref_bindings_in_block(
    block: &TirBlock,
    type_table: &TypeTable,
    bindings: &mut HashMap<u32, LicmRefBinding>,
) {
    for stmt in &block.stmts {
        collect_licm_ref_bindings_in_stmt(stmt, type_table, bindings);
    }
}

fn collect_licm_ref_bindings_in_stmt(
    stmt: &TirStmt,
    type_table: &TypeTable,
    bindings: &mut HashMap<u32, LicmRefBinding>,
) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index,
            value,
            type_id,
            ..
        } => {
            // Check if this is: let x: &T = &y (immutable ref to a local)
            if matches!(type_table.get(*type_id), ResolvedType::Ref(_))
                && let TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: source,
                } = &value.kind
                && let TirExprKind::Local {
                    index: source_idx,
                    name: source_name,
                } = &source.kind
            {
                bindings.insert(
                    *local_index,
                    LicmRefBinding {
                        source_index: *source_idx,
                        source_name: source_name.clone(),
                    },
                );
            }
            // Recurse into the value expression (for nested blocks)
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        TirStmtKind::Expr(expr) => {
            collect_licm_ref_bindings_in_expr(expr, type_table, bindings);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_licm_ref_bindings_in_expr(v, type_table, bindings);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_licm_ref_bindings_in_expr(condition, type_table, bindings);
            collect_licm_ref_bindings_in_block(then_block, type_table, bindings);
            if let Some(eb) = else_block {
                collect_licm_ref_bindings_in_block(eb, type_table, bindings);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_licm_ref_bindings_in_expr(condition, type_table, bindings);
            collect_licm_ref_bindings_in_block(body, type_table, bindings);
        }
        TirStmtKind::For {
            init,
            condition,
            body,
            update,
        } => {
            for s in init {
                collect_licm_ref_bindings_in_stmt(s, type_table, bindings);
            }
            if let Some(c) = condition {
                collect_licm_ref_bindings_in_expr(c, type_table, bindings);
            }
            collect_licm_ref_bindings_in_block(body, type_table, bindings);
            if let Some(u) = update {
                collect_licm_ref_bindings_in_expr(u, type_table, bindings);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_licm_ref_bindings_in_block(body, type_table, bindings);
        }
        TirStmtKind::ForOf { body, .. } => {
            collect_licm_ref_bindings_in_block(body, type_table, bindings);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_licm_ref_bindings_in_expr(scrutinee, type_table, bindings);
            collect_licm_ref_bindings_in_block(then_block, type_table, bindings);
            if let Some(eb) = else_block {
                collect_licm_ref_bindings_in_block(eb, type_table, bindings);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_licm_ref_bindings_in_expr(v, type_table, bindings);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn collect_licm_ref_bindings_in_expr(
    expr: &TirExpr,
    type_table: &TypeTable,
    bindings: &mut HashMap<u32, LicmRefBinding>,
) {
    // Recurse into all sub-expressions to find nested let bindings
    match &expr.kind {
        TirExprKind::Block(block) => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_licm_ref_bindings_in_block(block, type_table, bindings);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_licm_ref_bindings_in_expr(condition, type_table, bindings);
            collect_licm_ref_bindings_in_block(then_branch, type_table, bindings);
            if let Some(eb) = else_branch {
                collect_licm_ref_bindings_in_block(eb, type_table, bindings);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_licm_ref_bindings_in_expr(left, type_table, bindings);
            collect_licm_ref_bindings_in_expr(right, type_table, bindings);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        TirExprKind::Index { expr: inner, index } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
            collect_licm_ref_bindings_in_expr(index, type_table, bindings);
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                collect_licm_ref_bindings_in_expr(arg, type_table, bindings);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_licm_ref_bindings_in_expr(receiver, type_table, bindings);
            for arg in args {
                collect_licm_ref_bindings_in_expr(arg, type_table, bindings);
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_licm_ref_bindings_in_expr(arg, type_table, bindings);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_licm_ref_bindings_in_expr(callee, type_table, bindings);
            for arg in args {
                collect_licm_ref_bindings_in_expr(arg, type_table, bindings);
            }
        }
        TirExprKind::Assign { target, value } => {
            collect_licm_ref_bindings_in_expr(target, type_table, bindings);
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_licm_ref_bindings_in_expr(inner, type_table, bindings);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_licm_ref_bindings_in_expr(&field.value, type_table, bindings);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_licm_ref_bindings_in_expr(elem, type_table, bindings);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_licm_ref_bindings_in_expr(body, type_table, bindings);
        }
        TirExprKind::OptionSome { value } => {
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_licm_ref_bindings_in_expr(field, type_table, bindings);
            }
        }
        TirExprKind::Move { value } => {
            collect_licm_ref_bindings_in_expr(value, type_table, bindings);
        }
        // Leaf nodes - no nested expressions
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
        | TirExprKind::Match { .. } => {}
    }
}

/// Represents a hoistable expression with its replacement info
#[derive(Debug)]
struct HoistCandidate {
    /// The original expression pattern to match (field access on a local)
    local_index: u32,
    /// The name of the local variable (for unparsing)
    local_name: String,
    field_index: u32,
    field_name: String,
    /// The type of the field access result
    type_id: TypeId,
    /// The new local index to use for the hoisted value
    new_local_index: u32,
}

/// Find field accesses on loop-invariant expressions that can be hoisted.
/// Returns a list of candidates to hoist.
fn find_hoist_candidates_in_block(
    block: &TirBlock,
    modified_vars: &HashSet<u32>,
    ref_bindings: &HashMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut HashSet<(u32, u32)>, // (local_index, field_index) pairs already seen
    next_local: &mut u32,
) {
    for stmt in &block.stmts {
        find_hoist_candidates_in_stmt(
            stmt,
            modified_vars,
            ref_bindings,
            candidates,
            seen,
            next_local,
        );
    }
}

fn find_hoist_candidates_in_stmt(
    stmt: &TirStmt,
    modified_vars: &HashSet<u32>,
    ref_bindings: &HashMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut HashSet<(u32, u32)>,
    next_local: &mut u32,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::Expr(expr) => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                find_hoist_candidates_in_expr(
                    v,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            find_hoist_candidates_in_expr(
                condition,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_block(
                then_block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            if let Some(eb) = else_block {
                find_hoist_candidates_in_block(
                    eb,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::While { condition, body } => {
            find_hoist_candidates_in_expr(
                condition,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_block(
                body,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::For {
            init,
            condition,
            body,
            update,
        } => {
            for s in init {
                find_hoist_candidates_in_stmt(
                    s,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
            if let Some(c) = condition {
                find_hoist_candidates_in_expr(
                    c,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
            find_hoist_candidates_in_block(
                body,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            if let Some(u) = update {
                find_hoist_candidates_in_expr(
                    u,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::Loop { body } => {
            find_hoist_candidates_in_block(
                body,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::ForOf { body, .. } => {
            find_hoist_candidates_in_block(
                body,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            find_hoist_candidates_in_expr(
                scrutinee,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_block(
                then_block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            if let Some(eb) = else_block {
                find_hoist_candidates_in_block(
                    eb,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                find_hoist_candidates_in_expr(
                    v,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn find_hoist_candidates_in_expr(
    expr: &TirExpr,
    modified_vars: &HashSet<u32>,
    ref_bindings: &HashMap<u32, LicmRefBinding>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut HashSet<(u32, u32)>,
    next_local: &mut u32,
) {
    match &expr.kind {
        // This is the key pattern: field access on a loop-invariant local
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => {
            if let TirExprKind::Local { index, name } = &inner.kind {
                // Case 1: Direct access on a loop-invariant local
                if !modified_vars.contains(index) {
                    let key = (*index, *field_index);
                    if !seen.contains(&key) {
                        seen.insert(key);
                        candidates.push(HoistCandidate {
                            local_index: *index,
                            local_name: name.clone(),
                            field_index: *field_index,
                            field_name: field_name.clone(),
                            type_id: expr.type_id,
                            new_local_index: *next_local,
                        });
                        *next_local += 1;
                    }
                }
                // Case 2: Access through an immutable reference to a loop-invariant local
                // e.g., `let self: &T = &source; ... self.field ...`
                // Since &T guarantees immutability, self.field == source.field
                else if let Some(ref_binding) = ref_bindings.get(index)
                    && !modified_vars.contains(&ref_binding.source_index)
                {
                    let key = (ref_binding.source_index, *field_index);
                    if !seen.contains(&key) {
                        seen.insert(key);
                        candidates.push(HoistCandidate {
                            local_index: ref_binding.source_index,
                            local_name: ref_binding.source_name.clone(),
                            field_index: *field_index,
                            field_name: field_name.clone(),
                            type_id: expr.type_id,
                            new_local_index: *next_local,
                        });
                        *next_local += 1;
                    }
                }
            }
            // Still recurse into inner expression
            find_hoist_candidates_in_expr(
                inner,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Binary { left, right, .. } => {
            find_hoist_candidates_in_expr(
                left,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_expr(
                right,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Unary { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Assign { target, value } => {
            find_hoist_candidates_in_expr(
                target,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Cast { expr, .. } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                find_hoist_candidates_in_expr(
                    arg,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            find_hoist_candidates_in_expr(
                receiver,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            for arg in args {
                find_hoist_candidates_in_expr(
                    arg,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                find_hoist_candidates_in_expr(
                    arg,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::Index { expr, index } => {
            find_hoist_candidates_in_expr(
                expr,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_expr(
                index,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::Block(block) => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            find_hoist_candidates_in_expr(
                condition,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            find_hoist_candidates_in_block(
                then_branch,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            if let Some(eb) = else_branch {
                find_hoist_candidates_in_block(
                    eb,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                find_hoist_candidates_in_expr(
                    &field.value,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                find_hoist_candidates_in_expr(
                    elem,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                find_hoist_candidates_in_expr(
                    elem,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            find_hoist_candidates_in_expr(
                callee,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
            for arg in args {
                find_hoist_candidates_in_expr(
                    arg,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::Closure { body, .. } => {
            find_hoist_candidates_in_expr(
                body,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::OptionSome { value } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                find_hoist_candidates_in_expr(
                    field,
                    modified_vars,
                    ref_bindings,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::Move { value } => {
            find_hoist_candidates_in_expr(
                value,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        TirExprKind::LabeledBlock { block, .. } => {
            find_hoist_candidates_in_block(
                block,
                modified_vars,
                ref_bindings,
                candidates,
                seen,
                next_local,
            );
        }
        // Leaf nodes
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
        | TirExprKind::Match { .. } => {}
    }
}

/// Replace field accesses with references to hoisted locals
fn replace_hoisted_in_block(
    block: &mut TirBlock,
    candidates: &[HoistCandidate],
    ref_bindings: &HashMap<u32, LicmRefBinding>,
) {
    for stmt in &mut block.stmts {
        replace_hoisted_in_stmt(stmt, candidates, ref_bindings);
    }
}

fn replace_hoisted_in_stmt(
    stmt: &mut TirStmt,
    candidates: &[HoistCandidate],
    ref_bindings: &HashMap<u32, LicmRefBinding>,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        TirStmtKind::Expr(expr) => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_hoisted_in_expr(v, candidates, ref_bindings);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_hoisted_in_expr(condition, candidates, ref_bindings);
            replace_hoisted_in_block(then_block, candidates, ref_bindings);
            if let Some(eb) = else_block {
                replace_hoisted_in_block(eb, candidates, ref_bindings);
            }
        }
        TirStmtKind::While { condition, body } => {
            replace_hoisted_in_expr(condition, candidates, ref_bindings);
            replace_hoisted_in_block(body, candidates, ref_bindings);
        }
        TirStmtKind::For {
            init,
            condition,
            body,
            update,
        } => {
            for s in init {
                replace_hoisted_in_stmt(s, candidates, ref_bindings);
            }
            if let Some(c) = condition {
                replace_hoisted_in_expr(c, candidates, ref_bindings);
            }
            replace_hoisted_in_block(body, candidates, ref_bindings);
            if let Some(u) = update {
                replace_hoisted_in_expr(u, candidates, ref_bindings);
            }
        }
        TirStmtKind::Loop { body } => {
            replace_hoisted_in_block(body, candidates, ref_bindings);
        }
        TirStmtKind::ForOf { body, .. } => {
            replace_hoisted_in_block(body, candidates, ref_bindings);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            replace_hoisted_in_expr(scrutinee, candidates, ref_bindings);
            replace_hoisted_in_block(then_block, candidates, ref_bindings);
            if let Some(eb) = else_block {
                replace_hoisted_in_block(eb, candidates, ref_bindings);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                replace_hoisted_in_expr(v, candidates, ref_bindings);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn replace_hoisted_in_expr(
    expr: &mut TirExpr,
    candidates: &[HoistCandidate],
    ref_bindings: &HashMap<u32, LicmRefBinding>,
) {
    // First, check if this expression matches a hoist candidate
    if let TirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &expr.kind
        && let TirExprKind::Local { index, .. } = &inner.kind
    {
        // Case 1: Direct match - local.field where local is the hoisted source
        for candidate in candidates {
            if candidate.local_index == *index && candidate.field_index == *field_index {
                // Replace with a reference to the hoisted local
                expr.kind = TirExprKind::Local {
                    index: candidate.new_local_index,
                    name: format!("_licm_{}", candidate.field_name),
                };
                return;
            }
        }
        // Case 2: Look through immutable reference - ref_var.field where ref_var = &source
        if let Some(ref_binding) = ref_bindings.get(index) {
            for candidate in candidates {
                if candidate.local_index == ref_binding.source_index
                    && candidate.field_index == *field_index
                {
                    // Replace with a reference to the hoisted local
                    expr.kind = TirExprKind::Local {
                        index: candidate.new_local_index,
                        name: format!("_licm_{}", candidate.field_name),
                    };
                    return;
                }
            }
        }
    }

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. } => {
            replace_hoisted_in_expr(inner, candidates, ref_bindings);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_hoisted_in_expr(left, candidates, ref_bindings);
            replace_hoisted_in_expr(right, candidates, ref_bindings);
        }
        TirExprKind::Unary { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        TirExprKind::Assign { target, value } => {
            replace_hoisted_in_expr(target, candidates, ref_bindings);
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        TirExprKind::Cast { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                replace_hoisted_in_expr(arg, candidates, ref_bindings);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            replace_hoisted_in_expr(receiver, candidates, ref_bindings);
            for arg in args {
                replace_hoisted_in_expr(arg, candidates, ref_bindings);
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                replace_hoisted_in_expr(arg, candidates, ref_bindings);
            }
        }
        TirExprKind::Index { expr, index } => {
            replace_hoisted_in_expr(expr, candidates, ref_bindings);
            replace_hoisted_in_expr(index, candidates, ref_bindings);
        }
        TirExprKind::Block(block) => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            replace_hoisted_in_expr(condition, candidates, ref_bindings);
            replace_hoisted_in_block(then_branch, candidates, ref_bindings);
            if let Some(eb) = else_branch {
                replace_hoisted_in_block(eb, candidates, ref_bindings);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                replace_hoisted_in_expr(&mut field.value, candidates, ref_bindings);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                replace_hoisted_in_expr(elem, candidates, ref_bindings);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                replace_hoisted_in_expr(elem, candidates, ref_bindings);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            replace_hoisted_in_expr(callee, candidates, ref_bindings);
            for arg in args {
                replace_hoisted_in_expr(arg, candidates, ref_bindings);
            }
        }
        TirExprKind::Closure { body, .. } => {
            replace_hoisted_in_expr(body, candidates, ref_bindings);
        }
        TirExprKind::OptionSome { value } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                replace_hoisted_in_expr(field, candidates, ref_bindings);
            }
        }
        TirExprKind::Move { value } => {
            replace_hoisted_in_expr(value, candidates, ref_bindings);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            replace_hoisted_in_block(block, candidates, ref_bindings);
        }
        // Leaf nodes
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
        | TirExprKind::Match { .. } => {}
    }
}

/// Apply LICM to a single loop, returning hoisting statements to prepend
/// `extra_modified` contains variables that are implicitly modified (e.g., for-of binding)
/// Runs iteratively until no more candidates are found (for second-level hoisting)
fn licm_loop(
    loop_body: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    extra_modified: &HashSet<u32>,
) -> Vec<TirStmt> {
    let mut all_hoist_stmts = Vec::new();

    // Run LICM iteratively until no more candidates are found
    // This enables second-level hoisting (e.g., hoisting _licm_entries.repr after _licm_entries)
    // Limit iterations to prevent pathological cases
    const MAX_LICM_ITERATIONS: usize = 10;
    for _iteration in 0..MAX_LICM_ITERATIONS {
        // Step 1: Collect all variables modified in the loop
        let mut modified_vars = extra_modified.clone();
        collect_modified_vars_in_block(loop_body, &mut modified_vars);

        // Step 2: Collect immutable reference bindings for look-through optimization
        // This allows hoisting field accesses like `self.field` where `self: &T = &source`
        let ref_bindings = collect_immutable_ref_bindings(loop_body, type_table);

        // Step 3: Find field accesses that can be hoisted
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        let mut next_local = *local_count;
        find_hoist_candidates_in_block(
            loop_body,
            &modified_vars,
            &ref_bindings,
            &mut candidates,
            &mut seen,
            &mut next_local,
        );

        if candidates.is_empty() {
            break;
        }

        // Step 4: Create hoisting statements
        for candidate in &candidates {
            // Get the type of the original local to build the field access expression
            let local_type_id = if (candidate.local_index as usize) < local_types.len() {
                local_types[candidate.local_index as usize]
            } else {
                // Fallback: use the candidate's type_id
                candidate.type_id
            };

            // Create field access expression: local.field
            let field_access_expr = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(TirExpr::new(
                        TirExprKind::Local {
                            index: candidate.local_index,
                            name: candidate.local_name.clone(),
                        },
                        local_type_id,
                        crate::token::Span::new(0, 0, 0, 0),
                    )),
                    field_index: candidate.field_index,
                    field_name: candidate.field_name.clone(),
                },
                candidate.type_id,
                crate::token::Span::new(0, 0, 0, 0),
            );

            // Create let statement for the hoisted value
            let hoist_stmt = TirStmt::new(
                TirStmtKind::Let {
                    name: format!("_licm_{}", candidate.field_name),
                    local_index: candidate.new_local_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id: candidate.type_id,
                    value: field_access_expr,
                },
                crate::token::Span::new(0, 0, 0, 0),
            );
            all_hoist_stmts.push(hoist_stmt);

            // Add the type to local_types
            local_types.push(candidate.type_id);
        }

        // Update local count
        *local_count = next_local;

        // Step 5: Replace field accesses in the loop body with references to hoisted locals
        replace_hoisted_in_block(loop_body, &candidates, &ref_bindings);
    }

    // Also need to handle nested loops - apply LICM recursively
    licm_block(loop_body, local_count, local_types, type_table);

    all_hoist_stmts
}

/// Apply LICM to all loops in a block
fn licm_block(
    block: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
) {
    let mut new_stmts = Vec::new();

    for mut stmt in std::mem::take(&mut block.stmts) {
        match &mut stmt.kind {
            TirStmtKind::While { body, .. } => {
                // Apply LICM to the while loop
                let empty_set = HashSet::new();
                let hoist_stmts = licm_loop(body, local_count, local_types, type_table, &empty_set);

                // Prepend hoisting statements
                new_stmts.extend(hoist_stmts);
                new_stmts.push(stmt);
            }
            TirStmtKind::For { body, .. } => {
                // Apply LICM to the for loop body
                let empty_set = HashSet::new();
                let hoist_stmts = licm_loop(body, local_count, local_types, type_table, &empty_set);

                // Prepend hoisting statements
                new_stmts.extend(hoist_stmts);
                new_stmts.push(stmt);
            }
            TirStmtKind::Loop { body } => {
                // Apply LICM to the loop body
                let empty_set = HashSet::new();
                let hoist_stmts = licm_loop(body, local_count, local_types, type_table, &empty_set);

                // Prepend hoisting statements
                new_stmts.extend(hoist_stmts);
                new_stmts.push(stmt);
            }
            TirStmtKind::ForOf {
                binding_local,
                body,
                ..
            } => {
                // The binding variable changes each iteration - include it as modified
                let mut extra_modified = HashSet::new();
                extra_modified.insert(*binding_local);
                let hoist_stmts =
                    licm_loop(body, local_count, local_types, type_table, &extra_modified);

                // Prepend hoisting statements
                new_stmts.extend(hoist_stmts);
                new_stmts.push(stmt);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                // Recurse into if branches
                licm_block(then_block, local_count, local_types, type_table);
                if let Some(eb) = else_block {
                    licm_block(eb, local_count, local_types, type_table);
                }
                new_stmts.push(stmt);
            }
            TirStmtKind::LabeledBlock { block: inner, .. } => {
                licm_block(inner, local_count, local_types, type_table);
                new_stmts.push(stmt);
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                licm_block(then_block, local_count, local_types, type_table);
                if let Some(eb) = else_block {
                    licm_block(eb, local_count, local_types, type_table);
                }
                new_stmts.push(stmt);
            }
            // Other statements don't contain loops at the statement level
            _ => {
                new_stmts.push(stmt);
            }
        }
    }

    block.stmts = new_stmts;
}

/// Apply LICM to a function
fn licm_function(func: &mut TirFunction, type_table: &TypeTable) {
    if let Some(ref mut body) = func.body {
        let mut local_count = func.local_count;
        let mut local_types = func.local_types.clone();

        licm_block(body, &mut local_count, &mut local_types, type_table);

        func.local_count = local_count;
        func.local_types = local_types;
    }
}

// =============================================================================
// Reference Elimination Optimization
// =============================================================================

/// Information about a reference binding that may be eliminable.
/// Pattern: `let ref_var: &T = &local_var` or `let ref_var: &mut T = &mut local_var`
#[derive(Debug)]
struct RefBinding {
    /// Local index of the reference variable (`ref_var`)
    ref_local: u32,
    /// Local index of the original variable (`local_var`)
    target_local: u32,
    /// Name of the original variable (for reconstruction)
    target_name: String,
    /// Whether this is a mutable reference
    #[allow(dead_code)]
    is_mut: bool,
}

/// Analyze a Let statement to see if it binds a reference to a local variable.
fn analyze_ref_binding(stmt: &TirStmt) -> Option<RefBinding> {
    let TirStmtKind::Let {
        local_index, value, ..
    } = &stmt.kind
    else {
        return None;
    };

    // Check if value is &local or &mut local
    let TirExprKind::Unary { op, expr } = &value.kind else {
        return None;
    };

    let is_mut = match op {
        TirUnaryOp::Ref => false,
        TirUnaryOp::MutRef => true,
        _ => return None,
    };

    // The inner expression must be a local variable
    let TirExprKind::Local { index, name } = &expr.kind else {
        return None;
    };

    Some(RefBinding {
        ref_local: *local_index,
        target_local: *index,
        target_name: name.clone(),
        is_mut,
    })
}

/// Check if an expression is a use of the given local variable.
fn is_local_use(expr: &TirExpr, local_index: u32) -> bool {
    matches!(&expr.kind, TirExprKind::Local { index, .. } if *index == local_index)
}

/// Track all uses of a local variable in an expression.
/// Returns (`is_only_field_access`, `uses_count`)
/// If `is_only_field_access` is true, all uses are field accesses.
fn track_local_uses_in_expr(expr: &TirExpr, local_index: u32) -> (bool, u32) {
    match &expr.kind {
        TirExprKind::Local { index, .. } if *index == local_index => {
            // Direct use of the local (not through field access) - not eliminable
            (false, 1)
        }
        TirExprKind::FieldAccess {
            expr: inner,
            ..
        } => {
            if is_local_use(inner, local_index) {
                // Field access on the local - this is what we want to optimize
                (true, 1)
            } else {
                // Field access on something else, recurse
                track_local_uses_in_expr(inner, local_index)
            }
        }
        // Recursively check nested expressions
        TirExprKind::Binary { left, right, .. } => {
            let (l_ok, l_count) = track_local_uses_in_expr(left, local_index);
            let (r_ok, r_count) = track_local_uses_in_expr(right, local_index);
            (l_ok && r_ok, l_count + r_count)
        }
        TirExprKind::Unary { expr: inner, .. } => track_local_uses_in_expr(inner, local_index),
        TirExprKind::Cast { expr: inner, .. } => track_local_uses_in_expr(inner, local_index),
        TirExprKind::Assign { target, value } => {
            let (t_ok, t_count) = track_local_uses_in_expr(target, local_index);
            let (v_ok, v_count) = track_local_uses_in_expr(value, local_index);
            (t_ok && v_ok, t_count + v_count)
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            let mut total = 0;
            let mut all_ok = true;
            for arg in args {
                let (ok, count) = track_local_uses_in_expr(arg, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            let (r_ok, r_count) = track_local_uses_in_expr(receiver, local_index);
            let mut total = r_count;
            let mut all_ok = r_ok;
            for arg in args {
                let (ok, count) = track_local_uses_in_expr(arg, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::IndirectCall { callee, args } => {
            let (c_ok, c_count) = track_local_uses_in_expr(callee, local_index);
            let mut total = c_count;
            let mut all_ok = c_ok;
            for arg in args {
                let (ok, count) = track_local_uses_in_expr(arg, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::Index { expr: inner, index } => {
            let (i_ok, i_count) = track_local_uses_in_expr(inner, local_index);
            let (x_ok, x_count) = track_local_uses_in_expr(index, local_index);
            (i_ok && x_ok, i_count + x_count)
        }
        TirExprKind::Block(block) => track_local_uses_in_block(block, local_index),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (c_ok, c_count) = track_local_uses_in_expr(condition, local_index);
            let (t_ok, t_count) = track_local_uses_in_block(then_branch, local_index);
            let (e_ok, e_count) = else_branch
                .as_ref()
                .map_or((true, 0), |eb| track_local_uses_in_block(eb, local_index));
            (c_ok && t_ok && e_ok, c_count + t_count + e_count)
        }
        TirExprKind::StructLiteral { fields, .. } => {
            let mut total = 0;
            let mut all_ok = true;
            for field in fields {
                let (ok, count) = track_local_uses_in_expr(&field.value, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            let mut total = 0;
            let mut all_ok = true;
            for elem in elements {
                let (ok, count) = track_local_uses_in_expr(elem, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::OptionSome { value } => track_local_uses_in_expr(value, local_index),
        TirExprKind::VariantConstruct { fields, .. } => {
            let mut total = 0;
            let mut all_ok = true;
            for field in fields {
                let (ok, count) = track_local_uses_in_expr(field, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        TirExprKind::Move { value } => track_local_uses_in_expr(value, local_index),
        TirExprKind::LabeledBlock { block, .. } => track_local_uses_in_block(block, local_index),
        TirExprKind::Closure { body, .. } => track_local_uses_in_expr(body, local_index),
        TirExprKind::Match { expr: inner, arms } => {
            let (i_ok, i_count) = track_local_uses_in_expr(inner, local_index);
            let mut total = i_count;
            let mut all_ok = i_ok;
            for arm in arms {
                let (ok, count) = track_local_uses_in_expr(&arm.body, local_index);
                all_ok = all_ok && ok;
                total += count;
            }
            (all_ok, total)
        }
        // Leaf nodes - no uses
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. } // Different local
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => (true, 0),
    }
}

/// Track all uses of a local variable in a block.
fn track_local_uses_in_block(block: &TirBlock, local_index: u32) -> (bool, u32) {
    let mut total = 0;
    let mut all_ok = true;
    for stmt in &block.stmts {
        let (ok, count) = track_local_uses_in_stmt(stmt, local_index);
        all_ok = all_ok && ok;
        total += count;
    }
    (all_ok, total)
}

/// Track all uses of a local variable in a statement.
fn track_local_uses_in_stmt(stmt: &TirStmt, local_index: u32) -> (bool, u32) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => track_local_uses_in_expr(value, local_index),
        TirStmtKind::Expr(expr) => track_local_uses_in_expr(expr, local_index),
        TirStmtKind::Return { value } => value
            .as_ref()
            .map_or((true, 0), |v| track_local_uses_in_expr(v, local_index)),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (c_ok, c_count) = track_local_uses_in_expr(condition, local_index);
            let (t_ok, t_count) = track_local_uses_in_block(then_block, local_index);
            let (e_ok, e_count) = else_block
                .as_ref()
                .map_or((true, 0), |eb| track_local_uses_in_block(eb, local_index));
            (c_ok && t_ok && e_ok, c_count + t_count + e_count)
        }
        TirStmtKind::While { condition, body } => {
            let (c_ok, c_count) = track_local_uses_in_expr(condition, local_index);
            let (b_ok, b_count) = track_local_uses_in_block(body, local_index);
            (c_ok && b_ok, c_count + b_count)
        }
        TirStmtKind::Loop { body } => track_local_uses_in_block(body, local_index),
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            let (mut all_ok, mut total_count) = (true, 0);
            for s in init {
                let (ok, count) = track_local_uses_in_stmt(s, local_index);
                all_ok = all_ok && ok;
                total_count += count;
            }
            let (c_ok, c_count) = condition
                .as_ref()
                .map_or((true, 0), |c| track_local_uses_in_expr(c, local_index));
            let (u_ok, u_count) = update
                .as_ref()
                .map_or((true, 0), |u| track_local_uses_in_expr(u, local_index));
            let (b_ok, b_count) = track_local_uses_in_block(body, local_index);
            (
                all_ok && c_ok && u_ok && b_ok,
                total_count + c_count + u_count + b_count,
            )
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            let (i_ok, i_count) = track_local_uses_in_expr(iterable, local_index);
            let (b_ok, b_count) = track_local_uses_in_block(body, local_index);
            (i_ok && b_ok, i_count + b_count)
        }
        TirStmtKind::LabeledBlock { block, .. } => track_local_uses_in_block(block, local_index),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let (s_ok, s_count) = track_local_uses_in_expr(scrutinee, local_index);
            let (t_ok, t_count) = track_local_uses_in_block(then_block, local_index);
            let (e_ok, e_count) = else_block
                .as_ref()
                .map_or((true, 0), |eb| track_local_uses_in_block(eb, local_index));
            (s_ok && t_ok && e_ok, s_count + t_count + e_count)
        }
        TirStmtKind::Break { value, .. } => value
            .as_ref()
            .map_or((true, 0), |v| track_local_uses_in_expr(v, local_index)),
        TirStmtKind::Continue => (true, 0),
    }
}

/// Replace field accesses on `ref_local` with field accesses on `target_local`.
fn replace_ref_field_access_in_expr(
    expr: &mut TirExpr,
    ref_local: u32,
    target_local: u32,
    target_name: &str,
) {
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. } => {
            if is_local_use(inner, ref_local) {
                // Replace the inner local with the target local
                **inner = TirExpr::new(
                    TirExprKind::Local {
                        index: target_local,
                        name: target_name.to_string(),
                    },
                    inner.type_id, // Keep the type - codegen handles ref vs value
                    inner.span,
                );
            } else {
                replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_ref_field_access_in_expr(left, ref_local, target_local, target_name);
            replace_ref_field_access_in_expr(right, ref_local, target_local, target_name);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
        }
        TirExprKind::Assign { target, value } => {
            replace_ref_field_access_in_expr(target, ref_local, target_local, target_name);
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                replace_ref_field_access_in_expr(arg, ref_local, target_local, target_name);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            replace_ref_field_access_in_expr(receiver, ref_local, target_local, target_name);
            for arg in args {
                replace_ref_field_access_in_expr(arg, ref_local, target_local, target_name);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            replace_ref_field_access_in_expr(callee, ref_local, target_local, target_name);
            for arg in args {
                replace_ref_field_access_in_expr(arg, ref_local, target_local, target_name);
            }
        }
        TirExprKind::Index { expr: inner, index } => {
            replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
            replace_ref_field_access_in_expr(index, ref_local, target_local, target_name);
        }
        TirExprKind::Block(block) => {
            replace_ref_field_access_in_block(block, ref_local, target_local, target_name);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            replace_ref_field_access_in_expr(condition, ref_local, target_local, target_name);
            replace_ref_field_access_in_block(then_branch, ref_local, target_local, target_name);
            if let Some(eb) = else_branch {
                replace_ref_field_access_in_block(eb, ref_local, target_local, target_name);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                replace_ref_field_access_in_expr(
                    &mut field.value,
                    ref_local,
                    target_local,
                    target_name,
                );
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                replace_ref_field_access_in_expr(elem, ref_local, target_local, target_name);
            }
        }
        TirExprKind::OptionSome { value } => {
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                replace_ref_field_access_in_expr(field, ref_local, target_local, target_name);
            }
        }
        TirExprKind::Move { value } => {
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            replace_ref_field_access_in_block(block, ref_local, target_local, target_name);
        }
        TirExprKind::Closure { body, .. } => {
            replace_ref_field_access_in_expr(body, ref_local, target_local, target_name);
        }
        TirExprKind::Match { expr: inner, arms } => {
            replace_ref_field_access_in_expr(inner, ref_local, target_local, target_name);
            for arm in arms {
                replace_ref_field_access_in_expr(
                    &mut arm.body,
                    ref_local,
                    target_local,
                    target_name,
                );
            }
        }
        // Leaf nodes - nothing to replace
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Replace field accesses in a block.
fn replace_ref_field_access_in_block(
    block: &mut TirBlock,
    ref_local: u32,
    target_local: u32,
    target_name: &str,
) {
    for stmt in &mut block.stmts {
        replace_ref_field_access_in_stmt(stmt, ref_local, target_local, target_name);
    }
}

/// Replace field accesses in a statement.
fn replace_ref_field_access_in_stmt(
    stmt: &mut TirStmt,
    ref_local: u32,
    target_local: u32,
    target_name: &str,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            replace_ref_field_access_in_expr(value, ref_local, target_local, target_name);
        }
        TirStmtKind::Expr(expr) => {
            replace_ref_field_access_in_expr(expr, ref_local, target_local, target_name);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_ref_field_access_in_expr(v, ref_local, target_local, target_name);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_ref_field_access_in_expr(condition, ref_local, target_local, target_name);
            replace_ref_field_access_in_block(then_block, ref_local, target_local, target_name);
            if let Some(eb) = else_block {
                replace_ref_field_access_in_block(eb, ref_local, target_local, target_name);
            }
        }
        TirStmtKind::While { condition, body } => {
            replace_ref_field_access_in_expr(condition, ref_local, target_local, target_name);
            replace_ref_field_access_in_block(body, ref_local, target_local, target_name);
        }
        TirStmtKind::Loop { body } => {
            replace_ref_field_access_in_block(body, ref_local, target_local, target_name);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                replace_ref_field_access_in_stmt(s, ref_local, target_local, target_name);
            }
            if let Some(c) = condition {
                replace_ref_field_access_in_expr(c, ref_local, target_local, target_name);
            }
            if let Some(u) = update {
                replace_ref_field_access_in_expr(u, ref_local, target_local, target_name);
            }
            replace_ref_field_access_in_block(body, ref_local, target_local, target_name);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            replace_ref_field_access_in_expr(iterable, ref_local, target_local, target_name);
            replace_ref_field_access_in_block(body, ref_local, target_local, target_name);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            replace_ref_field_access_in_block(block, ref_local, target_local, target_name);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            replace_ref_field_access_in_expr(scrutinee, ref_local, target_local, target_name);
            replace_ref_field_access_in_block(then_block, ref_local, target_local, target_name);
            if let Some(eb) = else_block {
                replace_ref_field_access_in_block(eb, ref_local, target_local, target_name);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                replace_ref_field_access_in_expr(v, ref_local, target_local, target_name);
            }
        }
        TirStmtKind::Continue => {}
    }
}

/// Eliminate unnecessary reference bindings in a function.
/// After inlining, we often have patterns like:
///   let self: &Array<T> = &arr;
///   ... self.repr ...
/// This can be optimized to:
///   ... arr.repr ...
fn eliminate_refs_in_function(func: &mut TirFunction, _type_table: &TypeTable) {
    let Some(body) = &mut func.body else {
        return;
    };

    // First pass: find all ref bindings
    let mut ref_bindings: Vec<RefBinding> = Vec::new();
    collect_ref_bindings(&body.stmts, &mut ref_bindings);

    if ref_bindings.is_empty() {
        return;
    }

    // Second pass: for each ref binding, check if all uses are field accesses
    let mut eliminable_bindings: Vec<RefBinding> = Vec::new();
    for binding in ref_bindings {
        let (all_field_access, _count) = track_local_uses_in_block(body, binding.ref_local);
        if all_field_access {
            eliminable_bindings.push(binding);
        }
    }

    // Third pass: replace field accesses and remove dead bindings
    for binding in &eliminable_bindings {
        replace_ref_field_access_in_block(
            body,
            binding.ref_local,
            binding.target_local,
            &binding.target_name,
        );
    }

    // Fourth pass: remove the now-dead Let statements
    // We need to handle nested blocks, so we do this recursively
    let dead_locals: HashSet<u32> = eliminable_bindings.iter().map(|b| b.ref_local).collect();
    remove_dead_ref_bindings(&mut body.stmts, &dead_locals);
}

/// Collect ref bindings from statements (only at the top level of each block).
fn collect_ref_bindings(stmts: &[TirStmt], bindings: &mut Vec<RefBinding>) {
    for stmt in stmts {
        if let Some(binding) = analyze_ref_binding(stmt) {
            bindings.push(binding);
        }
        // Also check nested blocks
        collect_ref_bindings_in_stmt(stmt, bindings);
    }
}

/// Collect ref bindings from nested blocks within a statement.
fn collect_ref_bindings_in_stmt(stmt: &TirStmt, bindings: &mut Vec<RefBinding>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirStmtKind::Expr(expr) => {
            collect_ref_bindings_in_expr(expr, bindings);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_ref_bindings_in_expr(v, bindings);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_ref_bindings_in_expr(condition, bindings);
            collect_ref_bindings(&then_block.stmts, bindings);
            if let Some(eb) = else_block {
                collect_ref_bindings(&eb.stmts, bindings);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_ref_bindings_in_expr(condition, bindings);
            collect_ref_bindings(&body.stmts, bindings);
        }
        TirStmtKind::Loop { body } => {
            collect_ref_bindings(&body.stmts, bindings);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                collect_ref_bindings_in_stmt(s, bindings);
            }
            if let Some(c) = condition {
                collect_ref_bindings_in_expr(c, bindings);
            }
            if let Some(u) = update {
                collect_ref_bindings_in_expr(u, bindings);
            }
            collect_ref_bindings(&body.stmts, bindings);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            collect_ref_bindings_in_expr(iterable, bindings);
            collect_ref_bindings(&body.stmts, bindings);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_ref_bindings(&block.stmts, bindings);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_ref_bindings_in_expr(scrutinee, bindings);
            collect_ref_bindings(&then_block.stmts, bindings);
            if let Some(eb) = else_block {
                collect_ref_bindings(&eb.stmts, bindings);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_ref_bindings_in_expr(v, bindings);
            }
        }
        TirStmtKind::Continue => {}
    }
}

/// Collect ref bindings from nested blocks within an expression.
fn collect_ref_bindings_in_expr(expr: &TirExpr, bindings: &mut Vec<RefBinding>) {
    match &expr.kind {
        TirExprKind::Block(block) => {
            collect_ref_bindings(&block.stmts, bindings);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_ref_bindings(&block.stmts, bindings);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_ref_bindings_in_expr(condition, bindings);
            collect_ref_bindings(&then_branch.stmts, bindings);
            if let Some(eb) = else_branch {
                collect_ref_bindings(&eb.stmts, bindings);
            }
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_ref_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_ref_bindings_in_expr(receiver, bindings);
            for arg in args {
                collect_ref_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_ref_bindings_in_expr(callee, bindings);
            for arg in args {
                collect_ref_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_ref_bindings_in_expr(left, bindings);
            collect_ref_bindings_in_expr(right, bindings);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_ref_bindings_in_expr(inner, bindings);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_ref_bindings_in_expr(inner, bindings);
        }
        TirExprKind::Assign { target, value } => {
            collect_ref_bindings_in_expr(target, bindings);
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_ref_bindings_in_expr(inner, bindings);
        }
        TirExprKind::Index { expr: inner, index } => {
            collect_ref_bindings_in_expr(inner, bindings);
            collect_ref_bindings_in_expr(index, bindings);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_ref_bindings_in_expr(&field.value, bindings);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_ref_bindings_in_expr(elem, bindings);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_ref_bindings_in_expr(field, bindings);
            }
        }
        TirExprKind::Move { value } => {
            collect_ref_bindings_in_expr(value, bindings);
        }
        TirExprKind::Closure { body, .. } => {
            collect_ref_bindings_in_expr(body, bindings);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_ref_bindings_in_expr(inner, bindings);
            for arm in arms {
                collect_ref_bindings_in_expr(&arm.body, bindings);
            }
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Remove Let statements for dead reference locals.
fn remove_dead_ref_bindings(stmts: &mut Vec<TirStmt>, dead_locals: &HashSet<u32>) {
    stmts.retain(|stmt| {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            !dead_locals.contains(local_index)
        } else {
            true
        }
    });

    // Recursively process nested blocks
    for stmt in stmts {
        remove_dead_ref_bindings_in_stmt(stmt, dead_locals);
    }
}

/// Remove dead ref bindings from nested blocks in a statement.
fn remove_dead_ref_bindings_in_stmt(stmt: &mut TirStmt, dead_locals: &HashSet<u32>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirStmtKind::Expr(expr) => {
            remove_dead_ref_bindings_in_expr(expr, dead_locals);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                remove_dead_ref_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            remove_dead_ref_bindings_in_expr(condition, dead_locals);
            remove_dead_ref_bindings(&mut then_block.stmts, dead_locals);
            if let Some(eb) = else_block {
                remove_dead_ref_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirStmtKind::While { condition, body } => {
            remove_dead_ref_bindings_in_expr(condition, dead_locals);
            remove_dead_ref_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::Loop { body } => {
            remove_dead_ref_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                remove_dead_ref_bindings_in_stmt(s, dead_locals);
            }
            if let Some(c) = condition {
                remove_dead_ref_bindings_in_expr(c, dead_locals);
            }
            if let Some(u) = update {
                remove_dead_ref_bindings_in_expr(u, dead_locals);
            }
            remove_dead_ref_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            remove_dead_ref_bindings_in_expr(iterable, dead_locals);
            remove_dead_ref_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            remove_dead_ref_bindings(&mut block.stmts, dead_locals);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            remove_dead_ref_bindings_in_expr(scrutinee, dead_locals);
            remove_dead_ref_bindings(&mut then_block.stmts, dead_locals);
            if let Some(eb) = else_block {
                remove_dead_ref_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                remove_dead_ref_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::Continue => {}
    }
}

/// Remove dead ref bindings from nested blocks in an expression.
fn remove_dead_ref_bindings_in_expr(expr: &mut TirExpr, dead_locals: &HashSet<u32>) {
    match &mut expr.kind {
        TirExprKind::Block(block) => {
            remove_dead_ref_bindings(&mut block.stmts, dead_locals);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            remove_dead_ref_bindings(&mut block.stmts, dead_locals);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            remove_dead_ref_bindings_in_expr(condition, dead_locals);
            remove_dead_ref_bindings(&mut then_branch.stmts, dead_locals);
            if let Some(eb) = else_branch {
                remove_dead_ref_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                remove_dead_ref_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            remove_dead_ref_bindings_in_expr(receiver, dead_locals);
            for arg in args {
                remove_dead_ref_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            remove_dead_ref_bindings_in_expr(callee, dead_locals);
            for arg in args {
                remove_dead_ref_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            remove_dead_ref_bindings_in_expr(left, dead_locals);
            remove_dead_ref_bindings_in_expr(right, dead_locals);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Assign { target, value } => {
            remove_dead_ref_bindings_in_expr(target, dead_locals);
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Index { expr: inner, index } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
            remove_dead_ref_bindings_in_expr(index, dead_locals);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                remove_dead_ref_bindings_in_expr(&mut field.value, dead_locals);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                remove_dead_ref_bindings_in_expr(elem, dead_locals);
            }
        }
        TirExprKind::OptionSome { value } => {
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                remove_dead_ref_bindings_in_expr(field, dead_locals);
            }
        }
        TirExprKind::Move { value } => {
            remove_dead_ref_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::Closure { body, .. } => {
            remove_dead_ref_bindings_in_expr(body, dead_locals);
        }
        TirExprKind::Match { expr: inner, arms } => {
            remove_dead_ref_bindings_in_expr(inner, dead_locals);
            for arm in arms {
                remove_dead_ref_bindings_in_expr(&mut arm.body, dead_locals);
            }
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Eliminate unnecessary reference bindings in all functions.
fn eliminate_unnecessary_refs(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            eliminate_refs_in_function(&mut func, &type_table);
        }
    }
}

// =============================================================================
// Copy Propagation Optimization
// =============================================================================

/// Information about a copy binding that may be eliminable.
/// Pattern: `let x: T = y` where y is a local variable or simple literal
#[derive(Debug, Clone)]
struct CopyBinding {
    /// Local index of the target variable (x)
    target_local: u32,
    /// The source expression (either a Local or a simple literal)
    source: CopySource,
    /// Type of the binding
    type_id: TypeId,
    /// Whether the target is mutable
    #[allow(dead_code)]
    is_mut: bool,
    /// Locals assigned within the containing labeled block (empty if not in a labeled block).
    /// Used to check if source is modified within the block scope.
    block_local_assigned: HashSet<u32>,
}

/// Source of a copy binding
#[derive(Debug, Clone)]
enum CopySource {
    /// Copy from another local variable
    Local { index: u32, name: String },
    /// Copy from an integer literal
    IntLiteral { value: u64, repr: String },
    /// Copy from a float literal
    FloatLiteral { value: f64, repr: String },
    /// Copy from a bool literal
    BoolLiteral(bool),
    /// Copy from a char literal
    CharLiteral(char),
}

/// Usage information for a local variable
#[derive(Debug, Default)]
struct LocalUsage {
    /// Number of times the local is read
    read_count: u32,
    /// Whether the local is ever assigned to (after initialization)
    is_assigned: bool,
    /// Whether the local is used in a loop condition (risky to propagate)
    in_loop_condition: bool,
    /// Whether the local has its address taken
    address_taken: bool,
    /// Whether the local is captured by a closure
    is_captured: bool,
}

/// Analyze a Let statement to see if it's a copy binding.
fn analyze_copy_binding(stmt: &TirStmt) -> Option<CopyBinding> {
    let TirStmtKind::Let {
        local_index,
        is_mut,
        value,
        ..
    } = &stmt.kind
    else {
        return None;
    };

    let source = match &value.kind {
        TirExprKind::Local { index, name } => CopySource::Local {
            index: *index,
            name: name.clone(),
        },
        TirExprKind::IntLiteral { value, repr } => CopySource::IntLiteral {
            value: *value,
            repr: repr.clone(),
        },
        TirExprKind::FloatLiteral { value, repr } => CopySource::FloatLiteral {
            value: *value,
            repr: repr.clone(),
        },
        TirExprKind::BoolLiteral(b) => CopySource::BoolLiteral(*b),
        TirExprKind::CharLiteral(c) => CopySource::CharLiteral(*c),
        _ => return None,
    };

    Some(CopyBinding {
        target_local: *local_index,
        source,
        type_id: value.type_id,
        is_mut: *is_mut,
        block_local_assigned: HashSet::new(),
    })
}

/// Collect usage information for all locals in a function body.
fn collect_local_usage(body: &TirBlock) -> HashMap<u32, LocalUsage> {
    let mut usage: HashMap<u32, LocalUsage> = HashMap::new();
    collect_usage_in_block(body, &mut usage, false);
    usage
}

/// Collect which locals are assigned within a block (non-recursively for labeled blocks).
/// This is used to check if a source variable is modified within a labeled block scope.
fn collect_assigned_in_block(block: &TirBlock) -> HashSet<u32> {
    let mut assigned: HashSet<u32> = HashSet::new();
    collect_assigned_in_stmts(&block.stmts, &mut assigned);
    assigned
}

fn collect_assigned_in_stmts(stmts: &[TirStmt], assigned: &mut HashSet<u32>) {
    for stmt in stmts {
        collect_assigned_in_stmt(stmt, assigned);
    }
}

fn collect_assigned_in_stmt(stmt: &TirStmt, assigned: &mut HashSet<u32>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_assigned_in_expr(value, assigned);
        }
        TirStmtKind::Expr(expr) => {
            collect_assigned_in_expr(expr, assigned);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_assigned_in_expr(v, assigned);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_assigned_in_expr(condition, assigned);
            collect_assigned_in_stmts(&then_block.stmts, assigned);
            if let Some(eb) = else_block {
                collect_assigned_in_stmts(&eb.stmts, assigned);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_assigned_in_expr(condition, assigned);
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                collect_assigned_in_stmt(s, assigned);
            }
            if let Some(c) = condition {
                collect_assigned_in_expr(c, assigned);
            }
            if let Some(u) = update {
                collect_assigned_in_expr(u, assigned);
            }
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            collect_assigned_in_expr(iterable, assigned);
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::Loop { body } => {
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_assigned_in_stmts(&block.stmts, assigned);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_assigned_in_expr(scrutinee, assigned);
            collect_assigned_in_stmts(&then_block.stmts, assigned);
            if let Some(eb) = else_block {
                collect_assigned_in_stmts(&eb.stmts, assigned);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_assigned_in_expr(v, assigned);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn collect_assigned_in_expr(expr: &TirExpr, assigned: &mut HashSet<u32>) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            if let TirExprKind::Local { index, .. } = &target.kind {
                assigned.insert(*index);
            }
            collect_assigned_in_expr(target, assigned);
            collect_assigned_in_expr(value, assigned);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_assigned_in_expr(left, assigned);
            collect_assigned_in_expr(right, assigned);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_assigned_in_expr(inner, assigned);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_assigned_in_expr(arg, assigned);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_assigned_in_expr(receiver, assigned);
            for arg in args {
                collect_assigned_in_expr(arg, assigned);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_assigned_in_expr(callee, assigned);
            for arg in args {
                collect_assigned_in_expr(arg, assigned);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            collect_assigned_in_expr(inner, assigned);
        }
        TirExprKind::Move { value } => {
            collect_assigned_in_expr(value, assigned);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_assigned_in_expr(inner, assigned);
            collect_assigned_in_expr(index, assigned);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_assigned_in_stmts(&block.stmts, assigned);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_assigned_in_expr(condition, assigned);
            collect_assigned_in_stmts(&then_branch.stmts, assigned);
            if let Some(eb) = else_branch {
                collect_assigned_in_stmts(&eb.stmts, assigned);
            }
        }
        TirExprKind::Match { expr, arms } => {
            collect_assigned_in_expr(expr, assigned);
            for arm in arms {
                collect_assigned_in_expr(&arm.body, assigned);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_assigned_in_expr(&field.value, assigned);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_assigned_in_expr(elem, assigned);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_assigned_in_expr(value, assigned);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_assigned_in_expr(field, assigned);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_assigned_in_expr(body, assigned);
        }
        // Terminals - no nested expressions
        TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit => {}
    }
}

fn collect_usage_in_block(block: &TirBlock, usage: &mut HashMap<u32, LocalUsage>, in_loop: bool) {
    for stmt in &block.stmts {
        collect_usage_in_stmt(stmt, usage, in_loop);
    }
}

fn collect_usage_in_stmt(stmt: &TirStmt, usage: &mut HashMap<u32, LocalUsage>, in_loop: bool) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_usage_in_expr(value, usage, in_loop, false);
        }
        TirStmtKind::Expr(expr) => {
            collect_usage_in_expr(expr, usage, in_loop, false);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_usage_in_expr(v, usage, in_loop, false);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_usage_in_expr(condition, usage, in_loop, false);
            collect_usage_in_block(then_block, usage, in_loop);
            if let Some(eb) = else_block {
                collect_usage_in_block(eb, usage, in_loop);
            }
        }
        TirStmtKind::While { condition, body } => {
            // Mark uses in condition as in_loop_condition
            collect_usage_in_expr(condition, usage, true, true);
            collect_usage_in_block(body, usage, true);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                collect_usage_in_stmt(s, usage, true);
            }
            if let Some(c) = condition {
                collect_usage_in_expr(c, usage, true, true);
            }
            if let Some(u) = update {
                collect_usage_in_expr(u, usage, true, false);
            }
            collect_usage_in_block(body, usage, true);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            collect_usage_in_expr(iterable, usage, in_loop, false);
            collect_usage_in_block(body, usage, true);
        }
        TirStmtKind::Loop { body } => {
            collect_usage_in_block(body, usage, true);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_usage_in_block(block, usage, in_loop);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_usage_in_expr(scrutinee, usage, in_loop, false);
            collect_usage_in_block(then_block, usage, in_loop);
            if let Some(eb) = else_block {
                collect_usage_in_block(eb, usage, in_loop);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_usage_in_expr(v, usage, in_loop, false);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn collect_usage_in_expr(
    expr: &TirExpr,
    usage: &mut HashMap<u32, LocalUsage>,
    in_loop: bool,
    in_condition: bool,
) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            let entry = usage.entry(*index).or_default();
            entry.read_count += 1;
            if in_loop && in_condition {
                entry.in_loop_condition = true;
            }
        }
        TirExprKind::Assign { target, value } => {
            // Check if target is a local being assigned
            if let TirExprKind::Local { index, .. } = &target.kind {
                usage.entry(*index).or_default().is_assigned = true;
            }
            collect_usage_in_expr(target, usage, in_loop, in_condition);
            collect_usage_in_expr(value, usage, in_loop, in_condition);
        }
        TirExprKind::Unary { op, expr: inner } => {
            // Check for address-taken
            if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                && let TirExprKind::Local { index, .. } = &inner.kind
            {
                usage.entry(*index).or_default().address_taken = true;
            }
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_usage_in_expr(left, usage, in_loop, in_condition);
            collect_usage_in_expr(right, usage, in_loop, in_condition);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_usage_in_expr(arg, usage, in_loop, in_condition);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_usage_in_expr(receiver, usage, in_loop, in_condition);
            for arg in args {
                collect_usage_in_expr(arg, usage, in_loop, in_condition);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_usage_in_expr(callee, usage, in_loop, in_condition);
            for arg in args {
                collect_usage_in_expr(arg, usage, in_loop, in_condition);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
            collect_usage_in_expr(index, usage, in_loop, in_condition);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
        }
        TirExprKind::Block(block) => {
            collect_usage_in_block(block, usage, in_loop);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_usage_in_block(block, usage, in_loop);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_usage_in_expr(condition, usage, in_loop, in_condition);
            collect_usage_in_block(then_branch, usage, in_loop);
            if let Some(eb) = else_branch {
                collect_usage_in_block(eb, usage, in_loop);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_usage_in_expr(&field.value, usage, in_loop, in_condition);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                collect_usage_in_expr(elem, usage, in_loop, in_condition);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_usage_in_expr(elem, usage, in_loop, in_condition);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_usage_in_expr(value, usage, in_loop, in_condition);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_usage_in_expr(field, usage, in_loop, in_condition);
            }
        }
        TirExprKind::Move { value } => {
            collect_usage_in_expr(value, usage, in_loop, in_condition);
        }
        TirExprKind::Closure { body, captures, .. } => {
            // Mark all captured variables as captured
            for capture in captures {
                usage.entry(capture.outer_index).or_default().is_captured = true;
            }
            collect_usage_in_expr(body, usage, in_loop, in_condition);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
            for arm in arms {
                collect_usage_in_expr(&arm.body, usage, in_loop, in_condition);
            }
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Check if a binding can be safely eliminated via copy propagation.
fn can_propagate_copy(
    binding: &CopyBinding,
    usage: &HashMap<u32, LocalUsage>,
    type_table: &TypeTable,
) -> bool {
    let target_usage = usage.get(&binding.target_local);

    // If the target is never used, it can be eliminated (dead code)
    let Some(target_usage) = target_usage else {
        return true;
    };

    // Don't propagate if target is assigned to after initialization
    if target_usage.is_assigned {
        return false;
    }

    // Don't propagate if address is taken (could be modified through pointer)
    if target_usage.address_taken {
        return false;
    }

    // Don't propagate if target is captured by a closure
    // (closure captures need to preserve the value at capture time)
    if target_usage.is_captured {
        return false;
    }

    match &binding.source {
        CopySource::Local { index, .. } => {
            // For local-to-local copy:
            // Safe if source is not modified after the copy
            let source_usage = usage.get(index);
            if let Some(su) = source_usage {
                // Check if source is assigned
                if su.is_assigned {
                    // Source is assigned somewhere in the function.
                    // But if this binding is inside a labeled block and source is NOT
                    // assigned within that block, it's safe to propagate because
                    // source can't be modified between the binding and use within the block.
                    if binding.block_local_assigned.contains(index) {
                        // Source is assigned within the same labeled block - not safe
                        return false;
                    }
                    // Source is assigned elsewhere but not in this block scope - safe
                }
            }

            // For value types (structs, arrays, tuples, strings), only propagate
            // if the source is dead after this binding (read_count == 1)
            if needs_value_copy(binding.type_id, type_table)
                && let Some(su) = source_usage
            {
                // Source must only be read once (in this binding) and not captured
                if su.read_count > 1 || su.address_taken || su.is_captured {
                    return false;
                }
            }
            // If no usage info, source is unused - safe to eliminate

            true
        }
        // Literals are always safe to propagate
        CopySource::IntLiteral { .. }
        | CopySource::FloatLiteral { .. }
        | CopySource::BoolLiteral(_)
        | CopySource::CharLiteral(_) => true,
    }
}

/// Substitute local references in a block.
/// Replaces uses of `from_local` with the expression from `source`.
fn substitute_in_block(block: &mut TirBlock, from_local: u32, source: &CopySource) {
    for stmt in &mut block.stmts {
        substitute_in_stmt(stmt, from_local, source);
    }
}

fn substitute_in_stmt(stmt: &mut TirStmt, from_local: u32, source: &CopySource) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            substitute_in_expr(value, from_local, source);
        }
        TirStmtKind::Expr(expr) => {
            substitute_in_expr(expr, from_local, source);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                substitute_in_expr(v, from_local, source);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            substitute_in_expr(condition, from_local, source);
            substitute_in_block(then_block, from_local, source);
            if let Some(eb) = else_block {
                substitute_in_block(eb, from_local, source);
            }
        }
        TirStmtKind::While { condition, body } => {
            substitute_in_expr(condition, from_local, source);
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                substitute_in_stmt(s, from_local, source);
            }
            if let Some(c) = condition {
                substitute_in_expr(c, from_local, source);
            }
            if let Some(u) = update {
                substitute_in_expr(u, from_local, source);
            }
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            substitute_in_expr(iterable, from_local, source);
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::Loop { body } => {
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            substitute_in_block(block, from_local, source);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            substitute_in_expr(scrutinee, from_local, source);
            substitute_in_block(then_block, from_local, source);
            if let Some(eb) = else_block {
                substitute_in_block(eb, from_local, source);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                substitute_in_expr(v, from_local, source);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn substitute_in_expr(expr: &mut TirExpr, from_local: u32, source: &CopySource) {
    // Check if this is a local that needs substitution
    if let TirExprKind::Local { index, .. } = &expr.kind
        && *index == from_local
    {
        // Replace with the source expression
        expr.kind = match source {
            CopySource::Local {
                index: src_idx,
                name: src_name,
            } => TirExprKind::Local {
                index: *src_idx,
                name: src_name.clone(),
            },
            CopySource::IntLiteral { value, repr } => TirExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            CopySource::FloatLiteral { value, repr } => TirExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            CopySource::BoolLiteral(b) => TirExprKind::BoolLiteral(*b),
            CopySource::CharLiteral(c) => TirExprKind::CharLiteral(*c),
        };
        return;
    }

    // Recurse into child expressions
    match &mut expr.kind {
        TirExprKind::Local { .. } => {
            // Already handled above
        }
        TirExprKind::Binary { left, right, .. } => {
            substitute_in_expr(left, from_local, source);
            substitute_in_expr(right, from_local, source);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            substitute_in_expr(inner, from_local, source);
        }
        TirExprKind::Assign { target, value } => {
            substitute_in_expr(target, from_local, source);
            substitute_in_expr(value, from_local, source);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                substitute_in_expr(arg, from_local, source);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            substitute_in_expr(receiver, from_local, source);
            for arg in args {
                substitute_in_expr(arg, from_local, source);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            substitute_in_expr(callee, from_local, source);
            for arg in args {
                substitute_in_expr(arg, from_local, source);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            substitute_in_expr(inner, from_local, source);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            substitute_in_expr(inner, from_local, source);
            substitute_in_expr(index, from_local, source);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            substitute_in_expr(inner, from_local, source);
        }
        TirExprKind::Block(block) => {
            substitute_in_block(block, from_local, source);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            substitute_in_block(block, from_local, source);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            substitute_in_expr(condition, from_local, source);
            substitute_in_block(then_branch, from_local, source);
            if let Some(eb) = else_branch {
                substitute_in_block(eb, from_local, source);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                substitute_in_expr(&mut field.value, from_local, source);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                substitute_in_expr(elem, from_local, source);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                substitute_in_expr(elem, from_local, source);
            }
        }
        TirExprKind::OptionSome { value } => {
            substitute_in_expr(value, from_local, source);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                substitute_in_expr(field, from_local, source);
            }
        }
        TirExprKind::Move { value } => {
            substitute_in_expr(value, from_local, source);
        }
        TirExprKind::Closure { body, .. } => {
            substitute_in_expr(body, from_local, source);
        }
        TirExprKind::Match { expr: inner, arms } => {
            substitute_in_expr(inner, from_local, source);
            for arm in arms {
                substitute_in_expr(&mut arm.body, from_local, source);
            }
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Collect copy bindings from statements.
/// `block_local_assigned` contains locals assigned within the current labeled block scope.
fn collect_copy_bindings(
    stmts: &[TirStmt],
    bindings: &mut Vec<CopyBinding>,
    block_local_assigned: &HashSet<u32>,
) {
    for stmt in stmts {
        if let Some(mut binding) = analyze_copy_binding(stmt) {
            binding.block_local_assigned = block_local_assigned.clone();
            bindings.push(binding);
        }
        collect_copy_bindings_in_stmt(stmt, bindings, block_local_assigned);
    }
}

fn collect_copy_bindings_in_stmt(
    stmt: &TirStmt,
    bindings: &mut Vec<CopyBinding>,
    block_local_assigned: &HashSet<u32>,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
        }
        TirStmtKind::Expr(expr) => {
            collect_copy_bindings_in_expr(expr, bindings, block_local_assigned);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_copy_bindings_in_expr(v, bindings, block_local_assigned);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_copy_bindings_in_expr(condition, bindings, block_local_assigned);
            collect_copy_bindings(&then_block.stmts, bindings, block_local_assigned);
            if let Some(eb) = else_block {
                collect_copy_bindings(&eb.stmts, bindings, block_local_assigned);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_copy_bindings_in_expr(condition, bindings, block_local_assigned);
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                collect_copy_bindings_in_stmt(s, bindings, block_local_assigned);
            }
            if let Some(c) = condition {
                collect_copy_bindings_in_expr(c, bindings, block_local_assigned);
            }
            if let Some(u) = update {
                collect_copy_bindings_in_expr(u, bindings, block_local_assigned);
            }
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            collect_copy_bindings_in_expr(iterable, bindings, block_local_assigned);
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::Loop { body } => {
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            // For labeled blocks, compute which locals are assigned within
            let local_assigned = collect_assigned_in_block(block);
            collect_copy_bindings(&block.stmts, bindings, &local_assigned);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_copy_bindings_in_expr(scrutinee, bindings, block_local_assigned);
            collect_copy_bindings(&then_block.stmts, bindings, block_local_assigned);
            if let Some(eb) = else_block {
                collect_copy_bindings(&eb.stmts, bindings, block_local_assigned);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_copy_bindings_in_expr(v, bindings, block_local_assigned);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn collect_copy_bindings_in_expr(
    expr: &TirExpr,
    bindings: &mut Vec<CopyBinding>,
    block_local_assigned: &HashSet<u32>,
) {
    match &expr.kind {
        TirExprKind::Block(block) => {
            collect_copy_bindings(&block.stmts, bindings, block_local_assigned);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            // For labeled block expressions, compute which locals are assigned within
            let local_assigned = collect_assigned_in_block(block);
            collect_copy_bindings(&block.stmts, bindings, &local_assigned);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_copy_bindings_in_expr(condition, bindings, block_local_assigned);
            collect_copy_bindings(&then_branch.stmts, bindings, block_local_assigned);
            if let Some(eb) = else_branch {
                collect_copy_bindings(&eb.stmts, bindings, block_local_assigned);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_copy_bindings_in_expr(left, bindings, block_local_assigned);
            collect_copy_bindings_in_expr(right, bindings, block_local_assigned);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_copy_bindings_in_expr(inner, bindings, block_local_assigned);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings, block_local_assigned);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_copy_bindings_in_expr(receiver, bindings, block_local_assigned);
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings, block_local_assigned);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_copy_bindings_in_expr(callee, bindings, block_local_assigned);
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings, block_local_assigned);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. } => {
            collect_copy_bindings_in_expr(inner, bindings, block_local_assigned);
        }
        TirExprKind::Assign { target, value } => {
            collect_copy_bindings_in_expr(target, bindings, block_local_assigned);
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_copy_bindings_in_expr(&field.value, bindings, block_local_assigned);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_copy_bindings_in_expr(elem, bindings, block_local_assigned);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_copy_bindings_in_expr(field, bindings, block_local_assigned);
            }
        }
        TirExprKind::Move { value } => {
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
        }
        TirExprKind::Closure { body, .. } => {
            collect_copy_bindings_in_expr(body, bindings, block_local_assigned);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_copy_bindings_in_expr(inner, bindings, block_local_assigned);
            for arm in arms {
                collect_copy_bindings_in_expr(&arm.body, bindings, block_local_assigned);
            }
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Remove dead copy bindings from statements.
fn remove_copy_bindings(stmts: &mut Vec<TirStmt>, dead_locals: &HashSet<u32>) {
    stmts.retain(|stmt| {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            !dead_locals.contains(local_index)
        } else {
            true
        }
    });

    for stmt in stmts {
        remove_copy_bindings_in_stmt(stmt, dead_locals);
    }
}

fn remove_copy_bindings_in_stmt(stmt: &mut TirStmt, dead_locals: &HashSet<u32>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirStmtKind::Expr(expr) => {
            remove_copy_bindings_in_expr(expr, dead_locals);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                remove_copy_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            remove_copy_bindings_in_expr(condition, dead_locals);
            remove_copy_bindings(&mut then_block.stmts, dead_locals);
            if let Some(eb) = else_block {
                remove_copy_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirStmtKind::While { condition, body } => {
            remove_copy_bindings_in_expr(condition, dead_locals);
            remove_copy_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                remove_copy_bindings_in_stmt(s, dead_locals);
            }
            if let Some(c) = condition {
                remove_copy_bindings_in_expr(c, dead_locals);
            }
            if let Some(u) = update {
                remove_copy_bindings_in_expr(u, dead_locals);
            }
            remove_copy_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            remove_copy_bindings_in_expr(iterable, dead_locals);
            remove_copy_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::Loop { body } => {
            remove_copy_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            remove_copy_bindings(&mut block.stmts, dead_locals);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            remove_copy_bindings_in_expr(scrutinee, dead_locals);
            remove_copy_bindings(&mut then_block.stmts, dead_locals);
            if let Some(eb) = else_block {
                remove_copy_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                remove_copy_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn remove_copy_bindings_in_expr(expr: &mut TirExpr, dead_locals: &HashSet<u32>) {
    match &mut expr.kind {
        TirExprKind::Block(block) => {
            remove_copy_bindings(&mut block.stmts, dead_locals);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            remove_copy_bindings(&mut block.stmts, dead_locals);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            remove_copy_bindings_in_expr(condition, dead_locals);
            remove_copy_bindings(&mut then_branch.stmts, dead_locals);
            if let Some(eb) = else_branch {
                remove_copy_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            remove_copy_bindings_in_expr(left, dead_locals);
            remove_copy_bindings_in_expr(right, dead_locals);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            remove_copy_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                remove_copy_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            remove_copy_bindings_in_expr(receiver, dead_locals);
            for arg in args {
                remove_copy_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            remove_copy_bindings_in_expr(callee, dead_locals);
            for arg in args {
                remove_copy_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. } => {
            remove_copy_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Assign { target, value } => {
            remove_copy_bindings_in_expr(target, dead_locals);
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                remove_copy_bindings_in_expr(&mut field.value, dead_locals);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                remove_copy_bindings_in_expr(elem, dead_locals);
            }
        }
        TirExprKind::OptionSome { value } => {
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                remove_copy_bindings_in_expr(field, dead_locals);
            }
        }
        TirExprKind::Move { value } => {
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::Closure { body, .. } => {
            remove_copy_bindings_in_expr(body, dead_locals);
        }
        TirExprKind::Match { expr: inner, arms } => {
            remove_copy_bindings_in_expr(inner, dead_locals);
            for arm in arms {
                remove_copy_bindings_in_expr(&mut arm.body, dead_locals);
            }
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => {}
    }
}

/// Eliminate trivial copy bindings in a function.
fn propagate_copies_in_function(func: &mut TirFunction, type_table: &TypeTable) {
    let Some(body) = &mut func.body else {
        return;
    };

    // Iterate until no more changes
    // We process ONE binding per iteration to avoid interference between substitutions
    // (e.g., if `let a = 5; let x = a;`, substituting both at once would break references)
    loop {
        // Collect all copy bindings
        let mut copy_bindings: Vec<CopyBinding> = Vec::new();
        // Start with empty set - bindings inside labeled blocks will get their own set
        collect_copy_bindings(&body.stmts, &mut copy_bindings, &HashSet::new());

        if copy_bindings.is_empty() {
            break;
        }

        // Collect usage information
        let usage = collect_local_usage(body);

        // Find FIRST binding that can be eliminated (one at a time for safety)
        let mut to_eliminate: Option<CopyBinding> = None;
        for binding in copy_bindings {
            if can_propagate_copy(&binding, &usage, type_table) {
                to_eliminate = Some(binding);
                break;
            }
        }

        let Some(binding) = to_eliminate else {
            break;
        };

        // Apply substitution for this one binding
        substitute_in_block(body, binding.target_local, &binding.source);

        // Remove the dead binding
        let dead_locals: HashSet<u32> = [binding.target_local].into_iter().collect();
        remove_copy_bindings(&mut body.stmts, &dead_locals);
    }
}

/// Apply copy propagation to all functions in the project.
fn propagate_copies(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            propagate_copies_in_function(&mut func, &type_table);
        }
    }
}

/// Apply Loop-Invariant Code Motion to all functions in the project.
fn apply_licm(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            licm_function(&mut func, &type_table);
        }
    }
}

// =============================================================================
// Move Insertion Optimization
// =============================================================================

/// Check if an expression produces a fresh value that can be moved.
/// Fresh values are those that don't need copying because they're newly created.
fn is_fresh_value(expr: &TirExpr) -> bool {
    match &expr.kind {
        // Literals always produce fresh values
        TirExprKind::StringLiteral(_)
        | TirExprKind::StructLiteral { .. }
        | TirExprKind::ArrayLiteral { .. }
        | TirExprKind::TupleLiteral { .. }
        | TirExprKind::Null => true,

        // All call variants return fresh values (callee constructs/copies the return value)
        TirExprKind::Call { .. }
        | TirExprKind::StaticCall { .. }
        | TirExprKind::MethodCall { .. }
        | TirExprKind::EffectCall { .. }
        | TirExprKind::IndirectCall { .. } => true,

        // OptionSome is fresh if its inner value is fresh
        TirExprKind::OptionSome { value } => is_fresh_value(value),

        // VariantConstruct is fresh (it's a literal-like construction)
        TirExprKind::VariantConstruct { .. } => true,

        // Move is already marked as fresh
        TirExprKind::Move { .. } => true,

        // Everything else is not fresh
        _ => false,
    }
}

/// Check if a type requires value copying (composite types with value semantics).
fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
        ResolvedType::Tuple(elements) => !elements.is_empty(),
        ResolvedType::Option(inner) => needs_value_copy(*inner, type_table),
        // References, primitives, etc. don't need copying
        _ => false,
    }
}

/// Wrap an expression in Move if it's a fresh value that would otherwise be copied.
fn wrap_in_move_if_eligible(expr: TirExpr, type_table: &TypeTable) -> TirExpr {
    if needs_value_copy(expr.type_id, type_table) && is_fresh_value(&expr) {
        let type_id = expr.type_id;
        let span = expr.span;
        TirExpr::new(
            TirExprKind::Move {
                value: Box::new(expr),
            },
            type_id,
            span,
        )
    } else {
        expr
    }
}

/// Insert move semantics for fresh values in a block.
fn insert_moves_in_block(block: &mut TirBlock, type_table: &TypeTable) {
    for stmt in &mut block.stmts {
        insert_moves_in_stmt(stmt, type_table);
    }
}

/// Insert move semantics for fresh values in a statement.
fn insert_moves_in_stmt(stmt: &mut TirStmt, type_table: &TypeTable) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            // First recursively process nested expressions (e.g., LabeledBlock containing Let)
            insert_moves_in_expr(value, type_table);
            // Then wrap the value in Move if eligible
            let old_value = std::mem::replace(
                value,
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, stmt.span),
            );
            *value = wrap_in_move_if_eligible(old_value, type_table);
        }
        TirStmtKind::Expr(expr) => {
            insert_moves_in_expr(expr, type_table);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                insert_moves_in_expr(v, type_table);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            insert_moves_in_expr(condition, type_table);
            insert_moves_in_block(then_block, type_table);
            if let Some(eb) = else_block {
                insert_moves_in_block(eb, type_table);
            }
        }
        TirStmtKind::While { condition, body } => {
            insert_moves_in_expr(condition, type_table);
            insert_moves_in_block(body, type_table);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                insert_moves_in_stmt(s, type_table);
            }
            if let Some(c) = condition {
                insert_moves_in_expr(c, type_table);
            }
            if let Some(u) = update {
                insert_moves_in_expr(u, type_table);
            }
            insert_moves_in_block(body, type_table);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            insert_moves_in_expr(iterable, type_table);
            insert_moves_in_block(body, type_table);
        }
        TirStmtKind::Loop { body } => {
            insert_moves_in_block(body, type_table);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            insert_moves_in_block(block, type_table);
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                insert_moves_in_expr(v, type_table);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            insert_moves_in_expr(scrutinee, type_table);
            insert_moves_in_block(then_block, type_table);
            if let Some(eb) = else_block {
                insert_moves_in_block(eb, type_table);
            }
        }
    }
}

/// Insert move semantics in nested expressions (for consistency).
fn insert_moves_in_expr(expr: &mut TirExpr, type_table: &TypeTable) {
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            insert_moves_in_expr(left, type_table);
            insert_moves_in_expr(right, type_table);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            insert_moves_in_expr(inner, type_table);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            // Wrap arguments in Move if they are fresh values (argument passing is assignment)
            for arg in args.iter_mut() {
                insert_moves_in_expr(arg, type_table);
            }
            for i in 0..args.len() {
                let arg = std::mem::replace(
                    &mut args[i],
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                );
                args[i] = wrap_in_move_if_eligible(arg, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            insert_moves_in_expr(receiver, type_table);
            // Wrap arguments in Move if they are fresh values
            for arg in args.iter_mut() {
                insert_moves_in_expr(arg, type_table);
            }
            for i in 0..args.len() {
                let arg = std::mem::replace(
                    &mut args[i],
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                );
                args[i] = wrap_in_move_if_eligible(arg, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            insert_moves_in_expr(callee, type_table);
            // Wrap arguments in Move if they are fresh values
            for arg in args.iter_mut() {
                insert_moves_in_expr(arg, type_table);
            }
            for i in 0..args.len() {
                let arg = std::mem::replace(
                    &mut args[i],
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                );
                args[i] = wrap_in_move_if_eligible(arg, type_table);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            insert_moves_in_expr(inner, type_table);
        }
        TirExprKind::Index { expr: inner, index } => {
            insert_moves_in_expr(inner, type_table);
            insert_moves_in_expr(index, type_table);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            insert_moves_in_expr(inner, type_table);
        }
        TirExprKind::Assign { target, value } => {
            insert_moves_in_expr(target, type_table);
            // Wrap the assigned value in Move if eligible (same as Let)
            let old_value = std::mem::replace(
                value.as_mut(),
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
            );
            **value = wrap_in_move_if_eligible(old_value, type_table);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                insert_moves_in_expr(&mut field.value, type_table);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                insert_moves_in_expr(elem, type_table);
            }
        }
        TirExprKind::OptionSome { value } => {
            insert_moves_in_expr(value, type_table);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                insert_moves_in_expr(field, type_table);
            }
        }
        TirExprKind::Move { value } => {
            insert_moves_in_expr(value, type_table);
        }
        TirExprKind::Block(block) => {
            insert_moves_in_block(block, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            insert_moves_in_expr(condition, type_table);
            insert_moves_in_block(then_branch, type_table);
            if let Some(eb) = else_branch {
                insert_moves_in_block(eb, type_table);
            }
        }
        TirExprKind::Closure { body, .. } => {
            insert_moves_in_expr(body, type_table);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            insert_moves_in_block(block, type_table);
        }
        // Leaf nodes - no nested expressions
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
        | TirExprKind::Match { .. } => {}
    }
}

/// Insert move optimization for all functions in the project.
fn insert_moves(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(ref mut body) = func.body {
                insert_moves_in_block(body, &type_table);
            }
        }
    }
}

// =============================================================================
// Value Copy Type Collection
// =============================================================================

/// Collect all types that need value copying in a function body.
/// This is needed for codegen to pre-allocate scratch locals for copy operations.
fn collect_value_copy_types_in_block(
    block: &TirBlock,
    type_table: &TypeTable,
    copy_types: &mut std::collections::HashSet<TypeId>,
) {
    for stmt in &block.stmts {
        collect_value_copy_types_in_stmt(stmt, type_table, copy_types);
    }
}

/// Collect value copy types from a statement.
fn collect_value_copy_types_in_stmt(
    stmt: &TirStmt,
    type_table: &TypeTable,
    copy_types: &mut std::collections::HashSet<TypeId>,
) {
    match &stmt.kind {
        TirStmtKind::Let { type_id, value, .. } => {
            // If assigning to a value type from a non-fresh expression, we need copy
            if needs_value_copy(*type_id, type_table) && !is_fresh_value(value) {
                copy_types.insert(*type_id);
            }
            collect_value_copy_types_in_expr(value, type_table, copy_types);
        }
        TirStmtKind::Expr(expr) => {
            collect_value_copy_types_in_expr(expr, type_table, copy_types);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_value_copy_types_in_expr(v, type_table, copy_types);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_value_copy_types_in_expr(condition, type_table, copy_types);
            collect_value_copy_types_in_block(then_block, type_table, copy_types);
            if let Some(eb) = else_block {
                collect_value_copy_types_in_block(eb, type_table, copy_types);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_value_copy_types_in_expr(condition, type_table, copy_types);
            collect_value_copy_types_in_block(body, type_table, copy_types);
        }
        TirStmtKind::Loop { body } => {
            collect_value_copy_types_in_block(body, type_table, copy_types);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                collect_value_copy_types_in_stmt(s, type_table, copy_types);
            }
            if let Some(cond) = condition {
                collect_value_copy_types_in_expr(cond, type_table, copy_types);
            }
            if let Some(upd) = update {
                collect_value_copy_types_in_expr(upd, type_table, copy_types);
            }
            collect_value_copy_types_in_block(body, type_table, copy_types);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            collect_value_copy_types_in_expr(iterable, type_table, copy_types);
            collect_value_copy_types_in_block(body, type_table, copy_types);
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_value_copy_types_in_expr(v, type_table, copy_types);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_value_copy_types_in_expr(scrutinee, type_table, copy_types);
            collect_value_copy_types_in_block(then_block, type_table, copy_types);
            if let Some(eb) = else_block {
                collect_value_copy_types_in_block(eb, type_table, copy_types);
            }
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_value_copy_types_in_block(block, type_table, copy_types);
        }
    }
}

/// Collect value copy types from an expression.
fn collect_value_copy_types_in_expr(
    expr: &TirExpr,
    type_table: &TypeTable,
    copy_types: &mut std::collections::HashSet<TypeId>,
) {
    match &expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            collect_value_copy_types_in_expr(left, type_table, copy_types);
            collect_value_copy_types_in_expr(right, type_table, copy_types);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_value_copy_types_in_expr(inner, type_table, copy_types);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_value_copy_types_in_expr(arg, type_table, copy_types);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_value_copy_types_in_expr(receiver, type_table, copy_types);
            for arg in args {
                collect_value_copy_types_in_expr(arg, type_table, copy_types);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_value_copy_types_in_expr(callee, type_table, copy_types);
            for arg in args {
                collect_value_copy_types_in_expr(arg, type_table, copy_types);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            // Field access on a value type requires a copy source local
            if needs_value_copy(inner.type_id, type_table) && !is_fresh_value(inner) {
                copy_types.insert(inner.type_id);
            }
            collect_value_copy_types_in_expr(inner, type_table, copy_types);
        }
        TirExprKind::Index { expr: inner, index } => {
            // Index access on a value type (tuple) requires a copy source local
            if needs_value_copy(inner.type_id, type_table) && !is_fresh_value(inner) {
                copy_types.insert(inner.type_id);
            }
            collect_value_copy_types_in_expr(inner, type_table, copy_types);
            collect_value_copy_types_in_expr(index, type_table, copy_types);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_value_copy_types_in_expr(inner, type_table, copy_types);
        }
        TirExprKind::Assign { target, value } => {
            collect_value_copy_types_in_expr(target, type_table, copy_types);
            // If assigning a value type, we might need to copy
            if needs_value_copy(value.type_id, type_table) && !is_fresh_value(value) {
                copy_types.insert(value.type_id);
            }
            collect_value_copy_types_in_expr(value, type_table, copy_types);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_value_copy_types_in_expr(&field.value, type_table, copy_types);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_value_copy_types_in_expr(elem, type_table, copy_types);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_value_copy_types_in_expr(value, type_table, copy_types);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_value_copy_types_in_expr(field, type_table, copy_types);
            }
        }
        TirExprKind::Move { value } => {
            collect_value_copy_types_in_expr(value, type_table, copy_types);
        }
        TirExprKind::Block(block) => {
            collect_value_copy_types_in_block(block, type_table, copy_types);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_value_copy_types_in_expr(condition, type_table, copy_types);
            collect_value_copy_types_in_block(then_branch, type_table, copy_types);
            if let Some(eb) = else_branch {
                collect_value_copy_types_in_block(eb, type_table, copy_types);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_value_copy_types_in_expr(body, type_table, copy_types);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_value_copy_types_in_block(block, type_table, copy_types);
        }
        // Leaf nodes - no nested expressions
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
        | TirExprKind::Match { .. } => {}
    }
}

/// Collect value copy types for all functions in the project.
/// This populates `needed_copy_types` which codegen uses to pre-allocate scratch locals.
fn collect_value_copy_types(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            // Collect into a temporary set first to avoid borrow conflicts
            let mut copy_types = std::collections::HashSet::new();
            if let Some(ref body) = func.body {
                collect_value_copy_types_in_block(body, &type_table, &mut copy_types);
            }
            func.needed_copy_types.extend(copy_types);
        }
    }
}

/// Optimize a Project by analyzing and populating its usage fields.
///
/// This is the main entry point for the optimizer. Based on the optimization
/// level, it either performs DCE analysis or enables all features.
pub fn optimize(mut project: Project, opt_level: OptLevel) -> Project {
    match opt_level {
        OptLevel::None => {
            populate_all_features(&mut project);
        }
        OptLevel::Basic => {
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
        }
        OptLevel::Full => {
            inline_functions(&mut project);
            eliminate_unnecessary_refs(&mut project);
            propagate_copies(&mut project);
            apply_licm(&mut project);
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
        }
        OptLevel::Size => {
            inline_functions(&mut project);
            eliminate_unnecessary_refs(&mut project);
            propagate_copies(&mut project);
            apply_licm(&mut project);
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
            project.strip_names = true;
        }
    }

    // Insert move optimization for all optimization levels (after inlining)
    // This eliminates unnecessary copies for fresh values
    insert_moves(&mut project);

    // Collect value copy types for all functions
    // This populates needed_copy_types for codegen to pre-allocate scratch locals
    collect_value_copy_types(&mut project);

    project
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
