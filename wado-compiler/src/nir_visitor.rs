//! Generic visitor traits for mutable and immutable traversal of NIR trees.
//!
//! Provides two visitor traits:
//! - `NirMutVisitor`: mutable traversal (monomorphizer, lowering)
//! - `NirRefVisitor`: immutable traversal (analysis, collection)
//!
//! Also provides utility functions for common NIR queries like `block_has_break_to`.

use crate::nir::{NirBlock, NirExpr, NirExprKind, NirPattern, NirStmt, NirStmtKind};

/// Trait for mutable traversal of NIR trees.
///
/// Override `visit_*` methods to add custom logic at specific nodes.
/// Call the corresponding `walk_*` method within your override to recurse into children.
/// The default implementations simply delegate to `walk_*`.
pub trait NirMutVisitor {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        self.walk_expr(expr);
    }
    fn visit_stmt(&mut self, stmt: &mut NirStmt) {
        self.walk_stmt(stmt);
    }
    fn visit_block(&mut self, block: &mut NirBlock) {
        self.walk_block(block);
    }
    fn visit_pattern(&mut self, pattern: &mut NirPattern) {
        self.walk_pattern(pattern);
    }

    fn walk_block(&mut self, block: &mut NirBlock) {
        for stmt in &mut block.stmts {
            self.visit_stmt(stmt);
        }
    }

    fn walk_stmt(&mut self, stmt: &mut NirStmt) {
        match &mut stmt.kind {
            NirStmtKind::Let { value, .. } => {
                self.visit_expr(value);
            }
            NirStmtKind::Expr(expr) => {
                self.visit_expr(expr);
            }
            NirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.visit_expr(expr);
                }
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.visit_expr(condition);
                self.visit_block(then_block);
                if let Some(else_blk) = else_block {
                    self.visit_block(else_blk);
                }
            }
            NirStmtKind::Loop { body } => {
                self.visit_block(body);
            }
            NirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.visit_expr(v);
                }
            }
            NirStmtKind::Continue => {}
            NirStmtKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            NirStmtKind::LetDestructure { pattern, value, .. } => {
                self.visit_pattern(pattern);
                self.visit_expr(value);
            }
        }
    }

    fn walk_pattern(&mut self, pattern: &mut NirPattern) {
        match pattern {
            NirPattern::Wildcard | NirPattern::Binding { .. } | NirPattern::Literal(_) => {}
            NirPattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.visit_pattern(p);
                }
            }
            NirPattern::Variant { bindings, .. } => {
                for binding in bindings {
                    self.visit_pattern(binding);
                }
            }
            NirPattern::Enum { .. } | NirPattern::Range { .. } => {}
            // `ConstantValue { expr }` carries a sub-expression; recurse so
            // expression-level visitors see it. `NirRefVisitor::walk_pattern`
            // mirrors this.
            NirPattern::ConstantValue { expr } => {
                self.visit_expr(expr);
            }
            NirPattern::Struct { fields, .. } => {
                for field in fields {
                    self.visit_pattern(&mut field.pattern);
                }
            }
            NirPattern::Or(alternatives) => {
                for p in alternatives {
                    self.visit_pattern(p);
                }
            }
        }
    }

    fn walk_expr(&mut self, expr: &mut NirExpr) {
        match &mut expr.kind {
            NirExprKind::IntLiteral { .. }
            | NirExprKind::FloatLiteral { .. }
            | NirExprKind::BoolLiteral(_)
            | NirExprKind::CharLiteral(_)
            | NirExprKind::StringLiteral(_)
            | NirExprKind::BytesLiteral(_)
            | NirExprKind::Null
            | NirExprKind::Unit
            | NirExprKind::Local { .. }
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::EnumConstruct { .. } => {}
            NirExprKind::GlobalVarSet { value, .. } => {
                self.visit_expr(value);
            }
            NirExprKind::Binary { left, right, .. } => {
                self.visit_expr(left);
                self.visit_expr(right);
            }
            NirExprKind::Unary { expr: inner, .. }
            | NirExprKind::Cast { expr: inner, .. }
            | NirExprKind::FieldAccess { expr: inner, .. }
            | NirExprKind::VariantTag { expr: inner }
            | NirExprKind::VariantTest { expr: inner, .. }
            | NirExprKind::VariantPayload { expr: inner, .. }
            | NirExprKind::ClosureToCanonical { functor: inner, .. } => {
                self.visit_expr(inner);
            }
            NirExprKind::Assign { target, value }
            | NirExprKind::Index {
                expr: target,
                index: value,
            } => {
                self.visit_expr(target);
                self.visit_expr(value);
            }
            NirExprKind::Call { args, .. } => {
                for arg in args {
                    self.visit_expr(&mut arg.expr);
                }
            }
            NirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            NirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(&mut arg.expr);
                }
            }
            NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition);
                self.visit_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.visit_block(else_blk);
                }
            }
            NirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    self.visit_pattern(&mut arm.pattern);
                    if let Some(guard) = &mut arm.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_expr(&mut arm.body);
                }
            }
            NirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.visit_expr(&mut field.value);
                }
            }
            NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
                for elem in elements {
                    self.visit_expr(elem);
                }
            }
            NirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.visit_expr(payload_expr);
                }
            }
            NirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            NirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    self.visit_block(arm);
                }
                self.visit_block(default);
            }
        }
    }
}

