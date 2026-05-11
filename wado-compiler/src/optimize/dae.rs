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
//! - Synthesised CM bridges (`is_cm_export`, `is_cm_binding`,
//!   `is_dispatch_wrapper`). These sit on the world boundary; their
//!   signatures are observed by the host runtime, not just by the
//!   in-project call graph.
//! - `is_ambient` functions (special call shapes — effect-handler
//!   dispatch carries an implicit handler param the call sites assume).
//! - Non-`Regular` `FunctionKind` entries (specialised stubs).
//! - Builtin / wasm-asset modules (their signatures are part of the ABI).
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

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{
    FunctionKind, NirExpr, NirExprKind, NirFunction, NirPattern, NirStmt, NirStmtKind,
};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirMutVisitor, NirRefVisitor};

use super::elide_local::is_pure_expr;

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
///   `lower::closure`'s fn-param-specialisation `MethodCall(g, __call,
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
    // `is_export` and `is_async` are intentionally absent. Both flags
    // describe the user's source-level intent (this is exported / this
    // was originally `async`), not a real-call-shape constraint that
    // DAE would have to honour:
    //
    // * Every `export fn` reaches the runtime through a synthesised
    //   `is_cm_export` wrapper. The wrapper is the boundary; the user
    //   function is internal-only, with the wrapper as its sole caller
    //   — and the rewriter updates that call site. Pinning the user
    //   function blocked DAE for arguments like an unused `request` on
    //   `export async fn handle(request)`.
    //
    // * `is_async` propagates from desugar untouched, but the body is
    //   already lowered to `cm_raw_call task-return(...)` and the call
    //   shape from the `is_cm_export` wrapper is a regular `Call` —
    //   the async ABI flattening (outptr / indirect params) only
    //   applies to WASI imports inside `wir_build`, not to user
    //   functions. The legacy WIR DAE did not check `is_async`.
    if func.is_cm_export || func.is_cm_binding || func.is_dispatch_wrapper || func.is_ambient {
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
    //
    // The closure-functor `^Inspect` / `^InspectAlt` impls are the
    // exception: their only callers are the matching
    // `__closure_inspect_wrapper_*` (vtable-shaped, but the wrapper body
    // is generated per-functor and adapts to surviving impl params), so
    // shrinking the impl signature is safe.
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

/// Returns one bool per parameter: `true` means the parameter is unused
/// and safe-to-remove. The receiver position (index 0 for methods) gets
/// no special pin — `apply_dae` knows how to rewrite a method whose
/// `self` is dead into a plain `Call(method_func, args)` at every call
/// site (`MethodCall → Call` collapse, see `CallRewriter`). The
/// validator (`CallSiteValidator`) gates this on receiver purity so
/// that dropping the receiver evaluation cannot strip an observable
/// effect.
fn find_dead_params(func: &NirFunction) -> Vec<bool> {
    if func.params.is_empty() {
        return Vec::new();
    }

    let body = func.body.as_ref().unwrap();
    let mut reads: IndexSet<u32> = IndexSet::default();
    super::elide_local::collect_reads_in_block(body, &mut reads);
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

pub(super) fn collect_pinned(project: &NirPackage) -> IndexSet<FnKey> {
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

impl NirRefVisitor for FuncRefCollector<'_> {
    fn visit_expr(&mut self, expr: &NirExpr) {
        if let NirExprKind::FuncRef {
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
    project: &NirPackage,
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

impl NirRefVisitor for CallSiteValidator<'_> {
    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Call { func, args, .. } => {
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
            NirExprKind::MethodCall {
                func,
                receiver,
                args,
                ..
            } => {
                let key = (func.module_source.clone(), func.name.clone());
                if let Some(dead) = self.candidates.get(&key)
                    && !self.rejected.contains(&key)
                {
                    // If the rewriter is going to drop the receiver, the
                    // MethodCall collapses to a `Call` and the receiver
                    // expression is discarded — it must be pure for that to
                    // be observation-free.
                    let drops_receiver = dead.first() == Some(&true);
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

fn apply_dae(project: &mut NirPackage, confirmed: &IndexMap<FnKey, Vec<bool>>) {
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

impl NirMutVisitor for CallRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        // First descend so nested calls are rewritten with the same rules
        // before we mutate the current expression's shape.
        self.walk_expr(expr);

        match &mut expr.kind {
            NirExprKind::Call { func, args, .. } => {
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
            NirExprKind::MethodCall { func, args, .. } => {
                let key = (func.module_source.clone(), func.name.clone());
                let Some(dead) = self.confirmed.get(&key).cloned() else {
                    return;
                };
                let drops_receiver = dead.first() == Some(&true);
                if drops_receiver {
                    // The callee's receiver was DAE'd: collapse
                    // `MethodCall(recv, name, args)` to a plain
                    // `Call(method_func, surviving_args)`. The receiver
                    // expression has already been verified pure by
                    // `CallSiteValidator`; dropping it is safe.
                    let NirExprKind::MethodCall {
                        func,
                        type_args,
                        args,
                        ..
                    } = std::mem::replace(&mut expr.kind, NirExprKind::Unit)
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
                    expr.kind = NirExprKind::Call {
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

    // Apply the remap to every Local / Capture reference in the body via a
    // generic `NirMutVisitor` walk, overriding only the leaves that carry
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

impl NirMutVisitor for LocalRemap<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        match &mut expr.kind {
            NirExprKind::Local { index, .. } => *index = self.lookup(*index),
            NirExprKind::Closure { captures, .. } => {
                for cap in captures {
                    cap.outer_index = self.lookup(cap.outer_index);
                }
                // Do NOT recurse into the closure body — its `Local` nodes
                // index the closure's own locals, not the outer function's.
            }
            _ => self.walk_expr(expr),
        }
    }

    fn visit_stmt(&mut self, stmt: &mut NirStmt) {
        match &mut stmt.kind {
            NirStmtKind::Let { local_index, .. } => {
                *local_index = self.lookup(*local_index);
                self.walk_stmt(stmt);
            }
            NirStmtKind::VariadicForOf { binding_local, .. } => {
                *binding_local = self.lookup(*binding_local);
                self.walk_stmt(stmt);
            }
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_pattern(&mut self, pattern: &mut NirPattern) {
        if let NirPattern::Binding { local_index, .. } = pattern {
            *local_index = self.lookup(*local_index);
        }
        self.walk_pattern(pattern);
    }
}
