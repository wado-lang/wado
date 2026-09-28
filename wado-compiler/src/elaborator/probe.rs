//! Coverage probes: where `wado test --coverage` asks for them, reify puts a
//! call to `core:rt::coverage_probe(id)` at the start of each region the plan
//! numbers. See [WEP: Test Coverage](../../../docs/wep-2026-09-28-test-coverage.md).

use crate::ast::{AstId, Block};
use crate::call_args::CallArgs;
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::coverage::ProbeSite;
use crate::tir::{
    CallArg, FunctionRef, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeTable,
};
use crate::token::Span;

use super::reify::Reify;

impl<H: CompilerHost> Reify<'_, H> {
    /// The probe `site` on node `id` takes, as a statement, when the plan puts
    /// one there.
    pub(super) fn probe(&mut self, site: ProbeSite, id: AstId, span: Span) -> Option<TirStmt> {
        let probe = self.coverage?.probe(site, id)?;
        self.probes_emitted.insert(probe);
        let (module_source, name) = {
            let type_table = self.tysys.type_table.borrow();
            let (module, name) = type_table
                .compiler_items()
                .require_function(CompilerItem::CoverageProbe);
            (module.clone(), name.to_string())
        };
        let id_literal = TirExpr::new(
            TirExprKind::IntLiteral {
                value: u64::from(probe),
                repr: probe.to_string(),
            },
            TypeTable::I32,
            span,
        );
        let call = TirExpr::new(
            TirExprKind::Call {
                func: Box::new(FunctionRef {
                    module_source,
                    name,
                    template: None,
                    monomorph_info: None,
                    method_info: None,
                }),
                type_args: Vec::new(),
                args: CallArgs::free(vec![CallArg::new(id_literal, false)]),
            },
            TypeTable::UNIT,
            span,
        );
        Some(TirStmt::new(TirStmtKind::Expr(call), span))
    }

    /// `block` with the probe for `site` on `id` put first.
    pub(super) fn probe_block(
        &mut self,
        site: ProbeSite,
        id: AstId,
        mut block: TirBlock,
    ) -> TirBlock {
        if let Some(probe) = self.probe(site, id, block.span) {
            block.stmts.insert(0, probe);
        }
        block
    }

    /// The `else` branch of the `if` `id`: `else_block` through `reify`, or,
    /// where the source omits it and its skipped path is a region, the probe
    /// alone.
    pub(super) fn else_branch(
        &mut self,
        else_block: Option<&Block>,
        id: AstId,
        span: Span,
        reify: impl FnOnce(&mut Self, &Block) -> TirBlock,
    ) -> Option<TirBlock> {
        if let Some(block) = else_block {
            return Some(reify(self, block));
        }
        let probe = self.probe(ProbeSite::ImplicitElse, id, span)?;
        Some(TirBlock::new(vec![probe], span))
    }

    /// `expr` run after the probe for `site` on `id`: a block gains the probe
    /// as its first statement, and any other expression is wrapped in one.
    pub(super) fn probe_expr(&mut self, site: ProbeSite, id: AstId, expr: TirExpr) -> TirExpr {
        let Some(probe) = self.probe(site, id, expr.span) else {
            return expr;
        };
        match expr.kind {
            TirExprKind::Block(mut block) => {
                block.stmts.insert(0, probe);
                TirExpr::new(TirExprKind::Block(block), expr.type_id, expr.span)
            }
            kind => {
                let (type_id, span) = (expr.type_id, expr.span);
                let inner = TirExpr::new(kind, type_id, span);
                let stmts = vec![probe, TirStmt::new(TirStmtKind::Expr(inner), span)];
                TirExpr::new(
                    TirExprKind::Block(TirBlock::new(stmts, span)),
                    type_id,
                    span,
                )
            }
        }
    }

    /// Account for the probes of the `for-of` `id`, whose body reify unrolls
    /// no instance of: its regions never run, and report so.
    pub(super) fn unroll_away_probes(&mut self, id: AstId) {
        if let Some(coverage) = self.coverage {
            self.probes_unrolled_away
                .extend(coverage.for_of_probes(id).iter().copied());
        }
    }

    /// Check that every probed region of each function reify emitted took its
    /// probe, or has no instance to take it in: a region reify reached without
    /// one would report as never run.
    pub(super) fn assert_probes_complete(&self) {
        let Some(coverage) = self.coverage else {
            return;
        };
        for &probe in &self.probes_emitted {
            let regions = coverage
                .function_probes(probe)
                .expect("an emitted probe is a planned one");
            let missing: Vec<u32> = regions
                .iter()
                .copied()
                .filter(|region| {
                    !self.probes_emitted.contains(region)
                        && !self.probes_unrolled_away.contains(region)
                })
                .collect();
            assert!(
                missing.is_empty(),
                "coverage regions {missing:?} of the function holding region {probe} \
                 in {} took no probe",
                self.current_module_source
            );
        }
    }
}
