//! Generic visitor traits for mutable and immutable traversal of NIR trees.
//!
//! Provides three visitor traits:
//! - `NirMutVisitor`: mutable traversal (monomorphizer, lowering)
//! - `NirRefVisitor`: immutable traversal (analysis, collection)
//! - `NirOptVisitor`: mutable traversal with change tracking (optimization passes)
//!
//! Also provides utility functions for common NIR queries like `block_has_break_to`.

use crate::nir::{
    NirBlock, NirExpr, NirExprKind, NirPattern, NirStmt, NirStmtKind, NirTemplatePart,
};
use crate::nir_package::NirPackage;

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
            NirStmtKind::IfLet {
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
            NirStmtKind::LetDestructure { pattern, value, .. } => {
                self.visit_pattern(pattern);
                self.visit_expr(value);
            }
            NirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase");
            }
            NirStmtKind::VariadicForOf { iterable, body, .. } => {
                self.visit_expr(iterable);
                self.visit_block(body);
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
            NirPattern::Enum { .. }
            | NirPattern::ConstantValue { .. }
            | NirPattern::Range { .. } => {}
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
            | NirExprKind::FuncRef { .. }
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::Capture { .. }
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
            | NirExprKind::TupleSpread { expr: inner }
            | NirExprKind::TupleZip { expr: inner }
            | NirExprKind::TypePackExpansion {
                call_expr: inner, ..
            }
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
            NirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.visit_expr(elem);
                }
            }
            NirExprKind::Closure { body, .. } => {
                self.visit_expr(body);
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
            NirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let NirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.visit_expr(inner);
                    }
                }
            }
            NirExprKind::WithHandler { bindings, body, .. } => {
                for binding in bindings {
                    self.visit_expr(&mut binding.handler);
                }
                self.visit_block(body);
            }
            NirExprKind::Resume { value } => {
                self.visit_expr(value);
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
            NirPattern::Enum { .. }
            | NirPattern::ConstantValue { .. }
            | NirPattern::Range { .. } => {}
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
            NirStmtKind::IfLet {
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
            NirStmtKind::LetDestructure { pattern, value, .. } => {
                self.visit_pattern(pattern);
                self.visit_expr(value);
            }
            NirStmtKind::TaskReturn { .. } => {
                unreachable!("TaskReturn should be eliminated by synthesis before this phase");
            }
            NirStmtKind::VariadicForOf { iterable, body, .. } => {
                self.visit_expr(iterable);
                self.visit_block(body);
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
            | NirExprKind::FuncRef { .. }
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::Capture { .. }
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
            | NirExprKind::TupleSpread { expr: inner }
            | NirExprKind::TupleZip { expr: inner }
            | NirExprKind::TypePackExpansion {
                call_expr: inner, ..
            }
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
            NirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.visit_expr(elem);
                }
            }
            NirExprKind::Closure { body, .. } => {
                self.visit_expr(body);
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
            NirExprKind::TemplateString { parts } => {
                for part in parts {
                    if let NirTemplatePart::Interpolation { expr: inner, .. } = part {
                        self.visit_expr(inner);
                    }
                }
            }
            NirExprKind::WithHandler { bindings, body, .. } => {
                for binding in bindings {
                    self.visit_expr(&binding.handler);
                }
                self.visit_block(body);
            }
            NirExprKind::Resume { value } => {
                self.visit_expr(value);
            }
        }
    }
}

/// Trait for visiting and transforming NIR nodes in optimization passes.
///
/// All methods return `true` if any changes were made.
/// Default implementations walk children recursively via the free functions
/// `opt_walk_block`, `opt_walk_stmt`, and `opt_walk_expr`.
pub trait NirOptVisitor {
    /// Visit a statement. Override to add statement-level transformation logic.
    /// Call `opt_walk_stmt(self, stmt)` to recurse into children.
    fn visit_stmt(&mut self, stmt: &mut NirStmt) -> bool
    where
        Self: Sized,
    {
        opt_walk_stmt(self, stmt)
    }

    /// Visit an expression. Override to add custom transformation logic.
    /// Call `opt_walk_expr(self, expr)` to recurse into children.
    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool
    where
        Self: Sized,
    {
        opt_walk_expr(self, expr)
    }

    /// Visit a block. Override for block-level transformations (e.g., stmt removal).
    /// Call `opt_walk_block(self, block)` to recurse into children.
    fn visit_block(&mut self, block: &mut NirBlock) -> bool
    where
        Self: Sized,
    {
        opt_walk_block(self, block)
    }

    /// Visit a pattern. Override to rewrite pattern bindings or constants.
    /// Call `opt_walk_pattern(self, pattern)` to recurse into children.
    fn visit_pattern(&mut self, pattern: &mut NirPattern) -> bool
    where
        Self: Sized,
    {
        opt_walk_pattern(self, pattern)
    }
}

