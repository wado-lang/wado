//! Dead Argument Elimination for Wado TIR.
//!
//! Removes parameters that the callee body never reads, together with the
//! corresponding argument expressions at every call site. The dropped
//! arguments must be pure so removal cannot change observable behaviour.
//!
//! TIR analog of `wir_optimize/dae.rs`. Running at TIR exposes the freshly
//! dead expressions to the rest of the fixed-point loop (`copy_prop` /
//! `const_fold` / `dce`), and it shrinks signatures *before* `inline` so the
//! inliner is not deterred by parameters that would never be read in the
//! inlined body anyway.
//!
//! Pinning rules — the pass conservatively skips:
//!
//! - Functions without a body (imports / extern declarations).
//! - Functions exported at the world boundary (`is_export`, `is_cm_export`).
//! - Synthesised CM bridges (`is_cm_binding`, `is_dispatch_wrapper`).
//! - `is_ambient` / `is_async` functions (special call shapes).
//! - Non-`Regular` `FunctionKind` entries (specialised stubs).
//! - Builtin / wasm-asset modules (their signatures are part of the ABI).
//! - Trait methods (`method_info.trait_name.is_some()`) — sibling impls and
//!   the trait declaration share a signature contract.
//! - Functions whose pointer is taken via `FuncRef` anywhere in the project.
//! - Methods (`method_info.is_some()`) — for the receiver position only;
//!   higher positions are still candidates because they map cleanly onto
//!   `MethodCall.args`. The closure-functor `__call` exception lifts even
//!   the receiver pin, since `wir_build` can adapt the wrapper and the
//!   rewriter can collapse `MethodCall(g, __call, args)` to `Call(__call,
//!   args)` when `g` is observation-free; see `collect_closure_call_keys`
//!   and `apply_dae`.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{
    FunctionKind, TirExpr, TirExprKind, TirFunction, TirPattern, TirStmt, TirStmtKind,
};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

use super::elide_local::is_pure_expr;

pub(super) type FnKey = (ModuleSource, String);

pub fn eliminate_dead_arguments(project: &mut FlatPackage) -> bool {
    let pinned = collect_pinned(project);
    let closure_call_keys = collect_closure_call_keys(project);

    // Phase 1: identify candidate (function, dead positions) pairs.
    let mut candidates: IndexMap<FnKey, Vec<bool>> = IndexMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if !is_eligible(&func, &pinned) {
            continue;
        }
        let key = (func.module_source.clone(), func.name.clone());
        let dead = find_dead_params(&func, closure_call_keys.contains(&key));
        if dead.iter().any(|&d| d) {
            candidates.insert(key, dead);
        }
    }
    if candidates.is_empty() {
        return false;
    }

    // Phase 2: validate every call site passes side-effect-free args at the
    // dead positions. A single offending site rejects the candidate entirely.
    let confirmed = validate_call_sites(project, candidates, &closure_call_keys);
    if confirmed.is_empty() {
        return false;
    }

    // Phase 3: rewrite signatures and call sites.
    apply_dae(project, &confirmed, &closure_call_keys);
    true
}

/// Functions whose `dead[0]` (receiver-position deadness) is honoured rather
/// than forced to `false`. Closure functor `__call` methods have a special
/// dispatch path: their only call sites are `wir_build`'s synthesised
/// wrapper (which derives its external signature from
/// `ClosureFunctor::canonical_user_params` and adapts the inner call to
/// match `call_method.params` post-DAE) and the typed
/// `MethodCall(g, __call, args)`s that `lower::closure`'s
/// fn-param-specialisation produces. Both paths can be retargeted when
/// `self` (env) becomes unread, so we let DAE drop position 0 for them.
fn collect_closure_call_keys(project: &FlatPackage) -> IndexSet<FnKey> {
    project
        .closure_functors
        .iter()
        .map(|f| {
            let cm = f.call_method.borrow();
            (cm.module_source.clone(), cm.name.clone())
        })
        .collect()
}

fn is_eligible(func: &TirFunction, pinned: &IndexSet<FnKey>) -> bool {
    if func.body.is_none() {
        return false;
    }
    if func.is_export
        || func.is_cm_export
        || func.is_cm_binding
        || func.is_dispatch_wrapper
        || func.is_ambient
        || func.is_async
    {
        return false;
    }
    if !matches!(func.kind, FunctionKind::Regular) {
        return false;
    }
    if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
        return false;
    }
    // Trait methods have signature contracts shared with other impls
    // (and reachable via vtable / trait dispatch). Removing a param from
    // a single impl desynchronises the impl from the trait declaration
    // and from sibling impls, which then trap on dispatch. Skip them.
    if func
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
/// safe-to-remove. For methods the receiver position (index 0) is forced
/// live so the rewriter never has to convert `MethodCall` → `Call`. The
/// closure-functor `__call` exception (`is_closure_call == true`) honours
/// receiver-position deadness, since the rewriter knows how to retarget
/// closure call sites — see `apply_dae` and
/// `wir_build::register_closure_wrappers`.
fn find_dead_params(func: &TirFunction, is_closure_call: bool) -> Vec<bool> {
    if func.params.is_empty() {
        return Vec::new();
    }

    let body = func.body.as_ref().unwrap();
    let mut reads: IndexSet<u32> = IndexSet::default();
    super::elide_local::collect_reads_in_block(body, &mut reads);
    let kept_locals = &func.address_taken_locals;
    let stores_aliased = &func.stores_aliased_locals;

    let mut dead = Vec::with_capacity(func.params.len());
    let receiver_is_self_pinned = func.method_info.is_some() && !is_closure_call;
    for (i, p) in func.params.iter().enumerate() {
        if i == 0 && receiver_is_self_pinned {
            dead.push(false);
            continue;
        }
        let is_read = reads.contains(&p.local_index);
        let is_kept = kept_locals.contains(&p.local_index);
        let is_aliased = stores_aliased.contains(&p.local_index);
        let in_stores = func.stores.contains(&p.name);
        dead.push(!(is_read || is_kept || is_aliased || in_stores));
    }
    dead
}

