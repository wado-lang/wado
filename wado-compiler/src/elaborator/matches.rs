//! Annotation pass for the `matches` operator.
//!
//! The body walk records the `Matches` desugar tag plus the scrutinee /
//! pattern / guard facts; reify rebuilds the two-arm `TirExprKind::Match`
//! (`match s { p [&& guard] => true|guard, _ => false }`) from the AST and
//! those facts.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::TypeId;
use crate::type_table::TypeTable;

use super::Elaborator;
use super::types::FunctionContext;
use crate::elaborator::sem::types::DesugarKind;

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn desugar_matches_expr(
        &mut self,
        m: &ast::MatchesExpr,
        ctx: &mut FunctionContext,
        _expected_type: Option<TypeId>,
    ) -> TypeId {
        self.record_desugar(m.id, DesugarKind::Matches);
        let scrutinee_type = self.resolve_expr(&m.expr, ctx, None);

        // Pattern arm. The pattern's bindings (e.g. `Some(x)`) must be in
        // scope for the optional guard, and `resolve_if_pattern` records
        // their `local_symbols` / `local_types` entries.
        let ctx = &mut ctx.enter_scope();
        self.resolve_if_pattern(&m.pattern, scrutinee_type, ctx, m.span);
        if let Some(guard) = &m.guard {
            self.resolve_condition_expr(guard, ctx);
        }

        TypeTable::BOOL
    }
}
