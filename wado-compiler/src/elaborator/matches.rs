//! Annotation pass for the `matches` operator.
//!
//! The combined walk records the `Matches` desugar tag plus the scrutinee /
//! pattern / guard facts; reify rebuilds the two-arm `TirExprKind::Match`
//! (`match s { p [&& guard] => true|guard, _ => false }`) from the AST and
//! those facts.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{TirExpr, TirExprKind, TypeId, TypeTable};

use super::Elaborator;
use super::types::FunctionContext;

impl<H: CompilerHost> Elaborator<'_, H> {
    pub(super) fn desugar_matches_expr(
        &mut self,
        m: &ast::MatchesExpr,
        ctx: &mut FunctionContext,
        _expected_type: Option<TypeId>,
    ) -> TirExpr {
        self.record_desugar(m.id, super::sem::types::DesugarKind::Matches);
        let scrutinee = self.resolve_expr(&m.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        // Pattern arm. The pattern's bindings (e.g. `Some(x)`) must be in
        // scope for the optional guard, and `resolve_if_pattern` records
        // their `local_symbols` / `local_types` entries.
        ctx.enter_scope();
        let _ = self.resolve_if_pattern(&m.pattern, scrutinee_type, ctx, m.span);
        if let Some(guard) = &m.guard {
            let body = self.resolve_expr(guard, ctx, Some(TypeTable::BOOL));
            // `expected_type` on `resolve_expr` is only a coercion hint;
            // a non-bool guard (e.g. `Some(v) && v + 1`) would silently
            // pass and leave the synthesised `Match`'s declared type
            // (BOOL) inconsistent with the arm body's actual type.
            self.typecheck(body.type_id, TypeTable::BOOL, guard.span());
        }
        ctx.exit_scope();

        TirExpr::new(TirExprKind::Unit, TypeTable::BOOL, m.span)
    }
}