pub(super) fn collect_pinned(project: &FlatPackage) -> IndexSet<FnKey> {
    let mut pinned: IndexSet<FnKey> = IndexSet::default();
    let mut walker = FuncRefCollector { out: &mut pinned };
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            walker.visit_block(body);
        }
    }
    for global in &project.globals {
        walker.visit_expr(&global.initializer);
    }
    // Closure functor `__call` methods are NOT pinned wholesale.
    //
    // `wir_build::register_closure_wrappers` derives the function-table
    // wrapper's external signature from `ClosureFunctor::canonical_user_params`
    // / `canonical_return` — a snapshot taken at functor creation that DAE
    // never mutates — and the wrapper body adapts to whichever
    // `call_method.params` survive. So even when the closure is coerced to
    // a typed `fn(...)` and dispatched through the table, DAE shrinking
    // `__call.params` is safe: the table-level signature stays put, and
    // the wrapper drops the corresponding wrapper-local from its inner
    // call.
    //
    // Trait-shaped `__call`s (`^Inspect::inspect` /
    // `^InspectAlt::inspect_alt`) are still skipped by `is_eligible`'s
    // `trait_name` check, since their cross-impl signature contract is a
    // separate concern from the closure-functor / wrapper boundary.
    pinned
}

struct FuncRefCollector<'a> {
    out: &'a mut IndexSet<FnKey>,
}

impl TirRefVisitor for FuncRefCollector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::FuncRef {
            module_source,
            name,
        } = &expr.kind
        {
            self.out.insert((module_source.clone(), name.clone()));
        }
        self.walk_expr(expr);
    }
}

/// Walk every call site once per validation. A single impure argument at a
/// dead position drops the candidate completely so we never end up rewriting
/// some sites and not others.
fn validate_call_sites(
    project: &FlatPackage,
    mut candidates: IndexMap<FnKey, Vec<bool>>,
    closure_call_keys: &IndexSet<FnKey>,
) -> IndexMap<FnKey, Vec<bool>> {
    let mut validator = CallSiteValidator {
        candidates: &candidates,
        closure_call_keys,
        rejected: IndexSet::default(),
    };
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            validator.visit_block(body);
        }
    }
    for global in &project.globals {
        validator.visit_expr(&global.initializer);
    }
    for r in validator.rejected {
        candidates.shift_remove(&r);
    }
    candidates
}

struct CallSiteValidator<'a> {
    candidates: &'a IndexMap<FnKey, Vec<bool>>,
    closure_call_keys: &'a IndexSet<FnKey>,
    rejected: IndexSet<FnKey>,
}

