//! Which blocks are self-contained enough to run as a frame: one that builds a
//! value in locals of its own, touches only those, and yields the result — as
//! self-contained as a call body written inline, which is what lets a constant
//! string template fold to a literal. Asked before the run, since the run copies
//! the whole enclosing body while this only walks the block.

use crate::name::RefKind;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, LocalSet, NodeRef, Operand, PatKind, StmtKind,
};
use crate::tir::{TypeId, TypeTable};

use super::ProgramFacts;
use super::callee::{CallSite, Callee};
use super::place::write_root_local;

/// Record the local each `&mut` parameter of `site` writes. `None` when the
/// site does not match the signature, or when no local roots a write's place.
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
/// value. One leaving nothing on the stack has none to fold to, whatever its
/// last statement computed: `unit` is an inlined statement call, `never` the
/// `else` of a `let ... else { panic("…") }`.
pub(super) fn value_block_shape<'a>(
    body: &'a Body,
    e: ExprId,
    type_table: &TypeTable,
) -> Option<(BlockId, Option<&'a str>)> {
    if type_table.is_stackless(body.exprs[e].type_id) {
        return None;
    }
    block_shape(body, e)
}

/// The block a `Block` / `LabeledBlock` expression holds, whatever its type.
/// What a statement-position block runs, where no value is wanted and a unit
/// type is the ordinary case.
pub(super) fn block_shape(body: &Body, e: ExprId) -> Option<(BlockId, Option<&str>)> {
    match &body.exprs[e].kind {
        ExprKind::LabeledBlock { block, label, .. } => Some((*block, Some(label.as_str()))),
        _ => None,
    }
}

/// The global a block materializes: exactly `{ G = v; G }`, the shape
/// constant-object globalization leaves where it names a constant at a use site.
///
/// One recognizer for two consumers, which must agree: the store inside the pair
/// does not refuse the region around it, and the pair itself is not a region.
#[must_use]
pub fn materialization_pair(body: &Body, block: BlockId) -> Option<super::GlobalKey> {
    let [set, get] = body.blocks[block].stmts.as_slice() else {
        return None;
    };
    let (StmtKind::Expr(set), StmtKind::Expr(get)) =
        (&body.stmts[*set].kind, &body.stmts[*get].kind)
    else {
        return None;
    };
    let ExprKind::GlobalVarSet {
        module_source,
        name,
        ..
    } = &body.exprs[set.as_expr()?].kind
    else {
        return None;
    };
    let read = global_mention(body, get.as_expr()?)?;
    (read == (module_source.clone(), name.clone())).then_some(read)
}

/// The global an expression names, whether it reads or writes it.
#[must_use]
pub fn global_mention(body: &Body, e: ExprId) -> Option<super::GlobalKey> {
    match &body.exprs[e].kind {
        ExprKind::GlobalVarGet {
            module_source,
            name,
        }
        | ExprKind::GlobalVarSet {
            module_source,
            name,
            ..
        } => Some((module_source.clone(), name.clone())),
        _ => None,
    }
}

/// The value block behind a region-shaped expression: enough statements to be
/// worth running, ending on a statement that produces the value.
///
/// A single-statement block is the lattice projection's case. Refusing it here
/// is what keeps the attempt cheap on the blocks that are not regions. A
/// materialization is refused for the opposite reason: it would fold, and the
/// fold is the loss.
pub(super) fn region_shape<'a>(
    body: &'a Body,
    e: ExprId,
    type_table: &TypeTable,
) -> Option<(BlockId, Option<&'a str>)> {
    let (block, label) = value_block_shape(body, e, type_table)?;
    let stmts = &body.blocks[block].stmts;
    if stmts.len() < 2 {
        return None;
    }
    if materialization_pair(body, block).is_some() {
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

/// Why a block cannot run as a region frame. The fold needs only that it cannot;
/// a remark on a region that survived needs to say which fact stopped it.
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
            Self::UnaccountableWrite => "it writes a place that roots in no local",
            Self::OuterWrite => "it writes a local it does not own",
        }
    }
}

