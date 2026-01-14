//! Lowering pass for Wado TIR
//!
//! The lower phase performs type-driven transformations on TIR:
//! - String literal collection (for data section)
//! - Reactive signal dependency graph construction
//! - Method call resolution (direct vs. effect operation)
//! - Generic instantiation / monomorphization
//! - Closure capture analysis
//! - JSX element type binding (future)

use crate::tir::{TirBlock, TirExpr, TirExprKind, TirModule, TirStmt, TirStmtKind};

/// Lower a TIR module
///
/// Currently performs:
/// - String literal collection
pub fn lower(mut module: TirModule) -> TirModule {
    // Collect string literals
    let mut collector = StringCollector::new();
    collector.collect_module(&module);
    module.string_literals = collector.into_strings();

    module
}

/// Lower multiple modules
pub fn lower_modules(modules: Vec<TirModule>) -> Vec<TirModule> {
    modules.into_iter().map(lower).collect()
}

// ============================================================================
// String Literal Collection
// ============================================================================

/// Collects all string literals from a TIR module for the data section
struct StringCollector {
    strings: Vec<String>,
}

impl StringCollector {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
        }
    }

    fn into_strings(self) -> Vec<String> {
        self.strings
    }

    fn add_string(&mut self, s: String) {
        if !self.strings.contains(&s) {
            self.strings.push(s);
        }
    }

    fn collect_module(&mut self, module: &TirModule) {
        for func in &module.functions {
            if let Some(body) = &func.body {
                self.collect_block(body);
            }
        }
    }

    fn collect_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.collect_stmt(stmt);
        }
    }

    fn collect_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_expr(value);
            }
            TirStmtKind::Expr(expr) => {
                self.collect_expr(expr);
            }
            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.collect_expr(expr);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_expr(condition);
                self.collect_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_block(else_blk);
                }
            }
            TirStmtKind::While { condition, body } => {
                self.collect_expr(condition);
                self.collect_block(body);
            }
            TirStmtKind::Loop { body } => {
                self.collect_block(body);
            }
            TirStmtKind::Break | TirStmtKind::Continue => {}
            TirStmtKind::Assert {
                condition,
                condition_source,
                message,
                intermediates,
            } => {
                self.collect_expr(condition);
                if let Some(msg) = message {
                    self.collect_expr(msg);
                }
                for (_, expr, _) in intermediates {
                    self.collect_expr(expr);
                }
                // Collect static strings used in assert messages
                self.add_string("Assertion failed:\n".to_string());
                self.add_string("Assertion failed: ".to_string());
                self.add_string(format!("condition: {}\n", condition_source));
                self.add_string("\n".to_string());
                for (name, _, _) in intermediates {
                    self.add_string(format!("{}: ", name));
                }
            }
        }
    }

    fn collect_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::StringLiteral(s) => {
                self.add_string(s.clone());
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_expr(left);
                self.collect_expr(right);
            }
            TirExprKind::Unary { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.collect_expr(receiver);
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::EffectCall { args, .. } => {
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::Block(block) => {
                self.collect_block(block);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_expr(condition);
                self.collect_block(then_branch);
                if let Some(else_blk) = else_branch {
                    self.collect_block(else_blk);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_expr(&field.value);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.collect_expr(elem);
                }
            }
            TirExprKind::Assign { target, value } => {
                self.collect_expr(target);
                self.collect_expr(value);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::FieldAccess { expr: inner, .. } => {
                self.collect_expr(inner);
            }
            TirExprKind::Index { expr: array, index } => {
                self.collect_expr(array);
                self.collect_expr(index);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.collect_expr(scrutinee);
                for arm in arms {
                    self.collect_expr(&arm.body);
                }
            }
            TirExprKind::Closure { body, .. } => {
                self.collect_expr(body);
            }
            // Literals and simple expressions don't contain strings
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lower_passthrough() {
        let module = TirModule::new(vec!["test".to_string()]);
        let lowered = lower(module);
        assert_eq!(lowered.path, vec!["test".to_string()]);
    }

    #[test]
    fn test_string_collector_empty() {
        let module = TirModule::new(vec!["test".to_string()]);
        let lowered = lower(module);
        assert!(lowered.string_literals.is_empty());
    }
}
