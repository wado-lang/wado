//! Generic visitor traits for mutable and immutable traversal of TIR trees.
//!
//! Provides three visitor traits:
//! - `TirMutVisitor`: mutable traversal (monomorphizer, lowering)
//! - `TirRefVisitor`: immutable traversal (analysis, collection)
//! - `TirOptVisitor`: mutable traversal with change tracking (optimization passes)
//!
//! Also provides utility functions for common TIR queries like `block_has_break_to`.

use crate::flat_package::FlatPackage;
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
            TirPattern::Tuple(patterns, _) => {
                for p in patterns {
                    self.visit_pattern(p);
                }
            }
            TirPattern::Variant { bindings, .. } => {
                for binding in bindings {
                    self.visit_pattern(binding);
                }
            }
            TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => {}
            TirPattern::Struct { fields, .. } => {
                for field in fields {
                    self.visit_pattern(&mut field.pattern);
                }
            }
            TirPattern::Or(alternatives) => {
                for p in alternatives {
                    self.visit_pattern(p);
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
            | TirExprKind::TupleZip { expr: inner }
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
            TirExprKind::WithHandler { bindings, body, .. } => {
                for binding in bindings {
                    self.visit_expr(&mut binding.handler);
                }
                self.visit_block(body);
            }
            TirExprKind::Resume { value } => {
                self.visit_expr(value);
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
            | TirExprKind::TupleZip { expr: inner }
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
            TirExprKind::WithHandler { bindings, body, .. } => {
                for binding in bindings {
                    self.visit_expr(&binding.handler);
                }
                self.visit_block(body);
            }
            TirExprKind::Resume { value } => {
                self.visit_expr(value);
            }
        }
    }
}

/// Trait for visiting and transforming TIR nodes in optimization passes.
///
/// All methods return `true` if any changes were made.
/// Default implementations walk children recursively via the free functions
/// `opt_walk_block`, `opt_walk_stmt`, and `opt_walk_expr`.
pub trait TirOptVisitor {
    /// Visit a statement. Override to add statement-level transformation logic.
    /// Call `opt_walk_stmt(self, stmt)` to recurse into children.
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool
    where
        Self: Sized,
    {
        opt_walk_stmt(self, stmt)
    }

    /// Visit an expression. Override to add custom transformation logic.
    /// Call `opt_walk_expr(self, expr)` to recurse into children.
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool
    where
        Self: Sized,
    {
        opt_walk_expr(self, expr)
    }

    /// Visit a block. Override for block-level transformations (e.g., stmt removal).
    /// Call `opt_walk_block(self, block)` to recurse into children.
    fn visit_block(&mut self, block: &mut TirBlock) -> bool
    where
        Self: Sized,
    {
        opt_walk_block(self, block)
    }
}

/// Walk all statements in a block, visiting each recursively.
pub fn opt_walk_block(visitor: &mut impl TirOptVisitor, block: &mut TirBlock) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= visitor.visit_stmt(stmt);
    }
    changed
}

/// Walk a statement's children.
pub fn opt_walk_stmt(visitor: &mut impl TirOptVisitor, stmt: &mut TirStmt) -> bool {
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
pub fn opt_walk_expr(visitor: &mut impl TirOptVisitor, expr: &mut TirExpr) -> bool {
    let mut changed = false;
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= visitor.visit_expr(left);
            changed |= visitor.visit_expr(right);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
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
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
    changed
}

/// Check if any `break` statement in the block targets the given label.
///
/// Recursively traverses all TIR node types to avoid missing breaks nested
/// inside expressions like `VariantConstruct`, `TupleLiteral`, `StructLiteral`, etc.
pub fn block_has_break_to(label: &str, block: &TirBlock) -> bool {
    block.stmts.iter().any(|s| stmt_has_break_to(label, s))
}

pub fn stmt_has_break_to(label: &str, stmt: &TirStmt) -> bool {
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

pub fn expr_has_break_to(label: &str, expr: &TirExpr) -> bool {
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
        | TirExprKind::TupleZip { expr }
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
        TirExprKind::WithHandler { body, .. } => block_has_break_to(label, body),
        TirExprKind::Resume { value } => expr_has_break_to(label, value),
    }
}

/// Apply a visitor to all function bodies in a project.
pub fn visit_project_functions(
    project: &mut FlatPackage,
    visitor: &mut impl TirOptVisitor,
) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(ref mut body) = func.body {
            changed |= visitor.visit_block(body);
        }
    }
    changed
}
