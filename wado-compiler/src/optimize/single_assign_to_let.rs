//! A local written exactly once becomes a binding. A producer that preallocates
//! a slot and assigns it later — a power-assert capture, an optimizer pass
//! minting a temporary — otherwise hides a constant behind a mutable cell, and
//! const folding, field-read elision and bounds-check versioning all stop there.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::NirFunction;
use crate::nir_arena::{BlockId, Body, ExprKind, Operand, StmtKind};
use crate::nir_package::NirPackage;

pub fn promote_single_assignments(
    project: &mut NirPackage,
    gate: &mut super::gate::FunctionGate,
) -> bool {
    use cranelift_entity::EntityRef;

    let len = project.functions.len();
    gate.run_gated(super::gate::GatedPass::SingleAssignToLet, len, |fid| {
        promote_in_function(&mut project.functions[fid.index()].borrow_mut())
    })
}

fn promote_in_function(func: &mut NirFunction) -> bool {
    let Some(body) = func.body.as_mut() else {
        return false;
    };

    let mut writes: IndexMap<u32, usize> = IndexMap::default();
    count_writes(body, body.root, false, &mut writes);

    let param_count = u32::try_from(func.params.len()).unwrap_or(u32::MAX);
    let eligible: IndexSet<u32> = writes
        .iter()
        .filter(|(idx, count)| {
            **count == 1
                && **idx >= param_count
                && !func.address_taken_locals.contains(*idx)
                && !func.stores_aliased_locals.contains(*idx)
        })
        .map(|(idx, _)| *idx)
        .collect();
    if eligible.is_empty() {
        return false;
    }

    let locals = func.locals.clone();
    rewrite(body, body.root, &eligible, &locals)
}

/// Count writes to a bare `Local`. A write under a `Loop` runs more than once,
/// so it counts as many.
fn count_writes(body: &Body, block: BlockId, in_loop: bool, writes: &mut IndexMap<u32, usize>) {
    for &sid in &body.blocks[block].stmts {
        if let StmtKind::Expr(Operand::Expr(eid)) = &body.stmts[sid].kind
            && let ExprKind::Assign { target, .. } = &body.exprs[*eid].kind
            && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
        {
            *writes.entry(*index).or_insert(0) += if in_loop { 2 } else { 1 };
        }
        for child in child_blocks(body, sid) {
            let inner = in_loop || matches!(body.stmts[sid].kind, StmtKind::Loop { .. });
            count_writes(body, child, inner, writes);
        }
    }
}

fn child_blocks(body: &Body, sid: crate::nir_arena::StmtId) -> Vec<BlockId> {
    match &body.stmts[sid].kind {
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => else_block.map_or_else(|| vec![*then_block], |e| vec![*then_block, e]),
        StmtKind::Loop { body: b } => vec![*b],
        StmtKind::LabeledBlock { block, .. } => vec![*block],
        _ => Vec::new(),
    }
}

fn rewrite(
    body: &mut Body,
    block: BlockId,
    eligible: &IndexSet<u32>,
    locals: &[crate::nir::NirLocal],
) -> bool {
    let mut changed = false;
    let stmts = body.blocks[block].stmts.clone();
    for sid in stmts {
        let promoted = match &body.stmts[sid].kind {
            StmtKind::Expr(Operand::Expr(eid)) => match &body.exprs[*eid].kind {
                ExprKind::Assign { target, value } => match &body.exprs[*target].kind {
                    ExprKind::Local { index, .. } if eligible.contains(index) => {
                        Some((*index, value.clone()))
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        if let Some((index, value)) = promoted {
            let local = &locals[index as usize];
            body.stmts[sid].kind = StmtKind::Let {
                name: local.name.clone(),
                local_index: index,
                is_mut: false,
                is_reactive: false,
                type_id: local.type_id,
                value,
                skip_value_copy: false,
            };
            changed = true;
        }
        for child in child_blocks(body, sid) {
            changed |= rewrite(body, child, eligible, locals);
        }
    }
    changed
}
