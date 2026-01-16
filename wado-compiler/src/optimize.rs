//! Optimization pass for Wado TIR
//!
//! This module provides:
//! - Dead Code Elimination (DCE) at function level
//! - Optimization hints for conditional feature inclusion

use crate::name::{FreeFunctionName, FunctionId, MethodName};
use crate::tir::{
    PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirModule,
    TirStmtKind, TypeId, TypeTable,
};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

/// Call graph: function ID -> set of called function IDs
type CallGraph = HashMap<FunctionId, HashSet<FunctionId>>;

/// Effect usage: function ID -> set of (effect_name, operation_name) pairs
type EffectUsageMap = HashMap<FunctionId, HashSet<(String, String)>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptLevel {
    #[default]
    None,
    Basic,
    Full,
    /// Optimize for size (strips debug names)
    Size,
}

/// Hints collected during optimization that inform code generation
#[derive(Debug, Clone, Default)]
pub struct OptimizationHints {
    /// Set of reachable functions (from DCE analysis)
    pub reachable_functions: HashSet<FunctionId>,
    /// Whether float-to-string conversion is needed (derived from reachable functions)
    pub needs_f32_to_string: bool,
    pub needs_f64_to_string: bool,
    /// When true, all functions are considered reachable (DCE disabled)
    pub all_reachable: bool,
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
    /// Set of used WASI effects (e.g., "Stdout", "Stderr", "MonotonicClock")
    pub used_effects: HashSet<String>,
    /// Set of used WASI functions (e.g., "Stdout::write_via_stream")
    pub used_wasi_functions: HashSet<String>,
    /// Whether async primitives are needed (waitable-set, subtask-drop, etc.)
    pub needs_async_primitives: bool,
    /// Whether bool-to-string conversion is needed (for "truefalse" data)
    pub needs_bool_to_string: bool,
    /// Whether stream intrinsics are needed (stream-new, stream-write, etc.)
    pub needs_stream_intrinsics: bool,
}

impl OptimizationHints {
    /// Create hints with all optimizations disabled (for O0)
    pub fn no_optimization() -> Self {
        Self {
            reachable_functions: HashSet::new(),
            needs_f32_to_string: true,
            needs_f64_to_string: true,
            all_reachable: true,
            strip_names: false,
            used_effects: HashSet::new(),
            used_wasi_functions: HashSet::new(),
            needs_async_primitives: true,
            needs_bool_to_string: true,
            needs_stream_intrinsics: true,
        }
    }

    pub fn needs_float_to_string(&self) -> bool {
        self.needs_f32_to_string || self.needs_f64_to_string
    }

    /// Check if a function is reachable (should be included in the binary)
    pub fn is_reachable(&self, func_id: &FunctionId) -> bool {
        self.all_reachable || self.reachable_functions.contains(func_id)
    }

    /// Check if an effect is used (should be imported)
    pub fn is_effect_used(&self, effect_name: &str) -> bool {
        self.all_reachable || self.used_effects.contains(effect_name)
    }

    /// Check if a specific WASI function is used
    pub fn is_wasi_function_used(&self, func_name: &str) -> bool {
        self.all_reachable || self.used_wasi_functions.contains(func_name)
    }

    /// Check if an effect is explicitly tracked as used (ignores all_reachable)
    /// Use this for effects that require explicit call tracking
    pub fn is_effect_explicitly_used(&self, effect_name: &str) -> bool {
        self.used_effects.contains(effect_name)
    }
}

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
}

/// Analyze all TIR modules and compute optimization hints including DCE
pub fn analyze_all_modules(
    modules: &IndexMap<Vec<String>, TirModule>,
    entry_path: &[String],
) -> OptimizationHints {
    // Build call graph and effect usage from all modules
    let (call_graph, effect_usage) = build_analysis_graph(modules);

    // Find entry function (run in entry module)
    let entry_func = FunctionId::Free(FreeFunctionName::from_path_and_name(entry_path, "run"));

    // Compute reachable functions from entry point
    let mut reachable = compute_reachable(&call_graph, &entry_func);

    // Collect used effects from reachable functions
    let mut used_effects: HashSet<String> = HashSet::new();
    let mut used_wasi_functions: HashSet<String> = HashSet::new();
    for func_id in &reachable {
        if let Some(effects) = effect_usage.get(func_id) {
            for (effect_name, op_name) in effects {
                used_effects.insert(effect_name.clone());
                used_wasi_functions.insert(format!("{}::{}", effect_name, op_name));
            }
        }
    }

    // Helper to check if a core/internal function is reachable
    let core_internal = |name: &str| -> FunctionId {
        FunctionId::Free(FreeFunctionName::from_strs(&["core", "internal"], name))
    };

    // Derive feature hints from reachable functions
    let needs_f32_to_string = reachable.contains(&core_internal("f32_to_string"));
    let needs_f64_to_string = reachable.contains(&core_internal("f64_to_string"));
    let needs_bool_to_string = reachable.contains(&core_internal("bool_to_string"));

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
    // 2. Any builtin::stream_* functions being called (for ambient logging)
    // 3. Any builtin::call_indirect_* functions (ambient effect calls)
    let uses_stream_builtins = reachable.iter().any(|func_id| {
        if let FunctionId::Free(f) = func_id {
            f.name.starts_with("builtin::stream_")
                || f.name.starts_with("builtin::call_indirect_stdout")
                || f.name.starts_with("builtin::call_indirect_stderr")
        } else {
            false
        }
    });

    // Also mark effects as used if indirect calls are present
    if reachable.iter().any(|func_id| {
        matches!(func_id, FunctionId::Free(f) if f.name.contains("builtin::call_indirect_stdout"))
    }) {
        used_effects.insert("Stdout".to_string());
    }
    if reachable.iter().any(|func_id| {
        matches!(func_id, FunctionId::Free(f) if f.name.contains("builtin::call_indirect_stderr"))
    }) {
        used_effects.insert("Stderr".to_string());
    }

    // Stream intrinsics needed if Stdout/Stderr used OR stream builtins called
    let needs_stream_intrinsics =
        used_effects.contains("Stdout") || used_effects.contains("Stderr") || uses_stream_builtins;

    // Check if any async primitives are needed
    // Needed if any effect is used OR if stream builtins are used (for ambient logging)
    let needs_async_primitives = !used_effects.is_empty() || uses_stream_builtins;

    OptimizationHints {
        reachable_functions: reachable,
        needs_f32_to_string,
        needs_f64_to_string,
        all_reachable: false,
        strip_names: false, // Set by caller based on OptLevel
        used_effects,
        used_wasi_functions,
        needs_async_primitives,
        needs_bool_to_string,
        needs_stream_intrinsics,
    }
}

/// Build call graph and effect usage from all TIR modules
/// Returns (call_graph, effect_usage)
fn build_analysis_graph(modules: &IndexMap<Vec<String>, TirModule>) -> (CallGraph, EffectUsageMap) {
    let mut call_graph: CallGraph = HashMap::new();
    let mut effect_usage: EffectUsageMap = HashMap::new();

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
                effect_usage.insert(func_id, analysis.effect_calls);
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
                    effect_usage.insert(method_id, analysis.effect_calls);
                }
            }
        }
    }

    (call_graph, effect_usage)
}

/// Analyze a TIR function for callees and effect usage
fn analyze_function(
    func: &TirFunction,
    current_module: &[String],
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
    current_module: &[String],
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
        // Leaf nodes - no calls
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. } => {}
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
