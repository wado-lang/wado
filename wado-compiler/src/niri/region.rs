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

use crate::name::RefKind;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, LocalSet, NodeRef, Operand, PatKind, StmtKind,
};
use crate::tir::{TypeId, TypeTable};

use super::callee::{Callee, CallSite, CalleeMap};
use super::place::write_root_local;
use super::CtfeBuiltinMap;

/// Record the local each `&mut` parameter of `site` writes. `None` when the
/// site does not match the signature, or when a write's place no local roots.
fn write_targets(
    body: &Body,
    site: &CallSite<'_>,
    callee: &Callee,
    written: &mut LocalSet,
) -> Option<()> {
    for (index, op) in site.matching_operands(callee)? {
        if callee.writes_param(index) {
            written.insert(write_root_local(body, op)?);
        }
    }
    Some(())
}

/// The block behind a `Block` / `LabeledBlock` expression that can yield a
/// value. A unit-typed block has no value to fold to, whatever its last
/// statement computed — an inlined statement call leaves the callee's result
/// there while the block stands where the program expects none — and the type
/// is what says so.
pub(super) fn value_block_shape(body: &Body, e: ExprId) -> Option<(BlockId, Option<&str>)> {
    if body.exprs[e].type_id == TypeTable::UNIT {
        return None;
    }
    match &body.exprs[e].kind {
        ExprKind::Block(b) => Some((*b, None)),
        ExprKind::LabeledBlock { block, label, .. } => Some((*block, Some(label.as_str()))),
        _ => None,
    }
}

/// The value block behind a region-shaped expression: enough statements to be
/// worth running, ending on a statement that produces the value.
///
/// A single-statement block is the lattice projection's case. Refusing it here
/// is what keeps the attempt cheap on the blocks that are not regions.
pub(super) fn region_shape(body: &Body, e: ExprId) -> Option<(BlockId, Option<&str>)> {
    let (block, label) = value_block_shape(body, e)?;
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

/// The outer locals `block` only reads — the seeds a region frame needs —
/// provided every call in it is one a frame could run and nothing writes a
/// global. `None` disqualifies the region outright: an unrunnable call, a
/// global write, an outer local in a write position — where folding the
/// region would drop the write the program performs — or an outer local of
/// reference type, since reading a `&mut T` value hands a callee the same
/// write capability.
///
/// A write position is an `Assign` target, a `&mut` borrow, or an argument —
/// receiver included — the callee's signature takes by `&mut`. The signature
/// is the only reliable witness: `ArenaCallArg::is_mut` marks a by-value
/// `mut` parameter, the callee's own copy, and boxing can erase the borrow
/// node at the call site. A write whose place no local roots also
/// disqualifies: the executor resolves stores through the same chains, so an
/// unrooted one is a write this scan cannot account for, not a write that
/// will not happen.
///
/// Only the reachable nodes are scanned, so a mention an earlier rewrite
/// orphaned neither disqualifies the region nor keeps it from folding. What
/// the scan cannot see is which reachable nodes actually execute: a free local
/// read or an unrunnable call on a statically dead path costs the fold, which
/// is the price of answering before the body is cloned rather than after.
pub(super) fn region_free_reads(
    body: &Body,
    block: BlockId,
    callees: Option<&CalleeMap>,
    ctfe_builtins: Option<&CtfeBuiltinMap>,
    type_table: &TypeTable,
) -> Option<Vec<u32>> {
    fn record_write(body: &Body, op: Operand, written: &mut LocalSet) -> Option<()> {
        written.insert(write_root_local(body, op)?);
        Some(())
    }
    let mut declared = LocalSet::default();
    let mut seen = LocalSet::default();
    let mut mentioned: Vec<(u32, TypeId)> = Vec::new();
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
            NodeRef::Expr(e) => match &body.exprs[e].kind {
                ExprKind::GlobalVarSet { .. }
                | ExprKind::IndirectCall { .. }
                | ExprKind::CmRawCall { .. } => return None,
                ExprKind::Call { func_id, args, .. } => {
                    // A builtin never reaches NIR as a method call, so this is
                    // the only shape that may be one instead of a callee.
                    match callees.and_then(|m| m.get(func_id)) {
                        Some(callee) => {
                            let site = CallSite::of(body, e)?;
                            write_targets(body, &site, callee, &mut written)?;
                        }
                        None => {
                            let builtin = ctfe_builtins.and_then(|m| m.get(func_id))?;
                            if builtin.is_write() {
                                let target = args.first()?;
                                record_write(body, target.expr, &mut written)?;
                            }
                        }
                    }
                }
                ExprKind::MethodCall { func_id, .. } => {
                    let callee = callees.and_then(|m| m.get(func_id))?;
                    let site = CallSite::of(body, e)?;
                    write_targets(body, &site, callee, &mut written)?;
                }
                ExprKind::Local { index, .. } => {
                    if seen.insert(*index) {
                        mentioned.push((*index, body.exprs[e].type_id));
                    }
                }
                ExprKind::Assign { target, .. } => {
                    record_write(body, Operand::Expr(*target), &mut written)?;
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr,
                } => {
                    record_write(body, *expr, &mut written)?;
                }
                _ => {}
            },
            NodeRef::Block(_) => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    let mut out = Vec::new();
    for (index, ty) in mentioned {
        if declared.contains(index) {
            continue;
        }
        if written.contains(index) || RefKind::from_resolved(type_table.get(ty)).is_some() {
            return None;
        }
        out.push(index);
    }
    Some(out)
}
