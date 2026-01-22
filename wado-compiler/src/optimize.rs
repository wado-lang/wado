//! Optimization pass for Wado TIR
//!
//! This module provides:
//! - Dead Code Elimination (DCE) at function level
//! - Usage analysis for conditional feature inclusion
//! - Function inlining (via `optimize_inline` module)

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

/// WASI effects that can be used in Wado programs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WasiEffect {
    Stdout,
    Stderr,
    Environment,
    MonotonicClock,
    Exit,
}

impl WasiEffect {
    /// Parse effect name from string
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "Stdout" => Some(Self::Stdout),
            "Stderr" => Some(Self::Stderr),
            "Environment" => Some(Self::Environment),
            "MonotonicClock" => Some(Self::MonotonicClock),
            "Exit" => Some(Self::Exit),
            _ => None,
        }
    }

    /// Get the effect name as a string (for WASI interface names)
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdout => "Stdout",
            Self::Stderr => "Stderr",
            Self::Environment => "Environment",
            Self::MonotonicClock => "MonotonicClock",
            Self::Exit => "Exit",
        }
    }

    /// Standard effects (all except Exit which requires explicit usage)
    pub const STANDARD: &'static [WasiEffect] = &[
        WasiEffect::Stdout,
        WasiEffect::Stderr,
        WasiEffect::Environment,
        WasiEffect::MonotonicClock,
    ];
}

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
/// and populates the project's `reachable_functions`, `used_effects`,
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

    // Collect used effects and box primitives from reachable functions
    let mut used_effects: HashSet<WasiEffect> = HashSet::new();
    let mut used_wasi_functions: HashSet<String> = HashSet::new();
    let mut used_box_primitives: HashSet<PrimitiveType> = HashSet::new();
    for func_id in &reachable {
        if let Some(effects) = effect_usage.get(func_id) {
            for (effect_name, op_name) in effects {
                if let Some(effect) = WasiEffect::from_str(effect_name) {
                    used_effects.insert(effect);
                }
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

    // Add cm_list_string_to_array and helper if Environment effect functions are used
    // This conversion function is called from codegen, not Wado code
    // We need to compute transitive closure to include all functions they call
    if used_wasi_functions.contains("Environment::get_arguments")
        || used_wasi_functions.contains("Environment::get_environment")
    {
        let cm_list_func = core_internal("cm_list_string_to_array");
        let copy_string_func = core_internal("copy_string_from_linear");

        // Compute reachable functions from these entry points
        let cm_list_reachable = compute_reachable(&call_graph, &cm_list_func);
        let copy_string_reachable = compute_reachable(&call_graph, &copy_string_func);

        // Add all transitively reachable functions
        reachable.extend(cm_list_reachable);
        reachable.extend(copy_string_reachable);
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

    // Also mark effects as used if indirect calls are present (for ambient logging)
    if reachable
        .iter()
        .any(|func_id| matches!(func_id, FunctionId::Free(f) if is_builtin_call_indirect_stdout(f)))
    {
        used_effects.insert(WasiEffect::Stdout);
        used_wasi_functions.insert("Stdout::write_via_stream".to_string());
    }
    if reachable
        .iter()
        .any(|func_id| matches!(func_id, FunctionId::Free(f) if is_builtin_call_indirect_stderr(f)))
    {
        used_effects.insert(WasiEffect::Stderr);
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
    if !used_effects.is_empty() || uses_stream_builtins {
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
    project.used_effects = used_effects;
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
    // Standard effects (all except Exit which requires explicit usage)
    project.used_effects = WasiEffect::STANDARD.iter().copied().collect();
    // Standard WASI functions from the stdlib registry
    let (wasi_registry, _world_registry) = WasiRegistry::build_from_stdlib();
    project.used_wasi_functions = wasi_registry
        .all_function_names()
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
                condition,
                body,
                update,
            } => {
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
            condition,
            body,
            update,
        } => {
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
        TirStmtKind::Break { .. } | TirStmtKind::Continue => {}
    }
}

fn collect_modified_vars_in_expr(expr: &TirExpr, modified: &mut HashSet<u32>) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            // Check if target is a local variable being assigned
            if let TirExprKind::Local { index, .. } = &target.kind {
                modified.insert(*index);
            }
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
        | TirExprKind::Match { .. }
        | TirExprKind::LabeledBlock { .. } => {}
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

/// Represents a hoistable expression with its replacement info
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
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut HashSet<(u32, u32)>, // (local_index, field_index) pairs already seen
    next_local: &mut u32,
) {
    for stmt in &block.stmts {
        find_hoist_candidates_in_stmt(stmt, modified_vars, candidates, seen, next_local);
    }
}

fn find_hoist_candidates_in_stmt(
    stmt: &TirStmt,
    modified_vars: &HashSet<u32>,
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut HashSet<(u32, u32)>,
    next_local: &mut u32,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            find_hoist_candidates_in_expr(value, modified_vars, candidates, seen, next_local);
        }
        TirStmtKind::Expr(expr) => {
            find_hoist_candidates_in_expr(expr, modified_vars, candidates, seen, next_local);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                find_hoist_candidates_in_expr(v, modified_vars, candidates, seen, next_local);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            find_hoist_candidates_in_expr(condition, modified_vars, candidates, seen, next_local);
            find_hoist_candidates_in_block(then_block, modified_vars, candidates, seen, next_local);
            if let Some(eb) = else_block {
                find_hoist_candidates_in_block(eb, modified_vars, candidates, seen, next_local);
            }
        }
        TirStmtKind::While { condition, body } => {
            find_hoist_candidates_in_expr(condition, modified_vars, candidates, seen, next_local);
            find_hoist_candidates_in_block(body, modified_vars, candidates, seen, next_local);
        }
        TirStmtKind::For {
            condition,
            body,
            update,
        } => {
            if let Some(c) = condition {
                find_hoist_candidates_in_expr(c, modified_vars, candidates, seen, next_local);
            }
            find_hoist_candidates_in_block(body, modified_vars, candidates, seen, next_local);
            if let Some(u) = update {
                find_hoist_candidates_in_expr(u, modified_vars, candidates, seen, next_local);
            }
        }
        TirStmtKind::Loop { body } => {
            find_hoist_candidates_in_block(body, modified_vars, candidates, seen, next_local);
        }
        TirStmtKind::ForOf { body, .. } => {
            find_hoist_candidates_in_block(body, modified_vars, candidates, seen, next_local);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            find_hoist_candidates_in_block(block, modified_vars, candidates, seen, next_local);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            find_hoist_candidates_in_expr(scrutinee, modified_vars, candidates, seen, next_local);
            find_hoist_candidates_in_block(then_block, modified_vars, candidates, seen, next_local);
            if let Some(eb) = else_block {
                find_hoist_candidates_in_block(eb, modified_vars, candidates, seen, next_local);
            }
        }
        TirStmtKind::Break { .. } | TirStmtKind::Continue => {}
    }
}

fn find_hoist_candidates_in_expr(
    expr: &TirExpr,
    modified_vars: &HashSet<u32>,
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
            if let TirExprKind::Local { index, name } = &inner.kind
                && !modified_vars.contains(index)
            {
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
            // Still recurse into inner expression
            find_hoist_candidates_in_expr(inner, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::Binary { left, right, .. } => {
            find_hoist_candidates_in_expr(left, modified_vars, candidates, seen, next_local);
            find_hoist_candidates_in_expr(right, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::Unary { expr, .. } => {
            find_hoist_candidates_in_expr(expr, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::Assign { target, value } => {
            find_hoist_candidates_in_expr(target, modified_vars, candidates, seen, next_local);
            find_hoist_candidates_in_expr(value, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::Cast { expr, .. } => {
            find_hoist_candidates_in_expr(expr, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                find_hoist_candidates_in_expr(arg, modified_vars, candidates, seen, next_local);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            find_hoist_candidates_in_expr(receiver, modified_vars, candidates, seen, next_local);
            for arg in args {
                find_hoist_candidates_in_expr(arg, modified_vars, candidates, seen, next_local);
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                find_hoist_candidates_in_expr(arg, modified_vars, candidates, seen, next_local);
            }
        }
        TirExprKind::Index { expr, index } => {
            find_hoist_candidates_in_expr(expr, modified_vars, candidates, seen, next_local);
            find_hoist_candidates_in_expr(index, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::Block(block) => {
            find_hoist_candidates_in_block(block, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            find_hoist_candidates_in_expr(condition, modified_vars, candidates, seen, next_local);
            find_hoist_candidates_in_block(
                then_branch,
                modified_vars,
                candidates,
                seen,
                next_local,
            );
            if let Some(eb) = else_branch {
                find_hoist_candidates_in_block(eb, modified_vars, candidates, seen, next_local);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                find_hoist_candidates_in_expr(
                    &field.value,
                    modified_vars,
                    candidates,
                    seen,
                    next_local,
                );
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                find_hoist_candidates_in_expr(elem, modified_vars, candidates, seen, next_local);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                find_hoist_candidates_in_expr(elem, modified_vars, candidates, seen, next_local);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            find_hoist_candidates_in_expr(callee, modified_vars, candidates, seen, next_local);
            for arg in args {
                find_hoist_candidates_in_expr(arg, modified_vars, candidates, seen, next_local);
            }
        }
        TirExprKind::Closure { body, .. } => {
            find_hoist_candidates_in_expr(body, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::OptionSome { value } => {
            find_hoist_candidates_in_expr(value, modified_vars, candidates, seen, next_local);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                find_hoist_candidates_in_expr(field, modified_vars, candidates, seen, next_local);
            }
        }
        TirExprKind::Move { value } => {
            find_hoist_candidates_in_expr(value, modified_vars, candidates, seen, next_local);
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
        | TirExprKind::Match { .. }
        | TirExprKind::LabeledBlock { .. } => {}
    }
}

/// Replace field accesses with references to hoisted locals
fn replace_hoisted_in_block(block: &mut TirBlock, candidates: &[HoistCandidate]) {
    for stmt in &mut block.stmts {
        replace_hoisted_in_stmt(stmt, candidates);
    }
}

fn replace_hoisted_in_stmt(stmt: &mut TirStmt, candidates: &[HoistCandidate]) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            replace_hoisted_in_expr(value, candidates);
        }
        TirStmtKind::Expr(expr) => {
            replace_hoisted_in_expr(expr, candidates);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                replace_hoisted_in_expr(v, candidates);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            replace_hoisted_in_expr(condition, candidates);
            replace_hoisted_in_block(then_block, candidates);
            if let Some(eb) = else_block {
                replace_hoisted_in_block(eb, candidates);
            }
        }
        TirStmtKind::While { condition, body } => {
            replace_hoisted_in_expr(condition, candidates);
            replace_hoisted_in_block(body, candidates);
        }
        TirStmtKind::For {
            condition,
            body,
            update,
        } => {
            if let Some(c) = condition {
                replace_hoisted_in_expr(c, candidates);
            }
            replace_hoisted_in_block(body, candidates);
            if let Some(u) = update {
                replace_hoisted_in_expr(u, candidates);
            }
        }
        TirStmtKind::Loop { body } => {
            replace_hoisted_in_block(body, candidates);
        }
        TirStmtKind::ForOf { body, .. } => {
            replace_hoisted_in_block(body, candidates);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            replace_hoisted_in_block(block, candidates);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            replace_hoisted_in_expr(scrutinee, candidates);
            replace_hoisted_in_block(then_block, candidates);
            if let Some(eb) = else_block {
                replace_hoisted_in_block(eb, candidates);
            }
        }
        TirStmtKind::Break { .. } | TirStmtKind::Continue => {}
    }
}

fn replace_hoisted_in_expr(expr: &mut TirExpr, candidates: &[HoistCandidate]) {
    // First, check if this expression matches a hoist candidate
    if let TirExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &expr.kind
        && let TirExprKind::Local { index, .. } = &inner.kind
    {
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
    }

    // Recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. } => {
            replace_hoisted_in_expr(inner, candidates);
        }
        TirExprKind::Binary { left, right, .. } => {
            replace_hoisted_in_expr(left, candidates);
            replace_hoisted_in_expr(right, candidates);
        }
        TirExprKind::Unary { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates);
        }
        TirExprKind::Assign { target, value } => {
            replace_hoisted_in_expr(target, candidates);
            replace_hoisted_in_expr(value, candidates);
        }
        TirExprKind::Cast { expr, .. } => {
            replace_hoisted_in_expr(expr, candidates);
        }
        TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                replace_hoisted_in_expr(arg, candidates);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            replace_hoisted_in_expr(receiver, candidates);
            for arg in args {
                replace_hoisted_in_expr(arg, candidates);
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                replace_hoisted_in_expr(arg, candidates);
            }
        }
        TirExprKind::Index { expr, index } => {
            replace_hoisted_in_expr(expr, candidates);
            replace_hoisted_in_expr(index, candidates);
        }
        TirExprKind::Block(block) => {
            replace_hoisted_in_block(block, candidates);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            replace_hoisted_in_expr(condition, candidates);
            replace_hoisted_in_block(then_branch, candidates);
            if let Some(eb) = else_branch {
                replace_hoisted_in_block(eb, candidates);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                replace_hoisted_in_expr(&mut field.value, candidates);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                replace_hoisted_in_expr(elem, candidates);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                replace_hoisted_in_expr(elem, candidates);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            replace_hoisted_in_expr(callee, candidates);
            for arg in args {
                replace_hoisted_in_expr(arg, candidates);
            }
        }
        TirExprKind::Closure { body, .. } => {
            replace_hoisted_in_expr(body, candidates);
        }
        TirExprKind::OptionSome { value } => {
            replace_hoisted_in_expr(value, candidates);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                replace_hoisted_in_expr(field, candidates);
            }
        }
        TirExprKind::Move { value } => {
            replace_hoisted_in_expr(value, candidates);
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
        | TirExprKind::Match { .. }
        | TirExprKind::LabeledBlock { .. } => {}
    }
}

/// Apply LICM to a single loop, returning hoisting statements to prepend
/// `extra_modified` contains variables that are implicitly modified (e.g., for-of binding)
fn licm_loop(
    loop_body: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    extra_modified: &HashSet<u32>,
) -> Vec<TirStmt> {
    // Step 1: Collect all variables modified in the loop
    let mut modified_vars = extra_modified.clone();
    collect_modified_vars_in_block(loop_body, &mut modified_vars);

    // Step 2: Find field accesses that can be hoisted
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut next_local = *local_count;
    find_hoist_candidates_in_block(
        loop_body,
        &modified_vars,
        &mut candidates,
        &mut seen,
        &mut next_local,
    );

    if candidates.is_empty() {
        return Vec::new();
    }

    // Step 3: Create hoisting statements
    let mut hoist_stmts = Vec::new();
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
        hoist_stmts.push(hoist_stmt);

        // Add the type to local_types
        local_types.push(candidate.type_id);
    }

    // Update local count
    *local_count = next_local;

    // Step 4: Replace field accesses in the loop body with references to hoisted locals
    replace_hoisted_in_block(loop_body, &candidates);

    // Also need to handle nested loops - apply LICM recursively
    licm_block(loop_body, local_count, local_types, type_table);

    hoist_stmts
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
            condition,
            update,
            body,
        } => {
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
            condition,
            update,
            body,
        } => {
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
            apply_licm(&mut project);
            analyze_project(&mut project);
            remove_unreachable_functions(&mut project);
        }
        OptLevel::Size => {
            inline_functions(&mut project);
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
