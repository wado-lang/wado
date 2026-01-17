//! Optimization pass for Wado TIR
//!
//! This module provides:
//! - Dead Code Elimination (DCE) at function level
//! - Usage analysis for conditional feature inclusion

use crate::name::{FreeFunctionName, FunctionId, MethodName};
use crate::project::Project;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirModule,
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
}

/// Call graph: function ID -> set of called function IDs
type CallGraph = HashMap<FunctionId, HashSet<FunctionId>>;

/// Effect usage: function ID -> set of (effect_name, operation_name) pairs
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

/// Standard WASI functions for each effect (for O0 mode)
pub const STANDARD_WASI_FUNCTIONS: &[&str] = &[
    "Stdout::write_via_stream",
    "Stderr::write_via_stream",
    "Environment::get_arguments",
    "Environment::get_environment",
    "Environment::get_initial_cwd",
    "MonotonicClock::now",
    "MonotonicClock::get_resolution",
    "MonotonicClock::wait_until",
    "MonotonicClock::wait_for",
];

// =============================================================================
// Dead Code Elimination (DCE)
// =============================================================================

/// Analysis results for a single function
#[derive(Debug, Clone, Default)]
struct FunctionAnalysis {
    /// Functions called by this function
    callees: HashSet<FunctionId>,
    /// Effect calls: (effect_name, op_name)
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
    let entry_func =
        FunctionId::Free(FreeFunctionName::from_path_and_name(&project.entry_path, "run"));

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
                used_wasi_functions.insert(format!("{}::{}", effect_name, op_name));
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

    // Derive builtin usage from reachable internal functions
    // f64_to_string/f32_to_string call the bundled f64_to_buffer/f32_to_buffer
    let needs_f64_to_buffer = reachable.contains(&core_internal("f64_to_string"));
    let needs_f32_to_buffer = reachable.contains(&core_internal("f32_to_string"));

    // Add cm_list_string_to_array and helper if Environment effect functions are used
    // This conversion function is called from codegen, not Wado code
    if used_wasi_functions.contains("Environment::get_arguments")
        || used_wasi_functions.contains("Environment::get_environment")
    {
        reachable.insert(core_internal("cm_list_string_to_array"));
        reachable.insert(core_internal("copy_string_from_linear"));
    }