/// Trait for immutable traversal of NIR trees.
///
/// Like `NirMutVisitor` but takes `&NirExpr`/`&NirStmt` instead of `&mut`.
/// The visitor itself can be `&mut self` to accumulate results (e.g., collecting
/// instantiation sites).
pub trait NirRefVisitor {
    fn visit_expr(&mut self, expr: &NirExpr) {
        self.walk_expr(expr);
    }
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        self.walk_stmt(stmt);
    }
    fn visit_block(&mut self, block: &NirBlock) {
        self.walk_block(block);
    }
    fn visit_pattern(&mut self, pattern: &NirPattern) {
        self.walk_pattern(pattern);
    }

    fn walk_block(&mut self, block: &NirBlock) {
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
        }
    }

    fn walk_pattern(&mut self, pattern: &NirPattern) {
        match pattern {
            NirPattern::Wildcard | NirPattern::Binding { .. } | NirPattern::Literal(_) => {}
            NirPattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.visit_pattern(p);
                }
            }
            NirPattern::Variant { bindings, .. } => {
                for binding in bindings {
                    self.visit_pattern(binding);
                }
            }
            NirPattern::Enum { .. } | NirPattern::Range { .. } => {}
            // `ConstantValue { expr }` carries a sub-expression; recurse so
            // expression-level visitors see it. `NirMutVisitor::walk_pattern`
            // mirrors this.
            NirPattern::ConstantValue { expr } => {
                self.visit_expr(expr);
            }
            NirPattern::Struct { fields, .. } => {
                for field in fields {
                    self.visit_pattern(&field.pattern);
                }
            }
            NirPattern::Or(alternatives) => {
                for p in alternatives {
                    self.visit_pattern(p);
                }
            }
        }
    }

    fn walk_stmt(&mut self, stmt: &NirStmt) {
        match &stmt.kind {
            NirStmtKind::Let { value, .. } => {
                self.visit_expr(value);
            }
            NirStmtKind::Expr(expr) => {
                self.visit_expr(expr);
            }
            NirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.visit_expr(expr);
                }
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.visit_expr(condition);
                self.visit_block(then_block);
                if let Some(else_blk) = else_block {
                    self.visit_block(else_blk);
                }
            }
            NirStmtKind::Loop { body } => {
                self.visit_block(body);
            }
            NirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.visit_expr(v);
                }
            }
            NirStmtKind::Continue => {}
            NirStmtKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            NirStmtKind::LetDestructure { pattern, value, .. } => {
                self.visit_pattern(pattern);
                self.visit_expr(value);
            }
        }
    }

    fn walk_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::IntLiteral { .. }
            | NirExprKind::FloatLiteral { .. }
            | NirExprKind::BoolLiteral(_)
            | NirExprKind::CharLiteral(_)
            | NirExprKind::StringLiteral(_)
            | NirExprKind::BytesLiteral(_)
            | NirExprKind::Null
            | NirExprKind::Unit
            | NirExprKind::Local { .. }
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::EnumConstruct { .. } => {}
            NirExprKind::GlobalVarSet { value, .. } => {
                self.visit_expr(value);
            }
            NirExprKind::Binary { left, right, .. } => {
                self.visit_expr(left);
                self.visit_expr(right);
            }
            NirExprKind::Unary { expr: inner, .. }
            | NirExprKind::Cast { expr: inner, .. }
            | NirExprKind::FieldAccess { expr: inner, .. }
            | NirExprKind::VariantTag { expr: inner }
            | NirExprKind::VariantTest { expr: inner, .. }
            | NirExprKind::VariantPayload { expr: inner, .. }
            | NirExprKind::ClosureToCanonical { functor: inner, .. } => {
                self.visit_expr(inner);
            }
            NirExprKind::Assign { target, value }
            | NirExprKind::Index {
                expr: target,
                index: value,
            } => {
                self.visit_expr(target);
                self.visit_expr(value);
            }
            NirExprKind::Call { args, .. } => {
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
            }
            NirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            NirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
            }
            NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.visit_expr(condition);
                self.visit_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.visit_block(else_blk);
                }
            }
            NirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    self.visit_pattern(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_expr(&arm.body);
                }
            }
            NirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.visit_expr(&field.value);
                }
            }
            NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
                for elem in elements {
                    self.visit_expr(elem);
                }
            }
            NirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.visit_expr(payload_expr);
                }
            }
            NirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            NirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    self.visit_block(arm);
                }
                self.visit_block(default);
            }
        }
    }
}

