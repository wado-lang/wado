//! Which blocks are self-contained enough to run as a frame: one that builds a
//! value in locals of its own, touches only those, and yields the result — as
//! self-contained as a call body written inline, which is what lets a constant
//! string template fold to a literal. Asked before the run, since the run copies
//! the whole enclosing body while this only walks the block.

use crate::nir::NirUnaryOp;
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, LocalSet, NodeRef, Operand, PatKind, StmtKind,
};
use crate::tir::{TypeId, TypeTable};

use super::ProgramFacts;
use super::callee::{CallSite, Callee};
use super::place::write_root_local;

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
    block_shape(body, e)
}

/// The block a `Block` / `LabeledBlock` expression holds, whatever its type.
/// What a statement-position block runs, where no value is wanted and a unit
/// type is the ordinary case.
pub(super) fn block_shape(body: &Body, e: ExprId) -> Option<(BlockId, Option<&str>)> {
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

/// Why a block cannot run as a region frame. The fold only needs to know that
/// it cannot; a remark reporting a region that survived to the final IR needs
/// to say which fact stopped it, so the answer carries the reason rather than
/// collapsing to an absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionRefusal {
    /// A write to a module-scope global, whose lifetime outlives the frame.
    /// A store that only materializes a value for its own reader is not one.
    GlobalWrite,
    /// A call through a closure or across the CM boundary — no body to run.
    OpaqueCall,
    /// A call to a function a compile-time frame cannot run: impure, generic,
    /// async, or bodiless.
    UnrunnableCall,
    /// A write whose place roots in no local, so the frame cannot say what it
    /// lands on. Unaccountable, not absent.
    UnaccountableWrite,
    /// A write to a local declared outside the block, which the frame does not
    /// own.
    OuterWrite,
}

impl RegionRefusal {
    /// The refusal as a remark reads it, completing "this block computes a
    /// constant at run time: …".
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::GlobalWrite => "it writes a global",
            Self::OpaqueCall => "it calls through a closure or the component boundary",
            Self::UnrunnableCall => "it calls a function the engine cannot run",
            Self::UnaccountableWrite => "it writes a place no local roots",
            Self::OuterWrite => "it writes a local it does not own",
        }
    }
}

/// The outer locals `block` only reads — the seeds a region frame needs — or the
/// fact that disqualifies it. A write is an `Assign` target, a `&mut` borrow, or
/// a `&mut` parameter per the callee's signature — the signature being the only
/// reliable witness.
pub(super) fn region_free_reads(
    body: &Body,
    block: BlockId,
    facts: ProgramFacts<'_>,
    type_table: &TypeTable,
) -> Result<Vec<FreeRead>, RegionRefusal> {
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
                ExprKind::GlobalVarSet {
                    module_source,
                    name,
                    ..
                } => {
                    let key = (module_source.clone(), name.clone());
                    if !facts.materializes(&key) {
                        return Err(RegionRefusal::GlobalWrite);
                    }
                }
                ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => {
                    return Err(RegionRefusal::OpaqueCall);
                }
                ExprKind::Call { func_id, args, .. } => {
                    // A builtin never reaches NIR as a method call, so this is
                    // the only shape that may be one instead of a callee.
                    if let Some(callee) = facts.callees.and_then(|m| m.get(func_id)) {
                        let site =
                            CallSite::of(body, e).ok_or(RegionRefusal::UnaccountableWrite)?;
                        write_targets(body, &site, callee, &mut written)
                            .ok_or(RegionRefusal::UnaccountableWrite)?;
                    } else if facts
                        .ctfe_builtins
                        .and_then(|m| m.get(func_id))
                        .ok_or(RegionRefusal::UnrunnableCall)?
                        .is_write()
                    {
                        let target = args.first().ok_or(RegionRefusal::UnaccountableWrite)?;
                        record_write(body, target.expr, &mut written)
                            .ok_or(RegionRefusal::UnaccountableWrite)?;
                    }
                }
                ExprKind::Local { index, .. } => {
                    if seen.insert(*index) {
                        mentioned.push((*index, body.exprs[e].type_id));
                    }
                }
                ExprKind::Assign { target, .. } => {
                    record_write(body, Operand::Expr(*target), &mut written)
                        .ok_or(RegionRefusal::UnaccountableWrite)?;
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr,
                } => {
                    record_write(body, *expr, &mut written)
                        .ok_or(RegionRefusal::UnaccountableWrite)?;
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
        if written.contains(index) {
            return Err(RegionRefusal::OuterWrite);
        }
        out.push(FreeRead {
            index,
            is_reference: type_table.is_reference_shaped(ty),
        });
    }
    Ok(out)
}

/// A local a region reads without declaring, and whether it names a reference.
/// Seeding a reference is sound only where the frame already reads it as a
/// constant, which only the caller, holding the environment, knows.
pub(super) struct FreeRead {
    pub(super) index: u32,
    pub(super) is_reference: bool,
}
