//! TIR visitor infrastructure for optimization passes.
//!
//! Provides a `TirVisitor` trait with default traversal behavior. Passes override
//! `visit_expr` and/or `visit_block` to implement their transformations, calling
//! `walk_expr`/`walk_block` for the default recursive walk when needed.
//!
//! This eliminates the ~200-line traversal boilerplate that every pass previously
//! duplicated for walking `TirStmtKind` and `TirExprKind` variants.

use crate::project::Project;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind};

/// Trait for visiting and transforming TIR nodes.
///
/// All methods return `true` if any changes were made.
/// Default implementations walk children recursively.
pub(crate) trait TirVisitor {
    /// Visit a statement. Override to add statement-level transformation logic.
    /// Call `walk_stmt(self, stmt)` to recurse into children.
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool
    where
        Self: Sized,
    {
        walk_stmt(self, stmt)
    }

    /// Visit an expression. Override to add custom transformation logic.
    /// Call `walk_expr(self, expr)` to recurse into children.
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool
    where
        Self: Sized,
    {
        walk_expr(self, expr)
    }

    /// Visit a block. Override for block-level transformations (e.g., stmt removal).
    /// Call `walk_block(self, block)` to recurse into children.
    fn visit_block(&mut self, block: &mut TirBlock) -> bool
    where
        Self: Sized,
    {
        walk_block(self, block)
    }
}

/// Walk all statements in a block, visiting each recursively.
pub(crate) fn walk_block(visitor: &mut impl TirVisitor, block: &mut TirBlock) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= visitor.visit_stmt(stmt);
    }
    changed
}

/// Walk a statement's children.
pub(crate) fn walk_stmt(visitor: &mut impl TirVisitor, stmt: &mut TirStmt) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            visitor.visit_expr(value)
        }
        TirStmtKind::Expr(expr) => visitor.visit_expr(expr),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            value.as_mut().is_some_and(|v| visitor.visit_expr(v))
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = visitor.visit_expr(condition);
            changed |= visitor.visit_block(then_block);
            if let Some(eb) = else_block {
                changed |= visitor.visit_block(eb);
            }
            changed
        }
        TirStmtKind::Loop { body } => visitor.visit_block(body),
        TirStmtKind::LabeledBlock { block, .. } => visitor.visit_block(block),
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = visitor.visit_expr(scrutinee);
            changed |= visitor.visit_block(then_block);
            if let Some(eb) = else_block {
                changed |= visitor.visit_block(eb);
            }
            changed
        }
        TirStmtKind::Continue => false,
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

