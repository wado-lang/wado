//! Dead Argument Elimination — drops parameters the callee never reads, along
//! with the (pure) argument expressions at every call site. Running at NIR
//! rather than WIR feeds the freshly dead expressions back into the fixed-point
//! loop and shrinks signatures before `inline` sees them. [`is_dae_sroa_eligible`]
//! holds the pinning rules; neither `is_export` nor a method receiver is pinned.

use cranelift_entity::EntityRef;

use crate::compiler_item::CompilerItem;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FunctionKind, NirFunction};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtKind};
use crate::nir_package::NirPackage;

use super::arena_query;
use super::gate::{FunctionGate, GatedPass};
use crate::nir::FuncId;

/// A function's canonical [`FuncId`]: the candidate/confirmed/pinned sets key on
/// it, and a call site is matched by the stamped `func_id` on its call node.
pub(super) type FnKey = FuncId;

/// Dropping a parameter deletes the argument its callers pass, which can leave
/// one of *their* parameters dead — so the pass has a fixed point of its own,
/// as deep as the forwarding chain. `mark_changed` bumps a function's callers,
/// making each round's dirty set exactly the links the round before it freed;
/// reaching that fixed point here is what keeps the *outer* loop's iteration
/// count off the call graph's depth.
pub fn eliminate_dead_arguments(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    // Keyed by struct and method name, which no signature shrink moves, so this
    // is read once rather than per round.
    let closure_call_keys = collect_closure_call_keys(project);
    let mut changed = false;
    while eliminate_dead_arguments_round(project, gate, &closure_call_keys) {
        changed = true;
    }
    changed
}

/// One find / validate / apply sweep. Returns whether it rewrote anything —
/// every round drops at least one parameter, so the loop above terminates.
fn eliminate_dead_arguments_round(
    project: &mut NirPackage,
    gate: &mut FunctionGate,
    closure_call_keys: &IndexSet<FnKey>,
) -> bool {
    // Phase 1: identify candidate (function, dead positions) pairs.
    let mut candidates: IndexMap<FnKey, Vec<bool>> = IndexMap::default();
    for fid in gate.dirty_funcs(GatedPass::Dae, project.functions.len()) {
        let func = project.functions[fid.index()].borrow();
        let Some(key) = func.id else { continue };
        let is_closure_dae_relaxed = closure_call_keys.contains(&key);
        if !is_dae_sroa_eligible(&func, is_closure_dae_relaxed) {
            continue;
        }
        let dead = find_dead_params(&func);
        if dead.iter().any(|&d| d) {
            candidates.insert(key, dead);
        }
    }
    if candidates.is_empty() {
        return false;
    }

    // Phase 2: validate every call site passes side-effect-free args at the
    // dead positions. A single offending site rejects the candidate entirely.
    let confirmed = validate_call_sites(project, candidates);
    if confirmed.is_empty() {
        return false;
    }

    // Phase 3: rewrite signatures and call sites. dae is interprocedural and
    // scans all functions, but reports exactly the ones it touched so the gated
    // passes re-examine only those and their call-graph neighbours.
    let touched = apply_dae(project, &confirmed);
    let rewrote = !touched.is_empty();
    for idx in touched {
        gate.mark_changed(FuncId::new(idx));
    }
    rewrote
}

