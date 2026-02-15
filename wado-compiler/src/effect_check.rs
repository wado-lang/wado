//! Effect checking phase for Wado
//!
//! This phase validates that all function calls have the required effects.
//! A function can only call another function if it has all the effects
//! that the callee requires.
//!
//! Effect checking runs after type resolution (TIR construction) and before
//! lowering. It operates on the TIR and produces errors for any effect violations.

use indexmap::{IndexMap, IndexSet};

use crate::compiler_host::CompilerHost;
use crate::logger::{Bail, Logger};
use crate::name::ModuleSource;
use crate::tir::{
    FunctionRef, TirBlock, TirExpr, TirExprKind, TirFunction, TirModule, TirStmt, TirStmtKind,
};
use crate::token::Span;

/// Error from effect checking
#[derive(Debug, Clone)]
pub struct EffectError {
    /// The function being called
    pub callee: String,
    /// The missing effect
    pub missing_effect: String,
    /// Source location of the call
    pub span: Span,
}

impl std::fmt::Display for EffectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: missing effect '{}' required by '{}'",
            self.span.line, self.span.column, self.missing_effect, self.callee
        )
    }
}

impl std::error::Error for EffectError {}

impl From<EffectError> for crate::compiler_host::Diagnostic {
    fn from(e: EffectError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: format!(
                "missing effect '{}' required by '{}'",
                e.missing_effect, e.callee
            ),
            span: Some(DiagnosticSpan::from_span(&e.span, None)),
        }
    }
}

/// Check effects for all modules
///
/// Errors are emitted to the logger. Returns `Err(Bail)` if any errors found.
pub fn check_effects<H: CompilerHost>(
    modules: &IndexMap<ModuleSource, TirModule>,
    logger: &Logger<H>,
) -> Result<(), Bail> {
    let mut checker = EffectChecker::new(modules, logger);
    // Ignore Bail from limit - just check if there were any errors
    let _ = checker.check_all();
    logger.ok_or_bail(())
}

/// Effect checker that walks TIR and validates effect requirements
struct EffectChecker<'a, H: CompilerHost> {
    modules: &'a IndexMap<ModuleSource, TirModule>,
    logger: &'a Logger<'a, H>,
    /// Current function's effects (set when entering a function)
    current_effects: IndexSet<String>,
}

impl<'a, H: CompilerHost> EffectChecker<'a, H> {
    fn new(modules: &'a IndexMap<ModuleSource, TirModule>, logger: &'a Logger<'a, H>) -> Self {
        Self {
            modules,
            logger,
            current_effects: IndexSet::new(),
        }
    }

    /// Check all modules
    fn check_all(&mut self) -> Result<(), Bail> {
        for module in self.modules.values() {
            self.check_module(module)?;
        }
        Ok(())
    }

    /// Check a single module
    fn check_module(&mut self, module: &TirModule) -> Result<(), Bail> {
        // Check all functions
        for func_rc in &module.functions {
            let func = func_rc.borrow();
            self.check_function(&func)?;
        }

        // Check impl methods
        for impl_block in &module.impls {
            for method in &impl_block.methods {
                self.check_function(method)?;
            }
        }
        Ok(())
    }

    /// Check a single function
    fn check_function(&mut self, func: &TirFunction) -> Result<(), Bail> {
        // Skip test functions - they implicitly have all effects
        if func.name.starts_with("__test_") {
            return Ok(());
        }

        // Set current context
        self.current_effects = func.effects.iter().cloned().collect();

        // Check the body if present
        if let Some(body) = &func.body {
            self.check_block(body)?;
        }
        Ok(())
    }

    /// Check a block
    fn check_block(&mut self, block: &TirBlock) -> Result<(), Bail> {
        for stmt in &block.stmts {
            self.check_stmt(stmt)?;
        }
        Ok(())
    }

