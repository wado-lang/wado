//! Generic visitor traits for mutable and immutable traversal of TIR trees.
//!
//! These are used by the monomorphizer and potentially other passes that need
//! to walk TIR expressions, statements, blocks, and patterns.

use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirPattern, TirStmt, TirStmtKind, TirTemplatePart,
};

/// Trait for mutable traversal of TIR trees.
///
/// Override `visit_*` methods to add custom logic at specific nodes.
/// Call the corresponding `walk_*` method within your override to recurse into children.
/// The default implementations simply delegate to `walk_*`.
pub trait TirMutVisitor {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.walk_expr(expr);
    }
    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        self.walk_stmt(stmt);
    }
    fn visit_block(&mut self, block: &mut TirBlock) {
        self.walk_block(block);
    }
    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        self.walk_pattern(pattern);
    }

    fn walk_block(&mut self, block: &mut TirBlock) {
        for stmt in &mut block.stmts {
            self.visit_stmt(stmt);
        }
    }

    fn walk_stmt(&mut self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.visit_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.visit_expr(expr);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.visit_expr(expr);
                }
            }
            TirStmtKind::If {
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
            TirStmtKind::Loop { body } => {
                self.visit_block(body);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.visit_expr(v);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            TirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.visit_expr(scrutinee);
                self.visit_pattern(pattern);
                self.visit_block(then_block);
                if let Some(else_blk) = else_block {
                    self.visit_block(else_blk);
                }
            }
            TirStmtKind::LetDestructure { pattern, value, .. } => {
                self.visit_pattern(pattern);
                self.visit_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {}
            TirStmtKind::VariadicForOf { iterable, body, .. } => {
                self.visit_expr(iterable);
                self.visit_block(body);
            }
        }
    }

    fn walk_pattern(&mut self, pattern: &mut TirPattern) {
        match pattern {
            TirPattern::Wildcard | TirPattern::Binding { .. } | TirPattern::Literal(_) => {}
            TirPattern::Tuple(patterns) => {
                for p in patterns {
                    self.visit_pattern(p);
                }
            }
            TirPattern::Variant { bindings, .. } => {
                for binding in bindings {
                    self.visit_pattern(binding);
                }
            }
            TirPattern::Enum { .. } => {}
            TirPattern::Struct { fields, .. } => {
                for field in fields {
                    self.visit_pattern(&mut field.pattern);
                }
            }
        }
    }

    fn walk_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
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
            TirExprKind::GlobalVarSet { value, .. } => {
                self.visit_expr(value);
            }
            TirExprKind::Binary { left, right, .. } => {
                self.visit_expr(left);
                self.visit_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            }
            | TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::ClosureToCanonical { functor: inner, .. } => {
                self.visit_expr(inner);
            }
            TirExprKind::Assign { target, value }
            | TirExprKind::Index {
                expr: target,
                index: value,
            } => {
                self.visit_expr(target);
                self.visit_expr(value);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.visit_expr(&mut arg.expr);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(&mut arg.expr);
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            TirExprKind::If {
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
            TirExprKind::Match {
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
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.visit_expr(&mut field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.visit_expr(elem);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.visit_expr(body);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.visit_expr(payload_expr);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            TirExprKind::Switch {
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
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.visit_expr(inner);
                    }
                }
            }
        }
    }
}

/// Trait for immutable traversal of TIR trees.
///
/// Like `TirMutVisitor` but takes `&TirExpr`/`&TirStmt` instead of `&mut`.
/// The visitor itself can be `&mut self` to accumulate results (e.g., collecting
/// instantiation sites).
pub trait TirRefVisitor {
    fn visit_expr(&mut self, expr: &TirExpr) {
        self.walk_expr(expr);
    }
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        self.walk_stmt(stmt);
    }
    fn visit_block(&mut self, block: &TirBlock) {
        self.walk_block(block);
    }

    fn walk_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.visit_stmt(stmt);
        }
    }

    fn walk_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.visit_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.visit_expr(expr);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.visit_expr(expr);
                }
            }
            TirStmtKind::If {
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
            TirStmtKind::Loop { body } => {
                self.visit_block(body);
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.visit_expr(v);
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.visit_expr(scrutinee);
                self.visit_block(then_block);
                if let Some(else_blk) = else_block {
                    self.visit_block(else_blk);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.visit_expr(value);
            }
            TirStmtKind::TaskReturn { .. } => {}
            TirStmtKind::VariadicForOf { iterable, body, .. } => {
                self.visit_expr(iterable);
                self.visit_block(body);
            }
        }
    }

    fn walk_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
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
            TirExprKind::GlobalVarSet { value, .. } => {
                self.visit_expr(value);
            }
            TirExprKind::Binary { left, right, .. } => {
                self.visit_expr(left);
                self.visit_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            }
            | TirExprKind::VariantTag { expr: inner }
            | TirExprKind::VariantTest { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. }
            | TirExprKind::ClosureToCanonical { functor: inner, .. } => {
                self.visit_expr(inner);
            }
            TirExprKind::Assign { target, value }
            | TirExprKind::Index {
                expr: target,
                index: value,
            } => {
                self.visit_expr(target);
                self.visit_expr(value);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.visit_expr(receiver);
                for arg in args {
                    self.visit_expr(&arg.expr);
                }
            }
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.visit_block(block);
            }
            TirExprKind::If {
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
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.visit_expr(scrutinee);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.visit_expr(guard);
                    }
                    self.visit_expr(&arm.body);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.visit_expr(&field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.visit_expr(elem);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.visit_expr(body);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.visit_expr(payload_expr);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    self.visit_expr(arg);
                }
            }
            TirExprKind::Switch {
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
            TirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let TirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.visit_expr(inner);
                    }
                }
            }
        }
    }
}
