//! Desugar the `matches` operator at TIR-lowering time.
//!
//! `s matches { p [&& guard] }` is synthesised as
//! `match s { p => guard_or_true, _ => false }` and routed through
//! [`Resolver::resolve_match_expr`]. Keeping the AST `MatchesExpr`
//! until here means LSP queries (hover, jump-to-def, references) land
//! on the user's `matches` text rather than on a synthetic match arm.

use crate::ast::{self, AstId};
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
    ///
    /// Every node in the synthesised `MatchExpr` uses [`AstId::SYNTHETIC`],
    /// keeping the scaffold disjoint from LSP-visible parser ids — the
    /// user's cursor on a `matches` expression resolves to `m.id`, which
    /// is preserved on the original `MatchesExpr` and never overwritten
    /// by the scaffold.
    pub(super) fn desugar_matches_expr(
        &mut self,
        m: &ast::MatchesExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        let bool_lit = |value: bool| {
            ast::Expr::Literal(ast::LiteralExpr {
                id: AstId::SYNTHETIC,
                value: ast::Literal::Bool(value),
                span: m.span,
            })
        };

        let match_body = m.guard.clone().unwrap_or_else(|| bool_lit(true));

        let synthetic = ast::MatchExpr {
            id: AstId::SYNTHETIC,
            expr: m.expr.clone(),
            arms: vec![
                ast::MatchArm {
                    id: AstId::SYNTHETIC,
                    pattern: m.pattern.clone(),
                    guard: None,
                    body: match_body,
                    span: m.span,
                },
                ast::MatchArm {
                    id: AstId::SYNTHETIC,
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