    /// Check a statement
    fn check_stmt(&mut self, stmt: &TirStmt) -> Result<(), Bail> {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.check_expr(value)?;
            }
            TirStmtKind::Expr(expr) => self.check_expr(expr)?,
            TirStmtKind::Return { value } => {
                if let Some(e) = value {
                    self.check_expr(e)?;
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.check_expr(condition)?;
                self.check_block(then_block)?;
                if let Some(else_blk) = else_block {
                    self.check_block(else_blk)?;
                }
            }
            TirStmtKind::Loop { body } => {
                self.check_block(body)?;
            }
            TirStmtKind::Break { value, .. } => {
                if let Some(expr) = value {
                    self.check_expr(expr)?;
                }
            }
            TirStmtKind::Continue => {}
            TirStmtKind::LabeledBlock { block, .. } => {
                self.check_block(block)?;
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                self.check_expr(scrutinee)?;
                self.check_block(then_block)?;
                if let Some(else_blk) = else_block {
                    self.check_block(else_blk)?;
                }
            }
            TirStmtKind::LetPattern { value, .. } => {
                self.check_expr(value)?;
            }
        }
        Ok(())
    }

    /// Check an expression for effect violations
    fn check_expr(&mut self, expr: &TirExpr) -> Result<(), Bail> {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                self.check_call(func, expr.span)?;
                for arg in args {
                    self.check_expr(arg)?;
                }
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                self.check_expr(receiver)?;
                self.check_call(func, expr.span)?;
                for arg in args {
                    self.check_expr(arg)?;
                }
            }
            TirExprKind::StaticCall { func, args } => {
                self.check_call(func, expr.span)?;
                for arg in args {
                    self.check_expr(arg)?;
                }
            }
            TirExprKind::EffectCall {
                effect_name, args, ..
            } => {
                // Effect calls require the effect
                if !self.current_effects.contains(effect_name) {
                    self.logger.error(EffectError {
                        callee: effect_name.clone(),
                        missing_effect: effect_name.clone(),
                        span: expr.span,
                    })?;
                }
                for arg in args {
                    self.check_expr(arg)?;
                }
            }
            TirExprKind::CmRawCall { args, .. } => {
                // CmRawCall is used inside synthesized adapter functions;
                // no effect checking needed (adapter functions are always effectful)
                for arg in args {
                    self.check_expr(arg)?;
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.check_expr(callee)?;
                for arg in args {
                    self.check_expr(arg)?;
                }
                // TODO: Check closure effects when we have effect types on closures
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                self.check_expr(functor)?;
            }
            TirExprKind::Binary { left, right, .. } => {
                self.check_expr(left)?;
                self.check_expr(right)?;
            }
            TirExprKind::Unary { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Assign { target, value } => {
                self.check_expr(target)?;
                self.check_expr(value)?;
            }
            TirExprKind::Cast { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::FieldAccess { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Index { expr, index } => {
                self.check_expr(expr)?;
                self.check_expr(index)?;
            }
            TirExprKind::Block(block) => {
                self.check_block(block)?;
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.check_expr(condition)?;
                self.check_block(then_branch)?;
                if let Some(else_blk) = else_branch {
                    self.check_block(else_blk)?;
                }
            }
            TirExprKind::Match { expr, arms } => {
                self.check_expr(expr)?;
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.check_expr(guard)?;
                    }
                    self.check_expr(&arm.body)?;
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.check_expr(&field.value)?;
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    self.check_expr(elem)?;
                }
            }
            TirExprKind::ArrayLiteral { elements } => {
                for elem in elements {
                    self.check_expr(elem)?;
                }
            }
            TirExprKind::Closure { body, .. } => {
                // Closures inherit effects from enclosing function, so we continue checking
                self.check_expr(body)?;
            }
            TirExprKind::VariantConstruct { payload, .. } => {
                if let Some(payload_expr) = payload {
                    self.check_expr(payload_expr)?;
                }
            }
            TirExprKind::OptionSome { value } => {
                self.check_expr(value)?;
            }
            TirExprKind::Move { expr } => {
                self.check_expr(expr)?;
            }
            TirExprKind::LabeledBlock { block, .. } => {
                self.check_block(block)?;
            }
            TirExprKind::GlobalVarSet { value, .. } => {
                self.check_expr(value)?;
            }
            TirExprKind::IsNotNull { expr }
            | TirExprKind::UnwrapOption { expr, .. }
            | TirExprKind::VariantTag { expr }
            | TirExprKind::VariantTest { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::VariantPayload { expr, .. } => {
                self.check_expr(expr)?;
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                self.check_expr(scrutinee)?;
                for arm in arms {
                    self.check_block(arm)?;
                }
                self.check_block(default)?;
            }
            // Leaf expressions - no sub-expressions to check
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
            | TirExprKind::EnumConstruct { .. } => {}
        }
        Ok(())
    }

    /// Check a function call for effect violations
    fn check_call(&mut self, func_ref: &FunctionRef, span: Span) -> Result<(), Bail> {
        let callee_effects = self.get_function_effects(func_ref);

        for effect in &callee_effects {
            if !self.current_effects.contains(effect) {
                self.logger.error(EffectError {
                    callee: func_ref.name(),
                    missing_effect: effect.clone(),
                    span,
                })?;
            }
        }
        Ok(())
    }

    /// Get the effects required by a function
    fn get_function_effects(&self, func_ref: &FunctionRef) -> Vec<String> {
        match func_ref {
            FunctionRef::Resolved { func, .. } => func.borrow().effects.clone(),
            FunctionRef::External {
                module_source,
                name,
                ..
            } => {
                // Look up in the appropriate module
                if let Some(module) = self.modules.get(module_source) {
                    // Check functions
                    for func_rc in &module.functions {
                        let func = func_rc.borrow();
                        if func.name == *name {
                            return func.effects.clone();
                        }
                    }
                    // Check impl methods
                    for impl_block in &module.impls {
                        for method in &impl_block.methods {
                            if method.name == *name {
                                return method.effects.clone();
                            }
                        }
                    }
                }
                // Default: no effects required (builtins, unknown functions)
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_effect_error_display() {
        let error = EffectError {
            callee: "println".to_string(),
            missing_effect: "Stdout".to_string(),
            span: Span {
                start: 100,
                end: 107,
                line: 10,
                column: 5,
                end_line: 10,
            },
        };
        assert_eq!(
            error.to_string(),
            "10:5: missing effect 'Stdout' required by 'println'"
        );
    }
}