/// Walk all children of an expression.
pub(crate) fn walk_expr(visitor: &mut impl TirVisitor, expr: &mut TirExpr) -> bool {
    let mut changed = false;
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= visitor.visit_expr(left);
            changed |= visitor.visit_expr(right);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            changed |= visitor.visit_expr(inner);
        }
        TirExprKind::Assign { target, value } => {
            changed |= visitor.visit_expr(target);
            changed |= visitor.visit_expr(value);
        }
        TirExprKind::Index { expr: inner, index } => {
            changed |= visitor.visit_expr(inner);
            changed |= visitor.visit_expr(index);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                changed |= visitor.visit_expr(&mut arg.expr);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= visitor.visit_expr(arg);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= visitor.visit_expr(receiver);
            for arg in args {
                changed |= visitor.visit_expr(&mut arg.expr);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            changed |= visitor.visit_expr(callee);
            for arg in args {
                changed |= visitor.visit_expr(arg);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= visitor.visit_expr(functor);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            changed |= visitor.visit_block(block);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= visitor.visit_expr(condition);
            changed |= visitor.visit_block(then_branch);
            if let Some(eb) = else_branch {
                changed |= visitor.visit_block(eb);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= visitor.visit_expr(inner);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= visitor.visit_expr(guard);
                }
                changed |= visitor.visit_expr(&mut arm.body);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= visitor.visit_expr(&mut field.value);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                changed |= visitor.visit_expr(elem);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= visitor.visit_expr(p);
            }
        }
        TirExprKind::Closure { body, .. } => {
            changed |= visitor.visit_expr(body);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= visitor.visit_expr(value);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= visitor.visit_expr(scrutinee);
            for arm in arms {
                changed |= visitor.visit_block(arm);
            }
            changed |= visitor.visit_block(default);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
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
    changed
}

/// Check if any `break` statement in the block targets the given label.
///
/// This is the canonical implementation used by optimization passes to guard
/// against removing labeled blocks that are still targeted by inner breaks.
/// It recursively traverses all TIR node types to avoid missing breaks nested
/// inside expressions like `VariantConstruct`, `TupleLiteral`, `StructLiteral`, etc.
pub(crate) fn block_has_break_to(label: &str, block: &TirBlock) -> bool {
    block.stmts.iter().any(|s| stmt_has_break_to(label, s))
}

pub(crate) fn stmt_has_break_to(label: &str, stmt: &TirStmt) -> bool {
    match &stmt.kind {
        TirStmtKind::Break {
            label: Some(l),
            value,
        } => l == label || value.as_ref().is_some_and(|v| expr_has_break_to(label, v)),
        TirStmtKind::Break { value, .. } => {
            value.as_ref().is_some_and(|v| expr_has_break_to(label, v))
        }
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            expr_has_break_to(label, value)
        }
        TirStmtKind::Expr(expr) => expr_has_break_to(label, expr),
        TirStmtKind::Return { value } => {
            value.as_ref().is_some_and(|v| expr_has_break_to(label, v))
        }
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
        TirStmtKind::IfLet {
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
        TirStmtKind::Continue => false,
        TirStmtKind::TaskReturn { .. } | TirStmtKind::VariadicForOf { .. } => false,
    }
}

pub(crate) fn expr_has_break_to(label: &str, expr: &TirExpr) -> bool {
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
                || arms.iter().any(|arm| {
                    arm.guard
                        .as_ref()
                        .is_some_and(|g| expr_has_break_to(label, g))
                        || expr_has_break_to(label, &arm.body)
                })
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_break_to(label, scrutinee)
                || arms.iter().any(|arm| block_has_break_to(label, arm))
                || block_has_break_to(label, default)
        }
        TirExprKind::Binary { left, right, .. } => {
            expr_has_break_to(label, left) || expr_has_break_to(label, right)
        }
        TirExprKind::Unary { expr, .. }
        | TirExprKind::Cast { expr, .. }
        | TirExprKind::FieldAccess { expr, .. }
        | TirExprKind::TupleSpread { expr }
        | TirExprKind::TypePackExpansion {
            call_expr: expr, ..
        }
        | TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. }
        | TirExprKind::ClosureToCanonical { functor: expr, .. } => expr_has_break_to(label, expr),
        TirExprKind::Index { expr, index }
        | TirExprKind::Assign {
            target: expr,
            value: index,
        } => expr_has_break_to(label, expr) || expr_has_break_to(label, index),
        TirExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .is_some_and(|p| expr_has_break_to(label, p)),
        TirExprKind::TupleLiteral { elements } => {
            elements.iter().any(|e| expr_has_break_to(label, e))
        }
        TirExprKind::StructLiteral { fields, .. } => {
            fields.iter().any(|f| expr_has_break_to(label, &f.value))
        }
        TirExprKind::Call { args, .. } => args.iter().any(|a| expr_has_break_to(label, &a.expr)),
        TirExprKind::MethodCall { receiver, args, .. } => {
            expr_has_break_to(label, receiver)
                || args.iter().any(|a| expr_has_break_to(label, &a.expr))
        }
        TirExprKind::CmRawCall { args, .. } => args.iter().any(|a| expr_has_break_to(label, a)),
        TirExprKind::IndirectCall { callee, args } => {
            expr_has_break_to(label, callee) || args.iter().any(|a| expr_has_break_to(label, a))
        }
        TirExprKind::Closure { body, .. } => expr_has_break_to(label, body),
        TirExprKind::GlobalVarSet { value, .. } => expr_has_break_to(label, value),
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
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
        | TirExprKind::EnumConstruct { .. } => false,
        TirExprKind::TemplateString { .. } => false,
    }
}

/// Apply a visitor to all function bodies in a project.
pub(crate) fn visit_project_functions(
    project: &mut Project,
    visitor: &mut impl TirVisitor,
) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            if let Some(ref mut body) = func.body {
                changed |= visitor.visit_block(body);
            }
        }
    }
    changed
}
