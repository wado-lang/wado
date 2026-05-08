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
//! - Closure functor `__call` methods (vtable-shaped).
//! - Functions whose pointer is taken via `FuncRef` anywhere in the project.
//! - Methods (`method_info.is_some()`) — for the receiver position only;
//!   higher positions are still candidates because they map cleanly onto
//!   `MethodCall.args`. Dropping the receiver would require rewriting the
//!   call shape from `MethodCall` to `Call`, which is out of scope here.

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

    // Phase 1: identify candidate (function, dead positions) pairs.
    let mut candidates: IndexMap<FnKey, Vec<bool>> = IndexMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if !is_eligible(&func, &pinned) {
            continue;
        }
        let dead = find_dead_params(&func);
        if dead.iter().any(|&d| d) {
            candidates.insert(
                (func.module_source.clone(), func.name.clone()),
                dead,
            );
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
/// safe-to-remove. For methods the receiver position (index 0) is always
/// reported as live so we never try to drop it.
fn find_dead_params(func: &TirFunction) -> Vec<bool> {
    if func.params.is_empty() {
        return Vec::new();
    }

    let body = func.body.as_ref().unwrap();
    let mut reads: IndexSet<u32> = IndexSet::default();
    super::elide_local::collect_reads_in_block(body, &mut reads);
    let kept_locals = &func.address_taken_locals;
    let stores_aliased = &func.stores_aliased_locals;

    let mut dead = Vec::with_capacity(func.params.len());
    let receiver_is_self = func.method_info.is_some();
    for (i, p) in func.params.iter().enumerate() {
        if i == 0 && receiver_is_self {
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
    for functor in &project.closure_functors {
        let cm = functor.call_method.borrow();
        pinned.insert((cm.module_source.clone(), cm.name.clone()));
    }
    pinned
}

struct FuncRefCollector<'a> {
    out: &'a mut IndexSet<FnKey>,
}

impl TirRefVisitor for FuncRefCollector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::FuncRef { module_source, name } = &expr.kind {
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
) -> IndexMap<FnKey, Vec<bool>> {
    let mut validator = CallSiteValidator {
        candidates: &candidates,
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
            TirExprKind::MethodCall { func, args, .. } => {
                let key = (func.module_source.clone(), func.name.clone());
                if let Some(dead) = self.candidates.get(&key)
                    && !self.rejected.contains(&key)
                {
                    // params[0] is the receiver (always live by construction
                    // in `find_dead_params`); params[i+1] maps to args[i].
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
            _ => {}
        }
        self.walk_expr(expr);
    }
}


fn apply_dae(project: &mut FlatPackage, confirmed: &IndexMap<FnKey, Vec<bool>>) {
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
    let mut rewriter = CallRewriter { confirmed };
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
}

impl TirMutVisitor for CallRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
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
                if let Some(dead) = self.confirmed.get(&key) {
                    // Dead positions are indexed against the callee's params;
                    // shift by one to skip the receiver, which DAE never drops.
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
        self.walk_expr(expr);
    }
}

fn shrink_params_and_renumber(func: &mut TirFunction, dead: &[bool]) {
    // Compute the set of dead local_indices (one per dead param).
    let mut dead_local_indices: IndexSet<u32> = IndexSet::default();
    for (i, &d) in dead.iter().enumerate() {
        if d
            && let Some(p) = func.params.get(i)
        {
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

