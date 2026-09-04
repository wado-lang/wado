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
/// value. A block leaving nothing on the stack has no value to fold to,
/// whatever its last statement computed, and the type is what says so. Unit is
/// the inlined statement call, whose result stands where the program expects
/// none; never is the `else` of a `let ... else { panic("…") }`, which builds a
/// constant message and then diverges — a block whose fold was never available,
/// rather than one the engine missed.
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
        ExprKind::Block(b) => Some((*b, None)),
        ExprKind::LabeledBlock { block, label, .. } => Some((*block, Some(label.as_str()))),
        _ => None,
    }
}

/// The global a block materializes: exactly `{ G = v; G }`, the second
/// statement reading what the first wrote. The shape
/// [constant-object globalization] leaves where it names a constant at a use
/// site.
///
/// Two consumers, one recognizer. A store in this shape serves the read below it
/// and nothing else, so a region carrying one may still run; and the block
/// itself is not a region, since folding it would write the literal over the
/// naming construct and undo the sharing globalization arranged.
///
/// [constant-object globalization]: ../docs/wep-2026-05-31-const-object-globalization.md
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

/// What a frame would need to run `block`: the outer locals it only reads, and
/// the first fact that disqualifies it, if any. A write is an `Assign` target, a
/// `&mut` borrow, or a `&mut` parameter per the callee's signature — the
/// signature being the only reliable witness.
///
/// The walk finishes even once a refusal is found, because the two answers are
/// independent and a caller may want either. Returning on the first refusal
/// hides the reads discovered after it, and a block reading a runtime local was
/// never a constant whatever else is wrong with it.
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
    let mut mentioned: Vec<(u32, TypeId)> = Vec::new();
    let mut promoted: Vec<u32> = Vec::new();
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
                        refusal = refusal.or(Some(RegionRefusal::GlobalWrite));
                    }
                }
                ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => {
                    refusal = refusal.or(Some(RegionRefusal::OpaqueCall));
                }
                ExprKind::Call { func_id, args, .. } => {
                    // A builtin never reaches NIR as a method call, so this is
                    // the only shape that may be one instead of a callee.
                    if let Some(callee) = facts.callees.and_then(|m| m.get(func_id)) {
                        let accounted = CallSite::of(body, e)
                            .and_then(|site| write_targets(body, &site, callee, &mut written));
                        if accounted.is_none() {
                            refusal = refusal.or(Some(RegionRefusal::UnaccountableWrite));
                        }
                    } else {
                        match facts.ctfe_builtins.and_then(|m| m.get(func_id)) {
                            None => {
                                refusal = refusal.or(Some(RegionRefusal::UnrunnableCall));
                            }
                            Some(builtin) if builtin.is_write() => {
                                let written_place = args
                                    .first()
                                    .and_then(|t| record_write(body, t.expr, &mut written));
                                if written_place.is_none() {
                                    refusal = refusal.or(Some(RegionRefusal::UnaccountableWrite));
                                }
                            }
                            Some(_) => {}
                        }
                    }
                }
                ExprKind::Local { index, .. } => {
                    if seen.insert(*index) {
                        mentioned.push((*index, body.exprs[e].type_id));
                    }
                }
                ExprKind::Assign { target, .. } => {
                    if record_write(body, Operand::Expr(*target), &mut written).is_none() {
                        refusal = refusal.or(Some(RegionRefusal::UnaccountableWrite));
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr,
                } => {
                    if record_write(body, *expr, &mut written).is_none() {
                        refusal = refusal.or(Some(RegionRefusal::UnaccountableWrite));
                    }
                }
                _ => {}
            },
            NodeRef::Block(_) => {}
        }
        // A promoted operand is not a child, so the walk above never reaches
        // the locals its value names. Missing them does not mis-fold — the
        // frame seeds nothing for a local it never heard of, so the run
        // abandons — but it makes the block look self-contained when it reads
        // the program's runtime state, and a remark believing that reports a
        // constant that was never one.
        body.for_each_operand(node, |op| {
            let Operand::Value(v) = op else {
                return;
            };
            let mut named = crate::hashmap::IndexSet::default();
            body.values.collect_opaque_locals(v, &mut named);
            for index in named {
                if seen.insert(index) {
                    promoted.push(index);
                }
            }
        });
        body.for_each_child(node, |c| stack.push(c));
    }
    let mut out = Vec::new();
    for (index, ty) in mentioned {
        if declared.contains(index) {
            continue;
        }
        if written.contains(index) {
            refusal = refusal.or(Some(RegionRefusal::OuterWrite));
            continue;
        }
        out.push(FreeRead {
            index,
            is_reference: type_table.is_reference_shaped(ty),
        });
    }
    // A local reached only through a pool value carries no skeleton node to
    // read a type off, and seeding one whose shape the frame cannot check
    // would hand a value where the program holds an alias. Refused as a
    // reference, which is what a seed of unknown shape is worth.
    for index in promoted {
        if declared.contains(index) {
            continue;
        }
        if written.contains(index) {
            refusal = refusal.or(Some(RegionRefusal::OuterWrite));
            continue;
        }
        out.push(FreeRead {
            index,
            is_reference: true,
        });
    }
    RegionNeeds {
        free_reads: out,
        refusal,
    }
}

/// What running `block` as a frame would take. A region with no refusal and no
/// free reads depends on nothing outside itself, so it denotes a constant.
pub(super) struct RegionNeeds {
    pub(super) free_reads: Vec<FreeRead>,
    pub(super) refusal: Option<RegionRefusal>,
}

/// A local a region reads without declaring, and whether it names a reference.
/// Seeding a reference is sound only where the frame already reads it as a
/// constant, which only the caller, holding the environment, knows.
pub(super) struct FreeRead {
    pub(super) index: u32,
    pub(super) is_reference: bool,
}