    // Add array_copy_string if Array<String> value semantics are needed
    // This is called from codegen for tuple-to-array coercion and value copying
    // For now, include it if any string array operations are likely (conservative)
    // TODO: Track actual Array<String> usage more precisely
    reachable.insert(core_internal("array_copy_string"));

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
        if let FunctionId::Free(f) = func_id {
            if is_builtin_func(f) {
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

    // Effect usage requires async builtins for waiting on subtasks
    if !used_effects.is_empty() || uses_stream_builtins {
        for builtin in CanonBuiltin::ASYNC {
            used_builtins.insert(*builtin);
        }
    }

    // Apply results to project
    project.reachable_functions = reachable;
    project.all_reachable = false;
    project.used_effects = used_effects;
    project.used_wasi_functions = used_wasi_functions;
    project.used_builtins = used_builtins;
    project.used_box_primitives = used_box_primitives;
}

/// Populate project with all features enabled (no DCE, for O0 mode).
fn populate_all_features(project: &mut Project) {
    use PrimitiveType::*;

    project.reachable_functions = HashSet::new();
    project.all_reachable = true;
    // Standard effects (all except Exit which requires explicit usage)
    project.used_effects = WasiEffect::STANDARD.iter().copied().collect();
    // Standard WASI functions for the above effects
    project.used_wasi_functions = STANDARD_WASI_FUNCTIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    // All importable builtins when DCE is disabled
    project.used_builtins = CanonBuiltin::ALL.iter().copied().collect();
    // All primitives that map to box types when DCE is disabled
    project.used_box_primitives = HashSet::from([I32, I64, F32, F64]);
}

/// Per-function box primitives usage
type BoxPrimitivesMap = HashMap<FunctionId, HashSet<PrimitiveType>>;

/// Build call graph and effect usage from all TIR modules
/// Returns (call_graph, effect_usage, box_primitives_map)
fn build_analysis_graph(
    modules: &IndexMap<Vec<String>, TirModule>,
) -> (CallGraph, EffectUsageMap, BoxPrimitivesMap) {
    let mut call_graph: CallGraph = HashMap::new();
    let mut effect_usage: EffectUsageMap = HashMap::new();
    let mut box_primitives_map: BoxPrimitivesMap = HashMap::new();

    for (path, module) in modules {
        let type_table = &module.type_table;

        // Analyze functions (including methods stored as functions)
        for func in &module.functions {
            // Methods have names like "Point::sum", regular functions don't contain "::"
            let func_id = if let Some(sep_pos) = func.name.find("::") {
                // This is a method - use MethodName
                let struct_name = &func.name[..sep_pos];
                let method_name = &func.name[sep_pos + 2..];
                FunctionId::Method(MethodName::new(
                    path.join("/"),
                    struct_name.to_string(),
                    None,
                    method_name.to_string(),
                ))
            } else {
                // Regular function - use FreeFunctionName
                FunctionId::Free(FreeFunctionName::from_path_and_name(path, &func.name))
            };
            let analysis = analyze_function(func, path, type_table);
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
                let analysis = analyze_function(method, path, type_table);
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
        {
            if let ResolvedType::Primitive(prim) = type_table.get(*inner) {
                analysis.used_box_primitives.insert(*prim);
            }
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
                {
                    if let ResolvedType::Primitive(prim) = type_table.get(*inner) {
                        analysis.used_box_primitives.insert(*prim);
                    }
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
            TirStmtKind::Assert {
                condition,
                message,
                intermediates,
                ..
            } => {
                analyze_expr(condition, current_module, type_table, analysis);
                if let Some(msg) = message {
                    analyze_expr(msg, current_module, type_table, analysis);
                }
                for (_, expr, type_id) in intermediates {
                    analyze_expr(expr, current_module, type_table, analysis);
                    // Assert formatting calls to_string on intermediate values
                    add_to_string_callee(*type_id, type_table, analysis);
                }
                // Assert codegen uses string_concat and panic directly
                analysis
                    .callees
                    .insert(FunctionId::Free(FreeFunctionName::from_strs(
                        &["core", "internal"],
                        "string_concat",
                    )));
                analysis
                    .callees
                    .insert(FunctionId::Free(FreeFunctionName::from_strs(
                        &["core", "prelude"],
                        "panic",
                    )));
                // Assert failure prints to stderr, so we need Stderr effect
                analysis
                    .effect_calls
                    .insert(("Stderr".to_string(), "write_via_stream".to_string()));
            }
            TirStmtKind::Break | TirStmtKind::Continue => {}
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
        TirExprKind::Call {
            module_path,
            func_name,
            args,
            ..
        } => {
            // Invariant: TirExprKind::Call should never have method names (containing "::")
            // Methods use TirExprKind::MethodCall instead. The only exception is "builtin::*".
            debug_assert!(
                !func_name.contains("::") || func_name.starts_with("builtin::"),
                "TirExprKind::Call should not have method-style names: {}",
                func_name
            );

            // Build function ID for the called function
            let callee_path = if module_path.is_empty() {
                current_module
            } else {
                module_path.as_slice()
            };
            let callee_id =
                FunctionId::Free(FreeFunctionName::from_path_and_name(callee_path, func_name));
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
                    .is_some_and(|c| c.is_ascii_uppercase())
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
            method_name,
            args,
            ..
        } => {
            // Get receiver type to determine method target
            let receiver_type = type_table.get(receiver.type_id);
            match receiver_type {
                ResolvedType::Struct {
                    name, module_path, ..
                } => {
                    // Struct method call - use FunctionId::Method
                    let method_id = FunctionId::Method(MethodName::new(
                        module_path.join("/"),
                        name.clone(),
                        None,
                        method_name.to_string(),
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
                _ => {}
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
            if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef) {
                if let ResolvedType::Primitive(prim) = type_table.get(expr.type_id) {
                    analysis.used_box_primitives.insert(*prim);
                }
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
        TirExprKind::StaticCall {
            func_name,
            module_path,
            args,
        } => {
            // Static method call - func_name already contains "StructName::method_name"
            // The function is registered as a free function with mangled name
            let callee_path = if module_path.is_empty() {
                current_module
            } else {
                module_path.as_slice()
            };
            let callee_id =
                FunctionId::Free(FreeFunctionName::from_path_and_name(callee_path, func_name));
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

/// Add the appropriate to_string function call for a type
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
        ResolvedType::String => {
            // String.to_string() is a no-op, no function call needed
        }
        _ => {}
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

/// Optimize a Project by analyzing and populating its usage fields.
///
/// This is the main entry point for the optimizer. Based on the optimization
/// level, it either performs DCE analysis or enables all features.
pub fn optimize(mut project: Project, opt_level: OptLevel) -> Project {
    match opt_level {
        OptLevel::None => {
            populate_all_features(&mut project);
        }
        OptLevel::Basic | OptLevel::Full => {
            analyze_project(&mut project);
        }
        OptLevel::Size => {
            analyze_project(&mut project);
            project.strip_names = true;
        }
    }

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
