//! Optimization pass for Wado TIR
//!
//! This module provides:
//! - Dead Code Elimination (DCE) at function level
//! - Optimization hints for conditional feature inclusion

use crate::name::{MethodNameInfo, build_method_mangled_name, build_qualified_name};
use crate::tir::{
    PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirModule,
    TirStmtKind, TypeId, TypeTable,
};
use std::collections::{HashMap, HashSet};

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
    /// Set of reachable function qualified names (from DCE analysis)
    pub reachable_functions: HashSet<String>,
    /// Whether float-to-string conversion is needed (derived from reachable functions)
    pub needs_f32_to_string: bool,
    pub needs_f64_to_string: bool,
    /// When true, all functions are considered reachable (DCE disabled)
    pub all_reachable: bool,
    /// When true, strip debug name sections for smaller binary size (-Os)
    pub strip_names: bool,
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
        }
    }

    pub fn needs_float_to_string(&self) -> bool {
        self.needs_f32_to_string || self.needs_f64_to_string
    }

    /// Check if a function is reachable (should be included in the binary)
    pub fn is_reachable(&self, qualified_name: &str) -> bool {
        self.all_reachable || self.reachable_functions.contains(qualified_name)
    }
}

// =============================================================================
// Dead Code Elimination (DCE)
// =============================================================================

/// Analyze all TIR modules and compute optimization hints including DCE
pub fn analyze_all_modules(
    modules: &HashMap<Vec<String>, TirModule>,
    entry_path: &[String],
) -> OptimizationHints {
    // Build call graph from all modules
    let call_graph = build_call_graph(modules);

    // Find entry function (run in entry module)
    let entry_func = build_qualified_name(entry_path, "run");

    // Compute reachable functions from entry point
    let reachable = compute_reachable(&call_graph, &entry_func);

    // Derive float-to-string hints from reachable functions
    let needs_f32_to_string = reachable.contains("core::internal::f32_to_string");
    let needs_f64_to_string = reachable.contains("core::internal::f64_to_string");

    OptimizationHints {
        reachable_functions: reachable,
        needs_f32_to_string,
        needs_f64_to_string,
        all_reachable: false,
        strip_names: false, // Set by caller based on OptLevel
    }
}

/// Build a call graph from all TIR modules
/// Returns a map from function qualified name to set of called function qualified names
fn build_call_graph(modules: &HashMap<Vec<String>, TirModule>) -> HashMap<String, HashSet<String>> {
    let mut call_graph: HashMap<String, HashSet<String>> = HashMap::new();

    for (path, module) in modules {
        let type_table = &module.type_table;

        // Analyze regular functions
        for func in &module.functions {
            let func_name = build_qualified_name(path, &func.name);
            let callees = collect_callees(func, path, type_table);
            call_graph.insert(func_name, callees);
        }

        // Analyze impl methods
        for impl_block in &module.impls {
            // Get struct name from type
            let struct_name = match type_table.get(impl_block.target_type) {
                ResolvedType::Struct { name, .. } => name.clone(),
                _ => continue,
            };

            for method in &impl_block.methods {
                let method_qualified = build_method_mangled_name(&MethodNameInfo {
                    filename: path.join("/"),
                    struct_name: struct_name.clone(),
                    trait_name: None,
                    method_name: method.name.clone(),
                });
                let callees = collect_callees(method, path, type_table);
                call_graph.insert(method_qualified, callees);
            }
        }
    }

    call_graph
}

/// Collect all function calls from a TIR function
fn collect_callees(
    func: &TirFunction,
    current_module: &[String],
    type_table: &TypeTable,
) -> HashSet<String> {
    let mut callees = HashSet::new();
    if let Some(body) = &func.body {
        collect_callees_block(body, current_module, type_table, &mut callees);
    }
    callees
}

fn collect_callees_block(
    block: &TirBlock,
    current_module: &[String],
    type_table: &TypeTable,
    callees: &mut HashSet<String>,
) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                collect_callees_expr(value, current_module, type_table, callees);
            }
            TirStmtKind::Expr(expr) => {
                collect_callees_expr(expr, current_module, type_table, callees);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    collect_callees_expr(expr, current_module, type_table, callees);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_callees_expr(condition, current_module, type_table, callees);
                collect_callees_block(then_block, current_module, type_table, callees);
                if let Some(else_blk) = else_block {
                    collect_callees_block(else_blk, current_module, type_table, callees);
                }
            }
            TirStmtKind::While { condition, body } => {
                collect_callees_expr(condition, current_module, type_table, callees);
                collect_callees_block(body, current_module, type_table, callees);
            }
            TirStmtKind::Loop { body } => {
                collect_callees_block(body, current_module, type_table, callees);
            }
            TirStmtKind::Assert {
                condition,
                message,
                intermediates,
                ..
            } => {
                collect_callees_expr(condition, current_module, type_table, callees);
                if let Some(msg) = message {
                    collect_callees_expr(msg, current_module, type_table, callees);
                }
                for (_, expr, type_id) in intermediates {
                    collect_callees_expr(expr, current_module, type_table, callees);
                    // Assert formatting calls to_string on intermediate values
                    add_to_string_callee(*type_id, type_table, callees);
                }
            }
            TirStmtKind::Break | TirStmtKind::Continue => {}
        }
    }
}

