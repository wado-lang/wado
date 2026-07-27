//! Dead Argument Elimination for Wado NIR.
//!
//! Removes parameters that the callee body never reads, together with the
//! corresponding argument expressions at every call site. The dropped
//! arguments must be pure so removal cannot change observable behaviour.
//!
//! NIR analog of `wir_optimize/dae.rs`. Running at NIR exposes the freshly
//! dead expressions to the rest of the fixed-point loop (`copy_prop` /
//! `const_fold` / `dce`), and it shrinks signatures *before* `inline` so the
//! inliner is not deterred by parameters that would never be read in the
//! inlined body anyway.
//!
//! Pinning rules — the pass conservatively skips:
//!
//! - Functions without a body (imports / extern declarations).
//! - Synthesised CM bridges (`is_cm_export`, `is_cm_binding`,
//!   `is_dispatch_wrapper`). These sit on the world boundary; their
//!   signatures are observed by the host runtime, not just by the
//!   in-project call graph.
//! - `is_ambient` functions (special call shapes — effect-handler
//!   dispatch carries an implicit handler param the call sites assume).
//! - Non-`Regular` `FunctionKind` entries (specialised stubs).
//! - Builtin / wasm-asset modules (their signatures are part of the ABI).
//! - Allocator entry points (`allocator_tag.is_some()`) — wired raw into
//!   the canonical ABI as `realloc`, with no `is_cm_export` wrapper to
//!   absorb a shrunken signature.
//! - Closure `__call` functors (`is_closure_call`) — their function-table
//!   wrapper snapshots the signature. Concrete trait-impl methods are NOT
//!   pinned: after monomorphization they are plain functions whose call sites
//!   all carry a resolved `func_id` (matching `sroa_param`'s relaxation), so a
//!   dead parameter is dropped soundly. The closure-functor `^Inspect` /
//!   `^InspectAlt` impls are the relaxed exception even among `__call`s.
//! - Functions whose pointer is taken via `FuncRef` anywhere in the project.
//!
//! `is_export` is NOT a pin: every user `export fn` reaches the world
//! boundary through a synthesised CM wrapper (`__cm_export__<name>`,
//! `is_cm_export = true`). The wrapper is the boundary; the user
//! function is internal — only the wrapper calls it — and the validator
//! sees the call site, so DAE shrinks the user function safely. The
//! wrapper's signature is held fixed by the `is_cm_export` pin above.
//!
//! Methods receive no special pin either. A method whose `self` is dead
//! is rewritten by `apply_dae` from `MethodCall(recv, name, args)` to
//! `Call(method_func, args)` at every call site, mirroring what was
//! previously implemented for closure-functor `__call` only. The
//! `closure_call_keys` set still exists for one purpose: to lift the
//! `trait_name` pin on the closure-functor's `^Inspect` /
//! `^InspectAlt` impls, where the matching wrapper adapts to the
//! shrunken signature. Everything else flows through the general path.
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): the function-body work
//! (dead-param detection, call-site validation, local renumbering, call-site
//! rewriting) reads and mutates the arena `Body` directly. Global initializers
//! are arena `Body`s too (Phase 5 group 1a), so the same arena routines run on
//! them directly.

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

pub fn eliminate_dead_arguments(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let closure_call_keys = collect_closure_call_keys(project);

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
    for idx in touched {
        gate.mark_changed(FuncId::new(idx));
    }
    true
}

