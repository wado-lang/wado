//! Arena and value-pool census, printed under `WADO_TRACE=arena_census`. It
//! sizes the compile-speed items in the NIR optimizer architecture WEP.

use std::cmp::Reverse;

use crate::compiler_trace;
use crate::hashmap::IndexMap;
use crate::nir_arena::{Body, ExprKind, NodeRef, Operand};
use crate::nir_package::NirPackage;
use crate::trace::filter;

const TARGET: &str = "arena_census";

#[derive(Default)]
struct Census {
    bodies: usize,
    exprs_allocated: usize,
    exprs_reachable: usize,
    stmts_allocated: usize,
    stmts_reachable: usize,
    blocks_allocated: usize,
    blocks_reachable: usize,
    pats_allocated: usize,
    pats_reachable: usize,
    /// The promoted half of the operand bridge, over reachable nodes.
    value_operands: usize,
    /// The unpromoted half, so the printout carries the ratio.
    expr_operands: usize,
    pool_values: usize,
    with_value_graph: usize,
    loop_entry_snapshots: usize,
    loop_entry_locals: usize,
    /// Reachable expressions per `classify` verdict.
    per_kind: IndexMap<(&'static str, bool), usize>,
}

/// Report the census for `project` at the pipeline point named by `at`.
pub(super) fn report(project: &NirPackage, at: &str) {
    if !filter().enabled(TARGET) {
        return;
    }
    let mut census = Census::default();
    for func in &project.functions {
        if let Some(body) = &func.borrow().body {
            census.add(body);
        }
    }
    for global in &project.globals {
        census.add(global.init.slot_expr().body());
    }
    census.print(at);
}

impl Census {
    fn add(&mut self, body: &Body) {
        self.bodies += 1;
        self.exprs_allocated += body.exprs.len();
        self.stmts_allocated += body.stmts.len();
        self.blocks_allocated += body.blocks.len();
        self.pats_allocated += body.pats.len();
        self.pool_values += body.values.len();
        if let Some(graph) = &body.value_graph {
            self.with_value_graph += 1;
            self.loop_entry_snapshots += graph.loop_entry_values.len();
            self.loop_entry_locals += graph
                .loop_entry_values
                .values()
                .map(IndexMap::len)
                .sum::<usize>();
        }
        body.for_each_reachable_node(|node| {
            match node {
                NodeRef::Expr(e) => {
                    self.exprs_reachable += 1;
                    *self
                        .per_kind
                        .entry(classify(&body.exprs[e].kind))
                        .or_default() += 1;
                }
                NodeRef::Stmt(_) => self.stmts_reachable += 1,
                NodeRef::Block(_) => self.blocks_reachable += 1,
                NodeRef::Pat(_) => self.pats_reachable += 1,
            }
            body.for_each_operand(node, |op| match op {
                Operand::Value(_) => self.value_operands += 1,
                Operand::Expr(_) => self.expr_operands += 1,
            });
        });
    }

    fn print(&self, at: &str) {
        let pct = |n: usize| 100.0 * n as f64 / self.exprs_reachable.max(1) as f64;
        let bloat = |allocated: usize, reachable: usize| allocated as f64 / reachable.max(1) as f64;
        compiler_trace!(
            TARGET,
            "{at}: {} bodies, reachable expr/stmt/block/pat {}/{}/{}/{}, \
             bloat {:.2}x/{:.2}x/{:.2}x/{:.2}x, operands {} value + {} expr, \
             pool {} values, {} bodies with a graph ({} loop snapshots, {} locals)",
            self.bodies,
            self.exprs_reachable,
            self.stmts_reachable,
            self.blocks_reachable,
            self.pats_reachable,
            bloat(self.exprs_allocated, self.exprs_reachable),
            bloat(self.stmts_allocated, self.stmts_reachable),
            bloat(self.blocks_allocated, self.blocks_reachable),
            bloat(self.pats_allocated, self.pats_reachable),
            self.value_operands,
            self.expr_operands,
            self.pool_values,
            self.with_value_graph,
            self.loop_entry_snapshots,
            self.loop_entry_locals,
        );
        let pure: usize = self
            .per_kind
            .iter()
            .filter(|((_, pure), _)| *pure)
            .map(|(_, n)| n)
            .sum();
        compiler_trace!(
            TARGET,
            "{at}: pure kinds {:.1}% of reachable exprs",
            pct(pure)
        );
        let mut rows: Vec<(usize, &str, bool)> = self
            .per_kind
            .iter()
            .map(|(&(name, pure), &n)| (n, name, pure))
            .collect();
        rows.sort_by_key(|&(n, _, _)| Reverse(n));
        for (n, name, pure) in rows {
            compiler_trace!(
                TARGET,
                "{at}:   {:>7} {:>5.1}%  {name}{}",
                n,
                pct(n),
                if pure { " (pure)" } else { "" }
            );
        }
    }
}

/// An expression kind's census name, and whether it is a *pure kind*: one the
/// value pool has a `ValueKind` for, so only a freeze that reaches it stands
/// between the node and retirement from the skeleton.
fn classify(kind: &ExprKind) -> (&'static str, bool) {
    match kind {
        ExprKind::Dead => ("Dead", false),
        ExprKind::PackedArray(_) => ("PackedArray", false),
        // `ValueKind::Opaque` carries a local through `OpaqueSource::Local`.
        ExprKind::Local { .. } => ("Local", true),
        ExprKind::GlobalVarGet { .. } => ("GlobalVarGet", false),
        ExprKind::GlobalVarSet { .. } => ("GlobalVarSet", false),
        ExprKind::Binary { .. } => ("Binary", true),
        ExprKind::Unary { .. } => ("Unary", true),
        ExprKind::Assign { .. } => ("Assign", false),
        ExprKind::Cast { .. } => ("Cast", true),
        ExprKind::Call { .. } => ("Call", false),
        ExprKind::CmRawCall { .. } => ("CmRawCall", false),
        ExprKind::FieldAccess { .. } => ("FieldAccess", true),
        ExprKind::Index { .. } => ("Index", false),
        ExprKind::If { .. } => ("If", false),
        ExprKind::Match { .. } => ("Match", false),
        ExprKind::StructLiteral { .. } => ("StructLiteral", false),
        ExprKind::TupleLiteral { .. } => ("TupleLiteral", false),
        ExprKind::ArrayLiteral { .. } => ("ArrayLiteral", false),
        ExprKind::IndirectCall { .. } => ("IndirectCall", false),
        ExprKind::ClosureToCanonical { .. } => ("ClosureToCanonical", false),
        ExprKind::VariantConstruct { .. } => ("VariantConstruct", false),
        ExprKind::EnumConstruct { .. } => ("EnumConstruct", false),
        ExprKind::LabeledBlock { .. } => ("LabeledBlock", false),
        // Pure, but no `ValueKind` names a tag or a tag test.
        ExprKind::VariantTag { .. } => ("VariantTag", false),
        ExprKind::VariantTest { .. } => ("VariantTest", false),
        ExprKind::VariantPayload { .. } => ("VariantPayload", false),
        ExprKind::Switch { .. } => ("Switch", false),
    }
}