fn collect_callees_expr(
    expr: &TirExpr,
    current_module: &[String],
    type_table: &TypeTable,
    callees: &mut HashSet<String>,
) {
    match &expr.kind {
        TirExprKind::Call {
            module_path,
            func_name,
            args,
        } => {
            // Build qualified name for the called function
            let callee_path = if module_path.is_empty() {
                current_module
            } else {
                module_path.as_slice()
            };
            let callee_name = build_qualified_name(callee_path, func_name);
            callees.insert(callee_name);

            for arg in args {
                collect_callees_expr(arg, current_module, type_table, callees);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            method_name,
            args,
        } => {
            // Get receiver type to determine method target
            let receiver_type = type_table.get(receiver.type_id);
            match receiver_type {
                ResolvedType::Struct {
                    name, module_path, ..
                } => {
                    // Struct method call
                    let method_qualified = build_method_mangled_name(&MethodNameInfo {
                        filename: module_path.join("/"),
                        struct_name: name.clone(),
                        trait_name: None,
                        method_name: method_name.to_string(),
                    });
                    callees.insert(method_qualified);
                }
                ResolvedType::Primitive(_) => {
                    // Primitive method call (e.g., i32.to_string())
                    if method_name == "to_string" {
                        add_to_string_callee(receiver.type_id, type_table, callees);
                    }
                    // Other primitive methods are inline (no function call)
                }
                _ => {}
            }

            collect_callees_expr(receiver, current_module, type_table, callees);
            for arg in args {
                collect_callees_expr(arg, current_module, type_table, callees);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_callees_expr(left, current_module, type_table, callees);
            collect_callees_expr(right, current_module, type_table, callees);
        }
        TirExprKind::Unary { expr, .. } => {
            collect_callees_expr(expr, current_module, type_table, callees);
        }
        TirExprKind::Assign { target, value } => {
            collect_callees_expr(target, current_module, type_table, callees);
            collect_callees_expr(value, current_module, type_table, callees);
        }
        TirExprKind::Cast { expr, .. } => {
            collect_callees_expr(expr, current_module, type_table, callees);
        }
        TirExprKind::EffectCall { args, .. } => {
            // Effect calls are WASI imports, not user functions
            for arg in args {
                collect_callees_expr(arg, current_module, type_table, callees);
            }
        }
        TirExprKind::FieldAccess { expr, .. } => {
            collect_callees_expr(expr, current_module, type_table, callees);
        }
        TirExprKind::Index { expr, index } => {
            collect_callees_expr(expr, current_module, type_table, callees);
            collect_callees_expr(index, current_module, type_table, callees);
        }
        TirExprKind::Block(block) => {
            collect_callees_block(block, current_module, type_table, callees);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_callees_expr(condition, current_module, type_table, callees);
            collect_callees_block(then_branch, current_module, type_table, callees);
            if let Some(else_blk) = else_branch {
                collect_callees_block(else_blk, current_module, type_table, callees);
            }
        }
        TirExprKind::Match { expr, arms } => {
            collect_callees_expr(expr, current_module, type_table, callees);
            for arm in arms {
                collect_callees_expr(&arm.body, current_module, type_table, callees);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_callees_expr(&field.value, current_module, type_table, callees);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_callees_expr(elem, current_module, type_table, callees);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_callees_expr(body, current_module, type_table, callees);
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
fn add_to_string_callee(type_id: TypeId, type_table: &TypeTable, callees: &mut HashSet<String>) {
    match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => {
            let func_name = match prim {
                PrimitiveType::I32
                | PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::U8
                | PrimitiveType::U16
                | PrimitiveType::U32 => "core::internal::i32_to_string",
                PrimitiveType::I64 | PrimitiveType::U64 => "core::internal::i64_to_string",
                PrimitiveType::F32 => "core::internal::f32_to_string",
                PrimitiveType::F64 => "core::internal::f64_to_string",
                PrimitiveType::Bool => "core::internal::bool_to_string",
                PrimitiveType::Char => "core::internal::char_to_string",
                _ => return,
            };
            callees.insert(func_name.to_string());
        }
        ResolvedType::String => {
            // String.to_string() is a no-op, no function call needed
        }
        _ => {}
    }
}

/// Compute the set of reachable functions from an entry point
fn compute_reachable(
    call_graph: &HashMap<String, HashSet<String>>,
    entry: &str,
) -> HashSet<String> {
    let mut reachable = HashSet::new();
    let mut worklist = vec![entry.to_string()];

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

    #[test]
    fn test_empty_reachable_set() {
        let call_graph = HashMap::new();
        let reachable = compute_reachable(&call_graph, "run");
        assert!(reachable.contains("run"));
        assert_eq!(reachable.len(), 1);
    }

    #[test]
    fn test_transitive_reachability() {
        let mut call_graph = HashMap::new();
        call_graph.insert("run".to_string(), HashSet::from(["foo".to_string()]));
        call_graph.insert("foo".to_string(), HashSet::from(["bar".to_string()]));
        call_graph.insert("bar".to_string(), HashSet::new());
        call_graph.insert("unused".to_string(), HashSet::from(["bar".to_string()]));

        let reachable = compute_reachable(&call_graph, "run");
        assert!(reachable.contains("run"));
        assert!(reachable.contains("foo"));
        assert!(reachable.contains("bar"));
        assert!(!reachable.contains("unused"));
    }
}
