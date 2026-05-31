//! Single-field parameter SROA for Wado NIR.
//!
//! NIR analog of `wir_optimize/sroa_param.rs`. Rewrites internal functions whose
//! parameter type is `&S` / `&mut S` for some single-field struct `S` (with `Box<T>`
//! the canonical case) to take the inner scalar `T` directly. At call sites, the
//! corresponding `StructLiteral S { field: val }` allocation is replaced with
//! `val`, eliminating heap traffic.
//!
//! Running this at NIR (instead of WIR, where the legacy implementation lived)
//! lets the rewritten signature flow through the rest of the optimizer
//! — `inline`, `copy_prop`, `sroa`, `const_fold`, `dce` — and turns the
//! Box-wrapped method-receiver use sites into plain scalar use sites that
//! downstream passes can see and elide.
//!
//! Eligibility (Phase 1): a parameter is a candidate when its type is
//! `Ref(struct_id)` or `MutRef(struct_id)` and the referenced struct has
//! exactly one field. The function must not be pinned (no CM bridges, trait
//! methods, allocators, etc. — same set DAE skips). Address-taken / stores-
//! aliased params are excluded so the rewrite cannot strand a dangling
//! reference.
//!
//! Validation (Phase 2): every read of `Local { index: param.local_index }`
//! must be either
//!
//! 1. wrapped in `FieldAccess { expr: Local(idx), field_name }` (the scalar
//!    read the caller will see post-rewrite), or
//! 2. an argument at a Call / `MethodCall` position whose callee is ALSO a
//!    SROA candidate at the same position (the param flows through chained
//!    helpers untouched).
//!
//! Any other use — taking `&local`, writing the local, passing as receiver of
//! a non-SROA'd method — invalidates the candidate. The validation iterates
//! to a fix-point, so invalidating one candidate may cascade through chains.
//!
//! Rewrite (Phase 3):
//!
//! - **Callee body**: `FieldAccess(Local(idx), field)` → `Local(idx)` (with
//!   the new scalar type). The param's and local's `type_id` are switched to
//!   the inner scalar.
//! - **Caller call sites** (every function + global initializer): at each
//!   SROA'd arg position, `StructLiteral S { field: val }` → `val`. A bare
//!   `Local` whose own param was SROA'd from the same struct type passes
//!   through untouched. Anything else becomes
//!   `FieldAccess(arg, field_name)`.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{CallArg, FunctionKind, NirBlock, NirExpr, NirExprKind, NirFunction, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirMutVisitor, NirRefVisitor};
use crate::tir::{ResolvedType, TypeId, TypeTable};

type FnKey = (ModuleSource, String);

/// Per-candidate metadata captured during Phase 1.
///
/// The struct identity is keyed by `(name, module_source)` rather than by
/// `TypeId`: the NIR type table can intern the same `ResolvedType::Struct`
/// definition at multiple ids (e.g. a fresh entry per resolution site of
/// `Box<char>`), so a `TypeId` equality check at call sites would miss
/// shapes that are structurally the same struct.
#[derive(Clone)]
struct SroaInfo {
    /// Canonical struct identity — `(struct_name, module_source)`.
    struct_key: (String, ModuleSource),
    /// `TypeId` of the wrapper struct as the param resolved to (one of
    /// possibly several equivalent interned ids). Kept for diagnostic
    /// purposes only; comparisons go through `struct_key`.
    #[allow(dead_code)]
    struct_type_id: TypeId,
    /// Type of the wrapper's sole field — the new scalar parameter type.
    inner_type_id: TypeId,
    /// Field name of the wrapper struct's sole field.
    field_name: String,
}

/// Whole-project pass entry point.
///
/// Returns `true` when any rewrite happened. Idempotent: running twice on the
/// same `NirPackage` produces the same result.
pub fn sroa_single_field_parameters(project: &mut NirPackage) -> bool {
    let candidates = collect_and_validate(project);
    if candidates.is_empty() {
        return false;
    }
    rewrite_callees(project, &candidates);
    rewrite_call_sites(project, &candidates);
    true
}