impl TirRefVisitor for CallSiteValidator<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                let key = (func.module_source.clone(), func.name.clone());
                if let Some(dead) = self.candidates.get(&key)
                    && !self.rejected.contains(&key)
                {
                    // Call positions map 1:1 to params.
                    for (i, dead_at_i) in dead.iter().enumerate() {
                        if !*dead_at_i {
                            continue;
                        }
                        match args.get(i) {
                            Some(arg) if is_pure_expr(&arg.expr) => {}
                            _ => {
                                // Either an impure expr at a dead position,
                                // or the caller doesn't supply this arg
                                // (variadic / optional). Either way, bail.
                                self.rejected.insert(key.clone());
                                break;
                            }
                        }
                    }
                }
            }
            TirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                let key = (func.module_source.clone(), func.name.clone());
                if let Some(dead) = self.candidates.get(&key)
                    && !self.rejected.contains(&key)
                {
                    let drops_receiver =
                        self.closure_call_keys.contains(&key) && dead.first() == Some(&true);
                    // If the rewriter is going to drop position 0 (closure
                    // `__call` only), the MethodCall collapses to a `Call`
                    // and the receiver expression is discarded — it must be
                    // pure for that to be observation-free.
                    if drops_receiver && !is_pure_expr(receiver) {
                        self.rejected.insert(key.clone());
                    } else {
                        // Higher-position dead args: params[i+1] maps to
                        // args[i] regardless of whether position 0 was
                        // dropped (the receiver is structural, the
                        // argument-list shape is unchanged).
                        for (i, dead_at_i) in dead.iter().enumerate().skip(1) {
                            if !*dead_at_i {
                                continue;
                            }
                            match args.get(i - 1) {
                                Some(arg) if is_pure_expr(&arg.expr) => {}
                                _ => {
                                    self.rejected.insert(key.clone());
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

fn apply_dae(
    project: &mut FlatPackage,
    confirmed: &IndexMap<FnKey, Vec<bool>>,
    closure_call_keys: &IndexSet<FnKey>,
) {
    // Phase 3a: shrink the parameter list of every confirmed callee, then
    // renumber locals so `params[k].local_index == k` continues to hold.
    // `wir_build/translate.rs` declares `locals[i] for i >= params.len()`
    // as body locals; if the dead-param slot were left in place, its name
    // would either alias a live param's name (duplicate WIR DeclareLocal)
    // or shadow it.
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let key = (func.module_source.clone(), func.name.clone());
        if let Some(dead) = confirmed.get(&key) {
            shrink_params_and_renumber(&mut func, dead);
        }
    }

    // Phase 3b: rewrite every call site.
    let mut rewriter = CallRewriter {
        confirmed,
        closure_call_keys,
    };
    let funcs = project.functions.clone();
    for func_rc in &funcs {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            rewriter.visit_block(body);
        }
    }
    for global in &mut project.globals {
        rewriter.visit_expr(&mut global.initializer);
    }
}

struct CallRewriter<'a> {
    confirmed: &'a IndexMap<FnKey, Vec<bool>>,
    closure_call_keys: &'a IndexSet<FnKey>,
}

impl TirMutVisitor for CallRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        // First descend so nested calls are rewritten with the same rules
        // before we mutate the current expression's shape.
        self.walk_expr(expr);

        match &mut expr.kind {
            TirExprKind::Call { func, args, .. } => {
                let key = (func.module_source.clone(), func.name.clone());
                if let Some(dead) = self.confirmed.get(&key) {
                    let mut i = 0;
                    args.retain(|_| {
                        let alive = !dead.get(i).copied().unwrap_or(false);
                        i += 1;
                        alive
                    });
                }
            }
            TirExprKind::MethodCall { func, args, .. } => {
                let key = (func.module_source.clone(), func.name.clone());
                let Some(dead) = self.confirmed.get(&key).cloned() else {
                    return;
                };
                let drops_receiver =
                    self.closure_call_keys.contains(&key) && dead.first() == Some(&true);
                if drops_receiver {
                    // Closure functor `__call` whose `self` (env) was DAE'd:
                    // collapse `MethodCall(g, __call, args)` to a plain
                    // `Call(__call, surviving_args)`. The receiver
                    // expression has already been verified pure by
                    // `CallSiteValidator`; dropping it is safe.
                    let TirExprKind::MethodCall {
                        func,
                        type_args,
                        args,
                        ..
                    } = std::mem::replace(&mut expr.kind, TirExprKind::Unit)
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
                    expr.kind = TirExprKind::Call {
                        func,
                        type_args,
                        args: new_args,
                    };
                } else {
                    // Dead positions are indexed against the callee's
                    // params; shift by one to skip the receiver position
                    // (kept here either because it is alive or because the
                    // function is not a closure `__call`).
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
}

fn shrink_params_and_renumber(func: &mut TirFunction, dead: &[bool]) {
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

    // Apply the remap to every Local / Capture reference in the body via a
    // generic `TirMutVisitor` walk, overriding only the leaves that carry
    // local indices. The `Closure` arm explicitly stops the walk before
    // entering the closure body — closure-locals live in a separate index
    // namespace and the outer remap must not touch them.
    if let Some(body) = func.body.as_mut() {
        LocalRemap { remap: &remap }.visit_block(body);
    }
}

struct LocalRemap<'a> {
    remap: &'a [Option<u32>],
}

impl LocalRemap<'_> {
    fn lookup(&self, idx: u32) -> u32 {
        self.remap[idx as usize].expect("dead local referenced after DAE rewrite")
    }
}

impl TirMutVisitor for LocalRemap<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        match &mut expr.kind {
            TirExprKind::Local { index, .. } => *index = self.lookup(*index),
            TirExprKind::Closure { captures, .. } => {
                for cap in captures {
                    cap.outer_index = self.lookup(cap.outer_index);
                }
                // Do NOT recurse into the closure body — its `Local` nodes
                // index the closure's own locals, not the outer function's.
            }
            _ => self.walk_expr(expr),
        }
    }

    fn visit_stmt(&mut self, stmt: &mut TirStmt) {
        match &mut stmt.kind {
            TirStmtKind::Let { local_index, .. } => {
                *local_index = self.lookup(*local_index);
                self.walk_stmt(stmt);
            }
            TirStmtKind::VariadicForOf { binding_local, .. } => {
                *binding_local = self.lookup(*binding_local);
                self.walk_stmt(stmt);
            }
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_pattern(&mut self, pattern: &mut TirPattern) {
        if let TirPattern::Binding { local_index, .. } = pattern {
            *local_index = self.lookup(*local_index);
        }
        self.walk_pattern(pattern);
    }
}
