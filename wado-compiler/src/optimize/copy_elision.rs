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
//!
//! A copy that leaves only through the result is the caller's to judge. Such a
//! function gets a `$lend` clone without those copies, and a call moves to the
//! clone where its result holds what a copy would: its objects stay in the
//! caller, and nothing there writes the lent parts, through the result or the
//! argument they came from.

use cranelift_entity::EntityRef;

use crate::ast::Visibility;
use crate::hashmap::IndexMap;
use crate::name::{self, LocalMethodName};
use crate::nir::{FuncCell, FuncId, FunctionRef};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand};
use crate::nir_package::{LentParts, NirPackage};

use super::arena_query::holds_reference;
use super::gate::{FunctionGate, GatedPass};
use super::heap_effect::{HeapEffects, HeapEffectsCache, HeapFrame, params_of_bit};

/// Returns whether any copy was elided or any call lent to.
pub fn elide_value_copies(
    project: &mut NirPackage,
    gate: &mut FunctionGate,
    heap: &mut HeapEffectsCache,
) -> bool {
    let value_copies = project.value_copy_functions();
    let elided = {
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
    };
    let lent = lend_returned_parts(project, gate, heap, &value_copies);
    elided || lent
}

/// A copy call a body may drop, as its parent, the call, and its source.
type Site = (NodeRef, ExprId, Operand);

/// Each value copy call in `body`.
fn copy_calls(body: &Body, value_copies: &[bool]) -> Vec<Site> {
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
    copies
}

/// Whether nothing in the body tells the copy at `site` from its source,
/// where the copy's objects stay in the body or, with `or_returned`, leave it
/// only through its result.
fn copy_unobserved(
    frame: &HeapFrame,
    effects: &HeapEffects,
    body: &Body,
    (_, call, src): Site,
    or_returned: bool,
) -> bool {
    let copy = Operand::Expr(call);
    let keys = effects.reach(body.exprs[call].type_id);
    frame.operand_confined(effects, body, copy, or_returned)
        && !frame.operand_written(effects, body, &keys, copy)
        && !frame.operand_written(effects, body, &keys, src)
}

/// Each copy call the body may drop.
fn elidable_copies(
    body: &Body,
    effects: &HeapEffects,
    params: &[u32],
    value_copies: &[bool],
) -> Vec<Site> {
    let mut copies = copy_calls(body, value_copies);
    if copies.is_empty() {
        return copies;
    }
    let frame = HeapFrame::new(effects, body, params);
    copies.retain(|&site| copy_unobserved(&frame, effects, body, site, false));
    copies
}

/// The copy calls a caller may have the body drop, which hand their objects
/// to the result, and where the result then shares them with the arguments.
/// A source that may hold what a global does lends nothing: the caller cannot
/// see that global's writes.
fn lendable_copies(
    body: &Body,
    effects: &HeapEffects,
    params: &[u32],
    value_copies: &[bool],
) -> Option<(Vec<Site>, LentParts)> {
    let mut copies = copy_calls(body, value_copies);
    if copies.is_empty() {
        return None;
    }
    let frame = HeapFrame::new(effects, body, params);
    let mut result_depth = usize::MAX;
    let mut param_depths: IndexMap<usize, usize> = IndexMap::default();
    copies.retain(|&site @ (_, call, src)| {
        let Some(depth) = frame.returned_at(effects, body, Operand::Expr(call)) else {
            return false;
        };
        if !frame.operand_off_globals(effects, body, src)
            || !copy_unobserved(&frame, effects, body, site, true)
        {
            return false;
        }
        result_depth = result_depth.min(depth);
        for (param, below) in frame.operand_sources(effects, body, src) {
            let least = param_depths.entry(param).or_insert(below);
            *least = (*least).min(below);
        }
        true
    });
    (!copies.is_empty()).then(|| {
        let parts = LentParts {
            result_depth,
            param_depths: param_depths.into_iter().collect(),
        };
        (copies, parts)
    })
}

/// A function some call may lend from: the clone standing for it, or the
/// copies a clone would drop; and what the clone lends.
enum Lender {
    Minted(FuncId, LentParts),
    Sites(Vec<Site>, LentParts),
}

impl Lender {
    fn parts(&self) -> &LentParts {
        match self {
            Lender::Minted(_, parts) | Lender::Sites(_, parts) => parts,
        }
    }
}