// -----------------------------------------------------------------------
// Phase 1 + 2
// -----------------------------------------------------------------------

/// `(name, module_source)` → sole `(field_name, field_type_id)` of the
/// referenced struct, populated for every single-field struct in the project.
type SingleFieldIndex = IndexMap<(String, ModuleSource), (String, TypeId)>;

fn build_single_field_index(project: &NirPackage) -> SingleFieldIndex {
    let mut out: SingleFieldIndex = IndexMap::default();
    for s in &project.structs {
        if s.fields.len() != 1 {
            continue;
        }
        let f = &s.fields[0];
        out.insert(
            (s.name.clone(), s.module_source.clone()),
            (f.name.clone(), f.type_id),
        );
    }
    out
}

fn collect_and_validate(project: &NirPackage) -> IndexMap<(FnKey, usize), SroaInfo> {
    let type_table = project.type_table.borrow();
    let single_field = build_single_field_index(project);

    // Phase 1: gather every (func, param_idx) whose type points to a
    // single-field struct.
    let mut candidates: IndexMap<(FnKey, usize), SroaInfo> = IndexMap::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if !is_eligible(&func) || func.body.is_none() {
            continue;
        }
        let key: FnKey = (func.module_source.clone(), func.name.clone());
        for (pi, param) in func.params.iter().enumerate() {
            if func.address_taken_locals.contains(&param.local_index) {
                continue;
            }
            if func.stores_aliased_locals.contains(&param.local_index) {
                continue;
            }
            // The function's `stores[...]` clause declares that the
            // listed reference parameters are persisted past the
            // call. SROA-ing such a parameter from `&S` to its inner
            // scalar drops the reference identity the contract is
            // built on (storing `&local` keeps the caller's location
            // alive; storing a `T` value cannot). Pin the param even
            // when `body_uses_param_safely` would otherwise accept
            // it — the analysis defends against escape via bare
            // `Local` uses, but the `stores` contract is satisfied
            // by an indirect persistence (e.g. forwarding `&p` to a
            // callee that itself stores it) that the use checker
            // intentionally treats as a "soft" call-arg use.
            if func.stores.iter().any(|s| s == &param.name) {
                continue;
            }
            let Some(info) = candidate_info_for(param.type_id, &type_table, &single_field) else {
                continue;
            };
            // Aliasing guard: skip a reference parameter that may be
            // caller-aliased with another reference parameter of the same
            // single-field struct (see `param_may_alias_sibling`). Snapshotting
            // it to a by-value scalar at the call site would drop writes made
            // through the aliasing parameter during the call.
            if param_may_alias_sibling(&func, pi, &info.struct_key, &type_table) {
                continue;
            }
            candidates.insert((key.clone(), pi), info);
        }
    }
    if candidates.is_empty() {
        return candidates;
    }

    // Phase 2: drop candidates whose param escapes. The "call arg at a
    // SROA'd position" rule depends on the candidate set, so invalidations
    // can cascade — iterate to a fix-point.
    loop {
        let mut invalid: IndexSet<(FnKey, usize)> = IndexSet::default();
        for ((key, pi), _info) in &candidates {
            let Some(func_rc) = lookup_function(project, key) else {
                invalid.insert((key.clone(), *pi));
                continue;
            };
            let func = func_rc.borrow();
            let local_index = func.params[*pi].local_index;
            let body = func.body.as_ref().unwrap();
            if !body_uses_param_safely(body, local_index, &candidates) {
                invalid.insert((key.clone(), *pi));
            }
        }
        if invalid.is_empty() {
            break;
        }
        for k in &invalid {
            candidates.swap_remove(k);
        }
    }

    candidates
}

