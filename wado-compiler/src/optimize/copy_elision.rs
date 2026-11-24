//! Elide a `$value_copy$T(src)` that nothing can tell from `src`: the copy's
//! objects are allocated in this body and stay in it, and nothing in the body
//! writes an object of a type `T` reaches on either side. The two then share
//! storage, which the memory model allows.
//!
//! Lowering decides a copy per binding, so a value read into an aggregate the
//! body builds is copied whatever happens to it next. Here the copy is judged
//! in the body it lands in, after inlining has brought the aggregate's readers
//! into view: the inliner keeps a copy helper whole until the loop first
//! converges, so the call is still there to remove.

use cranelift_entity::EntityRef;

use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand};
use crate::nir_package::NirPackage;

use super::gate::{FunctionGate, GatedPass};
use super::heap_effect::{HeapEffects, HeapEffectsCache, HeapFrame};

/// Returns whether any copy was elided.
pub fn elide_value_copies(
    project: &mut NirPackage,
    gate: &mut FunctionGate,
    heap: &mut HeapEffectsCache,
) -> bool {
    let value_copies: Vec<bool> = project
        .functions
        .iter()
        .map(|f| f.borrow().is_value_copy())
        .collect();
    let type_table = project.type_table.borrow();
    let effects = heap.effects(project, &type_table, gate.exec());
    let functions = &project.functions;
    gate.run_gated_par(GatedPass::CopyElision, functions.len(), |_, fid| {
        if value_copies[fid.index()] {
            return false;
        }
        let mut func = functions[fid.index()].borrow_mut();
        let params: Vec<u32> = func.params.iter().map(|p| p.local_index).collect();
        let Some(body) = func.body.as_mut() else {
            return false;
        };
        let sites = elidable_copies(body, &effects, &params, &value_copies);
        for &(parent, call, src) in &sites {
            assert!(body.replace_operand_to(parent, call, src));
        }
        !sites.is_empty()
    })
}

/// Each copy call the body may drop, as its parent, the call, and its source.
fn elidable_copies(
    body: &Body,
    effects: &HeapEffects,
    params: &[u32],
    value_copies: &[bool],
) -> Vec<(NodeRef, ExprId, Operand)> {
    let mut copies = Vec::new();
    body.for_each_reachable_node(|parent| {
        body.for_each_operand(parent, |op| {
            if let Operand::Expr(call) = op
                && let ExprKind::Call { func_id, args, .. } = &body.exprs[call].kind
                && value_copies[func_id.index()]
            {
                let [arg] = args.split().1 else {
                    unreachable!("a value copy takes one argument");
                };
                copies.push((parent, call, arg.expr));
            }
        });
    });
    if copies.is_empty() {
        return copies;
    }
    let frame = HeapFrame::new(effects, body, params);
    copies.retain(|&(_, call, src)| {
        let copy = Operand::Expr(call);
        let keys = effects.reach(body.exprs[call].type_id);
        frame.operand_confined(effects, body, copy)
            && !frame.operand_written(effects, body, &keys, copy)
            && !frame.operand_written(effects, body, &keys, src)
    });
    copies
}
