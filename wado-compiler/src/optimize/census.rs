//! Arena and value-pool census, printed under `WADO_TRACE=arena_census`.
//!
//! What it answers: how much of the skeleton a walk covers, how much of it is
//! the pure kinds the value pool could hold instead, and how far the arena has
//! grown past what is still reachable. Those are the numbers that size the
//! compile-speed items in `docs/wep-2026-06-05-nir-optimizer-architecture.md`,
//! and they go stale on their own.

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
    /// Reachable `Operand::Value` slots — the promoted half of the bridge.
    promoted_operands: usize,
    /// Reachable `Operand::Expr` slots, for the ratio the bridge is at.
    expr_operands: usize,
    pool_values: usize,
    with_value_graph: usize,
    loop_entry_snapshots: usize,
    loop_entry_locals: usize,
    /// Reachable expression count per kind, in `EXPR_KINDS` order.
    per_kind: [usize; EXPR_KINDS.len()],
}

/// Report the census for `project` at the pipeline point named by `at`.
pub(super) fn report(project: &NirPackage, at: &str) {
    if !filter().enabled(TARGET) {
        return;
    }
    let mut c = Census::default();
    for func in &project.functions {
        if let Some(body) = &func.borrow().body {
            c.add(body);
        }
    }
    for global in &project.globals {
        c.add(global.init.slot_expr().body());
    }
    c.print(at);
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
                    self.per_kind[kind_index(&body.exprs[e].kind)] += 1;
                }
                NodeRef::Stmt(_) => self.stmts_reachable += 1,
                NodeRef::Block(_) => self.blocks_reachable += 1,
                NodeRef::Pat(_) => self.pats_reachable += 1,
            }
            body.for_each_operand(node, |op| match op {
                Operand::Value(_) => self.promoted_operands += 1,
                Operand::Expr(_) => self.expr_operands += 1,
            });
        });
    }

    fn print(&self, at: &str) {
        let reach = self.exprs_reachable.max(1) as f64;
        let pct = |n: usize| 100.0 * n as f64 / reach;
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
            self.promoted_operands,
            self.expr_operands,
            self.pool_values,
            self.with_value_graph,
            self.loop_entry_snapshots,
            self.loop_entry_locals,
        );
        let pure: usize = EXPR_KINDS
            .iter()
            .enumerate()
            .filter(|(_, (_, pure))| *pure)
            .map(|(i, _)| self.per_kind[i])
            .sum();
        compiler_trace!(
            TARGET,
            "{at}: pure kinds {:.1}% of reachable exprs",
            pct(pure)
        );
        let mut rows: Vec<(usize, &str, bool)> = EXPR_KINDS
            .iter()
            .enumerate()
            .map(|(i, (name, pure))| (self.per_kind[i], *name, *pure))
            .filter(|(n, _, _)| *n > 0)
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

/// Each expression kind and whether it is a *pure kind*: one whose whole
/// meaning a `ValueKind` can carry, so retiring it from the skeleton is only a
/// matter of the freeze reaching it.
const EXPR_KINDS: [(&str, bool); 27] = [
    ("Dead", false),
    ("PackedArray", false),
    ("Local", true),
    ("GlobalVarGet", false),
    ("GlobalVarSet", false),
    ("Binary", true),
    ("Unary", true),
    ("Assign", false),
    ("Cast", true),
    ("Call", false),
    ("CmRawCall", false),
    ("FieldAccess", true),
    ("Index", false),
    ("If", false),
    ("Match", false),
    ("StructLiteral", false),
    ("TupleLiteral", false),
    ("ArrayLiteral", false),
    ("IndirectCall", false),
    ("ClosureToCanonical", false),
    ("VariantConstruct", false),
    ("EnumConstruct", false),
    ("LabeledBlock", false),
    ("VariantTag", true),
    ("VariantTest", true),
    ("VariantPayload", false),
    ("Switch", false),
];

fn kind_index(kind: &ExprKind) -> usize {
    match kind {
        ExprKind::Dead => 0,
        ExprKind::PackedArray(_) => 1,
        ExprKind::Local { .. } => 2,
        ExprKind::GlobalVarGet { .. } => 3,
        ExprKind::GlobalVarSet { .. } => 4,
        ExprKind::Binary { .. } => 5,
        ExprKind::Unary { .. } => 6,
        ExprKind::Assign { .. } => 7,
        ExprKind::Cast { .. } => 8,
        ExprKind::Call { .. } => 9,
        ExprKind::CmRawCall { .. } => 10,
        ExprKind::FieldAccess { .. } => 11,
        ExprKind::Index { .. } => 12,
        ExprKind::If { .. } => 13,
        ExprKind::Match { .. } => 14,
        ExprKind::StructLiteral { .. } => 15,
        ExprKind::TupleLiteral { .. } => 16,
        ExprKind::ArrayLiteral { .. } => 17,
        ExprKind::IndirectCall { .. } => 18,
        ExprKind::ClosureToCanonical { .. } => 19,
        ExprKind::VariantConstruct { .. } => 20,
        ExprKind::EnumConstruct { .. } => 21,
        ExprKind::LabeledBlock { .. } => 22,
        ExprKind::VariantTag { .. } => 23,
        ExprKind::VariantTest { .. } => 24,
        ExprKind::VariantPayload { .. } => 25,
        ExprKind::Switch { .. } => 26,
    }
}