/// Move each call whose caller can take the callee's returned parts lent to
/// the callee's `$lend` clone, minting the clones those calls need.
fn lend_returned_parts(
    project: &mut NirPackage,
    gate: &mut FunctionGate,
    heap: &mut HeapEffectsCache,
    value_copies: &[bool],
) -> bool {
    let (lenders, lent) = {
        let type_table = project.type_table.borrow();
        let effects = heap.effects(project, &type_table, gate.exec());
        let functions = &project.functions;
        let indices: Vec<usize> = (0..functions.len()).collect();
        // A function may lend where it has a body returning storage, and is
        // neither a copy helper nor a clone that lends already. A recursive one
        // lends nothing: its clone still calls it, so splicing the clone into a
        // caller uncovers a call lending would move to the clone again, a round
        // at a time.
        let recursive = effects.recursive_functions();
        assert_eq!(
            recursive.len(),
            functions.len(),
            "the call graph covers every function"
        );
        let mut lenders: Vec<Option<Lender>> = Vec::with_capacity(indices.len());
        let mut candidates = Vec::new();
        for &fi in &indices {
            let f = functions[fi].borrow();
            let may_lend = !value_copies[fi]
                && !recursive[fi]
                && f.body.is_some()
                && !project.lending_clones.contains_key(&FuncId::new(fi))
                && holds_reference(&type_table, f.return_type);
            let mut clone = FunctionRef::from_resolved(&f, f.module_source.clone());
            rename_for_lending(&mut clone.name, &mut clone.method_info);
            let minted = may_lend.then(|| project.func_id_of(&clone)).flatten();
            lenders.push(minted.map(|id| Lender::Minted(id, project.lending_clones[&id].clone())));
            if may_lend && minted.is_none() {
                candidates.push(fi);
            }
        }
        let lendable = gate.exec().map(&candidates, |&fi| {
            let f = functions[fi].borrow();
            let params: Vec<u32> = f.params.iter().map(|p| p.local_index).collect();
            let body = f.body.as_ref().expect("a candidate has a body");
            lendable_copies(body, &effects, &params, value_copies)
        });
        for (fi, lendable) in candidates.into_iter().zip(lendable) {
            lenders[fi] = lendable.map(|(sites, parts)| Lender::Sites(sites, parts));
        }
        let lent = gate.exec().map(&indices, |&fi| {
            if value_copies[fi] {
                return Vec::new();
            }
            let f = functions[fi].borrow();
            let Some(body) = f.body.as_ref() else {
                return Vec::new();
            };
            let params: Vec<u32> = f.params.iter().map(|p| p.local_index).collect();
            lent_calls(body, &effects, &params, &lenders)
        });
        (lenders, lent)
    };
    let mut clones: IndexMap<FuncId, FuncId> = IndexMap::default();
    let mut changed = false;
    for (fi, calls) in lent.into_iter().enumerate() {
        if calls.is_empty() {
            continue;
        }
        let mut retargets = Vec::with_capacity(calls.len());
        for (call, callee) in calls {
            let clone = *clones
                .entry(callee)
                .or_insert_with(|| match &lenders[callee.index()] {
                    Some(Lender::Minted(id, _)) => *id,
                    Some(Lender::Sites(sites, parts)) => {
                        mint_lending_clone(project, callee, sites, parts.clone())
                    }
                    None => unreachable!("a lent call's callee lends"),
                });
            retargets.push((call, clone));
        }
        let mut f = project.functions[fi].borrow_mut();
        let body = f.body.as_mut().expect("a caller has a body");
        for (call, clone) in retargets {
            let ExprKind::Call { func_id, .. } = &mut body.exprs[call].kind else {
                unreachable!("a lent call is a call");
            };
            *func_id = clone;
        }
        drop(f);
        gate.mark_changed(FuncId::new(fi));
        changed = true;
    }
    changed
}

/// Each call in `body` whose callee lends, and whose result the body may take
/// lent: its objects stay in the body, and nothing in the body writes the lent
/// parts, through the result or the argument they came from.
fn lent_calls(
    body: &Body,
    effects: &HeapEffects,
    params: &[u32],
    lenders: &[Option<Lender>],
) -> Vec<(ExprId, FuncId)> {
    let mut calls = Vec::new();
    body.for_each_reachable_node(|node| {
        if let NodeRef::Expr(e) = node
            && let ExprKind::Call { func_id, .. } = &body.exprs[e].kind
            && lenders[func_id.index()].is_some()
        {
            calls.push((e, *func_id));
        }
    });
    if calls.is_empty() {
        return calls;
    }
    let frame = HeapFrame::new(effects, body, params);
    calls.retain(|&(call, callee)| {
        let ExprKind::Call { args, .. } = &body.exprs[call].kind else {
            unreachable!("a lent call is a call");
        };
        let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
        let parts = lenders[callee.index()]
            .as_ref()
            .expect("a lent call's callee lends")
            .parts();
        let result = Operand::Expr(call);
        let keys = effects.reach(body.exprs[call].type_id);
        let written = |op, depth| frame.operand_written_below(effects, body, &keys, op, depth);
        !frame.operand_stored_outside(effects, body, result)
            && !written(result, parts.result_depth)
            && parts.param_depths.iter().all(|&(bit, depth)| {
                params_of_bit(bit, args.len()).all(|j| !written(args[j], depth))
            })
    });
    calls
}

/// A function's identity renamed for its `$lend` clone: its name, and a
/// method's name, which is what keys a method.
fn rename_for_lending(name: &mut String, method_info: &mut Option<LocalMethodName>) {
    *name = name::lending_clone_name(name);
    if let Some(info) = method_info {
        info.method_name = name::lending_clone_name(&info.method_name);
    }
}

/// A clone of `origin` with the copies at `sites` dropped, which lends `parts`.
fn mint_lending_clone(
    project: &mut NirPackage,
    origin: FuncId,
    sites: &[Site],
    parts: LentParts,
) -> FuncId {
    let mut clone = project.functions[origin.index()].borrow().clone();
    let from = (clone.module_source.clone(), clone.name.clone());
    rename_for_lending(&mut clone.name, &mut clone.method_info);
    let id = project.next_func_id();
    clone.id = Some(id);
    clone.visibility = Visibility::Private;
    clone.is_export = false;
    clone.export_name = None;
    // The marker names the one canonical function behind a compiler item,
    // which peepholes synthesize calls against: the original.
    clone.compiler_item = None;
    let body = clone.body.as_mut().expect("a lender has a body");
    for &(parent, call, src) in sites {
        assert!(body.replace_operand_to(parent, call, src));
    }
    let key = FunctionRef::from_resolved(&clone, clone.module_source.clone()).function_id();
    project.func_index.insert(key, id);
    project.lending_clones.insert(id, parts);
    project.copy_function_strings(&from, (clone.module_source.clone(), clone.name.clone()));
    assert_eq!(
        id.index(),
        project.functions.len(),
        "a FuncId is its position"
    );
    project.functions.push(FuncCell::new(clone));
    id
}
