//! Write-only local elimination for Wado NIR.
//!
//! Eliminates `let x = expr;` bindings where the local `x` is never read,
//! never has its address taken, and never escapes via closure capture or a
//! `stores`-aliased call. When `expr` is pure the entire statement is removed;
//! otherwise the binding is replaced by `Expr(expr)` so the side effect still
//! runs.
//!
//! NIR analog of `wir_optimize/elide_local.rs`. Running at NIR exposes the
//! freshly dead expressions to the rest of the fixed-point loop
//! (`copy_prop` / `const_fold` / `dce`), which the WIR-level pass cannot.

use crate::hashmap::IndexSet;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};

use super::arena_query;

/// Two-tier view of the locals read through promoted `Operand::Value`s.
///
/// The value pool is append-only, so `opaque_local_sources` keeps naming a
/// local forever once any read of it was promoted — even after every operand
/// that referenced that value has been folded away. The pool-wide set stays as
/// the cheap first tier; when it is the *only* thing keeping a binding alive,
/// the precise second tier walks the body's operand slots and collects the
/// locals actually reachable from a live `Operand::Value`. The walk includes
/// orphaned slots (`Body::for_each_operand`), which only over-approximates —
/// sound for a keep-decision.
struct PromotedReads {
    pool: IndexSet<u32>,
    live: Option<IndexSet<u32>>,
}

impl PromotedReads {
    fn collect(body: &Body) -> Self {
        Self {
            pool: body.values.opaque_local_sources().collect(),
            live: None,
        }
    }

    fn contains(&mut self, body: &Body, local: u32) -> bool {
        if !self.pool.contains(&local) {
            return false;
        }
        // BISECT: pool-only (pre-precision behaviour)
        if true {
            return true;
        }
        self.live
            .get_or_insert_with(|| {
                let mut out = IndexSet::default();
                body.for_each_operand(|op| {
                    if let Operand::Value(v) = op {
                        body.values.collect_opaque_locals(v, &mut out);
                    }
                });
                out
            })
            .contains(&local)
    }
}

/// What to do with a statement that binds / assigns a write-only local.
enum Action {
    /// Keep the statement unchanged.
    Keep,
    /// Drop the statement entirely (its value is pure).
    Drop,
    /// Replace the statement with `Expr(value)` so the side effect still runs.
    Demote(ExprId),
}

pub(super) struct ElideRule<'a> {
    stores_aliased: &'a IndexSet<u32>,
}

impl<'a> ElideRule<'a> {
    /// Build the rule for one function. `stores_aliased` lists params whose
    /// reference escaped via a callee's `stores` declaration: the callee may
    /// retain that reference past its return, so writes through the local stay
    /// observable via the alias and the local must not be elided. It is read
    /// off the function before its body is borrowed for the engine. The other
    /// "kept" source — every live read of a local, including `&local` /
    /// `&mut local` and closure-capture reads — is exactly what the engine use
    /// index records, so the rule reads it directly via `Engine::is_local_read`
    /// rather than a separate walk. (`address_taken_locals` is intentionally
    /// *not* a source: it is a stale static record after `inline` / `ref_elim`,
    /// and source-1 reads already cover every live `&local`.)
    pub(super) fn new(stores_aliased: &'a IndexSet<u32>) -> Self {
        Self { stores_aliased }
    }
}

impl Rule for ElideRule<'_> {
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        // Locals read only through a promoted `Operand::Value` are live but
        // invisible to the use index, so keep them (see `PromotedReads` for the
        // pool-wide filter + precise live-operand walk).
        let mut promoted_reads = PromotedReads::collect(engine.body);
        let mut new_stmts = Vec::with_capacity(stmts.len());
        let mut changed = false;
        for stmt in stmts {
            match classify(engine, stmt, self.stores_aliased, &mut promoted_reads) {
                Action::Keep => new_stmts.push(stmt),
                Action::Drop => changed = true,
                Action::Demote(value) => {
                    let span = engine.body.stmts[stmt].span;
                    new_stmts.push(engine.alloc_stmt(StmtKind::Expr(value.into()), span));
                    changed = true;
                }
            }
        }
        if changed {
            engine.set_block_stmts(id, new_stmts);
        }
        changed
    }
}

/// Classify a statement for write-only-local elimination. Mirrors the former
/// tree `Elider`: an unread `let x = value` or a bare `x = value` (assign at
/// statement position) where `x` is unread is dropped when `value` is pure,
/// otherwise demoted to `Expr(value)`.
fn classify(
    engine: &Engine,
    stmt: StmtId,
    stores_aliased: &IndexSet<u32>,
    promoted_reads: &mut PromotedReads,
) -> Action {
    match &engine.body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let (idx, value) = (*local_index, *value);
            if is_kept(engine, idx, stores_aliased, promoted_reads) {
                Action::Keep
            } else if arena_query::is_pure_nontrapping_operand_typed(
                engine.body,
                value,
                engine.value_graph_type_table(),
            ) {
                Action::Drop
            } else {
                // Effectful or trap-capable ⟹ a skeleton expr (a promoted value
                // is pure and non-trapping → Drop above). Demote keeps it as a
                // bare `Expr(value)` so any trap it carries still fires.
                value.as_expr().map_or(Action::Drop, Action::Demote)
            }
        }
        // `x = value;` (Assign at stmt position) where `x` is unread. This
        // catches the SROA / variant-lowering shadow-temp pattern where a pass
        // introduces a local, writes to it via Assign, then a downstream pass
        // folds away the only read site. The matching `let x;` declaration
        // falls out once every write to `x` is gone.
        StmtKind::Expr(Operand::Expr(e)) => {
            let assign = match &engine.body.exprs[*e].kind {
                ExprKind::Assign { target, value } => Some((*target, *value)),
                _ => None,
            };
            if let Some((target, value)) = assign
                && let ExprKind::Local { index, .. } = &engine.body.exprs[target].kind
            {
                let index = *index;
                if !is_kept(engine, index, stores_aliased, promoted_reads) {
                    return if arena_query::is_pure_nontrapping_operand_typed(
                        engine.body,
                        value,
                        engine.value_graph_type_table(),
                    ) {
                        Action::Drop
                    } else {
                        // Effectful or trap-capable ⟹ a skeleton expr (a promoted
                        // value is pure and non-trapping → Drop above).
                        value.as_expr().map_or(Action::Drop, Action::Demote)
                    };
                }
            }
            Action::Keep
        }
        _ => Action::Keep,
    }
}

/// A local is kept (not elidable) when its reference escaped via a `stores`
/// alias, it is read anywhere in the body, or a live promoted value reads it.
fn is_kept(
    engine: &Engine,
    local: u32,
    stores_aliased: &IndexSet<u32>,
    promoted_reads: &mut PromotedReads,
) -> bool {
    stores_aliased.contains(&local)
        || engine.is_local_read(local)
        || promoted_reads.contains(engine.body, local)
}
