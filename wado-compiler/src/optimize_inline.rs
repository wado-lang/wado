//! Function inlining optimization for Wado TIR
//!
//! This module provides function inlining for small, pure functions.
//! It uses labeled block expressions for cleaner value handling.

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{
    FunctionRef, PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction,
    TirModule, TirPattern, TirStmt, TirStmtKind, TypeId, TypeTable,
};
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};

/// Maximum number of statements for a function to be inlined
const INLINE_THRESHOLD: usize = 20;

/// Count statements in a TIR block (recursive)
fn count_stmts(block: &TirBlock) -> usize {
    block
        .stmts
        .iter()
        .map(|s| match &s.kind {
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => 1 + count_stmts(then_block) + else_block.as_ref().map_or(0, count_stmts),
            TirStmtKind::While { body, .. }
            | TirStmtKind::Loop { body }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. }
            | TirStmtKind::LabeledBlock { block: body, .. } => 1 + count_stmts(body),
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => 1 + count_stmts(then_block) + else_block.as_ref().map_or(0, count_stmts),
            _ => 1,
        })
        .sum()
}

/// Check if a function is eligible for inlining
fn is_inline_eligible(
    func: &TirFunction,
    recursive_functions: &HashSet<String>,
    _module_path: &[String],
    type_table: &TypeTable,
) -> bool {
    // Must have a body
    let Some(body) = &func.body else {
        return false;
    };

    // Don't inline functions that return Never (!)
    // These are error/abort paths that are never hot, so no performance benefit to inlining
    if matches!(type_table.get(func.return_type), ResolvedType::Never) {
        return false;
    }

    // TODO: Don't inline functions with parameters that have complex nested generic types
    // (like Array<&mut BTreeNode<K,V>>). These can have type normalization issues
    // during monomorphization that cause type mismatches after inlining.
    // Fix the underlying type normalization issues to allow inlining these functions.
    for param in &func.params {
        if type_table.has_nested_generics(param.type_id) {
            return false;
        }
    }
    // Also check return type for nested generics
    if type_table.has_nested_generics(func.return_type) {
        return false;
    }

    // TODO: Check if any expression in the function body has complex nested generic types.
    // This catches cases like methods on TreeMap that access fields with nested generics.
    // Fix the underlying type normalization issues to allow inlining these functions.
    if body_has_complex_generic_types(body, type_table) {
        return false;
    }

    // No effects (pure functions only)
    if !func.effects.is_empty() {
        return false;
    }

    // Not recursive
    if recursive_functions.contains(&func.name) {
        return false;
    }

    // Only inline functions with a single return at the end
    // Functions with early returns (inside if/while) are too complex to inline
    if has_early_return(body) {
        return false;
    }

    // Small enough
    count_stmts(body) < INLINE_THRESHOLD
}

/// Check if a type has complex nested generics that could cause type normalization issues.
/// Uses the `TypeTable`'s type metadata to check for nested generic types.
fn has_complex_nested_generic(type_id: TypeId, type_table: &TypeTable) -> bool {
    type_table.has_nested_generics(type_id)
}

