//! Which blocks are closed enough to run as a frame.
//!
//! A closed region builds a value in locals of its own, writes only to those
//! locals, and yields the result as its value — as self-contained as a call
//! body, except that the caller wrote it inline. Recognizing that shape is what
//! lets a fully-constant string template fold to the literal the source could
//! have written.
//!
//! The check here is about where writes land, nothing more. Everything else —
//! a read the frame cannot answer, a call it cannot run, control flow leaving
//! the block — the frame run itself decides by abandoning the evaluation.
//! Writes are different: the frame executor performs a whole-local assignment
//! by binding its environment, so one targeting a local the region did not
//! declare would be silently dropped when the region is replaced by its value.

use crate::nir::NirUnaryOp;
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, LocalSet, NodeRef, Operand, PatKind, StmtKind,
};

use super::place::lvalue_root_local;

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

/// Whether every write inside `block` lands in a local the block itself
/// declares. A write whose target the scan cannot root — a global, a store
/// through something that is not a place — closes nothing and refuses the
/// region.
///
/// A method receiver counts as written whenever it roots at a local: whether
/// the callee takes `&mut self` is the frame's knowledge, not the scan's, and
/// a receiver the region declares is the common case either way.
pub(super) fn region_is_closed(body: &Body, block: BlockId) -> bool {
    let mut declared = LocalSet::default();
    let mut written = LocalSet::default();
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::Stmt(s) => match &body.stmts[s].kind {
                StmtKind::Let { local_index, .. } => {
                    declared.insert(*local_index);
                }
                StmtKind::Expr(_)
                | StmtKind::Return { .. }
                | StmtKind::If { .. }
                | StmtKind::Loop { .. }
                | StmtKind::Break { .. }
                | StmtKind::Continue
                | StmtKind::LabeledBlock { .. }
                | StmtKind::LetDestructure { .. } => {}
            },
            NodeRef::Pat(p) => {
                if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                    declared.insert(*local_index);
                }
            }
            NodeRef::Expr(e) => {
                if !record_expr_writes(body, e, &mut written) {
                    return false;
                }
            }
            NodeRef::Block(_) => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    written.iter().all(|index| declared.contains(index))
}

/// Record the local roots `e` may write into `written`; `false` when it writes
/// somewhere no local roots.
fn record_expr_writes(body: &Body, e: ExprId, written: &mut LocalSet) -> bool {
    fn root_or_refuse(body: &Body, op: Operand, written: &mut LocalSet) -> bool {
        match borrow_root_local(body, op) {
            Some(root) => {
                written.insert(root);
                true
            }
            None => false,
        }
    }
    match &body.exprs[e].kind {
        ExprKind::GlobalVarSet { .. } => false,
        ExprKind::Assign { target, .. } => root_or_refuse(body, (*target).into(), written),
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr,
        } => root_or_refuse(body, *expr, written),
        ExprKind::MethodCall { receiver, args, .. } => {
            if let Some(root) = borrow_root_local(body, *receiver) {
                written.insert(root);
            }
            args.iter()
                .filter(|a| a.is_mut)
                .all(|a| root_or_refuse(body, a.expr, written))
        }
        ExprKind::Call { args, .. } => args
            .iter()
            .filter(|a| a.is_mut)
            .all(|a| root_or_refuse(body, a.expr, written)),
        _ => true,
    }
}

/// [`lvalue_root_local`] through the borrow an argument wraps its place in —
/// `&mut c.repr` roots at `c`.
fn borrow_root_local(body: &Body, op: Operand) -> Option<u32> {
    match &body.exprs[op.as_expr()?].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr,
        } => borrow_root_local(body, *expr),
        _ => lvalue_root_local(body, op),
    }
}