/// Functions whose `dead[0]` (receiver-position deadness) is honoured rather
/// than forced to `false`, AND whose trait-method pin is lifted. These are
/// the closure-functor methods that have a controlled dispatch path:
///
/// - `__Closure_N::__call` (the closure body). Reached via the
///   synthesised `__closure_wrapper_*` (function-table dispatch) and via
///   `lower::plan::closure`'s fn-param-specialisation `MethodCall(g, __call,
///   args)`. `wir_build::register_closure_wrappers` adapts the wrapper
///   body to the surviving `call_method.params`.
///
/// - `__Closure_N^Inspect::inspect` and `^InspectAlt::inspect_alt`. The
///   only callers are the corresponding `__closure_inspect_wrapper_*` /
///   `__closure_inspect_alt_wrapper_*` (function-table dispatch, looked
///   up off `CanonicalClosure_K`'s `inspect` / `inspect_alt` slot). User
///   code reaches these via `Fn<N,Ret>^Inspect::inspect(closure_ref,
///   formatter)` / its alt twin, both of which dispatch through the
///   canonical struct rather than the per-functor impl directly.
///   `register_inspect_wrapper` adapts the wrapper body the same way the
///   call wrapper does, so DAE can safely drop the `self` (env) param
///   from the inspect impl when its synthesised body
///   (`f.write_str("...")`) doesn't read it.
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
    // Sweep the function list for synthesised
    // `__Closure_N^{Inspect,InspectAlt}::{inspect,inspect_alt}` impls.
    // These don't have a direct field on `ClosureFunctor`, so we
    // discriminate by `(struct_name == __Closure_N, trait is Inspect /
    // InspectAlt)`. The trait is matched against the canonical names in
    // the compiler-item registry — the same source
    // `generate_functor_format_methods` stamps into these impls'
    // `trait_name` — so a stdlib rename of either trait flows through
    // instead of a bare-literal `"Inspect"` match a user trait could
    // shadow. (`base_trait_module` is left `None` on these synthesis-
    // derived impls, so the registry name is the exact-identity anchor.)
    let type_table = project.type_table.borrow();
    let items = type_table.compiler_items();
    let inspect_name = items.trait_name(CompilerItem::Inspect);
    let inspect_alt_name = items.trait_name(CompilerItem::InspectAlt);
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(mi) = &func.method_info else {
            continue;
        };
        let Some(trait_name) = mi.trait_name.as_deref() else {
            continue;
        };
        if trait_name != inspect_name && trait_name != inspect_alt_name {
            continue;
        }
        if !functor_struct_names.contains(&(func.module_source.clone(), mi.struct_name.clone())) {
            continue;
        }
        if let Some(id) = func.id {
            keys.insert(id);
        }
    }
    keys
}

/// Shared pinning predicate for the two interprocedural signature-rewriting
/// passes, `dae` and `sroa_param` (which calls this via `super::dae`). Both
/// refuse the same world-boundary and ABI-fixed shapes — bodyless imports, CM
/// bridges, non-`Regular` kinds, builtin / wasm-asset modules, and allocator
/// entry points wired raw into the canonical ABI — and both treat a concrete
/// trait-impl method as eligible (after monomorphization it is a plain function
/// whose every call site carries a resolved `func_id`, so the rewrite reaches
/// them all). They diverge only on the closure `__call` functor policy, carried
/// by `relax_closure_call`:
///
/// - `false` (`sroa_param`, and `dae` for any `__call` not covered by its
///   relaxation): a closure `__call` stays pinned — its function-table wrapper
///   snapshots the signature.
/// - `true` (`dae`, for the closure-functor `^Inspect` / `^InspectAlt` /
///   `__call` impls whose wrapper adapts to the shrunken signature): the closure
///   pin is lifted.
///
/// `is_export` / `is_async` are intentionally *not* pins — both describe
/// user source-level intent, not a real call-shape constraint (see the module
/// doc). `sroa_param` layers its one extra pin (`is_value_copy`) on top of this.
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
    reads.extend(body.values.opaque_local_sources());
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