/// Check if a block has early returns (returns inside if/while blocks)
fn has_early_return(block: &TirBlock) -> bool {
    for (i, stmt) in block.stmts.iter().enumerate() {
        let is_last = i == block.stmts.len() - 1;
        match &stmt.kind {
            TirStmtKind::Return { .. } => {
                // Return is only OK if it's the last statement
                if !is_last {
                    return true;
                }
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                // Check if there are returns inside if blocks
                if block_has_return(then_block) {
                    return true;
                }
                if let Some(else_blk) = else_block
                    && block_has_return(else_blk)
                {
                    return true;
                }
            }
            TirStmtKind::While { body, .. }
            | TirStmtKind::Loop { body }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. }
            | TirStmtKind::LabeledBlock { block: body, .. } => {
                if block_has_return(body) {
                    return true;
                }
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                if block_has_return(then_block) {
                    return true;
                }
                if let Some(else_blk) = else_block
                    && block_has_return(else_blk)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Check if a block contains any return statement
fn block_has_return(block: &TirBlock) -> bool {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Return { .. } => return true,
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                if block_has_return(then_block) {
                    return true;
                }
                if let Some(else_blk) = else_block
                    && block_has_return(else_blk)
                {
                    return true;
                }
            }
            TirStmtKind::While { body, .. }
            | TirStmtKind::Loop { body }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. }
            | TirStmtKind::LabeledBlock { block: body, .. } => {
                if block_has_return(body) {
                    return true;
                }
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                if block_has_return(then_block) {
                    return true;
                }
                if let Some(else_blk) = else_block
                    && block_has_return(else_blk)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Check if any expression in the function body has a type with complex nested generics.
/// This catches cases where the function accesses fields or creates values with deeply nested
/// generic types that could cause type normalization issues during codegen.
fn body_has_complex_generic_types(body: &TirBlock, type_table: &TypeTable) -> bool {
    block_has_complex_generic_types(body, type_table)
}

fn block_has_complex_generic_types(block: &TirBlock, type_table: &TypeTable) -> bool {
    for stmt in &block.stmts {
        if stmt_has_complex_generic_types(stmt, type_table) {
            return true;
        }
    }
    false
}

fn stmt_has_complex_generic_types(stmt: &TirStmt, type_table: &TypeTable) -> bool {
    match &stmt.kind {
        TirStmtKind::Let { value, type_id, .. } => {
            // Check the declared type
            if has_complex_nested_generic(*type_id, type_table) {
                return true;
            }
            expr_has_complex_generic_types(value, type_table)
        }
        TirStmtKind::Expr(expr) => expr_has_complex_generic_types(expr, type_table),
        TirStmtKind::Return { value } => {
            if let Some(expr) = value {
                return expr_has_complex_generic_types(expr, type_table);
            }
            false
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_has_complex_generic_types(condition, type_table)
                || block_has_complex_generic_types(then_block, type_table)
                || else_block
                    .as_ref()
                    .is_some_and(|b| block_has_complex_generic_types(b, type_table))
        }
        TirStmtKind::While { condition, body } => {
            expr_has_complex_generic_types(condition, type_table)
                || block_has_complex_generic_types(body, type_table)
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            block_has_complex_generic_types(body, type_table)
        }
        TirStmtKind::For {
            init,
            condition,
            body,
            update,
        } => {
            init.iter()
                .any(|s| stmt_has_complex_generic_types(s, type_table))
                || condition
                    .as_ref()
                    .is_some_and(|e| expr_has_complex_generic_types(e, type_table))
                || block_has_complex_generic_types(body, type_table)
                || update
                    .as_ref()
                    .is_some_and(|e| expr_has_complex_generic_types(e, type_table))
        }
        TirStmtKind::ForOf {
            iterable,
            iterable_type,
            body,
            ..
        } => {
            has_complex_nested_generic(*iterable_type, type_table)
                || expr_has_complex_generic_types(iterable, type_table)
                || block_has_complex_generic_types(body, type_table)
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            expr_has_complex_generic_types(scrutinee, type_table)
                || block_has_complex_generic_types(then_block, type_table)
                || else_block
                    .as_ref()
                    .is_some_and(|b| block_has_complex_generic_types(b, type_table))
        }
        TirStmtKind::Break { value, .. } => value
            .as_ref()
            .is_some_and(|e| expr_has_complex_generic_types(e, type_table)),
        TirStmtKind::Continue => false,
    }
}

fn expr_has_complex_generic_types(expr: &TirExpr, type_table: &TypeTable) -> bool {
    // Check the expression's own type
    if has_complex_nested_generic(expr.type_id, type_table) {
        return true;
    }

    // Recursively check subexpressions
    match &expr.kind {
        TirExprKind::Call { args, .. }
        | TirExprKind::MethodCall { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => args
            .iter()
            .any(|a| expr_has_complex_generic_types(a, type_table)),
        TirExprKind::IndirectCall { callee, args } => {
            expr_has_complex_generic_types(callee, type_table)
                || args
                    .iter()
                    .any(|a| expr_has_complex_generic_types(a, type_table))
        }
        TirExprKind::Binary { left, right, .. }
        | TirExprKind::Assign {
            target: left,
            value: right,
        } => {
            expr_has_complex_generic_types(left, type_table)
                || expr_has_complex_generic_types(right, type_table)
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::OptionSome { value: inner }
        | TirExprKind::Move { value: inner } => expr_has_complex_generic_types(inner, type_table),
        TirExprKind::Index { expr: base, index } => {
            expr_has_complex_generic_types(base, type_table)
                || expr_has_complex_generic_types(index, type_table)
        }
        TirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| expr_has_complex_generic_types(&f.value, type_table)),
        TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => elements
            .iter()
            .any(|e| expr_has_complex_generic_types(e, type_table)),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_complex_generic_types(condition, type_table)
                || block_has_complex_generic_types(then_branch, type_table)
                || else_branch
                    .as_ref()
                    .is_some_and(|b| block_has_complex_generic_types(b, type_table))
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            block_has_complex_generic_types(block, type_table)
        }
        TirExprKind::Match { expr: inner, arms } => {
            expr_has_complex_generic_types(inner, type_table)
                || arms
                    .iter()
                    .any(|arm| expr_has_complex_generic_types(&arm.body, type_table))
        }
        TirExprKind::Closure { body, .. } => expr_has_complex_generic_types(body, type_table),
        TirExprKind::VariantConstruct { fields, .. } => fields
            .iter()
            .any(|f| expr_has_complex_generic_types(f, type_table)),
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
        | TirExprKind::Capture { .. } => false,
    }
}

/// Detect recursive functions using call graph analysis
fn find_recursive_functions(modules: &IndexMap<ModuleSource, TirModule>) -> HashSet<String> {
    let mut recursive = HashSet::new();

    // Build a simple call graph: function name -> called function names
    let mut call_graph: HashMap<String, HashSet<String>> = HashMap::new();

    for module in modules.values() {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            let callees = collect_callees_from_function(&func);
            call_graph.insert(func.name.clone(), callees);
        }
    }

    // Find functions that can reach themselves
    for func_name in call_graph.keys() {
        if can_reach(&call_graph, func_name, func_name, &mut HashSet::new()) {
            recursive.insert(func_name.clone());
        }
    }

    recursive
}

/// Collect all function names called from a function
fn collect_callees_from_function(func: &TirFunction) -> HashSet<String> {
    let mut callees = HashSet::new();
    if let Some(body) = &func.body {
        collect_callees_from_block(body, &mut callees);
    }
    callees
}

fn collect_callees_from_block(block: &TirBlock, callees: &mut HashSet<String>) {
    for stmt in &block.stmts {
        collect_callees_from_stmt(stmt, callees);
    }
}

fn collect_callees_from_stmt(stmt: &TirStmt, callees: &mut HashSet<String>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
            collect_callees_from_expr(value, callees);
        }
        TirStmtKind::Return { value } => {
            if let Some(expr) = value {
                collect_callees_from_expr(expr, callees);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_callees_from_expr(condition, callees);
            collect_callees_from_block(then_block, callees);
            if let Some(else_blk) = else_block {
                collect_callees_from_block(else_blk, callees);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_callees_from_expr(condition, callees);
            collect_callees_from_block(body, callees);
        }
        TirStmtKind::For {
            init,
            condition,
            body,
            update,
        } => {
            for s in init {
                collect_callees_from_stmt(s, callees);
            }
            if let Some(cond) = condition {
                collect_callees_from_expr(cond, callees);
            }
            collect_callees_from_block(body, callees);
            if let Some(upd) = update {
                collect_callees_from_expr(upd, callees);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::ForOf { body, .. } => {
            collect_callees_from_block(body, callees);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_callees_from_block(block, callees);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_callees_from_expr(scrutinee, callees);
            collect_callees_from_block(then_block, callees);
            if let Some(else_blk) = else_block {
                collect_callees_from_block(else_blk, callees);
            }
        }
        TirStmtKind::Break { .. } | TirStmtKind::Continue => {}
    }
}

fn collect_callees_from_expr(expr: &TirExpr, callees: &mut HashSet<String>) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            callees.insert(func.name());
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            // Method calls need to mark the method function as used
            // Use full_name() to get the qualified name including module path
            callees.insert(func.full_name());
            collect_callees_from_expr(receiver, callees);
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::StaticCall { func, args } => {
            callees.insert(func.name());
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_callees_from_expr(left, callees);
            collect_callees_from_expr(right, callees);
        }
        TirExprKind::Unary { expr, .. } => {
            collect_callees_from_expr(expr, callees);
        }
        TirExprKind::Assign { target, value } => {
            collect_callees_from_expr(target, callees);
            collect_callees_from_expr(value, callees);
        }
        TirExprKind::Cast { expr, .. } => {
            collect_callees_from_expr(expr, callees);
        }
        TirExprKind::FieldAccess { expr, .. } => {
            collect_callees_from_expr(expr, callees);
        }
        TirExprKind::Index { expr, index } => {
            collect_callees_from_expr(expr, callees);
            collect_callees_from_expr(index, callees);
        }
        TirExprKind::Block(block) => {
            collect_callees_from_block(block, callees);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_callees_from_expr(condition, callees);
            collect_callees_from_block(then_branch, callees);
            if let Some(else_blk) = else_branch {
                collect_callees_from_block(else_blk, callees);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_callees_from_expr(&field.value, callees);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_callees_from_expr(elem, callees);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_callees_from_expr(body, callees);
        }
        TirExprKind::IndirectCall { callee, args } => {
            collect_callees_from_expr(callee, callees);
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::Match { expr, arms } => {
            collect_callees_from_expr(expr, callees);
            for arm in arms {
                collect_callees_from_expr(&arm.body, callees);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_callees_from_expr(value, callees);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_callees_from_expr(field, callees);
            }
        }
        TirExprKind::Move { value } => {
            collect_callees_from_expr(value, callees);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_callees_from_block(block, callees);
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

/// Check if `start` can reach `target` in the call graph
fn can_reach(
    call_graph: &HashMap<String, HashSet<String>>,
    start: &str,
    target: &str,
    visited: &mut HashSet<String>,
) -> bool {
    if !visited.insert(start.to_string()) {
        return false; // Already visited
    }

    if let Some(callees) = call_graph.get(start) {
        for callee in callees {
            if callee == target {
                return true;
            }
            if can_reach(call_graph, callee, target, visited) {
                return true;
            }
        }
    }

    false
}

/// Inline eligible functions at their call sites
pub fn inline_functions(project: &mut Project) {
    let recursive_functions = find_recursive_functions(&project.tir_modules);

    // Collect inline candidates from all modules
    // Key: (module_path, func_name), Value: cloned function
    let mut inline_candidates: HashMap<(Vec<String>, String), TirFunction> = HashMap::new();

    // Also collect function_strings for each candidate (to update caller's strings after inlining)
    let mut candidate_strings: HashMap<(Vec<String>, String), Vec<String>> = HashMap::new();

    for (module_source, module) in &project.tir_modules {
        let module_path = module_source.to_path();
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if is_inline_eligible(
                &func,
                &recursive_functions,
                &module_path,
                &module.type_table.borrow(),
            ) {
                inline_candidates.insert((module_path.clone(), func.name.clone()), func.clone());
                // Get the strings used by this function
                if let Some(strings) = module.function_strings.get(&func.name) {
                    candidate_strings
                        .insert((module_path.clone(), func.name.clone()), strings.clone());
                }
            }
        }
    }

    if inline_candidates.is_empty() {
        return;
    }

    // Inline at call sites in each module
    for module in project.tir_modules.values_mut() {
        let module_path = module.module_source.to_path();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            let func_name = func.name.clone();
            if let Some(mut body) = func.body.take() {
                // Track which functions were inlined into this function
                let mut inlined_funcs: Vec<(Vec<String>, String)> = Vec::new();
                // Take ownership of local_count and local_types to avoid borrow conflicts
                let mut local_count = func.local_count;
                let mut local_types = std::mem::take(&mut func.local_types);
                // Counter for generating unique inline labels
                let mut inline_counter: u32 = 0;
                inline_calls_in_block(
                    &mut body,
                    &inline_candidates,
                    &module_path,
                    &mut local_count,
                    &mut local_types,
                    &module.type_table.borrow(),
                    &mut inlined_funcs,
                    &mut inline_counter,
                );
                func.local_count = local_count;
                func.local_types = local_types;
                func.body = Some(body);

                // Update function_strings: add strings from inlined functions to the caller
                for inlined_key in inlined_funcs {
                    if let Some(inlined_strings) = candidate_strings.get(&inlined_key) {
                        let caller_strings = module
                            .function_strings
                            .entry(func_name.clone())
                            .or_default();
                        for s in inlined_strings {
                            if !caller_strings.contains(s) {
                                caller_strings.push(s.clone());
                            }
                            // Also ensure the string is in the module's string_literals
                            if !module.string_literals.contains(s) {
                                module.string_literals.push(s.clone());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Inline function calls in a block
fn inline_calls_in_block(
    block: &mut TirBlock,
    candidates: &HashMap<(Vec<String>, String), TirFunction>,
    current_module: &[String],
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(Vec<String>, String)>,
    inline_counter: &mut u32,
) {
    let mut new_stmts = Vec::new();

    for stmt in std::mem::take(&mut block.stmts) {
        match stmt.kind {
            TirStmtKind::Let {
                name,
                local_index,
                is_mut,
                is_reactive,
                type_id,
                value,
            } => {
                // Try to inline the value expression if it's a call or method call
                let inline_result = try_inline_call_expr(
                    &value,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inline_counter,
                )
                .or_else(|| {
                    try_inline_method_call_expr(
                        &value,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        inline_counter,
                    )
                });

                if let Some((mut inlined_expr, inlined_key)) = inline_result {
                    // Track the inlined function
                    if !inlined_funcs.contains(&inlined_key) {
                        inlined_funcs.push(inlined_key);
                    }
                    // Process the inlined expression for nested inlining opportunities
                    inline_calls_in_expr(
                        &mut inlined_expr,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                        inline_counter,
                    );
                    // Create the let with the inlined labeled block expression
                    new_stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name,
                            local_index,
                            is_mut,
                            is_reactive,
                            type_id,
                            value: inlined_expr,
                        },
                        stmt.span,
                    ));
                } else {
                    // Recursively process nested calls in value
                    let mut new_value = value;
                    inline_calls_in_expr(
                        &mut new_value,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                        inline_counter,
                    );
                    new_stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name,
                            local_index,
                            is_mut,
                            is_reactive,
                            type_id,
                            value: new_value,
                        },
                        stmt.span,
                    ));
                }
            }
            TirStmtKind::Expr(expr) => {
                // Try to inline the expression if it's a call or method call
                let inline_result = try_inline_call_expr(
                    &expr,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inline_counter,
                )
                .or_else(|| {
                    try_inline_method_call_expr(
                        &expr,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        inline_counter,
                    )
                });

                if let Some((mut inlined_expr, inlined_key)) = inline_result {
                    if !inlined_funcs.contains(&inlined_key) {
                        inlined_funcs.push(inlined_key);
                    }
                    // Process the inlined expression for nested inlining opportunities
                    inline_calls_in_expr(
                        &mut inlined_expr,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                        inline_counter,
                    );
                    // For void functions, still emit the expression for side effects
                    new_stmts.push(TirStmt::new(TirStmtKind::Expr(inlined_expr), stmt.span));
                } else {
                    let mut new_expr = expr;
                    inline_calls_in_expr(
                        &mut new_expr,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                        inline_counter,
                    );
                    new_stmts.push(TirStmt::new(TirStmtKind::Expr(new_expr), stmt.span));
                }
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    let inline_result = try_inline_call_expr(
                        &expr,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        inline_counter,
                    )
                    .or_else(|| {
                        try_inline_method_call_expr(
                            &expr,
                            candidates,
                            current_module,
                            local_count,
                            local_types,
                            type_table,
                            inline_counter,
                        )
                    });

                    if let Some((mut inlined_expr, inlined_key)) = inline_result {
                        if !inlined_funcs.contains(&inlined_key) {
                            inlined_funcs.push(inlined_key);
                        }
                        // Process the inlined expression for nested inlining opportunities
                        inline_calls_in_expr(
                            &mut inlined_expr,
                            candidates,
                            current_module,
                            local_count,
                            local_types,
                            type_table,
                            &mut new_stmts,
                            inlined_funcs,
                            inline_counter,
                        );
                        new_stmts.push(TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(inlined_expr),
                            },
                            stmt.span,
                        ));
                    } else {
                        let mut new_expr = expr;
                        inline_calls_in_expr(
                            &mut new_expr,
                            candidates,
                            current_module,
                            local_count,
                            local_types,
                            type_table,
                            &mut new_stmts,
                            inlined_funcs,
                            inline_counter,
                        );
                        new_stmts.push(TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(new_expr),
                            },
                            stmt.span,
                        ));
                    }
                } else {
                    new_stmts.push(TirStmt::new(TirStmtKind::Return { value: None }, stmt.span));
                }
            }
            TirStmtKind::If {
                mut condition,
                mut then_block,
                else_block,
            } => {
                inline_calls_in_expr(
                    &mut condition,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    &mut new_stmts,
                    inlined_funcs,
                    inline_counter,
                );
                inline_calls_in_block(
                    &mut then_block,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                let new_else = else_block.map(|mut eb| {
                    inline_calls_in_block(
                        &mut eb,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        inlined_funcs,
                        inline_counter,
                    );
                    eb
                });
                new_stmts.push(TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block: new_else,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::While {
                condition,
                mut body,
            } => {
                // NOTE: We intentionally do NOT inline calls in the while condition.
                // The condition is evaluated on each iteration, so inlining would
                // place the preparatory statements outside the loop, causing them
                // to be evaluated only once instead of on each iteration.
                inline_calls_in_block(
                    &mut body,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                new_stmts.push(TirStmt::new(
                    TirStmtKind::While { condition, body },
                    stmt.span,
                ));
            }
            TirStmtKind::For {
                init,
                condition,
                mut body,
                update,
            } => {
                // NOTE: We intentionally do NOT inline calls in the for condition or update.
                // Both are evaluated on each iteration, so inlining would place the
                // preparatory statements outside the loop, causing them to be evaluated
                // only once instead of on each iteration.
                inline_calls_in_block(
                    &mut body,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                new_stmts.push(TirStmt::new(
                    TirStmtKind::For {
                        init,
                        condition,
                        body,
                        update,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Loop { mut body } => {
                inline_calls_in_block(
                    &mut body,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                new_stmts.push(TirStmt::new(TirStmtKind::Loop { body }, stmt.span));
            }
            TirStmtKind::ForOf {
                binding_local,
                binding_type,
                is_mut,
                mut iterable,
                iterable_type,
                mut body,
            } => {
                inline_calls_in_expr(
                    &mut iterable,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    &mut new_stmts,
                    inlined_funcs,
                    inline_counter,
                );
                inline_calls_in_block(
                    &mut body,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                new_stmts.push(TirStmt::new(
                    TirStmtKind::ForOf {
                        binding_local,
                        binding_type,
                        is_mut,
                        iterable,
                        iterable_type,
                        body,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::LabeledBlock { label, mut block } => {
                inline_calls_in_block(
                    &mut block,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                new_stmts.push(TirStmt::new(
                    TirStmtKind::LabeledBlock { label, block },
                    stmt.span,
                ));
            }
            TirStmtKind::IfPattern {
                mut scrutinee,
                pattern,
                mut then_block,
                else_block,
            } => {
                inline_calls_in_expr(
                    &mut scrutinee,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    &mut new_stmts,
                    inlined_funcs,
                    inline_counter,
                );
                inline_calls_in_block(
                    &mut then_block,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
                let new_else = else_block.map(|mut eb| {
                    inline_calls_in_block(
                        &mut eb,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        inlined_funcs,
                        inline_counter,
                    );
                    eb
                });
                new_stmts.push(TirStmt::new(
                    TirStmtKind::IfPattern {
                        scrutinee,
                        pattern,
                        then_block,
                        else_block: new_else,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Break { label, value } => {
                let new_value = value.map(|mut v| {
                    inline_calls_in_expr(
                        &mut v,
                        candidates,
                        current_module,
                        local_count,
                        local_types,
                        type_table,
                        &mut new_stmts,
                        inlined_funcs,
                        inline_counter,
                    );
                    v
                });
                new_stmts.push(TirStmt::new(
                    TirStmtKind::Break {
                        label,
                        value: new_value,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::Continue => {
                new_stmts.push(TirStmt::new(TirStmtKind::Continue, stmt.span));
            }
        }
    }

    block.stmts = new_stmts;
}

/// Create a default value for a type (for initializing result locals)
fn create_default_value(type_id: TypeId, type_table: &TypeTable, span: crate::Span) -> TirExpr {
    let kind = match type_table.get(type_id) {
        ResolvedType::Primitive(prim) => match prim {
            PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::I128
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64
            | PrimitiveType::U128 => TirExprKind::IntLiteral {
                value: 0,
                repr: "0".to_string(),
            },
            PrimitiveType::F32 | PrimitiveType::F64 => TirExprKind::FloatLiteral {
                value: 0.0,
                repr: "0.0".to_string(),
            },
            PrimitiveType::Bool => TirExprKind::BoolLiteral(false),
            PrimitiveType::Char => TirExprKind::CharLiteral('\0'),
        },
        // For all reference types (structs, arrays, options, etc.), use Null
        // The value will be immediately overwritten, so this is just a placeholder
        _ => TirExprKind::Null,
    };
    TirExpr::new(kind, type_id, span)
}

/// Try to inline a call expression, returning the inlined expression and key
fn try_inline_call_expr(
    expr: &TirExpr,
    candidates: &HashMap<(Vec<String>, String), TirFunction>,
    current_module: &[String],
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    _type_table: &TypeTable,
    inline_counter: &mut u32,
) -> Option<(TirExpr, (Vec<String>, String))> {
    let TirExprKind::Call {
        func,
        args,
        type_args,
    } = &expr.kind
    else {
        return None;
    };

    let module_path = func.module_path();
    let func_name = func.name();

    // Skip generic calls
    if !type_args.is_empty() {
        return None;
    }

    // Try to find the candidate function
    // First try the call site's module path, then fall back to entry module
    // (monomorphized functions are placed in the entry module)
    let target_module = if module_path.is_empty() {
        current_module.to_vec()
    } else {
        module_path.clone()
    };

    // Look up the candidate - try direct module first, then entry module for monomorphized functions
    let candidate = candidates
        .get(&(target_module.clone(), func_name.clone()))
        .or_else(|| {
            // For monomorphized functions, also try looking in the entry module (empty path)
            if target_module.is_empty() {
                None
            } else {
                candidates.get(&(vec![], func_name.clone()))
            }
        });

    let candidate = candidate?;

    // Get the function body
    let body = candidate.body.as_ref()?;

    // Generate unique label for this inline site
    // Sanitize function name for use as label (replace non-alphanumeric with _)
    let sanitized_name: String = func_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let label = format!("__inline_{}_{}", sanitized_name, *inline_counter);
    *inline_counter += 1;

    // Calculate local index offset for remapping
    let local_offset = *local_count;

    let callee_param_count = candidate.params.len() as u32;
    let callee_local_count = candidate.local_count;
    let new_locals_needed = callee_local_count.saturating_sub(callee_param_count);

    // Create argument bindings as let statements inside a labeled block
    // IMPORTANT: Push param types first to match index assignment order
    // (params get indices local_offset+0, local_offset+1, ..., then non-params follow)
    let mut block_stmts = Vec::new();
    let mut param_to_local: HashMap<u32, u32> = HashMap::new();

    for (i, (param, arg)) in candidate.params.iter().zip(args.iter()).enumerate() {
        let new_local_index = local_offset + i as u32;
        param_to_local.insert(param.local_index, new_local_index);

        // Extend local_types for parameter - use argument's type_id to match
        // the actual value being assigned (handles monomorphization type variance)
        local_types.push(arg.type_id);
        *local_count += 1;

        // Use original parameter name (not _inline_ prefix)
        block_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: param.name.clone(),
                local_index: new_local_index,
                is_mut: false, // Parameters are immutable
                is_reactive: false,
                type_id: arg.type_id,
                value: arg.clone(),
            },
            expr.span,
        ));
    }

    // param_offset marks where non-param locals start (after all params)
    let param_offset = local_offset + candidate.params.len() as u32;

    // Now extend local_types for the non-parameter locals
    for i in callee_param_count..callee_local_count {
        if let Some(&type_id) = candidate.local_types.get(i as usize) {
            local_types.push(type_id);
        }
    }
    *local_count += new_locals_needed;

    // Convert the body, transforming `return` into `break label: expr`
    let remapped_stmts = remap_and_convert_returns(
        body,
        &param_to_local,
        param_offset,
        callee_param_count,
        &label,
        &target_module,
    );

    block_stmts.extend(remapped_stmts);

    // Create a labeled block expression that produces the return value
    let inlined_expr = TirExpr::new(
        TirExprKind::LabeledBlock {
            label: label.clone(),
            block: TirBlock::new(block_stmts, expr.span),
            result_type: candidate.return_type,
        },
        candidate.return_type,
        expr.span,
    );

    // Return the inlined method key for string literal tracking
    let inlined_key = (target_module, func_name.clone());
    Some((inlined_expr, inlined_key))
}

/// Try to inline a method call expression, returning the inlined expression and key
fn try_inline_method_call_expr(
    expr: &TirExpr,
    candidates: &HashMap<(Vec<String>, String), TirFunction>,
    current_module: &[String],
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    _type_table: &TypeTable,
    inline_counter: &mut u32,
) -> Option<(TirExpr, (Vec<String>, String))> {
    let TirExprKind::MethodCall {
        receiver,
        func,
        args,
        type_args,
    } = &expr.kind
    else {
        return None;
    };

    let module_path = func.module_path();
    let func_name = func.name();

    // Skip generic calls
    if !type_args.is_empty() {
        return None;
    }

    // Try to find the candidate function
    // First try the call site's module path, then fall back to entry module
    // (monomorphized functions are placed in the entry module)
    let target_module = if module_path.is_empty() {
        current_module.to_vec()
    } else {
        module_path.clone()
    };

    // Look up the candidate - try direct module first, then entry module for monomorphized functions
    let candidate = candidates
        .get(&(target_module.clone(), func_name.clone()))
        .or_else(|| {
            // For monomorphized functions, also try looking in the entry module (empty path)
            if target_module.is_empty() {
                None
            } else {
                candidates.get(&(vec![], func_name.clone()))
            }
        });

    let candidate = candidate?;

    // Get the function body
    let body = candidate.body.as_ref()?;

    // Generate unique label for this inline site
    // Sanitize function name for use as label (replace non-alphanumeric with _)
    let sanitized_name: String = func_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let label = format!("__inline_{}_{}", sanitized_name, *inline_counter);
    *inline_counter += 1;

    // Calculate local index offset for remapping
    let local_offset = *local_count;

    let callee_param_count = candidate.params.len() as u32;
    let callee_local_count = candidate.local_count;
    let new_locals_needed = callee_local_count.saturating_sub(callee_param_count);

    // Create argument bindings as let statements inside a labeled block
    // IMPORTANT: Push param types first to match index assignment order
    // For methods, first param is `self` (receiver), then the rest are args
    let mut block_stmts = Vec::new();
    let mut param_to_local: HashMap<u32, u32> = HashMap::new();

    // Bind receiver to first parameter (self)
    // Use receiver's type_id to handle monomorphization type variance
    let first_param = &candidate.params[0];
    let self_local_index = local_offset;
    param_to_local.insert(first_param.local_index, self_local_index);
    local_types.push(receiver.type_id);
    *local_count += 1;

    // Use original parameter name (not _inline_ prefix)
    block_stmts.push(TirStmt::new(
        TirStmtKind::Let {
            name: first_param.name.clone(),
            local_index: self_local_index,
            is_mut: false,
            is_reactive: false,
            type_id: receiver.type_id,
            value: (**receiver).clone(),
        },
        expr.span,
    ));

    // Bind remaining args to remaining parameters
    // Use argument's type_id to handle monomorphization type variance
    for (i, (param, arg)) in candidate.params.iter().skip(1).zip(args.iter()).enumerate() {
        let new_local_index = local_offset + 1 + i as u32;
        param_to_local.insert(param.local_index, new_local_index);
        local_types.push(arg.type_id);
        *local_count += 1;

        // Use original parameter name (not _inline_ prefix)
        block_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: param.name.clone(),
                local_index: new_local_index,
                is_mut: false,
                is_reactive: false,
                type_id: arg.type_id,
                value: arg.clone(),
            },
            expr.span,
        ));
    }

    // param_offset marks where non-param locals start (after all params)
    let param_offset = local_offset + candidate.params.len() as u32;

    // Now extend local_types for the non-parameter locals
    for i in callee_param_count..callee_local_count {
        if let Some(&type_id) = candidate.local_types.get(i as usize) {
            local_types.push(type_id);
        }
    }
    *local_count += new_locals_needed;

    // Convert the body, transforming `return` into `break label: expr`
    let remapped_stmts = remap_and_convert_returns(
        body,
        &param_to_local,
        param_offset,
        callee_param_count,
        &label,
        &target_module,
    );

    block_stmts.extend(remapped_stmts);

    // Create a labeled block expression that produces the return value
    let inlined_expr = TirExpr::new(
        TirExprKind::LabeledBlock {
            label: label.clone(),
            block: TirBlock::new(block_stmts, expr.span),
            result_type: candidate.return_type,
        },
        candidate.return_type,
        expr.span,
    );

    // Return the inlined method key for string literal tracking
    let inlined_key = (target_module, func_name.clone());

    Some((inlined_expr, inlined_key))
}

/// Remap locals and convert returns to break statements with the given label
fn remap_and_convert_returns(
    block: &TirBlock,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &str,
    source_module: &[String],
) -> Vec<TirStmt> {
    let mut stmts = Vec::new();

    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Return { value } => {
                // Convert return to break with the inline label
                let break_value = value.as_ref().map(|v| {
                    remap_expr(v, param_to_local, local_offset, param_count, source_module)
                });
                stmts.push(TirStmt::new(
                    TirStmtKind::Break {
                        label: Some(label.to_string()),
                        value: break_value,
                    },
                    stmt.span,
                ));
            }
            _ => {
                stmts.push(remap_stmt_with_label(
                    stmt,
                    param_to_local,
                    local_offset,
                    param_count,
                    label,
                    source_module,
                ));
            }
        }
    }

    stmts
}

/// Remap local indices in a statement, converting nested returns to breaks
fn remap_stmt_with_label(
    stmt: &TirStmt,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &str,
    source_module: &[String],
) -> TirStmt {
    let kind = match &stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
        } => {
            let new_index =
                remap_local_index(*local_index, param_to_local, local_offset, param_count);
            TirStmtKind::Let {
                name: name.clone(),
                local_index: new_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: remap_expr(
                    value,
                    param_to_local,
                    local_offset,
                    param_count,
                    source_module,
                ),
            }
        }
        TirStmtKind::Expr(expr) => TirStmtKind::Expr(remap_expr(
            expr,
            param_to_local,
            local_offset,
            param_count,
            source_module,
        )),
        TirStmtKind::Return { value } => {
            // Convert return to break with label
            TirStmtKind::Break {
                label: Some(label.to_string()),
                value: value.as_ref().map(|v| {
                    remap_expr(v, param_to_local, local_offset, param_count, source_module)
                }),
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => TirStmtKind::If {
            condition: remap_expr(
                condition,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            then_block: remap_block_with_label(
                then_block,
                param_to_local,
                local_offset,
                param_count,
                label,
                source_module,
            ),
            else_block: else_block.as_ref().map(|b| {
                remap_block_with_label(
                    b,
                    param_to_local,
                    local_offset,
                    param_count,
                    label,
                    source_module,
                )
            }),
        },
        TirStmtKind::While { condition, body } => TirStmtKind::While {
            condition: remap_expr(
                condition,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            body: remap_block_with_label(
                body,
                param_to_local,
                local_offset,
                param_count,
                label,
                source_module,
            ),
        },
        TirStmtKind::For {
            init,
            condition,
            body,
            update,
        } => TirStmtKind::For {
            init: init
                .iter()
                .map(|s| {
                    remap_stmt_with_label(
                        s,
                        param_to_local,
                        local_offset,
                        param_count,
                        label,
                        source_module,
                    )
                })
                .collect(),
            condition: condition
                .as_ref()
                .map(|c| remap_expr(c, param_to_local, local_offset, param_count, source_module)),
            body: remap_block_with_label(
                body,
                param_to_local,
                local_offset,
                param_count,
                label,
                source_module,
            ),
            update: update
                .as_ref()
                .map(|u| remap_expr(u, param_to_local, local_offset, param_count, source_module)),
        },
        TirStmtKind::Loop { body } => TirStmtKind::Loop {
            body: remap_block_with_label(
                body,
                param_to_local,
                local_offset,
                param_count,
                label,
                source_module,
            ),
        },
        TirStmtKind::ForOf {
            binding_local,
            binding_type,
            is_mut,
            iterable,
            iterable_type,
            body,
        } => TirStmtKind::ForOf {
            binding_local: remap_local_index(
                *binding_local,
                param_to_local,
                local_offset,
                param_count,
            ),
            binding_type: *binding_type,
            is_mut: *is_mut,
            iterable: remap_expr(
                iterable,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            iterable_type: *iterable_type,
            body: remap_block_with_label(
                body,
                param_to_local,
                local_offset,
                param_count,
                label,
                source_module,
            ),
        },
        TirStmtKind::LabeledBlock {
            label: inner_label,
            block,
        } => TirStmtKind::LabeledBlock {
            label: inner_label.clone(),
            block: remap_block_with_label(
                block,
                param_to_local,
                local_offset,
                param_count,
                label,
                source_module,
            ),
        },
        TirStmtKind::IfPattern {
            scrutinee,
            pattern,
            then_block,
            else_block,
        } => TirStmtKind::IfPattern {
            scrutinee: remap_expr(
                scrutinee,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            pattern: remap_pattern(pattern, param_to_local, local_offset, param_count),
            then_block: remap_block_with_label(
                then_block,
                param_to_local,
                local_offset,
                param_count,
                label,
                source_module,
            ),
            else_block: else_block.as_ref().map(|b| {
                remap_block_with_label(
                    b,
                    param_to_local,
                    local_offset,
                    param_count,
                    label,
                    source_module,
                )
            }),
        },
        TirStmtKind::Break {
            label: break_label,
            value,
        } => TirStmtKind::Break {
            label: break_label.clone(),
            value: value
                .as_ref()
                .map(|v| remap_expr(v, param_to_local, local_offset, param_count, source_module)),
        },
        TirStmtKind::Continue => TirStmtKind::Continue,
    };

    TirStmt::new(kind, stmt.span)
}

/// Remap local indices in a pattern
fn remap_pattern(
    pattern: &TirPattern,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> TirPattern {
    match pattern {
        TirPattern::Wildcard => TirPattern::Wildcard,
        TirPattern::Binding { name, local_index } => TirPattern::Binding {
            name: name.clone(),
            local_index: remap_local_index(*local_index, param_to_local, local_offset, param_count),
        },
        TirPattern::Literal(lit) => TirPattern::Literal(lit.clone()),
        TirPattern::Tuple(patterns) => TirPattern::Tuple(
            patterns
                .iter()
                .map(|p| remap_pattern(p, param_to_local, local_offset, param_count))
                .collect(),
        ),
        TirPattern::Variant {
            enum_type,
            variant_name,
            bindings,
        } => TirPattern::Variant {
            enum_type: *enum_type,
            variant_name: variant_name.clone(),
            bindings: bindings
                .iter()
                .map(|p| remap_pattern(p, param_to_local, local_offset, param_count))
                .collect(),
        },
    }
}

/// Remap local indices in a block with label for return conversion
fn remap_block_with_label(
    block: &TirBlock,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &str,
    source_module: &[String],
) -> TirBlock {
    TirBlock::new(
        block
            .stmts
            .iter()
            .map(|s| {
                remap_stmt_with_label(
                    s,
                    param_to_local,
                    local_offset,
                    param_count,
                    label,
                    source_module,
                )
            })
            .collect(),
        block.span,
    )
}

/// Remap a local index
fn remap_local_index(
    index: u32,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> u32 {
    // If it's a parameter, use the param_to_local mapping
    if let Some(&new_index) = param_to_local.get(&index) {
        return new_index;
    }
    // Otherwise, offset the non-parameter locals
    if index >= param_count {
        local_offset + (index - param_count)
    } else {
        // This shouldn't happen if param_to_local is complete
        index
    }
}

/// Remap local indices in an expression
/// `source_module` is the module path where the inlined function came from,
/// used to convert local calls to use the full module path.
fn remap_expr(
    expr: &TirExpr,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    source_module: &[String],
) -> TirExpr {
    let kind = match &expr.kind {
        TirExprKind::Local { index, name } => {
            let new_index = remap_local_index(*index, param_to_local, local_offset, param_count);
            // Keep the original name - labeled blocks provide scoping
            TirExprKind::Local {
                index: new_index,
                name: name.clone(),
            }
        }
        TirExprKind::Binary { left, op, right } => TirExprKind::Binary {
            left: Box::new(remap_expr(
                left,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            op: *op,
            right: Box::new(remap_expr(
                right,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
        },
        TirExprKind::Unary { op, expr: inner } => TirExprKind::Unary {
            op: *op,
            expr: Box::new(remap_expr(
                inner,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
        },
        TirExprKind::Assign { target, value } => TirExprKind::Assign {
            target: Box::new(remap_expr(
                target,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            value: Box::new(remap_expr(
                value,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
        },
        TirExprKind::Cast {
            expr: inner,
            target_type,
        } => TirExprKind::Cast {
            expr: Box::new(remap_expr(
                inner,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            target_type: *target_type,
        },
        TirExprKind::Call {
            func,
            type_args,
            args,
        } => {
            // Convert local calls to use the source module path
            let remapped_func = remap_function_ref(func, source_module);
            TirExprKind::Call {
                func: remapped_func,
                type_args: type_args.clone(),
                args: args
                    .iter()
                    .map(|a| {
                        remap_expr(a, param_to_local, local_offset, param_count, source_module)
                    })
                    .collect(),
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            type_args,
            args,
        } => {
            // Convert local method calls to use the source module path
            let remapped_func = remap_function_ref(func, source_module);
            TirExprKind::MethodCall {
                receiver: Box::new(remap_expr(
                    receiver,
                    param_to_local,
                    local_offset,
                    param_count,
                    source_module,
                )),
                func: remapped_func,
                type_args: type_args.clone(),
                args: args
                    .iter()
                    .map(|a| {
                        remap_expr(a, param_to_local, local_offset, param_count, source_module)
                    })
                    .collect(),
            }
        }
        TirExprKind::StaticCall { func, args } => {
            // Convert local static calls to use the source module path
            let remapped_func = remap_function_ref(func, source_module);
            TirExprKind::StaticCall {
                func: remapped_func,
                args: args
                    .iter()
                    .map(|a| {
                        remap_expr(a, param_to_local, local_offset, param_count, source_module)
                    })
                    .collect(),
            }
        }
        TirExprKind::EffectCall {
            effect_name,
            op_name,
            args,
        } => TirExprKind::EffectCall {
            effect_name: effect_name.clone(),
            op_name: op_name.clone(),
            args: args
                .iter()
                .map(|a| remap_expr(a, param_to_local, local_offset, param_count, source_module))
                .collect(),
        },
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => TirExprKind::FieldAccess {
            expr: Box::new(remap_expr(
                inner,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            field_index: *field_index,
            field_name: field_name.clone(),
        },
        TirExprKind::Index { expr: inner, index } => TirExprKind::Index {
            expr: Box::new(remap_expr(
                inner,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            index: Box::new(remap_expr(
                index,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
        },
        TirExprKind::Block(block) => TirExprKind::Block(remap_block(
            block,
            param_to_local,
            local_offset,
            param_count,
            source_module,
        )),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => TirExprKind::If {
            condition: Box::new(remap_expr(
                condition,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            then_branch: remap_block(
                then_branch,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            else_branch: else_branch
                .as_ref()
                .map(|b| remap_block(b, param_to_local, local_offset, param_count, source_module)),
        },
        TirExprKind::Match { expr: inner, arms } => TirExprKind::Match {
            expr: Box::new(remap_expr(
                inner,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            arms: arms
                .iter()
                .map(|arm| crate::tir::TirMatchArm {
                    pattern: arm.pattern.clone(), // Patterns don't contain locals in the same sense
                    body: remap_expr(
                        &arm.body,
                        param_to_local,
                        local_offset,
                        param_count,
                        source_module,
                    ),
                    span: arm.span,
                })
                .collect(),
        },
        TirExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        } => TirExprKind::StructLiteral {
            struct_type: *struct_type,
            struct_name: struct_name.clone(),
            fields: fields
                .iter()
                .map(|f| crate::tir::TirStructField {
                    name: f.name.clone(),
                    value: remap_expr(
                        &f.value,
                        param_to_local,
                        local_offset,
                        param_count,
                        source_module,
                    ),
                    field_index: f.field_index,
                })
                .collect(),
        },
        TirExprKind::ArrayLiteral { elements } => TirExprKind::ArrayLiteral {
            elements: elements
                .iter()
                .map(|e| remap_expr(e, param_to_local, local_offset, param_count, source_module))
                .collect(),
        },
        TirExprKind::TupleLiteral { elements } => TirExprKind::TupleLiteral {
            elements: elements
                .iter()
                .map(|e| remap_expr(e, param_to_local, local_offset, param_count, source_module))
                .collect(),
        },
        TirExprKind::Closure {
            params,
            body,
            captures,
        } => TirExprKind::Closure {
            params: params.clone(),
            body: Box::new(remap_expr(
                body,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            captures: captures.clone(), // Captures reference outer scope, not remapped
        },
        TirExprKind::IndirectCall { callee, args } => TirExprKind::IndirectCall {
            callee: Box::new(remap_expr(
                callee,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
            args: args
                .iter()
                .map(|a| remap_expr(a, param_to_local, local_offset, param_count, source_module))
                .collect(),
        },
        TirExprKind::OptionSome { value } => TirExprKind::OptionSome {
            value: Box::new(remap_expr(
                value,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
        },
        TirExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            fields,
        } => TirExprKind::VariantConstruct {
            variant_type: *variant_type,
            case_index: *case_index,
            case_name: case_name.clone(),
            fields: fields
                .iter()
                .map(|f| remap_expr(f, param_to_local, local_offset, param_count, source_module))
                .collect(),
        },
        TirExprKind::Move { value } => TirExprKind::Move {
            value: Box::new(remap_expr(
                value,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            )),
        },
        TirExprKind::LabeledBlock {
            label,
            block,
            result_type,
        } => TirExprKind::LabeledBlock {
            label: label.clone(),
            block: remap_block(
                block,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            result_type: *result_type,
        },
        // Leaf nodes - no remapping needed
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::Capture { .. } => expr.kind.clone(),
    };

    TirExpr::new(kind, expr.type_id, expr.span)
}

/// Remap a function reference to use the source module path for local calls.
/// When inlining from module A into module B, local calls (empty `module_path`)
/// need to be converted to use module A's path.
fn remap_function_ref(func: &FunctionRef, source_module: &[String]) -> FunctionRef {
    // Skip if the source module is empty (no remapping needed)
    if source_module.is_empty() {
        return func.clone();
    }

    // Only remap if the func has an empty module path (local call)
    if func.module_path().is_empty() {
        // Convert to External with the source module path
        // Local calls within a module are not monomorphized, so monomorph_info is None
        FunctionRef::External {
            module_source: ModuleSource::from_path(source_module),
            name: func.name(),
            monomorph_info: None,
            method_info: func.method_info(),
        }
    } else {
        func.clone()
    }
}

/// Remap local indices in a block (without label for return conversion)
fn remap_block(
    block: &TirBlock,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    source_module: &[String],
) -> TirBlock {
    TirBlock::new(
        block
            .stmts
            .iter()
            .map(|s| remap_stmt(s, param_to_local, local_offset, param_count, source_module))
            .collect(),
        block.span,
    )
}

/// Remap local indices in a statement (without return conversion)
fn remap_stmt(
    stmt: &TirStmt,
    param_to_local: &HashMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    source_module: &[String],
) -> TirStmt {
    let kind = match &stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
        } => {
            let new_index =
                remap_local_index(*local_index, param_to_local, local_offset, param_count);
            TirStmtKind::Let {
                name: name.clone(),
                local_index: new_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: remap_expr(
                    value,
                    param_to_local,
                    local_offset,
                    param_count,
                    source_module,
                ),
            }
        }
        TirStmtKind::Expr(expr) => TirStmtKind::Expr(remap_expr(
            expr,
            param_to_local,
            local_offset,
            param_count,
            source_module,
        )),
        TirStmtKind::Return { value } => TirStmtKind::Return {
            value: value
                .as_ref()
                .map(|v| remap_expr(v, param_to_local, local_offset, param_count, source_module)),
        },
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => TirStmtKind::If {
            condition: remap_expr(
                condition,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            then_block: remap_block(
                then_block,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            else_block: else_block
                .as_ref()
                .map(|b| remap_block(b, param_to_local, local_offset, param_count, source_module)),
        },
        TirStmtKind::While { condition, body } => TirStmtKind::While {
            condition: remap_expr(
                condition,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            body: remap_block(
                body,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
        },
        TirStmtKind::For {
            init,
            condition,
            body,
            update,
        } => TirStmtKind::For {
            init: init
                .iter()
                .map(|s| remap_stmt(s, param_to_local, local_offset, param_count, source_module))
                .collect(),
            condition: condition
                .as_ref()
                .map(|c| remap_expr(c, param_to_local, local_offset, param_count, source_module)),
            body: remap_block(
                body,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            update: update
                .as_ref()
                .map(|u| remap_expr(u, param_to_local, local_offset, param_count, source_module)),
        },
        TirStmtKind::Loop { body } => TirStmtKind::Loop {
            body: remap_block(
                body,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
        },
        TirStmtKind::ForOf {
            binding_local,
            binding_type,
            is_mut,
            iterable,
            iterable_type,
            body,
        } => TirStmtKind::ForOf {
            binding_local: remap_local_index(
                *binding_local,
                param_to_local,
                local_offset,
                param_count,
            ),
            binding_type: *binding_type,
            is_mut: *is_mut,
            iterable: remap_expr(
                iterable,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            iterable_type: *iterable_type,
            body: remap_block(
                body,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
        },
        TirStmtKind::LabeledBlock { label, block } => TirStmtKind::LabeledBlock {
            label: label.clone(),
            block: remap_block(
                block,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
        },
        TirStmtKind::IfPattern {
            scrutinee,
            pattern,
            then_block,
            else_block,
        } => TirStmtKind::IfPattern {
            scrutinee: remap_expr(
                scrutinee,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            pattern: remap_pattern(pattern, param_to_local, local_offset, param_count),
            then_block: remap_block(
                then_block,
                param_to_local,
                local_offset,
                param_count,
                source_module,
            ),
            else_block: else_block
                .as_ref()
                .map(|b| remap_block(b, param_to_local, local_offset, param_count, source_module)),
        },
        TirStmtKind::Break { label, value } => TirStmtKind::Break {
            label: label.clone(),
            value: value
                .as_ref()
                .map(|v| remap_expr(v, param_to_local, local_offset, param_count, source_module)),
        },
        TirStmtKind::Continue => TirStmtKind::Continue,
    };

    TirStmt::new(kind, stmt.span)
}

/// Recursively inline calls within an expression
fn inline_calls_in_expr(
    expr: &mut TirExpr,
    candidates: &HashMap<(Vec<String>, String), TirFunction>,
    current_module: &[String],
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    pre_stmts: &mut Vec<TirStmt>,
    inlined_funcs: &mut Vec<(Vec<String>, String)>,
    inline_counter: &mut u32,
) {
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            inline_calls_in_expr(
                left,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
            inline_calls_in_expr(
                right,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
        }
        TirExprKind::Unary { expr: inner, .. } => {
            inline_calls_in_expr(
                inner,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
        }
        TirExprKind::Assign { target, value } => {
            inline_calls_in_expr(
                target,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
            inline_calls_in_expr(
                value,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
        }
        TirExprKind::Cast { expr: inner, .. } => {
            inline_calls_in_expr(
                inner,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
        }
        TirExprKind::Call { args, .. } => {
            // First, recursively process arguments
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                    inline_counter,
                );
            }
            // Try to inline this call
            if let Some((inlined_expr, inlined_key)) = try_inline_call_expr(
                expr,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                inline_counter,
            ) {
                if !inlined_funcs.contains(&inlined_key) {
                    inlined_funcs.push(inlined_key);
                }
                *expr = inlined_expr;
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            // First, recursively process subexpressions
            inline_calls_in_expr(
                receiver,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                    inline_counter,
                );
            }
            // Try to inline this method call
            if let Some((inlined_expr, inlined_key)) = try_inline_method_call_expr(
                expr,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                inline_counter,
            ) {
                if !inlined_funcs.contains(&inlined_key) {
                    inlined_funcs.push(inlined_key);
                }
                *expr = inlined_expr;
            }
        }
        TirExprKind::StaticCall { args, .. } => {
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                    inline_counter,
                );
            }
        }
        TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                    inline_counter,
                );
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            inline_calls_in_expr(
                inner,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
        }
        TirExprKind::Index { expr: inner, index } => {
            inline_calls_in_expr(
                inner,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
            inline_calls_in_expr(
                index,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                inline_calls_in_expr(
                    &mut field.value,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                    inline_counter,
                );
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                inline_calls_in_expr(
                    elem,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                    inline_counter,
                );
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            inline_calls_in_expr(
                callee,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
            for arg in args {
                inline_calls_in_expr(
                    arg,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                    inline_counter,
                );
            }
        }
        TirExprKind::OptionSome { value } => {
            inline_calls_in_expr(
                value,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                inline_calls_in_expr(
                    field,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    pre_stmts,
                    inlined_funcs,
                    inline_counter,
                );
            }
        }
        TirExprKind::Move { value } => {
            inline_calls_in_expr(
                value,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
        }
        TirExprKind::LabeledBlock { block, .. } => {
            // Process the block for nested inlining opportunities
            inline_calls_in_block(
                block,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                inlined_funcs,
                inline_counter,
            );
        }
        // For if/match expressions, we don't inline recursively here
        // as they would need proper block handling
        _ => {}
    }
}

// Note: create_default_value is kept but currently unused since we use labeled block expressions
// which don't need a default value initialization. It may be useful for future optimizations.
#[allow(dead_code)]
fn _create_default_value(type_id: TypeId, type_table: &TypeTable, span: crate::Span) -> TirExpr {
    create_default_value(type_id, type_table, span)
}