fn candidate_info_for(
    param_type: TypeId,
    type_table: &TypeTable,
    single_field: &SingleFieldIndex,
) -> Option<SroaInfo> {
    // Wado structs are GC-allocated reference types, so a function param
    // of struct type is already an indirect handle. NIR may surface this
    // as the bare `Struct` resolved type or as `Ref(Struct)` / `MutRef(Struct)`
    // depending on the source-level `&` / `&mut` annotation. Accept all
    // three shapes; the struct's identity is what matters.
    let struct_type_id = match type_table.get(param_type) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
        ResolvedType::Struct { .. } => param_type,
        _ => return None,
    };
    let (struct_name, struct_module) = match type_table.get(struct_type_id) {
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => (name.clone(), module_source.clone()),
        _ => return None,
    };
    let key = (struct_name, struct_module);
    let (field_name, inner_type_id) = single_field.get(&key)?.clone();
    // Reject inner field types with no Wasm representation: replacing
    // the wrapper parameter with `Unit` would yield a signature whose
    // arity disagrees with surviving non-SROA call sites (the WIR
    // builder filters Unit out of the Wasm result/params list), and
    // would otherwise propagate `Unit` into a slot the inliner / WIR
    // builder treat as a runtime value.
    if !is_sroa_eligible_inner_type(inner_type_id, type_table) {
        return None;
    }
    Some(SroaInfo {
        struct_key: key,
        struct_type_id,
        inner_type_id,
        field_name,
    })
}

fn is_sroa_eligible_inner_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    // Mirrors `wir_optimize/sroa_variant_return::is_eligible_field_type`
    // intent at NIR. Unit has no Wasm representation; replacing a
    // wrapper struct with a Unit-typed parameter strips the
    // corresponding arg slot and would mis-match against any surviving
    // call site that keeps the original signature. Never has no
    // run-time presence either.
    if type_id == crate::tir::TypeTable::UNIT || type_id == crate::tir::TypeTable::NEVER {
        return false;
    }
    // Abstract heap refs (e.g. `ref null any`) carry no precise
    // structural type, so the call-site `FieldAccess` extraction would
    // have no field to look up. Be defensive against these.
    if matches!(type_table.get(type_id), ResolvedType::Function { .. }) {
        // Function values are nullable references but the inner extraction
        // works through `FieldAccess` only when the wrapper struct has a
        // function-typed field by name. The single_field index already
        // recorded the field name, so a Function field is acceptable.
        // (Kept as a no-op branch for symmetry with future refinements.)
    }
    true
}

fn struct_key_of(type_id: TypeId, type_table: &TypeTable) -> Option<(String, ModuleSource)> {
    match type_table.get(type_id) {
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => Some((name.clone(), module_source.clone())),
        _ => None,
    }
}

/// The struct identity a *reference* parameter points at, or `None` for a
/// by-value struct param (a value-semantics copy that cannot alias the
/// caller's object) or a non-struct type. Only reference parameters carry the
/// caller's object identity and can therefore alias another reference.
fn reference_param_struct_key(
    type_id: TypeId,
    type_table: &TypeTable,
) -> Option<(String, ModuleSource)> {
    match type_table.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            struct_key_of(*inner, type_table)
        }
        _ => None,
    }
}

/// True when SROA-ing reference parameter `pi` of `func` would be unsound
/// because another reference parameter of the same function points at the same
/// single-field struct and may be caller-aliased.
///
/// Wado has no borrow checker and references alias (spec: "references alias,
/// every read and write lands on the same location"), so `f(&mut n, &mut n)`
/// is legal and the two parameters must observe each other's writes. SROA
/// rewrites a reference parameter to a by-value scalar read at the call site
/// (`f(n.value)`), snapshotting it — a write to the shared object through the
/// other parameter during the call would then be lost. By-value struct params
/// are independent copies, so only reference params count as aliases.
fn param_may_alias_sibling(
    func: &NirFunction,
    pi: usize,
    struct_key: &(String, ModuleSource),
    type_table: &TypeTable,
) -> bool {
    let pointee = func
        .params
        .get(pi)
        .map(|p| reference_param_struct_key(p.type_id, type_table));
    // The candidate itself must be a reference for the aliasing to matter.
    if pointee != Some(Some(struct_key.clone())) {
        return false;
    }
    func.params.iter().enumerate().any(|(pj, other)| {
        pj != pi
            && reference_param_struct_key(other.type_id, type_table).as_ref() == Some(struct_key)
    })
}