/// Closure-functor methods whose `is_closure_call` pin is lifted:
/// `__Closure_N::__call` and the
/// `^Inspect` / `^InspectAlt` impls. Each is reached only through a synthesised
/// function-table wrapper, and `register_closure_wrappers` /
/// `register_inspect_wrapper` adapt that wrapper to the shrunken signature.
fn collect_closure_call_keys(project: &NirPackage) -> IndexSet<FnKey> {
    let mut keys: IndexSet<FnKey> = IndexSet::default();
    let functor_struct_names: IndexSet<(ModuleSource, String)> = project
        .closure_functors
        .iter()
        .map(|f| (f.module_source.clone(), f.struct_name.clone()))
        .collect();
    for f in &project.closure_functors {
        let cm = f.call_method.borrow();
        if let Some(id) = cm.id {
            keys.insert(id);
        }
    }
    // Sweep for synthesised `__Closure_N^{Inspect,InspectAlt}` impls, which have
    // no field on `ClosureFunctor` to key off. The trait is matched against the
    // compiler-item registry — the same source `generate_functor_format_methods`
    // stamps into `trait_name` — so a stdlib rename flows through and no user
    // trait named `Inspect` can shadow it.
    let type_table = project.type_table.borrow();
    let items = type_table.compiler_items();
    let inspect_name = items.trait_name(CompilerItem::Inspect);
    let inspect_alt_name = items.trait_name(CompilerItem::InspectAlt);
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(mi) = &func.method_info else {
            continue;
        };
        let Some(trait_name) = mi.trait_name.as_ref() else {
            continue;
        };
        if trait_name.base_name() != inspect_name && trait_name.base_name() != inspect_alt_name {
            continue;
        }
        if !functor_struct_names.contains(&(func.module_source.clone(), mi.struct_name().clone())) {
            continue;
        }
        if let Some(id) = func.id {
            keys.insert(id);
        }
    }
    keys
}

/// Shared pinning predicate for `dae` and `sroa_param`: both refuse the same
/// world-boundary and ABI-fixed shapes, and both accept a concrete trait-impl
/// method (post-monomorphization every call site carries a resolved `func_id`).
/// They diverge only on `relax_closure_call` — a closure `__call` stays pinned
/// unless its function-table wrapper adapts to the shrunken signature.
pub(super) fn is_dae_sroa_eligible(func: &NirFunction, relax_closure_call: bool) -> bool {
    if func.body.is_none() {
        return false;
    }
    if func.is_cm_export || func.is_cm_binding || func.is_dispatch_wrapper || func.is_ambient {
        return false;
    }
    if !matches!(func.kind, FunctionKind::Regular) {
        return false;
    }
    if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
        return false;
    }
    if func.allocator_tag.is_some() {
        return false;
    }
    if !relax_closure_call && func.is_closure_call() {
        return false;
    }
    // No explicit pin set: `FuncRef` is lowered into a `Closure` literal (functor
    // struct) by `lower::plan::closure` before NIR is built, so there is no bare
    // function reference left to pin against.
    true
}

/// Returns one bool per parameter: `true` means the parameter is unused and
/// safe-to-remove. The receiver position (index 0 for methods) gets no special
/// pin — `apply_dae` rewrites a method whose `self` is dead into a plain
/// `Call(method_func, args)`, gated on receiver purity by the validator.
fn find_dead_params(func: &NirFunction) -> Vec<bool> {
    if func.params.is_empty() {
        return Vec::new();
    }

    let body = func.body.as_ref().unwrap();
    let mut reads: IndexSet<u32> = IndexSet::default();
    arena_query::collect_reads(body, &mut reads);
    // A promoted `Opaque(Local idx)` value reads `idx` from the value pool, not
    // the skeleton — count those so a param read only through a promoted value
    // is not seen as dead.
    arena_query::promoted_local_reads(body, &mut reads);
    let kept_locals = &func.address_taken_locals;
    let stores_aliased = &func.stores_aliased_locals;

    let mut dead = Vec::with_capacity(func.params.len());
    for p in &func.params {
        let is_read = reads.contains(&p.local_index);
        let is_kept = kept_locals.contains(&p.local_index);
        let is_aliased = stores_aliased.contains(&p.local_index);
        let in_stores = func.stores.contains(&p.name);
        dead.push(!(is_read || is_kept || is_aliased || in_stores));
    }
    dead
}

// ──────────────────────────────────────────────────────────────────────────────
// Call-site validation
// ──────────────────────────────────────────────────────────────────────────────

/// Walk every call site once. A single impure argument at a dead position
/// drops the candidate completely so we never rewrite some sites and not
/// others.
fn validate_call_sites(
    project: &NirPackage,
    mut candidates: IndexMap<FnKey, Vec<bool>>,
) -> IndexMap<FnKey, Vec<bool>> {
    let type_table = project.type_table.borrow();
    let mut rejected: IndexSet<FnKey> = IndexSet::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            validate_in_body(body, &candidates, &mut rejected, &type_table);
        }
    }
    for global in &project.globals {
        validate_in_body(
            global.init.slot_expr().body(),
            &candidates,
            &mut rejected,
            &type_table,
        );
    }
    for r in rejected {
        candidates.shift_remove(&r);
    }
    candidates
}