/// Validate every `Call` / `MethodCall` in a body: at each dead parameter
/// position the supplied argument (or the dropped receiver) must be pure.
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
    match &body.exprs[id].kind {
        ExprKind::Call { func_id, args, .. } => {
            let key = *func_id;
            let Some(dead) = candidates.get(&key) else {
                return;
            };
            if rejected.contains(&key) {
                return;
            }
            // Call positions map 1:1 to params.
            for (i, dead_at_i) in dead.iter().enumerate() {
                if !*dead_at_i {
                    continue;
                }
                // Dropping the argument erases its evaluation, so a trapping
                // (but side-effect-free) argument must keep the param alive.
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
        ExprKind::MethodCall {
            func_id,
            receiver,
            args,
            ..
        } => {
            let key = *func_id;
            let Some(dead) = candidates.get(&key) else {
                return;
            };
            if rejected.contains(&key) {
                return;
            }
            // If the rewriter drops the receiver, the MethodCall collapses to
            // a `Call` and the receiver is discarded — it must be pure.
            let drops_receiver = dead.first() == Some(&true);
            if drops_receiver
                && !receiver.as_expr().is_none_or(|e| {
                    arena_query::is_pure_nontrapping_expr_typed(body, e, Some(type_table))
                })
            {
                rejected.insert(key);
            } else {
                // params[i+1] maps to args[i] regardless of whether position 0
                // was dropped (the receiver is structural).
                for (i, dead_at_i) in dead.iter().enumerate().skip(1) {
                    if !*dead_at_i {
                        continue;
                    }
                    match args.get(i - 1) {
                        Some(arg)
                            if arena_query::is_pure_nontrapping_operand_typed(
                                body,
                                arg.expr,
                                Some(type_table),
                            ) => {}
                        _ => {
                            rejected.insert(key);
                            break;
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Application
// ──────────────────────────────────────────────────────────────────────────────

/// Applies the rewrites and returns the indices of every function whose body or
/// signature changed (confirmed callees + callers whose call sites were
/// rewritten), so the caller can mark exactly those dirty in the gate. The call
/// graph is unaffected: dae drops arguments and may collapse `MethodCall` →
/// `Call` on the *same* callee, never adding or removing an edge.
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

/// Rewrite every `Call` / `MethodCall` of a confirmed function in `body`:
/// drop the dead-position arguments, collapsing a receiver-dropped
/// `MethodCall` into a plain `Call`.
fn rewrite_calls_in_body(body: &mut Body, confirmed: &IndexMap<FnKey, Vec<bool>>) -> bool {
    let mut calls = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && matches!(
                &body.exprs[id].kind,
                ExprKind::Call { .. } | ExprKind::MethodCall { .. }
            )
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
    let key = match &body.exprs[id].kind {
        ExprKind::Call { func_id, .. } | ExprKind::MethodCall { func_id, .. } => *func_id,
        _ => return false,
    };
    let Some(dead) = confirmed.get(&key).cloned() else {
        return false;
    };
    match &mut body.exprs[id].kind {
        ExprKind::Call { args, .. } => {
            let mut i = 0;
            args.retain(|_| {
                let alive = !dead.get(i).copied().unwrap_or(false);
                i += 1;
                alive
            });
        }
        ExprKind::MethodCall { .. } => {
            let drops_receiver = dead.first() == Some(&true);
            if drops_receiver {
                // Collapse `MethodCall(recv, name, args)` to `Call(method_func,
                // surviving_args)`. The receiver was verified pure, so dropping
                // it (along with the dead `args`) is observation-free.
                let ExprKind::MethodCall {
                    func_id,
                    type_args,
                    args,
                    ..
                } = std::mem::replace(&mut body.exprs[id].kind, ExprKind::Dead)
                else {
                    unreachable!();
                };
                let mut new_args = Vec::with_capacity(args.len());
                for (idx, arg) in args.into_iter().enumerate() {
                    // dead[idx + 1] corresponds to args[idx] (params[i+1]).
                    if dead.get(idx + 1).copied().unwrap_or(false) {
                        continue;
                    }
                    new_args.push(arg);
                }
                body.exprs[id].kind = ExprKind::Call {
                    func_id,
                    type_args,
                    args: new_args,
                };
            } else if let ExprKind::MethodCall { args, .. } = &mut body.exprs[id].kind {
                let mut i = 0;
                args.retain(|_| {
                    let alive = !dead.get(i + 1).copied().unwrap_or(false);
                    i += 1;
                    alive
                });
            }
        }
        _ => return false,
    }
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