/// Walk `body` once and confirm every use of `local_index` is either
/// `FieldAccess(Local(idx), _)`, or an arg at a call position that is
/// already in `candidates`. Any other shape invalidates SROA for this param.
fn body_uses_param_safely(
    body: &NirBlock,
    local_index: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> bool {
    let mut checker = UseChecker {
        local_index,
        candidates,
        valid: true,
    };
    checker.visit_block(body);
    checker.valid
}

struct UseChecker<'a> {
    local_index: u32,
    candidates: &'a IndexMap<(FnKey, usize), SroaInfo>,
    valid: bool,
}

impl NirRefVisitor for UseChecker<'_> {
    fn visit_expr(&mut self, expr: &NirExpr) {
        if !self.valid {
            return;
        }
        match &expr.kind {
            // Bare local read at top level — only OK if the enclosing
            // construct (FieldAccess / Call arg / MethodCall receiver)
            // consumed it without recursing here. Reaching here means it
            // appears unwrapped → invalid.
            NirExprKind::Local { index, .. } if *index == self.local_index => {
                self.valid = false;
            }
            NirExprKind::FieldAccess { expr: inner, .. } => {
                if let NirExprKind::Local { index, .. } = &inner.kind
                    && *index == self.local_index
                {
                    // Allowed shape — don't recurse into the Local.
                    return;
                }
                self.visit_expr(inner);
            }
            NirExprKind::Call { func, args, .. } => {
                let key: FnKey = (func.module_source.clone(), func.name.clone());
                for (idx, arg) in args.iter().enumerate() {
                    self.check_call_arg(&key, idx, &arg.expr);
                    if !self.valid {
                        return;
                    }
                }
            }
            NirExprKind::MethodCall {
                receiver,
                func,
                args,
                ..
            } => {
                let key: FnKey = (func.module_source.clone(), func.name.clone());
                self.check_call_arg(&key, 0, receiver);
                if !self.valid {
                    return;
                }
                for (idx, arg) in args.iter().enumerate() {
                    self.check_call_arg(&key, idx + 1, &arg.expr);
                    if !self.valid {
                        return;
                    }
                }
            }
            NirExprKind::Assign { target, value } => {
                // Any write — through the param directly, through a field
                // of it, through an index into it, or through a deref —
                // would after SROA mutate a by-value local instead of the
                // caller's referent, silently dropping the side effect.
                // Conservatively reject when the target's place expression
                // is rooted at this param.
                if place_root_local(target) == Some(self.local_index) {
                    self.valid = false;
                    return;
                }
                self.visit_expr(target);
                if !self.valid {
                    return;
                }
                self.visit_expr(value);
            }
            _ => self.walk_expr(expr),
        }
    }
}

/// Return the bottom-most `Local { index }` of a place expression, or
/// `None` if the expression is not a write-target shape rooted at a local.
/// `*x.f`, `x.f.g`, `x[i]`, plain `x` all resolve to `Some(x_index)`.
fn place_root_local(target: &NirExpr) -> Option<u32> {
    match &target.kind {
        NirExprKind::Local { index, .. } => Some(*index),
        NirExprKind::FieldAccess { expr, .. } => place_root_local(expr),
        NirExprKind::Index { expr, .. } => place_root_local(expr),
        NirExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr,
        } => place_root_local(expr),
        _ => None,
    }
}

impl UseChecker<'_> {
    fn check_call_arg(&mut self, callee: &FnKey, pos: usize, arg: &NirExpr) {
        if let NirExprKind::Local { index, .. } = &arg.kind
            && *index == self.local_index
        {
            if !self.candidates.contains_key(&(callee.clone(), pos)) {
                self.valid = false;
            }
            return;
        }
        self.visit_expr(arg);
    }
}

// -----------------------------------------------------------------------
// Phase 3a: callee body rewrite
// -----------------------------------------------------------------------