/// What a frame would need to run `block`: the outer locals it only reads, and
/// the first fact that disqualifies it. A write is an `Assign` target, a `&mut`
/// borrow, or a `&mut` parameter per the callee's signature, the signature being
/// the only reliable witness.
///
/// The walk finishes past a refusal, since the two answers are independent:
/// stopping there would hide the reads after it, and a block reading a runtime
/// local was never a constant whatever else is wrong with it.
pub(super) fn region_needs(
    body: &Body,
    block: BlockId,
    facts: ProgramFacts<'_>,
    type_table: &TypeTable,
) -> RegionNeeds {
    fn record_write(body: &Body, op: Operand, written: &mut LocalSet) -> Option<()> {
        written.insert(write_root_local(body, op)?);
        Some(())
    }
    let mut refusal: Option<RegionRefusal> = None;
    let mut declared = LocalSet::default();
    let mut seen = LocalSet::default();
    let mut mentioned: Vec<(u32, Option<TypeId>)> = Vec::new();
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
                        refusal.get_or_insert(RegionRefusal::GlobalWrite);
                    }
                }
                ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => {
                    refusal.get_or_insert(RegionRefusal::OpaqueCall);
                }
                ExprKind::Call { func_id, args, .. } => {
                    // A builtin never reaches NIR as a method call, so this is
                    // the only shape that may be one instead of a callee.
                    if let Some(callee) = facts.callees.and_then(|m| m.get(func_id)) {
                        let accounted = CallSite::of(body, e)
                            .and_then(|site| write_targets(body, &site, callee, &mut written));
                        if accounted.is_none() {
                            refusal.get_or_insert(RegionRefusal::UnaccountableWrite);
                        }
                    } else {
                        match facts.ctfe_builtins.and_then(|m| m.get(func_id)) {
                            None => {
                                refusal.get_or_insert(RegionRefusal::UnrunnableCall);
                            }
                            Some(builtin) if builtin.is_write() => {
                                let written_place = args
                                    .first()
                                    .and_then(|t| record_write(body, t.expr, &mut written));
                                if written_place.is_none() {
                                    refusal.get_or_insert(RegionRefusal::UnaccountableWrite);
                                }
                            }
                            Some(_) => {}
                        }
                    }
                }
                ExprKind::Local { index, .. } => {
                    if seen.insert(*index) {
                        mentioned.push((*index, Some(body.exprs[e].type_id)));
                    }
                }
                ExprKind::Assign { target, .. } => {
                    if record_write(body, Operand::Expr(*target), &mut written).is_none() {
                        refusal.get_or_insert(RegionRefusal::UnaccountableWrite);
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr,
                } => {
                    if record_write(body, *expr, &mut written).is_none() {
                        refusal.get_or_insert(RegionRefusal::UnaccountableWrite);
                    }
                }
                _ => {}
            },
            NodeRef::Block(_) => {}
        }
        // A promoted operand is not a child, so the walk above never reaches
        // the locals its value names. Missing them makes a block look
        // self-contained while it reads the program's runtime state.
        body.for_each_operand(node, |op| {
            let Operand::Value(v) = op else {
                return;
            };
            let mut visited = crate::hashmap::IndexSet::default();
            body.values
                .for_each_opaque_local(v, &mut visited, |index, leaf| {
                    if seen.insert(index) {
                        mentioned.push((index, body.values.type_of(leaf)));
                    }
                });
        });
        body.for_each_child(node, |c| stack.push(c));
    }
    let mut out = Vec::new();
    let mut writes_outer = false;
    for (index, ty) in mentioned {
        if declared.contains(index) {
            continue;
        }
        if written.contains(index) {
            writes_outer = true;
            refusal.get_or_insert(RegionRefusal::OuterWrite);
            continue;
        }
        out.push(FreeRead {
            index,
            // A pool leaf without a recorded type counts as writable: the
            // frame cannot check its shape.
            writable_reference: ty.is_none_or(|ty| writable_reference(ty, type_table)),
        });
    }
    RegionNeeds {
        free_reads: out,
        refusal,
        writes_outer,
    }
}

/// What running `block` as a frame would take. A region with no refusal and no
/// free reads depends on nothing outside itself, so it denotes a constant.
pub(super) struct RegionNeeds {
    pub(super) free_reads: Vec<FreeRead>,
    pub(super) refusal: Option<RegionRefusal>,
    /// Whether an outer local is written. `refusal` may name an earlier fact
    /// instead, since the walk keeps the first it meets.
    pub(super) writes_outer: bool,
}

/// A local a region reads without declaring, and whether the mention names a
/// reference the program can write through. Seeding one is sound only where
/// the frame already reads it as a constant, which only the caller, holding
/// the environment, knows.
pub(super) struct FreeRead {
    pub(super) index: u32,
    pub(super) writable_reference: bool,
}

/// Whether `ty` is a reference something can write through: `&mut`, or a box
/// not known to stand for a shared borrow. A shared borrow only reads.
fn writable_reference(ty: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        RefKind::from_resolved(type_table.get(ty)),
        Some(RefKind::Mut)
    ) || type_table.is_mut_box(ty)
}
