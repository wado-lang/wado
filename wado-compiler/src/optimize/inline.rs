//! Function inlining optimization for Wado TIR
//!
//! This module provides function inlining for small functions.
//! It uses labeled block expressions for cleaner value handling.

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::name::ModuleSource;
use crate::tir::{
    CallArg, InlineHint, ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirPattern,
    TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::block_has_break_to;
use crate::token::Span;

// The inline threshold is based on expression count, which provides a more
// accurate measure of function complexity than statement count.
// - Simple statements like `let x = 1` have 1 expression
// - Complex statements like `let x = foo() + bar()` have 3+ expressions
// - Method calls, binary operations, field accesses all contribute

/// Count expressions in a TIR expression (recursive)
fn count_expr(expr: &TirExpr) -> usize {
    1 + match &expr.kind {
        TirExprKind::Binary { left, right, .. } => count_expr(left) + count_expr(right),
        TirExprKind::Unary { expr, .. } => count_expr(expr),
        TirExprKind::Call { args, .. } => args.iter().map(|a| count_expr(&a.expr)).sum(),
        TirExprKind::MethodCall { receiver, args, .. } => {
            count_expr(receiver) + args.iter().map(|a| count_expr(&a.expr)).sum::<usize>()
        }
        TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        } => count_expr(expr),
        TirExprKind::Index { expr, index, .. } => count_expr(expr) + count_expr(index),
        TirExprKind::TupleLiteral { elements } => elements.iter().map(count_expr).sum(),
        TirExprKind::StructLiteral { fields, .. } => {
            fields.iter().map(|f| count_expr(&f.value)).sum()
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            payload.as_ref().map_or(0, |p| count_expr(p))
        }
        TirExprKind::Assign { target, value } => count_expr(target) + count_expr(value),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            count_expr(condition)
                + count_block_exprs(then_branch)
                + else_branch.as_ref().map_or(0, count_block_exprs)
        }
        TirExprKind::Match { expr, arms } => {
            count_expr(expr)
                + arms
                    .iter()
                    .map(|arm| arm.guard.as_ref().map_or(0, count_expr) + count_expr(&arm.body))
                    .sum::<usize>()
        }
        TirExprKind::Block(block) => count_block_exprs(block),
        TirExprKind::Cast { expr, .. } => count_expr(expr),
        TirExprKind::GlobalVarSet { value, .. } => count_expr(value),
        // Leaf expressions (no children)
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::Null => 0,
        // Closure and effect-related expressions
        TirExprKind::Capture { .. } | TirExprKind::EnumConstruct { .. } => 0,
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::CmRawCall { args, .. } => args.iter().map(count_expr).sum(),
        TirExprKind::IndirectCall { callee, args } => {
            count_expr(callee) + args.iter().map(count_expr).sum::<usize>()
        }
        TirExprKind::Closure { body, .. } => count_expr(body),
        TirExprKind::ClosureToCanonical { functor, .. } => count_expr(functor),
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_expr(scrutinee)
                + arms.iter().map(count_block_exprs).sum::<usize>()
                + count_block_exprs(default)
        }
        // Lowered pattern matching nodes - count inner expressions
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => count_expr(expr),
        TirExprKind::LabeledBlock { block, .. } => count_block_exprs(block),
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Count expressions in a TIR block (recursive)
fn count_block_exprs(block: &TirBlock) -> usize {
    block
        .stmts
        .iter()
        .map(|s| match &s.kind {
            TirStmtKind::Expr(expr) => count_expr(expr),
            TirStmtKind::Let { value, .. } => count_expr(value),
            TirStmtKind::LetDestructure { value, .. } => count_expr(value),
            TirStmtKind::Return { value } => value.as_ref().map_or(0, count_expr),
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                count_expr(condition)
                    + count_block_exprs(then_block)
                    + else_block.as_ref().map_or(0, count_block_exprs)
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                count_expr(scrutinee)
                    + count_block_exprs(then_block)
                    + else_block.as_ref().map_or(0, count_block_exprs)
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                count_block_exprs(body)
            }
            TirStmtKind::Break { .. } | TirStmtKind::Continue => 0,
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        })
        .sum()
}

/// Compute the fully qualified name of a `TirFunction`, using the same format
/// as `FuncRef::full_name()`.  This is the key used by `collect_callees_from_expr`
/// so it must match exactly.
fn tir_function_full_name(func: &TirFunction) -> String {
    if let Some(info) = &func.method_info {
        info.to_mangled_name()
    } else if func.module_source.is_entry_point() {
        func.name.clone()
    } else {
        let path = func.module_source.to_path();
        format!("{}/{}", path.join("/"), &func.name)
    }
}

fn collect_inner_labels_from_block(block: &TirBlock, labels: &mut IndexSet<String>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::LabeledBlock { label, block } => {
                labels.insert(label.clone());
                collect_inner_labels_from_block(block, labels);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_inner_labels_from_expr(condition, labels);
                collect_inner_labels_from_block(then_block, labels);
                if let Some(else_block) = else_block {
                    collect_inner_labels_from_block(else_block, labels);
                }
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                collect_inner_labels_from_expr(scrutinee, labels);
                collect_inner_labels_from_block(then_block, labels);
                if let Some(else_block) = else_block {
                    collect_inner_labels_from_block(else_block, labels);
                }
            }
            TirStmtKind::Loop { body } => collect_inner_labels_from_block(body, labels),
            TirStmtKind::Expr(expr)
            | TirStmtKind::Let { value: expr, .. }
            | TirStmtKind::LetDestructure { value: expr, .. } => {
                collect_inner_labels_from_expr(expr, labels);
            }
            TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
                if let Some(value) = value {
                    collect_inner_labels_from_expr(value, labels);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }
}

