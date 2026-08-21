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

    // Every block of the body, not a walk from the root: an assignment can sit
    // in a block expression nested in an operand, which no statement-level walk
    // reaches.
    let blocks: Vec<BlockId> = body.blocks.keys().collect();

    let mut writes: IndexMap<u32, usize> = IndexMap::default();
    for &block in &blocks {
        for &sid in &body.blocks[block].stmts {
            if let Some(index) = assigned_local(body, sid) {
                *writes.entry(index).or_insert(0) += 1;
            }
        }
    }

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
    let mut changed = false;
    for &block in &blocks {
        let stmts = body.blocks[block].stmts.clone();
        for sid in stmts {
            let Some(index) = assigned_local(body, sid) else {
                continue;
            };
            if !eligible.contains(&index) {
                continue;
            }
            let StmtKind::Expr(Operand::Expr(eid)) = &body.stmts[sid].kind else {
                continue;
            };
            let ExprKind::Assign { value, .. } = &body.exprs[*eid].kind else {
                continue;
            };
            let value = value.clone();
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
    }
    changed
}

/// The local a statement assigns, when the statement is a bare write to one.
fn assigned_local(body: &Body, sid: crate::nir_arena::StmtId) -> Option<u32> {
    let StmtKind::Expr(Operand::Expr(eid)) = &body.stmts[sid].kind else {
        return None;
    };
    let ExprKind::Assign { target, .. } = &body.exprs[*eid].kind else {
        return None;
    };
    match &body.exprs[*target].kind {
        ExprKind::Local { index, .. } => Some(*index),
        _ => None,
    }
}