fn rewrite_callees(project: &mut NirPackage, candidates: &IndexMap<(FnKey, usize), SroaInfo>) {
    let funcs = project.functions.clone();
    for func_rc in &funcs {
        let mut func = func_rc.borrow_mut();
        let key: FnKey = (func.module_source.clone(), func.name.clone());
        // Collect (local_index, field_name) for every SROA'd param in
        // this function. Update both `params` and `locals` type ids.
        let mut affected: Vec<(u32, String)> = Vec::new();
        for pi in 0..func.params.len() {
            if let Some(info) = candidates.get(&(key.clone(), pi)) {
                let local_index = func.params[pi].local_index;
                affected.push((local_index, info.field_name.clone()));
                func.params[pi].type_id = info.inner_type_id;
                if let Some(local) = func.locals.get_mut(local_index as usize) {
                    local.type_id = info.inner_type_id;
                }
            }
        }
        if affected.is_empty() {
            continue;
        }
        if let Some(body) = func.body.as_mut() {
            let mut rewriter = ParamReadRewriter {
                affected: &affected,
            };
            rewriter.visit_block(body);
        }
    }
}

struct ParamReadRewriter<'a> {
    /// `(local_index, expected_field_name)` for each SROA'd param.
    affected: &'a [(u32, String)],
}

impl NirMutVisitor for ParamReadRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        // Pre-order: decide on the current expr based on its ORIGINAL shape
        // before any child rewrite reshapes it. A post-order walk would
        // rewrite an inner `FieldAccess(Local(b), F)` to `Local(b)` first,
        // and an outer `FieldAccess(_, F)` would then re-match the same
        // candidate even though it's a field access on a different struct.
        let should_replace = if let NirExprKind::FieldAccess {
            expr: inner,
            field_name,
            ..
        } = &expr.kind
        {
            if let NirExprKind::Local { index, .. } = &inner.kind {
                self.affected
                    .iter()
                    .any(|(idx, fname)| idx == index && fname == field_name)
            } else {
                false
            }
        } else {
            false
        };
        if should_replace {
            let NirExprKind::FieldAccess { expr: inner, .. } =
                std::mem::replace(&mut expr.kind, NirExprKind::Unit)
            else {
                unreachable!();
            };
            // Move the inner Local out (with its name) and use the outer
            // expr's type_id (which is the inner field's type — the new
            // scalar param type). The replacement is a bare Local; nothing
            // more to walk.
            *expr = NirExpr::new(inner.kind, expr.type_id, expr.span);
            return;
        }
        self.walk_expr(expr);
    }
}

// -----------------------------------------------------------------------
// Phase 3b: call-site rewrite
// -----------------------------------------------------------------------

fn rewrite_call_sites(project: &mut NirPackage, candidates: &IndexMap<(FnKey, usize), SroaInfo>) {
    // (callee_key, param_idx) → SroaInfo, regrouped by callee for cheap
    // lookup inside the visitor.
    let mut sroa_positions: IndexMap<FnKey, IndexMap<usize, SroaInfo>> = IndexMap::default();
    for ((key, pi), info) in candidates {
        sroa_positions
            .entry(key.clone())
            .or_default()
            .insert(*pi, info.clone());
    }

    let type_table_rc = project.type_table.clone();
    let funcs = project.functions.clone();
    for func_rc in &funcs {
        let mut func = func_rc.borrow_mut();
        let key: FnKey = (func.module_source.clone(), func.name.clone());
        // Per-caller: which of its locals correspond to a scalar SROA'd
        // param, with the canonical struct identity they were unwrapped
        // from.
        let mut scalar_param_struct: IndexMap<u32, (String, ModuleSource)> = IndexMap::default();
        for (pi, param) in func.params.iter().enumerate() {
            if let Some(info) = candidates.get(&(key.clone(), pi)) {
                scalar_param_struct.insert(param.local_index, info.struct_key.clone());
            }
        }
        if let Some(body) = func.body.as_mut() {
            let type_table = type_table_rc.borrow();
            let mut rewriter = CallArgRewriter {
                sroa_positions: &sroa_positions,
                scalar_param_struct: &scalar_param_struct,
                type_table: &type_table,
            };
            rewriter.visit_block(body);
        }
    }
    for global in &mut project.globals {
        let empty = IndexMap::default();
        let type_table = type_table_rc.borrow();
        let mut rewriter = CallArgRewriter {
            sroa_positions: &sroa_positions,
            scalar_param_struct: &empty,
            type_table: &type_table,
        };
        rewriter.visit_expr(&mut global.initializer);
    }
}