fn collect_inner_labels_from_expr(expr: &TirExpr, labels: &mut IndexSet<String>) {
    match &expr.kind {
        TirExprKind::Block(block) => collect_inner_labels_from_block(block, labels),
        TirExprKind::LabeledBlock { label, block, .. } => {
            labels.insert(label.clone());
            collect_inner_labels_from_block(block, labels);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_inner_labels_from_expr(condition, labels);
            collect_inner_labels_from_block(then_branch, labels);
            if let Some(else_branch) = else_branch {
                collect_inner_labels_from_block(else_branch, labels);
            }
        }
        TirExprKind::Match { expr, arms } => {
            collect_inner_labels_from_expr(expr, labels);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_inner_labels_from_expr(guard, labels);
                }
                collect_inner_labels_from_expr(&arm.body, labels);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_inner_labels_from_expr(scrutinee, labels);
            for arm in arms {
                collect_inner_labels_from_block(arm, labels);
            }
            collect_inner_labels_from_block(default, labels);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_inner_labels_from_expr(left, labels);
            collect_inner_labels_from_expr(right, labels);
        }
        TirExprKind::Unary { expr, .. }
        | TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::Cast { expr, .. }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => collect_inner_labels_from_expr(expr, labels),
        TirExprKind::TypePackExpansion { call_expr, .. } => {
            collect_inner_labels_from_expr(call_expr, labels);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                collect_inner_labels_from_expr(&arg.expr, labels);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_inner_labels_from_expr(receiver, labels);
            for arg in args {
                collect_inner_labels_from_expr(&arg.expr, labels);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_inner_labels_from_expr(arg, labels);
            }
        }
        TirExprKind::Index { expr, index } => {
            collect_inner_labels_from_expr(expr, labels);
            collect_inner_labels_from_expr(index, labels);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_inner_labels_from_expr(&field.value, labels);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                collect_inner_labels_from_expr(elem, labels);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload) = payload {
                collect_inner_labels_from_expr(payload, labels);
            }
        }
        TirExprKind::Assign { target, value } => {
            collect_inner_labels_from_expr(target, labels);
            collect_inner_labels_from_expr(value, labels);
        }
        TirExprKind::Closure { body, .. } => collect_inner_labels_from_expr(body, labels),
        TirExprKind::IndirectCall { callee, args } => {
            collect_inner_labels_from_expr(callee, labels);
            for arg in args {
                collect_inner_labels_from_expr(arg, labels);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_inner_labels_from_expr(functor, labels);
        }
        TirExprKind::GlobalVarSet { value, .. } => collect_inner_labels_from_expr(value, labels),
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Check if a function is eligible for inlining.
fn is_inline_eligible(
    func: &TirFunction,
    recursive_functions: &IndexSet<String>,
    _module_source: &ModuleSource,
    type_table: &TypeTable,
    inline_threshold: usize,
) -> bool {
    // #[inline(never)] unconditionally prevents inlining
    if func.inline_hint == InlineHint::Never {
        return false;
    }

    // Must have a body
    let Some(body) = &func.body else {
        return false;
    };

    // Don't inline CM binding functions - they are ABI bridges between
    // Wado GC types and CM linear memory that must remain as separate functions
    if func.is_cm_binding {
        return false;
    }

    // #[inline(always)] skips all heuristic checks (but still requires a body and non-adapter)
    if func.inline_hint == InlineHint::Always {
        return true;
    }

    // Don't inline functions that return Never (!)
    // These are error/abort paths that are never hot, so no performance benefit to inlining
    if matches!(type_table.get(func.return_type), ResolvedType::Never) {
        return false;
    }

    // Not recursive — compare using the same fully qualified name used to build
    // the recursive set, so that cross-module recursive functions are not missed.
    if recursive_functions.contains(&tir_function_full_name(func)) {
        return false;
    }

    // `#[inline]` hint raises the threshold by 5x, allowing functions up to 50
    // expressions (at the default threshold of 10) to be inlined.
    let effective_threshold = if func.inline_hint == InlineHint::Hint {
        inline_threshold * 5
    } else {
        inline_threshold
    };

    // Small enough (based on expression count)
    count_block_exprs(body) <= effective_threshold
}

/// Detect recursive functions using call graph analysis
fn find_recursive_functions(functions: &[Rc<RefCell<TirFunction>>]) -> IndexSet<String> {
    // Phase 1: Build fully-qualified-name→index mapping.
    // We use `tir_function_full_name` here so that the keys match the callee names
    // produced by `collect_callees_from_expr` (which uses `FuncRef::full_name()`).
    // Using only `func.name` caused cross-module recursive functions to go
    // undetected, because the callee strings carried the module prefix while
    // the node keys did not.
    let mut name_to_idx: IndexMap<String, usize> = IndexMap::default();
    let mut idx_to_name: Vec<String> = Vec::new();

    for func_rc in functions {
        let func = func_rc.borrow();
        let name = tir_function_full_name(&func);
        if !name_to_idx.contains_key(&name) {
            let idx = idx_to_name.len();
            idx_to_name.push(name.clone());
            name_to_idx.insert(name, idx);
        }
    }

    let n = idx_to_name.len();
    // Phase 2: Build call graph using indices (no String allocations in inner loop)
    let mut call_graph: Vec<Vec<usize>> = vec![Vec::new(); n];

    for func_rc in functions {
        let func = func_rc.borrow();
        let full_name = tir_function_full_name(&func);
        if let Some(caller_idx) = name_to_idx.get(&full_name) {
            let mut callee_names: IndexSet<String> = IndexSet::default();
            if let Some(body) = &func.body {
                collect_callees_from_block(body, &mut callee_names);
            }
            let callees: Vec<usize> = callee_names
                .iter()
                .filter_map(|name| name_to_idx.get(name).copied())
                .collect();
            call_graph[*caller_idx] = callees;
        }
    }

    // Phase 3: Find functions that can reach themselves using index-based DFS
    let mut recursive = IndexSet::default();
    let mut visited = vec![false; n];

    for func_idx in 0..n {
        visited.fill(false);
        if can_reach_idx(&call_graph, func_idx, func_idx, &mut visited) {
            recursive.insert(idx_to_name[func_idx].clone());
        }
    }

    recursive
}

fn can_reach_idx(
    call_graph: &[Vec<usize>],
    start: usize,
    target: usize,
    visited: &mut [bool],
) -> bool {
    if visited[start] {
        return false;
    }
    visited[start] = true;

    for &callee in &call_graph[start] {
        if callee == target {
            return true;
        }
        if can_reach_idx(call_graph, callee, target, visited) {
            return true;
        }
    }

    false
}

fn collect_callees_from_block(block: &TirBlock, callees: &mut IndexSet<String>) {
    for stmt in &block.stmts {
        collect_callees_from_stmt(stmt, callees);
    }
}

fn collect_callees_from_stmt(stmt: &TirStmt, callees: &mut IndexSet<String>) {
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
        TirStmtKind::Loop { body } => {
            collect_callees_from_block(body, callees);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_callees_from_block(block, callees);
        }
        TirStmtKind::IfLet {
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
        TirStmtKind::LetDestructure { value, .. } => {
            collect_callees_from_expr(value, callees);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn collect_callees_from_expr(expr: &TirExpr, callees: &mut IndexSet<String>) {
    match &expr.kind {
        TirExprKind::Call { func, args, .. } => {
            callees.insert(func.full_name());
            for arg in args {
                collect_callees_from_expr(&arg.expr, callees);
            }
        }
        TirExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            callees.insert(func.full_name());
            collect_callees_from_expr(receiver, callees);
            for arg in args {
                collect_callees_from_expr(&arg.expr, callees);
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
        TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TupleZip { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        } => {
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
        TirExprKind::TupleLiteral { elements } => {
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
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_callees_from_expr(functor, callees);
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_callees_from_expr(arg, callees);
            }
        }
        TirExprKind::Match { expr, arms } => {
            collect_callees_from_expr(expr, callees);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_callees_from_expr(guard, callees);
                }
                collect_callees_from_expr(&arm.body, callees);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_callees_from_expr(payload_expr, callees);
            }
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_callees_from_block(block, callees);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_callees_from_expr(value, callees);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => {
            collect_callees_from_expr(expr, callees);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_callees_from_expr(scrutinee, callees);
            for arm in arms {
                collect_callees_from_block(arm, callees);
            }
            collect_callees_from_block(default, callees);
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
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Inline eligible functions at their call sites
///
/// The `inline_threshold` parameter controls the maximum number of statements
/// a function can have to be considered for inlining.
pub fn inline_functions(project: &mut FlatPackage, inline_threshold: usize) -> bool {
    let recursive_functions = find_recursive_functions(&project.functions);

    // Collect inline candidates from all modules
    // Key: (module_source, func_name), Value: cloned function
    let mut inline_candidates: IndexMap<(ModuleSource, String), TirFunction> = IndexMap::default();

    // Also collect function_strings for each candidate (to update caller's strings after inlining)
    let mut candidate_strings: IndexMap<(ModuleSource, String), Vec<String>> = IndexMap::default();

    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let module_source = &func.module_source;
        let key = (module_source.clone(), func.name.clone());
        if is_inline_eligible(
            &func,
            &recursive_functions,
            module_source,
            &type_table,
            inline_threshold,
        ) {
            inline_candidates.insert(key.clone(), func.clone());
            // Get the strings used by this function
            if let Some(strings) = project.function_strings.get(&key) {
                candidate_strings.insert(key, strings.clone());
            }
        }
    }
    drop(type_table);

    if inline_candidates.is_empty() {
        return false;
    }

    let mut changed = false;

    // Inline at call sites
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let caller_module_source = func.module_source.clone();
        let func_name = func.name.clone();
        if let Some(mut body) = func.body.take() {
            // Track which functions were inlined into this function
            let mut inlined_funcs: Vec<(ModuleSource, String)> = Vec::new();
            // Take ownership of local_count and local_types to avoid borrow conflicts
            let mut local_count = func.local_count;
            let mut local_types = std::mem::take(&mut func.local_types);
            // Counter for generating unique inline labels
            let mut inline_counter: u32 = 0;
            inline_calls_in_block(
                &mut body,
                &inline_candidates,
                &caller_module_source,
                &mut local_count,
                &mut local_types,
                &project.type_table.borrow(),
                &mut inlined_funcs,
                &mut inline_counter,
            );
            func.local_count = local_count;
            func.local_types = local_types;
            func.body = Some(body);

            if !inlined_funcs.is_empty() {
                changed = true;
            }

            // Update function_strings: add strings from inlined functions to the caller
            let mut all_inlined_strings: IndexSet<String> = IndexSet::default();
            for inlined_key in inlined_funcs {
                if let Some(inlined_strings) = candidate_strings.get(&inlined_key) {
                    all_inlined_strings.extend(inlined_strings.iter().cloned());
                }
            }
            if !all_inlined_strings.is_empty() {
                // Need to drop func borrow before borrowing project.function_strings mutably
                drop(func);
                {
                    let caller_strings = project
                        .function_strings
                        .entry((caller_module_source.clone(), func_name.clone()))
                        .or_default();
                    let existing: IndexSet<&str> =
                        caller_strings.iter().map(String::as_str).collect();
                    let to_add: Vec<String> = all_inlined_strings
                        .iter()
                        .filter(|s| !existing.contains(s.as_str()))
                        .cloned()
                        .collect();
                    caller_strings.extend(to_add);
                }
                let to_add: Vec<String> = {
                    let existing_literals: IndexSet<&str> =
                        project.string_literals.iter().map(String::as_str).collect();
                    all_inlined_strings
                        .into_iter()
                        .filter(|s| !existing_literals.contains(s.as_str()))
                        .collect()
                };
                project.string_literals.extend(to_add);
            }
        }
    }
    changed
}

/// Inline function calls in a block
fn inline_calls_in_block(
    block: &mut TirBlock,
    candidates: &IndexMap<(ModuleSource, String), TirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<(ModuleSource, String)>,
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
                skip_value_copy,
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
                        inlined_funcs.push(inlined_key.clone());
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
                            skip_value_copy,
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
                            skip_value_copy,
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
            TirStmtKind::IfLet {
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
                    TirStmtKind::IfLet {
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
            TirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => {
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
                    TirStmtKind::LetDestructure {
                        pattern,
                        is_mut,
                        value: new_value,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase")
            }
            TirStmtKind::VariadicForOf { .. } => {
                unreachable!("VariadicForOf should be expanded during monomorphization")
            }
        }
    }

    block.stmts = new_stmts;
}

/// Look up an inline candidate by module path and function name.
fn find_inline_candidate<'a>(
    candidates: &'a IndexMap<(ModuleSource, String), TirFunction>,
    call_module_source: &ModuleSource,
    current_module: &ModuleSource,
    func_name: &str,
) -> Option<(&'a TirFunction, (ModuleSource, String))> {
    // Use the call's module_source directly; fall back to caller's module for local calls
    let target_module = if call_module_source.is_entry_point() {
        current_module.clone()
    } else {
        call_module_source.clone()
    };

    let key = (target_module, func_name.to_string());
    candidates.get(&key).map(|c| (c, key))
}

/// Binding for a single parameter during inlining.
///
/// Each binding becomes a `Let` statement at the head of the synthesized
/// labeled block. Fields carry the information needed without requiring the
/// shared helper to know whether the call site is a free function or a method.
struct InlineBinding {
    /// Original callee-side local index of the parameter being bound.
    /// Used to build the `param_to_local` remapping for the inlined body.
    callee_local_index: u32,
    /// Parameter name (used verbatim as the new `let` name for debuggability).
    name: String,
    /// Whether the synthesized `let` should be mutable. Free function calls
    /// preserve the original parameter's `is_mut`; method calls always pass
    /// `false` (the method body cannot rebind `self` or its arguments).
    is_mut: bool,
    /// Type of the value bound to the new local. This may differ from
    /// `param.type_id` due to monomorphization (arg type differs from param type)
    /// or `&mut self` wrapping (receiver gets wrapped in a `MutRef` unary).
    local_type: TypeId,
    /// The value expression bound to the new local.
    value: TirExpr,
}

/// Core inlining routine: builds a labeled block that binds each prepared
/// parameter value and executes the callee body with locals remapped into the
/// caller's frame.
///
/// Shared by `try_inline_call_expr` and `try_inline_method_call_expr`. The
/// difference between the two lies entirely in how they prepare `bindings`.
fn build_inlined_labeled_block(
    candidate: &TirFunction,
    body: &TirBlock,
    func_name: &str,
    bindings: Vec<InlineBinding>,
    call_span: Span,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    inline_counter: &mut u32,
) -> TirExpr {
    // Generate unique label for this inline site.
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

    // Calculate local index offset for remapping.
    let local_offset = *local_count;

    let callee_param_count = candidate.params.len() as u32;
    let callee_local_count = candidate.local_count;
    let new_locals_needed = callee_local_count.saturating_sub(callee_param_count);

    // IMPORTANT: Push param types first to match index assignment order
    // (params get indices local_offset+0, local_offset+1, ..., then non-params follow)
    let mut block_stmts = Vec::with_capacity(bindings.len() + body.stmts.len());
    let mut param_to_local: IndexMap<u32, u32> = IndexMap::default();

    for (i, binding) in bindings.into_iter().enumerate() {
        let new_local_index = local_offset + i as u32;
        param_to_local.insert(binding.callee_local_index, new_local_index);

        // Extend local_types for parameter using the binding's actual local_type
        // (handles monomorphization type variance and &mut self ref wrapping).
        local_types.push(binding.local_type);
        *local_count += 1;

        // Use original parameter name (not _inline_ prefix).
        block_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: binding.name,
                local_index: new_local_index,
                is_mut: binding.is_mut,
                is_reactive: false,
                type_id: binding.local_type,
                value: binding.value,
                skip_value_copy: false,
            },
            call_span,
        ));
    }

    // param_offset marks where non-param locals start (after all params).
    let param_offset = local_offset + callee_param_count;

    // Now extend local_types for the non-parameter locals.
    for i in callee_param_count..callee_local_count {
        if let Some(&type_id) = candidate.local_types.get(i as usize) {
            local_types.push(type_id);
        }
    }
    *local_count += new_locals_needed;

    let mut inner_labels: IndexSet<String> = IndexSet::default();
    collect_inner_labels_from_block(body, &mut inner_labels);
    let mut label_map: IndexMap<String, String> = IndexMap::default();
    for inner_label in inner_labels {
        label_map.insert(inner_label.clone(), format!("{label}__{inner_label}"));
    }

    // Convert the body, transforming `return` into `break label: expr`.
    let remapped_stmts = remap_and_convert_returns(
        body,
        &param_to_local,
        param_offset,
        callee_param_count,
        &label,
        &label_map,
    );

    block_stmts.extend(remapped_stmts);

    // Create a labeled block expression that produces the return value.
    TirExpr::new(
        TirExprKind::LabeledBlock {
            label,
            block: TirBlock::new(block_stmts, call_span),
            result_type: candidate.return_type,
        },
        candidate.return_type,
        call_span,
    )
}

/// Try to inline a free function call expression, returning the inlined
/// expression and the callee's lookup key.
fn try_inline_call_expr(
    expr: &TirExpr,
    candidates: &IndexMap<(ModuleSource, String), TirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    _type_table: &TypeTable,
    inline_counter: &mut u32,
) -> Option<(TirExpr, (ModuleSource, String))> {
    let TirExprKind::Call { func, args, .. } = &expr.kind else {
        return None;
    };

    let func_name = func.name.clone();
    let (candidate, inlined_key) =
        find_inline_candidate(candidates, &func.module_source, current_module, &func_name)?;
    let body = candidate.body.as_ref()?;

    // Use argument's type_id to match the actual value being assigned
    // (handles monomorphization type variance).
    let bindings: Vec<InlineBinding> = candidate
        .params
        .iter()
        .zip(args.iter())
        .map(|(param, arg)| InlineBinding {
            callee_local_index: param.local_index,
            name: param.name.clone(),
            is_mut: param.is_mut,
            local_type: arg.expr.type_id,
            value: arg.expr.clone(),
        })
        .collect();

    let inlined_expr = build_inlined_labeled_block(
        candidate,
        body,
        &func_name,
        bindings,
        expr.span,
        local_count,
        local_types,
        inline_counter,
    );

    Some((inlined_expr, inlined_key))
}

/// Try to inline a method call expression, returning the inlined expression
/// and the callee's lookup key.
fn try_inline_method_call_expr(
    expr: &TirExpr,
    candidates: &IndexMap<(ModuleSource, String), TirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    inline_counter: &mut u32,
) -> Option<(TirExpr, (ModuleSource, String))> {
    let TirExprKind::MethodCall {
        receiver,
        func,
        args,
        ..
    } = &expr.kind
    else {
        return None;
    };

    let func_name = func.name.clone();
    let (candidate, inlined_key) =
        find_inline_candidate(candidates, &func.module_source, current_module, &func_name)?;
    let body = candidate.body.as_ref()?;

    let mut bindings: Vec<InlineBinding> = Vec::with_capacity(candidate.params.len());

    // Bind receiver to first parameter (self).
    // For &mut self receivers, wrap in a MutRef expression so that field
    // mutations (`self.field = x`) translate to WIR StructSet on the original
    // receiver rather than on a value copy. A value copy would lose writes.
    // For &self receivers, a value copy is safe (no mutations) and lets copy
    // propagation simplify `self.field` → `receiver.field` without a ref level.
    let first_param = &candidate.params[0];
    let (self_type_id, self_value) =
        if matches!(type_table.get(first_param.type_id), ResolvedType::MutRef(_)) {
            if matches!(type_table.get(receiver.type_id), ResolvedType::MutRef(_)) {
                // Receiver is already &mut T — pass through without double-wrapping.
                // This happens when an &mut self method is called on a local whose
                // type is already &mut T (e.g. after inlining a sequence literal builder).
                (receiver.type_id, (**receiver).clone())
            } else {
                let ref_expr = TirExpr {
                    kind: TirExprKind::Unary {
                        op: TirUnaryOp::MutRef,
                        expr: receiver.clone(),
                    },
                    type_id: first_param.type_id,
                    span: expr.span,
                };
                (first_param.type_id, ref_expr)
            }
        } else {
            (receiver.type_id, (**receiver).clone())
        };
    bindings.push(InlineBinding {
        callee_local_index: first_param.local_index,
        name: first_param.name.clone(),
        is_mut: first_param.is_mut,
        local_type: self_type_id,
        value: self_value,
    });

    // Bind remaining args to remaining parameters.
    // Use argument's type_id to handle monomorphization type variance.
    for (param, arg) in candidate.params.iter().skip(1).zip(args.iter()) {
        bindings.push(InlineBinding {
            callee_local_index: param.local_index,
            name: param.name.clone(),
            is_mut: param.is_mut,
            local_type: arg.expr.type_id,
            value: arg.expr.clone(),
        });
    }

    let inlined_expr = build_inlined_labeled_block(
        candidate,
        body,
        &func_name,
        bindings,
        expr.span,
        local_count,
        local_types,
        inline_counter,
    );

    Some((inlined_expr, inlined_key))
}

/// Remap locals and convert returns to break statements with the given label.
///
/// Scope blocks (`LabeledBlock` stmts) whose labels are not targeted by any
/// `break` in their body are flattened into the parent. This is safe because
/// inlining remaps all locals to unique indices, making variable scoping
/// irrelevant. Without flattening, intermediate void scope blocks between the
/// inline label and a `break` targeting it produce invalid Wasm: the void
/// block's fallthrough leaves nothing on the stack for the outer typed block.
fn remap_and_convert_returns(
    block: &TirBlock,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &str,
    label_map: &IndexMap<String, String>,
) -> Vec<TirStmt> {
    let mut stmts = Vec::new();

    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Return { value } => {
                // Convert return to break with the inline label.
                // Use label-aware remapping because the return value expression
                // may itself contain nested blocks with return statements
                // (e.g., try-op expansions inside tuple/struct literals).
                let break_value = value.as_ref().map(|v| {
                    remap_expr_inner(
                        v,
                        param_to_local,
                        local_offset,
                        param_count,
                        Some(label),
                        label_map,
                    )
                });
                stmts.push(TirStmt::new(
                    TirStmtKind::Break {
                        label: Some(label.to_string()),
                        value: break_value,
                    },
                    stmt.span,
                ));
            }
            TirStmtKind::LabeledBlock {
                label: inner_label,
                block: inner_block,
            } if !block_has_break_to(inner_label, inner_block) => {
                // Flatten: scope block has no breaks targeting its own label,
                // so just inline its stmts into the parent
                let inner = remap_and_convert_returns(
                    inner_block,
                    param_to_local,
                    local_offset,
                    param_count,
                    label,
                    label_map,
                );
                stmts.extend(inner);
            }
            _ => {
                stmts.push(remap_stmt_with_label(
                    stmt,
                    param_to_local,
                    local_offset,
                    param_count,
                    label,
                    label_map,
                ));
            }
        }
    }

    stmts
}

