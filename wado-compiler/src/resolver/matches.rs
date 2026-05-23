//! AST→TIR lowering for the `matches` operator.
//!
//! `s matches { p [&& guard] }` is preserved by the desugar phase and
//! expanded here, at type-resolution time, into the equivalent
//! `match s { p => guard_or_true, _ => false }` and routed through
//! [`Resolver::resolve_match_expr`]. Keeping the AST `MatchesExpr` until
//! TIR lowering means LSP queries (hover, jump-to-def, references) land on
//! the user's `matches` text as-written instead of on a synthetic match arm.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{TirExpr, TypeId};

use super::Resolver;
use super::types::FunctionContext;

impl<H: CompilerHost> Resolver<'_, H> {
    /// Expand a `matches` expression into its `match`-shaped equivalent and
    /// resolve it. The expansion mirrors the historical desugar in
    /// `desugar::desugar_matches_expr`:
    ///
    /// - No guard: `s matches { p }` → `match s { p => true, _ => false }`
    /// - Guarded:  `s matches { p && g }` → `match s { p => g, _ => false }`
    ///   (the guard expression becomes the arm body; if it returns `false`
    ///   the overall expression is `false`, matching the original semantics.)
    pub(super) fn resolve_matches_expr(
        &mut self,
        m: &ast::MatchesExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        let bool_lit = |value: bool| {
            ast::Expr::Literal(ast::LiteralExpr {
                id: m.id,
                value: ast::Literal::Bool(value),
                span: m.span,
            })
        };

        let match_body = m.guard.clone().unwrap_or_else(|| bool_lit(true));

        let synthetic = ast::MatchExpr {
            id: m.id,
            expr: m.expr.clone(),
            arms: vec![
                ast::MatchArm {
                    id: m.id,
                    pattern: m.pattern.clone(),
                    guard: None,
                    body: match_body,
                    span: m.span,
                },
                ast::MatchArm {
                    id: m.id,
                    pattern: ast::Pattern::Wildcard,
                    guard: None,
                    body: bool_lit(false),
                    span: m.span,
                },
            ],
            span: m.span,
        };

        self.resolve_match_expr(&synthetic, ctx, expected_type)
    }
}
