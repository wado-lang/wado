//! Which blocks are closed enough to run as a frame.
//!
//! A closed region builds a value in locals of its own, writes only to those
//! locals, and yields the result as its value — as self-contained as a call
//! body, except that the caller wrote it inline. Recognizing that shape is what
//! lets a fully-constant string template fold to the literal the source could
//! have written.
//!
//! The check here is deliberately minimal, because the frame guards most write
//! channels itself: a call's write-back and every builtin write land through
//! [`Interpreter::place_value`] / `update_place`, which require the
//! environment to already hold the root — and a region frame's environment
//! starts empty, gaining keys only from the region's own `let`s, from
//! whole-local assignments, and through borrow aliases. So two static facts
//! close the induction: an assignment's root and a `&mut` borrow's root must
//! be locals the region declares. Everything else that touches outer state —
//! an unrunnable call, a global write, a read the frame cannot answer —
//! abandons the evaluation at run time, which forfeits the fold rather than
//! dropping a write.
//!
//! [`Interpreter::place_value`]: super::Interpreter::place_value

use crate::nir::NirUnaryOp;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, LocalSet, NodeRef, PatKind, StmtKind};

use super::place::place_of;

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

/// Whether every place the region can commit to roots at a local it declares.
///
/// Only two mention kinds matter (see the module doc): an assignment target,
/// which the executor performs by binding its root's environment entry, and a
/// `&mut` borrow, which can become an alias the executor later commits
/// through. A mention that names no local place needs no accounting — the
/// frame has nothing to bind it to and abandons the evaluation instead.
///
/// A global write is refused eagerly. The frame would abandon on it anyway,
/// so this loses no fold; it only skips cloning a body for a region that can
/// never finish.
pub(super) fn region_is_closed(body: &Body, block: BlockId) -> bool {
    let mut declared = LocalSet::default();
    let mut written = LocalSet::default();
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
            NodeRef::Expr(e) => {
                let committed = match &body.exprs[e].kind {
                    ExprKind::GlobalVarSet { .. } => return false,
                    ExprKind::Assign { target, .. } => place_of(body, (*target).into()),
                    ExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr,
                    } => place_of(body, *expr),
                    _ => None,
                };
                if let Some((root, _)) = committed {
                    written.insert(root);
                }
            }
            NodeRef::Block(_) => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    written.iter().all(|index| declared.contains(index))
}