struct CallArgRewriter<'a> {
    sroa_positions: &'a IndexMap<FnKey, IndexMap<usize, SroaInfo>>,
    scalar_param_struct: &'a IndexMap<u32, (String, ModuleSource)>,
    type_table: &'a TypeTable,
}

impl NirMutVisitor for CallArgRewriter<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        self.walk_expr(expr);

        match &mut expr.kind {
            NirExprKind::Call { func, args, .. } => {
                let key: FnKey = (func.module_source.clone(), func.name.clone());
                if let Some(positions) = self.sroa_positions.get(&key) {
                    let positions = positions.clone();
                    for (pi, info) in &positions {
                        if *pi < args.len() {
                            rewrite_arg(
                                &mut args[*pi].expr,
                                info,
                                self.scalar_param_struct,
                                self.type_table,
                            );
                        }
                    }
                }
            }
            NirExprKind::MethodCall { func, .. } => {
                let key: FnKey = (func.module_source.clone(), func.name.clone());
                if let Some(positions) = self.sroa_positions.get(&key).cloned() {
                    let receiver_rewritten = positions.contains_key(&0);
                    if receiver_rewritten {
                        // Receiver position is SROA'd, so its static type
                        // changes from `&Wrapper` to the inner scalar.
                        // NIR DCE dispatches non-monomorphized methods by
                        // the receiver's type — keeping the call as a
                        // `MethodCall` would have DCE look up
                        // `Wrapper::get` on `i32`. Re-emit as a `Call`
                        // with the receiver shifted into `args[0]` so
                        // the lookup goes through (module, name).
                        let kind = std::mem::replace(&mut expr.kind, NirExprKind::Unit);
                        let NirExprKind::MethodCall {
                            receiver,
                            func,
                            type_args,
                            mut args,
                            ..
                        } = kind
                        else {
                            unreachable!();
                        };
                        let mut new_receiver = *receiver;
                        if let Some(info) = positions.get(&0) {
                            rewrite_arg(
                                &mut new_receiver,
                                info,
                                self.scalar_param_struct,
                                self.type_table,
                            );
                        }
                        for (pi, info) in &positions {
                            if *pi == 0 {
                                continue;
                            }
                            let arg_idx = *pi - 1;
                            if arg_idx < args.len() {
                                rewrite_arg(
                                    &mut args[arg_idx].expr,
                                    info,
                                    self.scalar_param_struct,
                                    self.type_table,
                                );
                            }
                        }
                        let mut new_args = Vec::with_capacity(args.len() + 1);
                        new_args.push(CallArg::new(new_receiver, false));
                        new_args.extend(args);
                        expr.kind = NirExprKind::Call {
                            func,
                            type_args,
                            args: new_args,
                        };
                    } else {
                        // Only non-receiver params are SROA'd; the
                        // receiver's static type is unchanged so DCE /
                        // the inliner can still dispatch by type. Keep
                        // the `MethodCall` and rewrite only the affected
                        // args in place.
                        let NirExprKind::MethodCall { args, .. } = &mut expr.kind else {
                            unreachable!();
                        };
                        for (pi, info) in &positions {
                            // Position 0 is the receiver, args[i] corresponds
                            // to position i+1 in the callee's param list.
                            let arg_idx = pi.saturating_sub(1);
                            if *pi >= 1 && arg_idx < args.len() {
                                rewrite_arg(
                                    &mut args[arg_idx].expr,
                                    info,
                                    self.scalar_param_struct,
                                    self.type_table,
                                );
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn rewrite_arg(
    arg: &mut NirExpr,
    info: &SroaInfo,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
    type_table: &TypeTable,
) {
    // Peel auto-ref wrappers (`&x`, `&mut x`) introduced by the lower phase
    // when the receiver / arg was source-level `&self` / `&local`. After
    // SROA the callee expects the inner scalar; an outer Ref-of-FieldAccess
    // would have type `&T` instead of `T`, confusing the inliner's type-
    // driven param binding (it would emit `let self: T = &x.field;` and
    // downstream passes treat the local as `&T`).
    if matches!(
        arg.kind,
        NirExprKind::Unary {
            op: NirUnaryOp::Ref,
            ..
        } | NirExprKind::Unary {
            op: NirUnaryOp::MutRef,
            ..
        }
    ) {
        let kind = std::mem::replace(&mut arg.kind, NirExprKind::Unit);
        let NirExprKind::Unary { expr: inner, .. } = kind else {
            unreachable!();
        };
        *arg = *inner;
    }

    // Case 1: StructLiteral matching the wrapper's canonical identity → unwrap.
    if let NirExprKind::StructLiteral {
        struct_type,
        fields,
        ..
    } = &arg.kind
        && struct_key_of(*struct_type, type_table).as_ref() == Some(&info.struct_key)
        && fields.len() == 1
    {
        let NirExprKind::StructLiteral { mut fields, .. } =
            std::mem::replace(&mut arg.kind, NirExprKind::Unit)
        else {
            unreachable!();
        };
        *arg = fields.remove(0).value;
        return;
    }

    // Case 2: Local(x) where the caller's local at this index is itself a
    // scalar SROA'd param of the SAME canonical struct identity.
    if let NirExprKind::Local { index, .. } = &arg.kind
        && scalar_param_struct.get(index) == Some(&info.struct_key)
    {
        arg.type_id = info.inner_type_id;
        return;
    }

    // Case 3: general — extract the field via FieldAccess.
    let span = arg.span;
    let original = std::mem::replace(
        arg,
        NirExpr::new(NirExprKind::Unit, info.inner_type_id, span),
    );
    *arg = NirExpr::new(
        NirExprKind::FieldAccess {
            expr: Box::new(original),
            field_index: 0,
            field_name: info.field_name.clone(),
        },
        info.inner_type_id,
        span,
    );
}

// -----------------------------------------------------------------------
// Pinning + lookup helpers
// -----------------------------------------------------------------------

fn lookup_function<'a>(
    project: &'a NirPackage,
    key: &FnKey,
) -> Option<&'a std::rc::Rc<std::cell::RefCell<NirFunction>>> {
    project.functions.iter().find(|f| {
        let f = f.borrow();
        f.module_source == key.0 && f.name == key.1
    })
}

/// Same pinning rules DAE uses — see `optimize::dae::is_eligible`. A pinned
/// function's signature is part of an ABI contract (CM export wrapper, trait
/// dispatch slot, allocator entry point, …) that the rewrite must not
/// disturb.
fn is_eligible(func: &NirFunction) -> bool {
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
    if let Some(mi) = func.method_info.as_ref()
        && mi.trait_name.is_some()
    {
        return false;
    }
    // Closure functor `__call` methods have a `self` parameter whose type
    // is the functor struct itself. `wir_build::register_closure_wrappers`
    // names every wrapper arg from the `__call` params via the
    // `canonical_user_params` snapshot — `self` is expected to be the
    // first entry. Rewriting that `self` to its inner scalar (when the
    // functor struct happens to have a single captured field) breaks the
    // name match and trips the assertion in `wir_build/translate.rs`.
    if func.is_closure_call() {
        return false;
    }
    // Synthesized `$value_copy$T<id>` helpers carry an aliasing contract
    // that `optimize::alias::recognize_value_copy` and the const-fold
    // visitor's field-knowledge tracking rely on: a `Call(helper, [arg])`
    // is treated as "the result has the same field values as `arg` at
    // this point." Rewriting the helper's parameter from `T` to its
    // inner scalar drops the struct shape on the floor, so the
    // alias-aware field knowledge can no longer flow through the
    // `let snapshot = c` → `$value_copy$T(c)` → snapshot pattern.
    if func.is_value_copy() {
        return false;
    }
    true
}
