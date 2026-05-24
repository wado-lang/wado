//! TIR-direct lowering of the `matches` operator.
//!
//! `s matches { p [&& guard] }` lowers to a two-arm `TirExpr::Match`:
//! `match s { p => guard_or_true, _ => false }`. The match is built
//! directly in TIR (no synthetic AST), so the AST keeps the user's
//! `matches` shape for LSP cursor lookups and `wado format` round-trips.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{TirExpr, TirExprKind, TirMatchArm, TirPattern, TypeId, TypeTable};

use super::Resolver;
use super::types::FunctionContext;

impl<H: CompilerHost> Resolver<'_, H> {
    /// Lower `matches` to a TIR `Match`:
    ///
    /// - No guard: `s matches { p }` → `match s { p => true, _ => false }`
    /// - Guarded:  `s matches { p && g }` → `match s { p => g, _ => false }`
    ///
    /// When the pattern matches, the arm body returns the guard's value
    /// (or `true` if absent); the wildcard arm returns `false`. The wildcard
    /// trivially makes the match exhaustive, so no `check_match_exhaustiveness`
    /// call is needed here.
    pub(super) fn desugar_matches_expr(
        &mut self,
        m: &ast::MatchesExpr,
        ctx: &mut FunctionContext,
        _expected_type: Option<TypeId>,
    ) -> TirExpr {
        let scrutinee = self.resolve_expr(&m.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        // Pattern arm. The pattern's bindings (e.g. `Some(x)`) must be in
        // scope for the optional guard.
        ctx.enter_scope();
        let pattern_tir = self.resolve_if_pattern(&m.pattern, scrutinee_type, ctx, m.span);
        let arm_body = match &m.guard {
            Some(guard) => self.resolve_expr(guard, ctx, Some(TypeTable::BOOL)),
            None => bool_literal(true, m.span),
        };
        ctx.exit_scope();

        let arms = vec![
            TirMatchArm {
                pattern: pattern_tir,
                guard: None,
                body: arm_body,
                span: m.span,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: bool_literal(false, m.span),
                span: m.span,
            },
        ];

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            TypeTable::BOOL,
            m.span,
        )
    }
}

fn bool_literal(value: bool, span: crate::token::Span) -> TirExpr {
    TirExpr::new(TirExprKind::BoolLiteral(value), TypeTable::BOOL, span)
}
