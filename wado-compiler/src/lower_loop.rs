//! Loop lowering pass for Wado TIR
//!
//! This phase handles `ForOf` loops which still exist in TIR and need codegen support.
//!
//! Note: `While`, `For`, `WhilePattern`, and `ForPattern` are now desugared at the AST level
//! (in desugar.rs) before reaching TIR. This simplifies the TIR and allows the resolver
//! to work with only Loop constructs.
//!
//! `ForOf` is kept in TIR because the resolver has optimized handling for `Array<T>` iteration
//! that would be lost if we desugared it in the AST phase.

use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirModule, TirStmt, TirStmtKind,
};

/// Lower all loops in a module to canonical Loop form
pub fn lower_loops(module: &mut TirModule) {
    let mut lowerer = LoopLowerer::new();
    lowerer.lower_module(module);
}

struct LoopLowerer {
    _label_counter: u32,
}

impl LoopLowerer {
    fn new() -> Self {
        Self { _label_counter: 0 }
    }

    fn lower_module(&mut self, module: &mut TirModule) {
        for func in &mut module.functions {
            self.lower_function(&mut func.borrow_mut());
        }
    }

    fn lower_function(&mut self, func: &mut TirFunction) {
        if let Some(body) = &mut func.body {
            self.lower_block(body);
        }
    }

    fn lower_block(&mut self, block: &mut TirBlock) {
        let mut new_stmts = Vec::with_capacity(block.stmts.len());

        for stmt in std::mem::take(&mut block.stmts) {
            let lowered = self.lower_stmt(stmt);
            new_stmts.extend(lowered);
        }

        block.stmts = new_stmts;
    }

    fn lower_stmt(&mut self, mut stmt: TirStmt) -> Vec<TirStmt> {
        match &mut stmt.kind {
            // While, For, WhilePattern, ForPattern are now desugared at AST level
            // The resolver should never create these TIR variants anymore
            TirStmtKind::While { .. } => {
                unreachable!("While should be desugared before reaching TIR")
            }
            TirStmtKind::WhilePattern { .. } => {
                unreachable!("WhilePattern should be desugared before reaching TIR")
            }
            TirStmtKind::For { .. } => {
                unreachable!("For should be desugared before reaching TIR")
            }
            TirStmtKind::ForPattern { .. } => {
                unreachable!("ForPattern should be desugared before reaching TIR")
            }

            // ForOf is kept as-is - it has special handling in the resolver for
            // optimized Array<T> iteration
            TirStmtKind::ForOf { body, iterable, .. } => {
                self.lower_block(body);
                self.lower_expr(iterable);
                vec![stmt]
            }

            // Recursively process other statement kinds
            TirStmtKind::Loop { body } => {
                self.lower_block(body);
                vec![stmt]
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.lower_expr(condition);
                self.lower_block(then_block);
                if let Some(else_b) = else_block {
                    self.lower_block(else_b);
                }
                vec![stmt]
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.lower_expr(scrutinee);
                self.lower_block(then_block);
                if let Some(else_b) = else_block {
                    self.lower_block(else_b);
                }
                vec![stmt]
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                self.lower_block(block);
                vec![stmt]
            }
            TirStmtKind::Let { value, .. } => {
                self.lower_expr(value);
                vec![stmt]
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.lower_expr(value);
                vec![stmt]
            }
            TirStmtKind::Expr(expr) => {
                self.lower_expr(expr);
                vec![stmt]
            }
            TirStmtKind::Return { value } => {
                if let Some(v) = value {
                    self.lower_expr(v);
                }
                vec![stmt]
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.lower_expr(v);
                }
                vec![stmt]
            }
            TirStmtKind::Continue => vec![stmt],
        }
    }

    fn lower_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
                self.lower_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.lower_expr(condition);
                self.lower_block(then_branch);
                if let Some(else_b) = else_branch {
                    self.lower_block(else_b);
                }
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.lower_expr(scrutinee);
                for arm in arms {
                    self.lower_expr(&mut arm.body);
                }
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.lower_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.lower_expr(receiver);
                for arg in args {
                    self.lower_expr(arg);
                }
            }
            TirExprKind::IndirectCall { callee, args, .. } => {
                self.lower_expr(callee);
                for arg in args {
                    self.lower_expr(arg);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.lower_expr(left);
                self.lower_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.lower_expr(inner);
            }
            TirExprKind::Assign { target, value } => {
                self.lower_expr(target);
                self.lower_expr(value);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.lower_expr(inner);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.lower_expr(inner);
            }
            TirExprKind::Index { expr: inner, index } => {
                self.lower_expr(inner);
                self.lower_expr(index);
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.lower_expr(&mut field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
                for elem in elements {
                    self.lower_expr(elem);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.lower_expr(body);
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(p) = payload {
                    self.lower_expr(p);
                }
            }
            TirExprKind::OptionSome { value } => {
                self.lower_expr(value);
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.lower_expr(value);
            }
            TirExprKind::Move { value } => {
                self.lower_expr(value);
            }
            // Leaf expressions - no recursion needed
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::GlobalVarGet { .. }
            | TirExprKind::Capture { .. }
            | TirExprKind::EnumConstruct { .. }
            | TirExprKind::ClosureToCanonical { .. } => {}
        }
    }
}
