//! Which blocks are self-contained enough to run as a frame.
//!
//! A self-contained region builds a value in locals of its own, reads and
//! writes only those locals, and yields the result as its value — as
//! self-contained as a call body, except that the caller wrote it inline.
//! Recognizing that shape is what lets a fully-constant string template fold
//! to the literal the source could have written.
//!
//! Two questions decide it, and both are asked before the run, because the run
//! clones the whole enclosing body while these only walk the block.
//!
//! Does every local the region touches belong to the region. A region's frame
//! starts with an empty environment, so a read of an outer local yields
//! nothing and the run abandons, and a write to one would be dropped along
//! with the block the fold replaces.
//!
//! Is every call one a frame could run. A callee the map does not hold has no
//! body to execute and an indirect one no known target, so the run reaches it
//! and abandons — which is what `` `${f()}` `` over an unfoldable `f` does.
//! That shape is common, and paying a body clone per visit for it is the
//! difference between linear and quadratic const folding.
//!
//! What is left the run still decides for itself — a global it cannot read, a
//! loop past the budget, an operation that would trap — by abandoning the
//! evaluation, which forfeits the fold rather than dropping a write.

use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, LocalSet, NodeRef, PatKind, StmtKind};

use super::{CalleeMap, CtfeBuiltinMap};

/// The block behind a region-shaped expression: a `Block` or `LabeledBlock`
/// with enough statements to be worth running, ending on a statement that
/// yields the block's value. A single-statement block is the lattice
/// projection's case, and a block whose last statement yields nothing has no
/// value to fold to — refusing those before anything is cloned is what keeps
/// the attempt cheap on the blocks that are not regions.
pub(super) fn region_shape(body: &Body, e: ExprId) -> Option<(BlockId, Option<&str>)> {
    let (block, label) = match &body.exprs[e].kind {
        ExprKind::Block(b) => (*b, None),
        ExprKind::LabeledBlock { block, label, .. } => (*block, Some(label.as_str())),
        _ => return None,
    };
    let stmts = &body.blocks[block].stmts;
    if stmts.len() < 2 {
        return None;
    }
    match &body.stmts[*stmts.last()?].kind {
        StmtKind::Expr(_) | StmtKind::Break { value: Some(_), .. } => Some((block, label)),
        StmtKind::Break { value: None, .. }
        | StmtKind::Let { .. }
        | StmtKind::Return { .. }
        | StmtKind::If { .. }
        | StmtKind::Loop { .. }
        | StmtKind::Continue
        | StmtKind::LabeledBlock { .. }
        | StmtKind::LetDestructure { .. } => None,
    }
}

/// Whether every local `block` mentions is one `block` itself declares, every
/// call in it is one a frame could run, and nothing writes a global.
///
/// Only the reachable nodes are scanned, so a mention an earlier rewrite
/// orphaned neither disqualifies the region nor keeps it from folding. What
/// the scan cannot see is which reachable nodes actually execute: a free local
/// read or an unrunnable call on a statically dead path costs the fold, which
/// is the price of answering before the body is cloned rather than after.
pub(super) fn region_is_self_contained(
    body: &Body,
    block: BlockId,
    callees: Option<&CalleeMap>,
    ctfe_builtins: Option<&CtfeBuiltinMap>,
) -> bool {
    let runnable = |func_id| {
        callees.is_some_and(|m| m.contains_key(&func_id))
            || ctfe_builtins.is_some_and(|m| m.contains_key(&func_id))
    };
    let mut declared = LocalSet::default();
    let mut mentioned = LocalSet::default();
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::Stmt(s) => {
                if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind {
                    declared.insert(*local_index);
                }
            }
            NodeRef::Pat(p) => {
                if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                    declared.insert(*local_index);
                }
            }
            NodeRef::Expr(e) => match &body.exprs[e].kind {
                ExprKind::GlobalVarSet { .. }
                | ExprKind::IndirectCall { .. }
                | ExprKind::CmRawCall { .. } => return false,
                ExprKind::Call { func_id, .. } | ExprKind::MethodCall { func_id, .. } => {
                    if !runnable(*func_id) {
                        return false;
                    }
                }
                ExprKind::Local { index, .. } => {
                    mentioned.insert(*index);
                }
                _ => {}
            },
            NodeRef::Block(_) => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    mentioned.iter().all(|index| declared.contains(index))
}