/// Validate every call in a body: at each dead parameter position the supplied
/// argument must be pure.
fn validate_in_body(
    body: &Body,
    candidates: &IndexMap<FnKey, Vec<bool>>,
    rejected: &mut IndexSet<FnKey>,
    type_table: &crate::tir::TypeTable,
) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            validate_call(body, id, candidates, rejected, type_table);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

fn validate_call(
    body: &Body,
    id: ExprId,
    candidates: &IndexMap<FnKey, Vec<bool>>,
    rejected: &mut IndexSet<FnKey>,
    type_table: &crate::tir::TypeTable,
) {
    let ExprKind::Call { func_id, args, .. } = &body.exprs[id].kind else {
        return;
    };
    let key = *func_id;
    let Some(dead) = candidates.get(&key) else {
        return;
    };
    if rejected.contains(&key) {
        return;
    }
    for (i, dead_at_i) in dead.iter().enumerate() {
        if !*dead_at_i {
            continue;
        }
        // Dropping the argument erases its evaluation, so a trapping (but
        // side-effect-free) argument must keep the param alive.
        let pure = match args.get(i).map(|a| a.expr) {
            Some(Operand::Value(_)) => true,
            Some(Operand::Expr(e)) => {
                arena_query::is_pure_nontrapping_expr_typed(body, e, Some(type_table))
            }
            None => false,
        };
        if !pure {
            rejected.insert(key);
            break;
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Application
// ──────────────────────────────────────────────────────────────────────────────

/// Applies the rewrites and returns the indices of every function whose body or
/// signature changed (confirmed callees + callers whose call sites were
/// rewritten), so the caller can mark exactly those dirty in the gate. The call
/// graph is unaffected: dae drops arguments on the *same* callee — never
/// adding or removing an edge.
fn apply_dae(project: &mut NirPackage, confirmed: &IndexMap<FnKey, Vec<bool>>) -> Vec<usize> {
    let mut touched: IndexSet<usize> = IndexSet::default();
    // Phase 3a: shrink the parameter list of every confirmed callee, then
    // renumber locals so `params[k].local_index == k` continues to hold.
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        if let Some(dead) = func.id.and_then(|id| confirmed.get(&id)) {
            shrink_params_and_renumber(&mut func, dead);
            touched.insert(i);
        }
    }

    // Phase 3b: rewrite every call site.
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut()
            && rewrite_calls_in_body(body, confirmed)
        {
            touched.insert(i);
        }
    }
    for global in &mut project.globals {
        rewrite_calls_in_body(global.init.slot_expr_mut().body_mut(), confirmed);
    }
    touched.into_iter().collect()
}

/// Rewrite every call of a confirmed function in `body`: drop the dead-position
/// arguments, clearing `has_receiver` when the receiver itself is dropped.
fn rewrite_calls_in_body(body: &mut Body, confirmed: &IndexMap<FnKey, Vec<bool>>) -> bool {
    let mut calls = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && matches!(&body.exprs[id].kind, ExprKind::Call { .. })
        {
            calls.push(id);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    // Each rewrite drops arguments / collapses by the callee's dead set,
    // independent of the call's position, so processing order does not matter.
    let mut changed = false;
    for id in calls {
        changed |= rewrite_call(body, id, confirmed);
    }
    changed
}

fn rewrite_call(body: &mut Body, id: ExprId, confirmed: &IndexMap<FnKey, Vec<bool>>) -> bool {
    let ExprKind::Call { func_id, .. } = &body.exprs[id].kind else {
        return false;
    };
    let Some(dead) = confirmed.get(func_id).cloned() else {
        return false;
    };
    let ExprKind::Call {
        args, has_receiver, ..
    } = &mut body.exprs[id].kind
    else {
        return false;
    };
    // Dropping `args[0]` of a method call makes it a free call of the same
    // callee. The receiver was verified pure, so discarding it is
    // observation-free.
    if dead.first() == Some(&true) {
        *has_receiver = false;
    }
    let mut i = 0;
    args.retain(|_| {
        let alive = !dead.get(i).copied().unwrap_or(false);
        i += 1;
        alive
    });
    true
}

fn shrink_params_and_renumber(func: &mut NirFunction, dead: &[bool]) {
    // Compute the set of dead local_indices (one per dead param).
    let mut dead_local_indices: IndexSet<u32> = IndexSet::default();
    for (i, &d) in dead.iter().enumerate() {
        if d && let Some(p) = func.params.get(i) {
            dead_local_indices.insert(p.local_index);
        }
    }

    // Build the old → new local-index remap. Drop the dead slots and pack
    // surviving slots downward.
    let old_count = func.local_count() as usize;
    let mut remap: Vec<Option<u32>> = vec![None; old_count];
    let mut new_idx: u32 = 0;
    for old in 0..old_count {
        let old_u = u32::try_from(old).unwrap();
        if dead_local_indices.contains(&old_u) {
            continue;
        }
        remap[old] = Some(new_idx);
        new_idx += 1;
    }

    // Compact locals[] to drop the dead slots.
    let mut compact_locals = Vec::with_capacity(new_idx as usize);
    for (old, local) in std::mem::take(&mut func.locals).into_iter().enumerate() {
        if !dead_local_indices.contains(&u32::try_from(old).unwrap()) {
            compact_locals.push(local);
        }
    }
    func.locals = compact_locals;

    // Drop dead params and renumber surviving ones to point at the new
    // local positions.
    let mut i = 0;
    func.params.retain_mut(|p| {
        let alive = !dead.get(i).copied().unwrap_or(false);
        i += 1;
        if alive {
            p.local_index = remap[p.local_index as usize].unwrap();
            true
        } else {
            false
        }
    });

    // Apply the remap to address_taken_locals / stores_aliased_locals.
    func.address_taken_locals = func
        .address_taken_locals
        .iter()
        .filter_map(|i| remap[*i as usize])
        .collect();
    func.stores_aliased_locals = func
        .stores_aliased_locals
        .iter()
        .filter_map(|i| remap[*i as usize])
        .collect();

    // Apply the remap to every `Local` / `Let` / `Binding` local index in the
    // body. Closures are functor structs in NIR, so there is no nested-body
    // local namespace to skip.
    if let Some(body) = func.body.as_mut() {
        debug_assert!(
            {
                let mut reads = IndexSet::default();
                arena_query::collect_reads(body, &mut reads);
                arena_query::promoted_local_reads(body, &mut reads);
                reads.iter().all(|r| remap[*r as usize].is_some())
            },
            "[NIR] dae: dropping a local that is still read"
        );
        remap_locals(body, &remap);
        // Promoted `Opaque(Local idx)` values (extracted as `local.get idx`)
        // live in the value pool, not the skeleton, so `remap_locals` misses
        // them — remap their source indices too.
        body.values.remap_opaque_locals(&remap);
    }
}

fn remap_locals(body: &mut Body, remap: &[Option<u32>]) {
    let lookup = |idx: u32| remap[idx as usize].expect("dead local referenced after DAE rewrite");

    let mut exprs = Vec::new();
    let mut stmts = Vec::new();
    let mut pats = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        match node {
            NodeRef::Expr(id) => exprs.push(id),
            NodeRef::Stmt(id) => stmts.push(id),
            NodeRef::Pat(id) => pats.push(id),
            NodeRef::Block(_) => {}
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    for id in exprs {
        if let ExprKind::Local { index, .. } = &mut body.exprs[id].kind {
            *index = lookup(*index);
        }
    }
    for id in stmts {
        if let StmtKind::Let { local_index, .. } = &mut body.stmts[id].kind {
            *local_index = lookup(*local_index);
        }
    }
    for id in pats {
        if let PatKind::Binding { local_index, .. } = &mut body.pats[id].kind {
            *local_index = lookup(*local_index);
        }
    }
}