/// Check if any `break` statement in the block targets the given label.
///
/// Recursively traverses all NIR node types to avoid missing breaks nested
/// inside expressions like `VariantConstruct`, `TupleLiteral`, `StructLiteral`, etc.
pub fn block_has_break_to(label: &str, block: &NirBlock) -> bool {
    block.stmts.iter().any(|s| stmt_has_break_to(label, s))
}

pub fn stmt_has_break_to(label: &str, stmt: &NirStmt) -> bool {
    match &stmt.kind {
        NirStmtKind::Break {
            label: Some(l),
            value,
        } => l == label || value.as_ref().is_some_and(|v| expr_has_break_to(label, v)),
        NirStmtKind::Break { value, .. } => {
            value.as_ref().is_some_and(|v| expr_has_break_to(label, v))
        }
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            expr_has_break_to(label, value)
        }
        NirStmtKind::Expr(expr) => expr_has_break_to(label, expr),
        NirStmtKind::Return { value } => {
            value.as_ref().is_some_and(|v| expr_has_break_to(label, v))
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            block_has_break_to(label, body)
        }
        NirStmtKind::Continue => false,
    }
}

pub fn expr_has_break_to(label: &str, expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            block_has_break_to(label, block)
        }
        NirExprKind::If {
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
        NirExprKind::Match { expr, arms } => {
            expr_has_break_to(label, expr)
                || arms.iter().any(|arm| {
                    arm.guard
                        .as_ref()
                        .is_some_and(|g| expr_has_break_to(label, g))
                        || expr_has_break_to(label, &arm.body)
                })
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_break_to(label, scrutinee)
                || arms.iter().any(|arm| block_has_break_to(label, arm))
                || block_has_break_to(label, default)
        }
        NirExprKind::Binary { left, right, .. } => {
            expr_has_break_to(label, left) || expr_has_break_to(label, right)
        }
        NirExprKind::Unary { expr, .. }
        | NirExprKind::Cast { expr, .. }
        | NirExprKind::FieldAccess { expr, .. }
        | NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. }
        | NirExprKind::ClosureToCanonical { functor: expr, .. } => expr_has_break_to(label, expr),
        NirExprKind::Index { expr, index }
        | NirExprKind::Assign {
            target: expr,
            value: index,
        } => expr_has_break_to(label, expr) || expr_has_break_to(label, index),
        NirExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .is_some_and(|p| expr_has_break_to(label, p)),
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            elements.iter().any(|e| expr_has_break_to(label, e))
        }
        NirExprKind::StructLiteral { fields, .. } => {
            fields.iter().any(|f| expr_has_break_to(label, &f.value))
        }
        NirExprKind::Call { args, .. } => args.iter().any(|a| expr_has_break_to(label, &a.expr)),
        NirExprKind::MethodCall { receiver, args, .. } => {
            expr_has_break_to(label, receiver)
                || args.iter().any(|a| expr_has_break_to(label, &a.expr))
        }
        NirExprKind::CmRawCall { args, .. } => args.iter().any(|a| expr_has_break_to(label, a)),
        NirExprKind::IndirectCall { callee, args } => {
            expr_has_break_to(label, callee) || args.iter().any(|a| expr_has_break_to(label, a))
        }
        NirExprKind::GlobalVarSet { value, .. } => expr_has_break_to(label, value),
        // Leaf nodes
        NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => false,
    }
}
