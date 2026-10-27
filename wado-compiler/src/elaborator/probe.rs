//! What reify puts at the start of each region the coverage plan numbers: a
//! call to `core:rt::coverage_probe(id)` where `wado test --coverage` or
//! `--pgo` asks for one, and a cold marker where a profile recorded no run.
//! See [WEP: Test Coverage](../../../docs/wep-2026-09-28-test-coverage.md) and
//! [WEP: Profile-Guided Optimization](../../../docs/wep-2026-10-25-profile-guided-optimization.md).

use crate::ast::{AstId, Block};
use crate::compiler_host::CompilerHost;
use crate::compiler_item::CompilerItem;
use crate::coverage::ProbeSite;
use crate::hashmap::IndexSet;
use crate::synthesis::common::known_cold_path_stmt;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind};
use crate::token::Span;
use crate::type_table::TypeTable;

use super::reify::{Reify, rt_call};

impl<H: CompilerHost> Reify<'_, H> {
    /// What reify puts at `site` on node `id`, in order: the cold marker the
    /// profile asks for there, then the coverage probe.
    pub(super) fn site_stmts(&mut self, site: ProbeSite, id: AstId, span: Span) -> Vec<TirStmt> {
        let marker = self
            .cold
            .is_some_and(|cold| cold.marks(site, id))
            .then(|| known_cold_path_stmt(span));
        marker
            .into_iter()
            .chain(self.probe(site, id, span))
            .collect()
    }

    /// The probe `site` on node `id` takes, as a statement, when the plan puts
    /// one there.
    fn probe(&mut self, site: ProbeSite, id: AstId, span: Span) -> Option<TirStmt> {
        let probe = self.coverage?.probe(site, id)?;
        self.probes_emitted.insert(probe);
        let id_literal = TirExpr::new(
            TirExprKind::IntLiteral {
                value: u64::from(probe),
                repr: probe.to_string(),
            },
            TypeTable::I32,
            span,
        );
        let call = rt_call(
            &self.tysys.type_table.borrow(),
            CompilerItem::CoverageProbe,
            id_literal,
            TypeTable::UNIT,
            span,
        );
        Some(TirStmt::new(TirStmtKind::Expr(call), span))
    }

    /// `block` with what reify puts at `site` on `id` first.
    pub(super) fn probe_block(
        &mut self,
        site: ProbeSite,
        id: AstId,
        mut block: TirBlock,
    ) -> TirBlock {
        let head = self.site_stmts(site, id, block.span);
        block.stmts.splice(0..0, head);
        block
    }

    /// The `else` branch of the `if` `id`: `else_block` through `reify`, or,
    /// where the source omits it and its skipped path is a region, what reify
    /// puts there alone.
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
        let stmts = self.site_stmts(ProbeSite::ImplicitElse, id, span);
        (!stmts.is_empty()).then(|| TirBlock::new(stmts, span))
    }

    /// `expr` run after what reify puts at `site` on `id`: a block gains it
    /// as its first statements, and any other expression is wrapped in one.
    pub(super) fn probe_expr(&mut self, site: ProbeSite, id: AstId, expr: TirExpr) -> TirExpr {
        let mut head = self.site_stmts(site, id, expr.span);
        if head.is_empty() {
            return expr;
        }
        match expr.kind {
            TirExprKind::Block(mut block) => {
                block.stmts.splice(0..0, head);
                TirExpr::new(TirExprKind::Block(block), expr.type_id, expr.span)
            }
            kind => {
                let (type_id, span) = (expr.type_id, expr.span);
                let inner = TirExpr::new(kind, type_id, span);
                head.push(TirStmt::new(TirStmtKind::Expr(inner), span));
                TirExpr::new(TirExprKind::Block(TirBlock::new(head, span)), type_id, span)
            }
        }
    }

    /// Skip the probes of the body of the `for-of` `id`, which reify unrolls
    /// no instance of: its regions never run, and report so.
    pub(super) fn skip_for_of_body(&mut self, id: AstId) {
        if let Some(coverage) = self.coverage {
            self.probes_without_instance
                .extend(coverage.for_of_body(id));
        }
    }

    /// Check that every probed region of each function reify emitted took its
    /// probe, or has no instance to take it in: a region reify reached without
    /// one would report as never run.
    pub(super) fn assert_probes_complete(&self) {
        let Some(coverage) = self.coverage else {
            return;
        };
        let functions: IndexSet<usize> = self
            .probes_emitted
            .iter()
            .map(|&probe| {
                coverage
                    .function_of(probe)
                    .expect("an emitted probe is a planned one")
            })
            .collect();
        for function in functions {
            let missing: Vec<u32> = coverage
                .function_probes(function)
                .iter()
                .copied()
                .filter(|region| {
                    !self.probes_emitted.contains(region)
                        && !self.probes_without_instance.contains(region)
                })
                .collect();
            assert!(
                missing.is_empty(),
                "coverage regions {missing:?} of function {function} in {} took no probe",
                self.current_module_source
            );
        }
    }
}