/// Walk a pattern's children. Bindings, literals, enum tags, and ranges are
/// leaves; nested patterns inside `Tuple` / `Variant` / `Struct` / `Or` are
/// visited recursively, and the `expr` of a `ConstantValue` pattern flows
/// back through `visit_expr`.
pub fn opt_walk_pattern(visitor: &mut impl NirOptVisitor, pattern: &mut NirPattern) -> bool {
    let mut changed = false;
    match pattern {
        NirPattern::Wildcard
        | NirPattern::Binding { .. }
        | NirPattern::Literal(_)
        | NirPattern::Enum { .. }
        | NirPattern::Range { .. } => {}
        NirPattern::Tuple(patterns, _) | NirPattern::Or(patterns) => {
            for p in patterns {
                changed |= visitor.visit_pattern(p);
            }
        }
        NirPattern::Variant { bindings, .. } => {
            for p in bindings {
                changed |= visitor.visit_pattern(p);
            }
        }
        NirPattern::Struct { fields, .. } => {
            for f in fields {
                changed |= visitor.visit_pattern(&mut f.pattern);
            }
        }
        NirPattern::ConstantValue { expr } => {
            changed |= visitor.visit_expr(expr);
        }
    }
    changed
}

/// Walk all statements in a block, visiting each recursively.
pub fn opt_walk_block(visitor: &mut impl NirOptVisitor, block: &mut NirBlock) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= visitor.visit_stmt(stmt);
    }
    changed
}

/// Walk a statement's children.
pub fn opt_walk_stmt(visitor: &mut impl NirOptVisitor, stmt: &mut NirStmt) -> bool {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } => visitor.visit_expr(value),
        NirStmtKind::LetDestructure { pattern, value, .. } => {
            let mut changed = visitor.visit_pattern(pattern);
            changed |= visitor.visit_expr(value);
            changed
        }
        NirStmtKind::Expr(expr) => visitor.visit_expr(expr),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            value.as_mut().is_some_and(|v| visitor.visit_expr(v))
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } => visitor.visit_block(body),
        NirStmtKind::LabeledBlock { block, .. } => visitor.visit_block(block),
        NirStmtKind::IfLet {
            scrutinee,
            pattern,
            then_block,
            else_block,
        } => {
            let mut changed = visitor.visit_expr(scrutinee);
            changed |= visitor.visit_pattern(pattern);
            changed |= visitor.visit_block(then_block);
            if let Some(eb) = else_block {
                changed |= visitor.visit_block(eb);
            }
            changed
        }
        NirStmtKind::Continue => false,
        NirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
        NirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

/// Walk all children of an expression.
pub fn opt_walk_expr(visitor: &mut impl NirOptVisitor, expr: &mut NirExpr) -> bool {
    let mut changed = false;
    match &mut expr.kind {
        NirExprKind::Binary { left, right, .. } => {
            changed |= visitor.visit_expr(left);
            changed |= visitor.visit_expr(right);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => {
            changed |= visitor.visit_expr(inner);
        }
        NirExprKind::Assign { target, value } => {
            changed |= visitor.visit_expr(target);
            changed |= visitor.visit_expr(value);
        }
        NirExprKind::Index { expr: inner, index } => {
            changed |= visitor.visit_expr(inner);
            changed |= visitor.visit_expr(index);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                changed |= visitor.visit_expr(&mut arg.expr);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= visitor.visit_expr(arg);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            changed |= visitor.visit_expr(receiver);
            for arg in args {
                changed |= visitor.visit_expr(&mut arg.expr);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            changed |= visitor.visit_expr(callee);
            for arg in args {
                changed |= visitor.visit_expr(arg);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= visitor.visit_expr(functor);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            changed |= visitor.visit_block(block);
        }
        NirExprKind::If {
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
        NirExprKind::Match { expr: inner, arms } => {
            changed |= visitor.visit_expr(inner);
            for arm in arms {
                changed |= visitor.visit_pattern(&mut arm.pattern);
                if let Some(guard) = &mut arm.guard {
                    changed |= visitor.visit_expr(guard);
                }
                changed |= visitor.visit_expr(&mut arm.body);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= visitor.visit_expr(&mut field.value);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                changed |= visitor.visit_expr(elem);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= visitor.visit_expr(p);
            }
        }
        NirExprKind::Closure { body, .. } => {
            changed |= visitor.visit_expr(body);
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            changed |= visitor.visit_expr(value);
        }
        NirExprKind::Switch {
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
        NirExprKind::Local { .. }
        | NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => {}
        NirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        NirExprKind::WithHandler { .. } | NirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
    changed
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
        NirStmtKind::IfLet {
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
        NirStmtKind::Continue => false,
        NirStmtKind::TaskReturn { .. } | NirStmtKind::VariadicForOf { .. } => false,
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
        | NirExprKind::TupleSpread { expr }
        | NirExprKind::TupleZip { expr }
        | NirExprKind::TypePackExpansion {
            call_expr: expr, ..
        }
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
        NirExprKind::TupleLiteral { elements } => {
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
        NirExprKind::Closure { body, .. } => expr_has_break_to(label, body),
        NirExprKind::GlobalVarSet { value, .. } => expr_has_break_to(label, value),
        // Leaf nodes
        NirExprKind::Local { .. }
        | NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => false,
        NirExprKind::TemplateString { .. } => false,
        NirExprKind::WithHandler { body, .. } => block_has_break_to(label, body),
        NirExprKind::Resume { value } => expr_has_break_to(label, value),
    }
}

/// Apply a visitor to all function bodies in a project.
pub fn visit_project_functions(
    project: &mut NirPackage,
    visitor: &mut impl NirOptVisitor,
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
