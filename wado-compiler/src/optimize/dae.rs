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
//! - Trait methods (`method_info.trait_name.is_some()`) — sibling impls and
//!   the trait declaration share a signature contract.
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

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FunctionKind, NirFunction};
use crate::nir_arena::{Body, ExprId, ExprKind, NodeRef, PatKind, StmtKind};
use crate::nir_package::NirPackage;

use super::arena_query;

pub(super) type FnKey = (ModuleSource, String);

pub fn eliminate_dead_arguments(project: &mut NirPackage) -> bool {
    let pinned = collect_pinned(project);
    let closure_call_keys = collect_closure_call_keys(project);

    // Phase 1: identify candidate (function, dead positions) pairs.
    let mut candidates: IndexMap<FnKey, Vec<bool>> = IndexMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let key = (func.module_source.clone(), func.name.clone());
        let is_closure_dae_relaxed = closure_call_keys.contains(&key);
        if !is_eligible(&func, &pinned, is_closure_dae_relaxed) {
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

    // Phase 3: rewrite signatures and call sites.
    apply_dae(project, &confirmed);
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
        keys.insert((cm.module_source.clone(), cm.name.clone()));
    }
    // Sweep the function list for synthesised
    // `__Closure_N^{Inspect,InspectAlt}::{inspect,inspect_alt}` impls.
    // These don't have a direct field on `ClosureFunctor`, so we
    // discriminate by `(struct_name == __Closure_N, trait_name in
    // {Inspect, InspectAlt})`.
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        let Some(mi) = &func.method_info else {
            continue;
        };
        let Some(trait_name) = &mi.trait_name else {
            continue;
        };
        if trait_name != "Inspect" && trait_name != "InspectAlt" {
            continue;
        }
        if !functor_struct_names.contains(&(func.module_source.clone(), mi.struct_name.clone())) {
            continue;
        }
        keys.insert((func.module_source.clone(), func.name.clone()));
    }
    keys
}

fn is_eligible(func: &NirFunction, pinned: &IndexSet<FnKey>, is_closure_dae_relaxed: bool) -> bool {
    if func.body.is_none() {
        return false;
    }
    // `is_export` and `is_async` are intentionally absent — both describe
    // user source-level intent, not a real call-shape constraint (see the
    // module doc).
    if func.is_cm_export || func.is_cm_binding || func.is_dispatch_wrapper || func.is_ambient {
        return false;
    }
    if !matches!(func.kind, FunctionKind::Regular) {
        return false;
    }
    if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
        return false;
    }
    // Allocator entry points are wired straight into the canonical ABI as
    // `realloc`; their signature must survive verbatim.
    if func.allocator_tag.is_some() {
        return false;
    }
    // Trait methods share a signature contract with sibling impls and the
    // trait declaration; the closure-functor `^Inspect` / `^InspectAlt`
    // impls are the relaxed exception.
    if !is_closure_dae_relaxed
        && func
            .method_info
            .as_ref()
            .is_some_and(|mi| mi.trait_name.is_some())
    {
        return false;
    }
    if pinned.contains(&(func.module_source.clone(), func.name.clone())) {
        return false;
    }
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

pub(super) fn collect_pinned(_project: &NirPackage) -> IndexSet<FnKey> {
    // `FuncRef` is lowered into a `Closure` literal (functor struct) by
    // `lower::plan::closure` before NIR is constructed, so there is no bare
    // function reference left to pin. Closure functor `__call` methods are
    // NOT pinned wholesale either — `wir_build::register_closure_wrappers`
    // derives the function-table wrapper's external signature from
    // `ClosureFunctor::canonical_user_params` / `canonical_return`, a
    // snapshot taken at functor creation that DAE never mutates, and the
    // wrapper body adapts to whichever `call_method.params` survive.
    // Trait-shaped `__call`s (`^Inspect::inspect` / `^InspectAlt`) are
    // skipped separately by `is_eligible`'s `trait_name` check.
    IndexSet::default()
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
    let mut rejected: IndexSet<FnKey> = IndexSet::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            validate_in_body(body, &candidates, &mut rejected);
        }
    }
    for global in &project.globals {
        validate_in_body(&global.initializer, &candidates, &mut rejected);
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
) {
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            validate_call(body, id, candidates, rejected);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

fn validate_call(
    body: &Body,
    id: ExprId,
    candidates: &IndexMap<FnKey, Vec<bool>>,
    rejected: &mut IndexSet<FnKey>,
) {
    match &body.exprs[id].kind {
        ExprKind::Call { func, args, .. } => {
            let key = (func.module_source.clone(), func.name.clone());
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
                match args.get(i) {
                    Some(arg) if arena_query::is_pure_expr(body, arg.expr) => {}
                    _ => {
                        rejected.insert(key.clone());
                        break;
                    }
                }
            }
        }
        ExprKind::MethodCall {
            func,
            receiver,
            args,
            ..
        } => {
            let key = (func.module_source.clone(), func.name.clone());
            let Some(dead) = candidates.get(&key) else {
                return;
            };
            if rejected.contains(&key) {
                return;
            }
            // If the rewriter drops the receiver, the MethodCall collapses to
            // a `Call` and the receiver is discarded — it must be pure.
            let drops_receiver = dead.first() == Some(&true);
            if drops_receiver && !arena_query::is_pure_expr(body, *receiver) {
                rejected.insert(key.clone());
            } else {
                // params[i+1] maps to args[i] regardless of whether position 0
                // was dropped (the receiver is structural).
                for (i, dead_at_i) in dead.iter().enumerate().skip(1) {
                    if !*dead_at_i {
                        continue;
                    }
                    match args.get(i - 1) {
                        Some(arg) if arena_query::is_pure_expr(body, arg.expr) => {}
                        _ => {
                            rejected.insert(key.clone());
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

fn apply_dae(project: &mut NirPackage, confirmed: &IndexMap<FnKey, Vec<bool>>) {
    // Phase 3a: shrink the parameter list of every confirmed callee, then
    // renumber locals so `params[k].local_index == k` continues to hold.
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let key = (func.module_source.clone(), func.name.clone());
        if let Some(dead) = confirmed.get(&key) {
            shrink_params_and_renumber(&mut func, dead);
        }
    }

    // Phase 3b: rewrite every call site.
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            rewrite_calls_in_body(body, confirmed);
        }
    }
    for global in &mut project.globals {
        rewrite_calls_in_body(&mut global.initializer, confirmed);
    }
}

/// Rewrite every `Call` / `MethodCall` of a confirmed function in `body`:
/// drop the dead-position arguments, collapsing a receiver-dropped
/// `MethodCall` into a plain `Call`.
fn rewrite_calls_in_body(body: &mut Body, confirmed: &IndexMap<FnKey, Vec<bool>>) {
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
    for id in calls {
        rewrite_call(body, id, confirmed);
    }
}

fn rewrite_call(body: &mut Body, id: ExprId, confirmed: &IndexMap<FnKey, Vec<bool>>) {
    let key = match &body.exprs[id].kind {
        ExprKind::Call { func, .. } | ExprKind::MethodCall { func, .. } => {
            (func.module_source.clone(), func.name.clone())
        }
        _ => return,
    };
    let Some(dead) = confirmed.get(&key).cloned() else {
        return;
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
                    func,
                    type_args,
                    args,
                    ..
                } = std::mem::replace(&mut body.exprs[id].kind, ExprKind::Unit)
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
                    func,
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
        _ => {}
    }
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
    let old_count = func.local_count as usize;
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
    func.local_count = new_idx;

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