fn remap_stmt_with_label(
    stmt: &TirStmt,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &str,
    label_map: &IndexMap<String, String>,
) -> TirStmt {
    remap_stmt_inner(
        stmt,
        param_to_local,
        local_offset,
        param_count,
        Some(label),
        label_map,
    )
}

/// Remap local indices in a pattern
fn remap_pattern(
    pattern: &TirPattern,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> TirPattern {
    match pattern {
        TirPattern::Wildcard => TirPattern::Wildcard,
        TirPattern::Binding {
            name,
            local_index,
            type_id,
        } => TirPattern::Binding {
            name: name.clone(),
            local_index: remap_local_index(*local_index, param_to_local, local_offset, param_count),
            type_id: *type_id,
        },
        TirPattern::Literal(lit) => TirPattern::Literal(lit.clone()),
        TirPattern::Tuple(patterns, has_rest) => TirPattern::Tuple(
            patterns
                .iter()
                .map(|p| remap_pattern(p, param_to_local, local_offset, param_count))
                .collect(),
            *has_rest,
        ),
        TirPattern::Variant {
            enum_type,
            variant_name,
            bindings,
            payload_type,
        } => TirPattern::Variant {
            enum_type: *enum_type,
            variant_name: variant_name.clone(),
            bindings: bindings
                .iter()
                .map(|p| remap_pattern(p, param_to_local, local_offset, param_count))
                .collect(),
            payload_type: *payload_type,
        },
        TirPattern::Enum {
            enum_type,
            case_name,
            case_index,
        } => TirPattern::Enum {
            enum_type: *enum_type,
            case_name: case_name.clone(),
            case_index: *case_index,
        },
        TirPattern::Struct {
            struct_type,
            fields,
            has_rest,
        } => TirPattern::Struct {
            struct_type: *struct_type,
            fields: fields
                .iter()
                .map(|f| crate::tir::TirStructPatternField {
                    field_name: f.field_name.clone(),
                    field_index: f.field_index,
                    pattern: remap_pattern(&f.pattern, param_to_local, local_offset, param_count),
                })
                .collect(),
            has_rest: *has_rest,
        },
        TirPattern::Or(alternatives) => TirPattern::Or(
            alternatives
                .iter()
                .map(|p| remap_pattern(p, param_to_local, local_offset, param_count))
                .collect(),
        ),
        TirPattern::ConstantValue { expr } => TirPattern::ConstantValue { expr: expr.clone() },
        TirPattern::Range {
            start,
            end,
            inclusive,
            is_unsigned,
        } => TirPattern::Range {
            start: *start,
            end: *end,
            inclusive: *inclusive,
            is_unsigned: *is_unsigned,
        },
    }
}

/// Remap a local index
fn remap_local_index(
    index: u32,
    param_to_local: &IndexMap<u32, u32>,
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

fn remap_expr(
    expr: &TirExpr,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> TirExpr {
    remap_expr_inner(
        expr,
        param_to_local,
        local_offset,
        param_count,
        None,
        &IndexMap::default(),
    )
}

/// Remap local indices in an expression, optionally converting `return` to `break`.
///
/// When `label` is `Some`, any `return` statement reachable from this expression
/// (e.g. inside blocks nested in struct literals, tuple literals, variant payloads)
/// is converted to `break label: value`. This is critical for correct inlining of
/// functions whose bodies contain early returns inside nested expressions.
fn remap_expr_inner(
    expr: &TirExpr,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: Option<&str>,
    label_map: &IndexMap<String, String>,
) -> TirExpr {
    let re = |e: &TirExpr| {
        remap_expr_inner(
            e,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        )
    };
    let re_box = |e: &TirExpr| Box::new(re(e));
    let rb = |b: &TirBlock| {
        remap_block_inner(
            b,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        )
    };

    let kind = match &expr.kind {
        TirExprKind::Local { index, name } => {
            let new_index = remap_local_index(*index, param_to_local, local_offset, param_count);
            TirExprKind::Local {
                index: new_index,
                name: name.clone(),
            }
        }
        TirExprKind::Binary { left, op, right } => TirExprKind::Binary {
            left: re_box(left),
            op: *op,
            right: re_box(right),
        },
        TirExprKind::Unary { op, expr: inner } => TirExprKind::Unary {
            op: *op,
            expr: re_box(inner),
        },
        TirExprKind::Assign { target, value } => TirExprKind::Assign {
            target: re_box(target),
            value: re_box(value),
        },
        TirExprKind::Cast {
            expr: inner,
            target_type,
        } => TirExprKind::Cast {
            expr: re_box(inner),
            target_type: *target_type,
        },
        TirExprKind::Call {
            func,
            type_args,
            args,
        } => TirExprKind::Call {
            func: func.clone(),
            type_args: type_args.clone(),
            args: args
                .iter()
                .map(|a| CallArg::new(re(&a.expr), a.is_mut))
                .collect(),
        },
        TirExprKind::MethodCall {
            receiver,
            func,
            type_args,
            args,
            ..
        } => TirExprKind::method_call(
            re_box(receiver),
            func.clone(),
            type_args.clone(),
            args.iter()
                .map(|a| CallArg::new(re(&a.expr), a.is_mut))
                .collect(),
        ),
        TirExprKind::CmRawCall { local_name, args } => TirExprKind::CmRawCall {
            local_name: local_name.clone(),
            args: args.iter().map(&re).collect(),
        },
        TirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => TirExprKind::FieldAccess {
            expr: re_box(inner),
            field_index: *field_index,
            field_name: field_name.clone(),
        },
        TirExprKind::TupleSpread { expr: inner } => TirExprKind::TupleSpread {
            expr: re_box(inner),
        },
        TirExprKind::TupleZip { expr: inner } => TirExprKind::TupleZip {
            expr: re_box(inner),
        },
        TirExprKind::TypePackExpansion {
            call_expr: inner,
            pack_type_id,
        } => TirExprKind::TypePackExpansion {
            call_expr: re_box(inner),
            pack_type_id: *pack_type_id,
        },
        TirExprKind::Index { expr: inner, index } => TirExprKind::Index {
            expr: re_box(inner),
            index: re_box(index),
        },
        TirExprKind::Block(block) => TirExprKind::Block(rb(block)),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => TirExprKind::If {
            condition: re_box(condition),
            then_branch: rb(then_branch),
            else_branch: else_branch.as_ref().map(&rb),
        },
        TirExprKind::Match { expr: inner, arms } => TirExprKind::Match {
            expr: re_box(inner),
            arms: arms
                .iter()
                .map(|arm| crate::tir::TirMatchArm {
                    pattern: remap_pattern(&arm.pattern, param_to_local, local_offset, param_count),
                    guard: arm.guard.as_ref().map(&re),
                    body: re(&arm.body),
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
                    value: re(&f.value),
                    field_index: f.field_index,
                })
                .collect(),
        },
        TirExprKind::TupleLiteral { elements } => TirExprKind::TupleLiteral {
            elements: elements.iter().map(&re).collect(),
        },
        TirExprKind::Closure {
            params,
            body,
            captures,
            functor_id,
            source_text,
            address_taken_locals,
            local_count,
            local_types,
        } => TirExprKind::Closure {
            params: params.clone(),
            // Closures have their own return scope — don't propagate label
            body: Box::new(remap_expr(body, param_to_local, local_offset, param_count)),
            captures: captures.clone(),
            functor_id: *functor_id,
            source_text: source_text.clone(),
            address_taken_locals: address_taken_locals.clone(),
            local_count: *local_count,
            local_types: local_types.clone(),
        },
        TirExprKind::IndirectCall { callee, args } => TirExprKind::IndirectCall {
            callee: re_box(callee),
            args: args.iter().map(&re).collect(),
        },
        TirExprKind::ClosureToCanonical {
            functor,
            functor_id,
            target_fn_type,
            closure_module,
        } => TirExprKind::ClosureToCanonical {
            functor: re_box(functor),
            functor_id: *functor_id,
            target_fn_type: *target_fn_type,
            closure_module: closure_module.clone(),
        },
        TirExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            payload,
        } => TirExprKind::VariantConstruct {
            variant_type: *variant_type,
            case_index: *case_index,
            case_name: case_name.clone(),
            payload: payload.as_ref().map(|p| Box::new(re(p))),
        },
        TirExprKind::LabeledBlock {
            label: inner_label,
            block,
            result_type,
        } => TirExprKind::LabeledBlock {
            label: label_map
                .get(inner_label)
                .cloned()
                .unwrap_or_else(|| inner_label.clone()),
            block: rb(block),
            result_type: *result_type,
        },
        TirExprKind::GlobalVarSet {
            module_source,
            name,
            value,
        } => TirExprKind::GlobalVarSet {
            module_source: module_source.clone(),
            name: name.clone(),
            value: re_box(value),
        },
        TirExprKind::VariantTag { expr } => TirExprKind::VariantTag { expr: re_box(expr) },
        TirExprKind::VariantTest {
            expr,
            case_index,
            case_name,
        } => TirExprKind::VariantTest {
            expr: re_box(expr),
            case_index: *case_index,
            case_name: case_name.clone(),
        },
        TirExprKind::VariantPayload {
            expr,
            case_index,
            payload_type,
        } => TirExprKind::VariantPayload {
            expr: re_box(expr),
            case_index: *case_index,
            payload_type: *payload_type,
        },
        TirExprKind::Switch {
            scrutinee,
            min_value,
            arms,
            default,
        } => TirExprKind::Switch {
            scrutinee: re_box(scrutinee),
            min_value: *min_value,
            arms: arms.iter().map(&rb).collect(),
            default: rb(default),
        },
        // Leaf nodes - no remapping needed
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => expr.kind.clone(),
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    };

    TirExpr::new(kind, expr.type_id, expr.span)
}

fn remap_block_inner(
    block: &TirBlock,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: Option<&str>,
    label_map: &IndexMap<String, String>,
) -> TirBlock {
    let mut stmts = Vec::new();
    for stmt in &block.stmts {
        if let Some(label) = label {
            match &stmt.kind {
                TirStmtKind::LabeledBlock {
                    label: inner_label,
                    block: inner_block,
                } if !block_has_break_to(inner_label, inner_block) => {
                    // Flatten: scope block has no breaks targeting its own label
                    let inner = remap_and_convert_returns(
                        inner_block,
                        param_to_local,
                        local_offset,
                        param_count,
                        label,
                        label_map,
                    );
                    stmts.extend(inner);
                    continue;
                }
                _ => {}
            }
        }
        stmts.push(remap_stmt_inner(
            stmt,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        ));
    }
    TirBlock::new(stmts, block.span)
}

fn remap_stmt_inner(
    stmt: &TirStmt,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: Option<&str>,
    label_map: &IndexMap<String, String>,
) -> TirStmt {
    let re = |e: &TirExpr| {
        remap_expr_inner(
            e,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        )
    };
    let rb = |b: &TirBlock| {
        remap_block_inner(
            b,
            param_to_local,
            local_offset,
            param_count,
            label,
            label_map,
        )
    };

    let kind = match &stmt.kind {
        TirStmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
            skip_value_copy,
        } => {
            let new_index =
                remap_local_index(*local_index, param_to_local, local_offset, param_count);
            TirStmtKind::Let {
                name: name.clone(),
                local_index: new_index,
                is_mut: *is_mut,
                is_reactive: *is_reactive,
                type_id: *type_id,
                value: re(value),
                skip_value_copy: *skip_value_copy,
            }
        }
        TirStmtKind::Expr(expr) => TirStmtKind::Expr(re(expr)),
        TirStmtKind::Return { value } => {
            if let Some(label) = label {
                // Convert return to break with the inline label
                TirStmtKind::Break {
                    label: Some(label.to_string()),
                    value: value.as_ref().map(&re),
                }
            } else {
                TirStmtKind::Return {
                    value: value.as_ref().map(&re),
                }
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => TirStmtKind::If {
            condition: re(condition),
            then_block: rb(then_block),
            else_block: else_block.as_ref().map(&rb),
        },
        TirStmtKind::Loop { body } => TirStmtKind::Loop { body: rb(body) },
        TirStmtKind::LabeledBlock {
            label: inner_label,
            block,
        } => TirStmtKind::LabeledBlock {
            label: label_map
                .get(inner_label)
                .cloned()
                .unwrap_or_else(|| inner_label.clone()),
            block: rb(block),
        },
        TirStmtKind::IfLet {
            scrutinee,
            pattern,
            then_block,
            else_block,
        } => TirStmtKind::IfLet {
            scrutinee: re(scrutinee),
            pattern: remap_pattern(pattern, param_to_local, local_offset, param_count),
            then_block: rb(then_block),
            else_block: else_block.as_ref().map(rb),
        },
        TirStmtKind::Break {
            label: break_label,
            value,
        } => TirStmtKind::Break {
            label: break_label.as_ref().map(|break_label| {
                label_map
                    .get(break_label)
                    .cloned()
                    .unwrap_or_else(|| break_label.clone())
            }),
            value: value.as_ref().map(&re),
        },
        TirStmtKind::Continue => TirStmtKind::Continue,
        TirStmtKind::LetDestructure {
            pattern,
            is_mut,
            value,
        } => TirStmtKind::LetDestructure {
            pattern: remap_pattern(pattern, param_to_local, local_offset, param_count),
            is_mut: *is_mut,
            value: re(value),
        },
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    };

    TirStmt::new(kind, stmt.span)
}

/// Recursively inline calls within an expression
fn inline_calls_in_expr(
    expr: &mut TirExpr,
    candidates: &IndexMap<(ModuleSource, String), TirFunction>,
    current_module: &ModuleSource,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    pre_stmts: &mut Vec<TirStmt>,
    inlined_funcs: &mut Vec<(ModuleSource, String)>,
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
                    &mut arg.expr,
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
                    &mut arg.expr,
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
        TirExprKind::CmRawCall { args, .. } => {
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
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
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
        TirExprKind::TupleLiteral { elements } => {
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
        TirExprKind::ClosureToCanonical { functor, .. } => {
            inline_calls_in_expr(
                functor,
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
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                inline_calls_in_expr(
                    payload_expr,
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
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            inline_calls_in_expr(
                condition,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
            inline_calls_in_block(
                then_branch,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                inlined_funcs,
                inline_counter,
            );
            if let Some(else_block) = else_branch {
                inline_calls_in_block(
                    else_block,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
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
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    inline_calls_in_expr(
                        guard,
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
                inline_calls_in_expr(
                    &mut arm.body,
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
        TirExprKind::Block(block) => {
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
        TirExprKind::GlobalVarSet { value, .. } => {
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
        TirExprKind::Closure { body, .. } => {
            inline_calls_in_expr(
                body,
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
        TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
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
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            inline_calls_in_expr(
                scrutinee,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                pre_stmts,
                inlined_funcs,
                inline_counter,
            );
            for arm_block in arms {
                inline_calls_in_block(
                    arm_block,
                    candidates,
                    current_module,
                    local_count,
                    local_types,
                    type_table,
                    inlined_funcs,
                    inline_counter,
                );
            }
            inline_calls_in_block(
                default,
                candidates,
                current_module,
                local_count,
                local_types,
                type_table,
                inlined_funcs,
                inline_counter,
            );
        }
        // Leaf expressions (no sub-expressions to recurse into)
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}
