//! Desugar the `matches` operator at TIR-lowering time.
//!
//! `s matches { p [&& guard] }` is synthesised as
//! `match s { p => guard_or_true, _ => false }` and routed through
//! [`Resolver::resolve_match_expr`]. Keeping the AST `MatchesExpr`
//! until here means LSP queries (hover, jump-to-def, references) land
//! on the user's `matches` text rather than on a synthetic match arm.

use crate::ast;
use crate::compiler_host::CompilerHost;
use crate::tir::{TirExpr, TypeId};

use super::Resolver;
use super::types::FunctionContext;

impl<H: CompilerHost> Resolver<'_, H> {
    /// Desugar `matches` into its `match`-shaped equivalent and resolve:
    ///
    /// - No guard: `s matches { p }` → `match s { p => true, _ => false }`
    /// - Guarded:  `s matches { p && g }` → `match s { p => g, _ => false }`
    ///
    /// When the pattern matches, the arm body returns the guard's value
    /// (or `true` if absent); the wildcard arm returns `false`.
    pub(super) fn desugar_matches_expr(
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
