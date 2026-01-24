//! Lowering pass for Wado TIR
//!
//! The lower phase performs type-driven transformations on TIR:
//! - String literal collection (for data section)
//!
//! Note: Monomorphization has been moved to a separate phase (see `monomorphize.rs`)

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirModule, TirStmt, TirStmtKind};

/// Lower a TIR module
///
/// Performs string literal collection for the data section.
pub fn lower(mut module: TirModule) -> TirModule {
    // Collect string literals and their function mappings
    let mut collector = StringCollector::new();
    collector.collect_module(&module);
    let (strings, function_strings) = collector.into_results();
    module.string_literals = strings;
    module.function_strings = function_strings;

    module
}

/// Lower a Project (Project -> Project)
///
/// This is the main entry point for the lower phase. It lowers all TIR modules
/// in the project.
pub fn lower_project(mut project: Project) -> Project {
    project.tir_modules = lower_modules_indexed(project.tir_modules);
    project
}

/// Lower multiple modules
///
/// Applies lowering (string collection) to each module.
pub fn lower_modules_indexed(
    modules: IndexMap<ModuleSource, TirModule>,
) -> IndexMap<ModuleSource, TirModule> {
    modules
        .into_iter()
        .map(|(module_source, module)| (module_source, lower(module)))
        .collect()
}

// ============================================================================
// String Literal Collection
// ============================================================================

/// Collects all string literals from a TIR module for the data section,
/// tracking which function each string comes from for DCE
struct StringCollector {
    strings: Vec<String>,
    /// Map of function name → strings in that function (for DCE filtering)
    function_strings: HashMap<String, Vec<String>>,
    /// Current function being collected (for tracking)
    current_function: Option<String>,
}

impl StringCollector {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            function_strings: HashMap::new(),
            current_function: None,
        }
    }

    fn into_results(self) -> (Vec<String>, HashMap<String, Vec<String>>) {
        (self.strings, self.function_strings)
    }

    fn add_string(&mut self, s: String) {
        if !self.strings.contains(&s) {
            self.strings.push(s.clone());
        }
        // Also track which function this string belongs to
        if let Some(func_name) = &self.current_function {
            let func_strings = self.function_strings.entry(func_name.clone()).or_default();
            if !func_strings.contains(&s) {
                func_strings.push(s);
            }
        }
    }

    fn collect_module(&mut self, module: &TirModule) {
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                self.current_function = Some(func.name.clone());
                self.collect_block(body);
                self.current_function = None;
            }
        }
        // Also collect from trait impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                if let Some(body) = &method.body {
                    self.current_function = Some(method.name.clone());
                    self.collect_block(body);
                    self.current_function = None;
                }
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
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            } => {
                for s in init {
                    self.collect_stmt(s);
                }
                if let Some(cond) = condition {
                    self.collect_expr(cond);
                }
                self.collect_block(body);
                if let Some(upd) = update {
                    self.collect_expr(upd);
                }
            }
            TirStmtKind::Loop { body } => {
                self.collect_block(body);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.collect_expr(iterable);
                self.collect_block(body);
            }
            TirStmtKind::Break { .. } | TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.collect_block(block);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.collect_expr(scrutinee);
                self.collect_block(then_block);
                if let Some(else_blk) = else_block {
                    self.collect_block(else_blk);
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
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
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
            TirExprKind::IndirectCall { callee, args } => {
                self.collect_expr(callee);
                for arg in args {
                    self.collect_expr(arg);
                }
            }
            TirExprKind::OptionSome { value } => {
                self.collect_expr(value);
            }
            TirExprKind::VariantConstruct { fields, .. } => {
                for field in fields {
                    self.collect_expr(field);
                }
            }
            TirExprKind::Move { value } => {
                self.collect_expr(value);
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.collect_block(block);
            }
            // Literals and simple expressions don't contain strings
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::Capture { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lower_passthrough() {
        let module = TirModule::new(ModuleSource::Local {
            path: "test".to_string(),
        });
        let lowered = lower(module);
        assert_eq!(
            lowered.module_source,
            ModuleSource::Local {
                path: "test".to_string()
            }
        );
    }

    #[test]
    fn test_string_collector_empty() {
        let module = TirModule::new(ModuleSource::Local {
            path: "test".to_string(),
        });
        let lowered = lower(module);
        assert!(lowered.string_literals.is_empty());
    }
}
