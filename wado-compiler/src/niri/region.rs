//! Which blocks are self-contained enough to run as a frame.
//!
//! A self-contained region builds a value in locals of its own, reads and
//! writes only those locals, and yields the result as its value — as
//! self-contained as a call body, except that the caller wrote it inline.
//! Recognizing that shape is what lets a fully-constant string template fold
//! to the literal the source could have written.
//!
//! Everything here is asked before the run, because the run copies the whole
//! enclosing body while these only walk the block, and a codebase's blocks are
//! overwhelmingly not regions. What is left the run decides for itself — a
//! global it cannot read, a loop past the budget, an operation that would trap
//! — by abandoning the evaluation, which forfeits the fold rather than
//! dropping a write.

use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, LocalSet, NodeRef, PatKind, StmtKind};
use crate::tir::TypeTable;

use super::{CalleeMap, CtfeBuiltinMap};

/// The block behind a region-shaped expression: a `Block` or `LabeledBlock`
/// with enough statements to be worth running, yielding a value, and ending on
/// a statement that produces one.
///
/// A single-statement block is the lattice projection's case. A block that
/// yields nothing has no value to fold to, whatever its last statement
/// computed — an inlined statement call leaves the callee's result there while
/// the block stands where the program expects none — and the type is what says
/// so. Refusing both here is what keeps the attempt cheap on the blocks that
/// are not regions.
pub(super) fn region_shape(body: &Body, e: ExprId) -> Option<(BlockId, Option<&str>)> {
    if body.exprs[e].type_id == TypeTable::UNIT {
        return None;
    }
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
