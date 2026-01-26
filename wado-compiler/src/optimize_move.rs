//! Move insertion optimization and value copy type collection for Wado TIR
//!
//! This module provides two optimizations:
//!
//! 1. **Move Insertion**: Wraps fresh values in `Move` nodes to avoid unnecessary copies.
//!    Fresh values (literals, call results, etc.) can be moved directly without copying
//!    since they are newly created and owned by the current expression.
//!
//! 2. **Value Copy Type Collection**: Collects types that require value copying in each
//!    function body. This information is used by codegen to pre-allocate scratch locals
//!    for copy operations.

use crate::project::Project;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeId, TypeTable,
};
use std::collections::HashSet;

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

        // EnumConstruct is fresh (it's a literal-like construction)
        TirExprKind::EnumConstruct { .. } => true,

        // Move is already marked as fresh
        TirExprKind::Move { .. } => true,

        // Everything else is not fresh
        _ => false,
    }
}

/// Check if a type requires value copying (composite types with value semantics).
fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::Variant { .. } => true,
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
        TirStmtKind::WhilePattern {
            scrutinee, body, ..
        } => {
            insert_moves_in_expr(scrutinee, type_table);
            insert_moves_in_block(body, type_table);
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            body,
            update,
            ..
        } => {
            for s in init {
                insert_moves_in_stmt(s, type_table);
            }
            insert_moves_in_expr(scrutinee, type_table);
            insert_moves_in_block(body, type_table);
            if let Some(u) = update {
                insert_moves_in_expr(u, type_table);
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
        | TirExprKind::Match { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Insert move optimization for all functions in the project.
pub fn insert_moves(project: &mut Project) {
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
    copy_types: &mut HashSet<TypeId>,
) {
    for stmt in &block.stmts {
        collect_value_copy_types_in_stmt(stmt, type_table, copy_types);
    }
}

/// Collect value copy types from a statement.
fn collect_value_copy_types_in_stmt(
    stmt: &TirStmt,
    type_table: &TypeTable,
    copy_types: &mut HashSet<TypeId>,
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
        TirStmtKind::WhilePattern {
            scrutinee, body, ..
        } => {
            collect_value_copy_types_in_expr(scrutinee, type_table, copy_types);
            collect_value_copy_types_in_block(body, type_table, copy_types);
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            body,
            update,
            ..
        } => {
            for s in init {
                collect_value_copy_types_in_stmt(s, type_table, copy_types);
            }
            collect_value_copy_types_in_expr(scrutinee, type_table, copy_types);
            collect_value_copy_types_in_block(body, type_table, copy_types);
            if let Some(u) = update {
                collect_value_copy_types_in_expr(u, type_table, copy_types);
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
    copy_types: &mut HashSet<TypeId>,
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
        | TirExprKind::Match { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Collect value copy types for all functions in the project.
/// This populates `needed_copy_types` which codegen uses to pre-allocate scratch locals.
pub fn collect_value_copy_types(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            // Collect into a temporary set first to avoid borrow conflicts
            let mut copy_types = HashSet::new();
            if let Some(ref body) = func.body {
                collect_value_copy_types_in_block(body, &type_table, &mut copy_types);
            }
            func.needed_copy_types.extend(copy_types);
        }
    }
}
