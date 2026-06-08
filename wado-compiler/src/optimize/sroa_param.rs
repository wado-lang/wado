//! Single-field parameter SROA for Wado NIR.
//!
//! NIR analog of `wir_optimize/sroa_param.rs`. Rewrites internal functions whose
//! parameter type is `&S` / `&mut S` for some single-field struct `S` (with `Box<T>`
//! the canonical case) to take the inner scalar `T` directly. At call sites, the
//! corresponding `StructLiteral S { field: val }` allocation is replaced with
//! `val`, eliminating heap traffic.
//!
//! Eligibility (Phase 1): a parameter is a candidate when its type is
//! `Ref(struct_id)` / `MutRef(struct_id)` / bare `Struct` and the referenced
//! struct has exactly one field. The function must not be pinned. Address-taken
//! / stores-aliased / `stores`-declared params are excluded.
//!
//! Validation (Phase 2): every read of the param local must be either a
//! `FieldAccess(Local(idx), field)` (the scalar read) or an arg at a Call /
//! `MethodCall` position whose callee is ALSO a candidate at that position.
//! Iterates to a fix-point so cascades settle.
//!
//! Rewrite (Phase 3): callee bodies turn `FieldAccess(Local, field)` into the
//! scalar `Local`; call sites unwrap `StructLiteral { field: val }` to `val`
//! (or extract via `FieldAccess`), collapsing a receiver-SROA'd `MethodCall`
//! into a `Call`.
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): the validation walk and
//! both rewrite phases read and mutate the arena `Body` directly; global
//! initializers are arena bodies too, so the call-site rewrite runs on them
//! directly.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FunctionKind, NirFunction, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, Body, ExprId, ExprKind, ExprNode, NodeRef};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use cranelift_entity::EntityRef;

use super::arena_query::place_root_local;
use super::gate::{FunctionGate, FunctionId};

type FnKey = (ModuleSource, String);

/// Per-candidate metadata captured during Phase 1.
#[derive(Clone)]
struct SroaInfo {
    /// Canonical struct identity — `(struct_name, module_source)`.
    struct_key: (String, ModuleSource),
    #[allow(dead_code)]
    struct_type_id: TypeId,
    /// Type of the wrapper's sole field — the new scalar parameter type.
    inner_type_id: TypeId,
    /// Field name of the wrapper struct's sole field.
    field_name: String,
}

pub fn sroa_single_field_parameters(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let candidates = collect_and_validate(project);
    if candidates.is_empty() {
        return false;
    }
    // Interprocedural but not gate-skipped; report the functions it touched
    // (param-scalarized callees + callers whose call sites were rewritten) so
    // the gated passes re-examine only those instead of every function via
    // `bump_all`. The call graph is unaffected: arg rewrites and the
    // `MethodCall` → `Call` collapse keep the same callee, so no refresh.
    let mut touched: IndexSet<usize> = IndexSet::default();
    rewrite_callees(project, &candidates, &mut touched);
    rewrite_call_sites(project, &candidates, &mut touched);
    for idx in touched {
        gate.mark_changed(FunctionId::new(idx));
    }
    true
}

/// Move `src`'s node content into `id`; `src` is left as a dead `Unit`.
fn become_expr(body: &mut Body, id: ExprId, src: ExprId) {
    if id == src {
        return;
    }
    let ty = body.exprs[src].type_id;
    let span = body.exprs[src].span;
    let node = std::mem::replace(
        &mut body.exprs[src],
        ExprNode {
            kind: ExprKind::Unit,
            type_id: ty,
            span,
        },
    );
    body.exprs[id] = node;
}

// -----------------------------------------------------------------------
// Phase 1 + 2
// -----------------------------------------------------------------------

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
            if func.stores.iter().any(|s| s == &param.name) {
                continue;
            }
            let Some(info) = candidate_info_for(param.type_id, &type_table, &single_field) else {
                continue;
            };
            if param_may_alias_sibling(&func, pi, &info.struct_key, &type_table) {
                continue;
            }
            candidates.insert((key.clone(), pi), info);
        }
    }
    if candidates.is_empty() {
        return candidates;
    }

    // Phase 2: drop candidates whose param escapes — iterate to a fix-point.
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

fn is_sroa_eligible_inner_type(type_id: TypeId, _type_table: &TypeTable) -> bool {
    if type_id == crate::tir::TypeTable::UNIT || type_id == crate::tir::TypeTable::NEVER {
        return false;
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

fn reference_param_struct_key(
    type_id: TypeId,
    type_table: &TypeTable,
) -> Option<(String, ModuleSource)> {
    match type_table.get(type_id) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => struct_key_of(*inner, type_table),
        _ => None,
    }
}

fn param_may_alias_sibling(
    func: &NirFunction,
    pi: usize,
    struct_key: &(String, ModuleSource),
    type_table: &TypeTable,
) -> bool {
    let candidate_is_ref = func
        .params
        .get(pi)
        .map(|p| reference_param_struct_key(p.type_id, type_table))
        == Some(Some(struct_key.clone()));
    if !candidate_is_ref {
        return false;
    }
    func.params.iter().enumerate().any(|(pj, other)| {
        pj != pi
            && matches!(type_table.get(other.type_id), ResolvedType::MutRef(_))
            && reference_param_struct_key(other.type_id, type_table).as_ref() == Some(struct_key)
    })
}

// -----------------------------------------------------------------------
// Phase 2 — use checker (arena)
// -----------------------------------------------------------------------

fn body_uses_param_safely(
    body: &Body,
    local_index: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> bool {
    check_node(body, NodeRef::Block(body.root), local_index, candidates)
}

fn check_node(
    body: &Body,
    node: NodeRef,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> bool {
    if let NodeRef::Expr(id) = node {
        check_expr(body, id, idx, candidates)
    } else {
        let mut kids = Vec::new();
        body.for_each_child(node, |c| kids.push(c));
        kids.into_iter()
            .all(|c| check_node(body, c, idx, candidates))
    }
}

fn check_expr(
    body: &Body,
    id: ExprId,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> bool {
    match &body.exprs[id].kind {
        // Bare local read reaching here (not consumed by a borrowing parent)
        // is an unwrapped use → invalid.
        ExprKind::Local { index, .. } if *index == idx => false,
        ExprKind::FieldAccess { expr: inner, .. } => {
            let inner = *inner;
            if matches!(&body.exprs[inner].kind, ExprKind::Local { index, .. } if *index == idx) {
                return true;
            }
            check_expr(body, inner, idx, candidates)
        }
        ExprKind::Call { func, args, .. } => {
            let key: FnKey = (func.module_source.clone(), func.name.clone());
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            args.iter()
                .enumerate()
                .all(|(i, &a)| check_call_arg(body, &key, i, a, idx, candidates))
        }
        ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let key: FnKey = (func.module_source.clone(), func.name.clone());
            let receiver = *receiver;
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            check_call_arg(body, &key, 0, receiver, idx, candidates)
                && args
                    .iter()
                    .enumerate()
                    .all(|(i, &a)| check_call_arg(body, &key, i + 1, a, idx, candidates))
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if place_root_local(body, target) == Some(idx) {
                return false;
            }
            check_expr(body, target, idx, candidates) && check_expr(body, value, idx, candidates)
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            kids.into_iter()
                .all(|c| check_node(body, c, idx, candidates))
        }
    }
}

fn check_call_arg(
    body: &Body,
    callee: &FnKey,
    pos: usize,
    arg: ExprId,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> bool {
    if matches!(&body.exprs[arg].kind, ExprKind::Local { index, .. } if *index == idx) {
        return candidates.contains_key(&(callee.clone(), pos));
    }
    check_expr(body, arg, idx, candidates)
}

// -----------------------------------------------------------------------
// Phase 3a: callee body rewrite (arena)
// -----------------------------------------------------------------------

fn rewrite_callees(
    project: &mut NirPackage,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
    touched: &mut IndexSet<usize>,
) {
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        let key: FnKey = (func.module_source.clone(), func.name.clone());
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
            let root = body.root;
            rewrite_param_reads(body, NodeRef::Block(root), &affected);
        }
        touched.insert(i);
    }
}

/// Pre-order: replace `FieldAccess(Local(idx), field)` for a SROA'd `(idx,
/// field)` with the bare scalar `Local`, before children are reshaped.
fn rewrite_param_reads(body: &mut Body, node: NodeRef, affected: &[(u32, String)]) {
    if let NodeRef::Expr(id) = node {
        let replace = if let ExprKind::FieldAccess {
            expr: inner,
            field_name,
            ..
        } = &body.exprs[id].kind
        {
            matches!(&body.exprs[*inner].kind, ExprKind::Local { index, .. }
                if affected.iter().any(|(li, fname)| li == index && fname == field_name))
        } else {
            false
        };
        if replace {
            let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[id].kind else {
                unreachable!();
            };
            let inner = *inner;
            // The node keeps its (field-scalar) type_id / span; its kind becomes
            // the inner Local.
            body.exprs[id].kind = body.exprs[inner].kind.clone();
            return;
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        rewrite_param_reads(body, c, affected);
    }
}

// -----------------------------------------------------------------------
// Phase 3b: call-site rewrite (arena)
// -----------------------------------------------------------------------

fn rewrite_call_sites(
    project: &mut NirPackage,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
    touched: &mut IndexSet<usize>,
) {
    let mut sroa_positions: IndexMap<FnKey, IndexMap<usize, SroaInfo>> = IndexMap::default();
    for ((key, pi), info) in candidates {
        sroa_positions
            .entry(key.clone())
            .or_default()
            .insert(*pi, info.clone());
    }

    let type_table_rc = project.type_table.clone();
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        let key: FnKey = (func.module_source.clone(), func.name.clone());
        let mut scalar_param_struct: IndexMap<u32, (String, ModuleSource)> = IndexMap::default();
        for (pi, param) in func.params.iter().enumerate() {
            if let Some(info) = candidates.get(&(key.clone(), pi)) {
                scalar_param_struct.insert(param.local_index, info.struct_key.clone());
            }
        }
        if let Some(body) = func.body.as_mut() {
            let root = body.root;
            let type_table = type_table_rc.borrow();
            if rewrite_calls_node(
                body,
                NodeRef::Block(root),
                &sroa_positions,
                &scalar_param_struct,
                &type_table,
            ) {
                touched.insert(i);
            }
        }
    }
    let empty = IndexMap::default();
    for global in &mut project.globals {
        let body = global.initializer.body_mut();
        let root = body.root;
        let type_table = type_table_rc.borrow();
        rewrite_calls_node(
            body,
            NodeRef::Block(root),
            &sroa_positions,
            &empty,
            &type_table,
        );
    }
}

/// Post-order: rewrite children first, then the call at this node.
fn rewrite_calls_node(
    body: &mut Body,
    node: NodeRef,
    sroa_positions: &IndexMap<FnKey, IndexMap<usize, SroaInfo>>,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
    type_table: &TypeTable,
) -> bool {
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    let mut changed = false;
    for c in kids {
        changed |= rewrite_calls_node(body, c, sroa_positions, scalar_param_struct, type_table);
    }
    if let NodeRef::Expr(id) = node {
        changed |= rewrite_call_expr(body, id, sroa_positions, scalar_param_struct, type_table);
    }
    changed
}

fn rewrite_call_expr(
    body: &mut Body,
    id: ExprId,
    sroa_positions: &IndexMap<FnKey, IndexMap<usize, SroaInfo>>,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
    type_table: &TypeTable,
) -> bool {
    match &body.exprs[id].kind {
        ExprKind::Call { func, args, .. } => {
            let key: FnKey = (func.module_source.clone(), func.name.clone());
            let Some(positions) = sroa_positions.get(&key).cloned() else {
                return false;
            };
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for (pi, info) in &positions {
                if *pi < args.len() {
                    rewrite_arg(body, args[*pi], info, scalar_param_struct, type_table);
                }
            }
            true
        }
        ExprKind::MethodCall { func, .. } => {
            let key: FnKey = (func.module_source.clone(), func.name.clone());
            let Some(positions) = sroa_positions.get(&key).cloned() else {
                return false;
            };
            if positions.contains_key(&0) {
                // Receiver SROA'd: collapse `MethodCall` → `Call`.
                let ExprKind::MethodCall {
                    receiver,
                    func,
                    type_args,
                    args,
                    ..
                } = std::mem::replace(&mut body.exprs[id].kind, ExprKind::Unit)
                else {
                    unreachable!();
                };
                if let Some(info) = positions.get(&0) {
                    rewrite_arg(body, receiver, info, scalar_param_struct, type_table);
                }
                for (pi, info) in &positions {
                    if *pi == 0 {
                        continue;
                    }
                    let arg_idx = *pi - 1;
                    if arg_idx < args.len() {
                        rewrite_arg(
                            body,
                            args[arg_idx].expr,
                            info,
                            scalar_param_struct,
                            type_table,
                        );
                    }
                }
                let mut new_args = Vec::with_capacity(args.len() + 1);
                new_args.push(ArenaCallArg {
                    expr: receiver,
                    is_mut: false,
                });
                new_args.extend(args);
                body.exprs[id].kind = ExprKind::Call {
                    func,
                    type_args,
                    args: new_args,
                };
            } else {
                let ExprKind::MethodCall { args, .. } = &body.exprs[id].kind else {
                    unreachable!();
                };
                let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
                for (pi, info) in &positions {
                    let arg_idx = pi.saturating_sub(1);
                    if *pi >= 1 && arg_idx < args.len() {
                        rewrite_arg(body, args[arg_idx], info, scalar_param_struct, type_table);
                    }
                }
            }
            true
        }
        _ => false,
    }
}

fn rewrite_arg(
    body: &mut Body,
    arg: ExprId,
    info: &SroaInfo,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
    type_table: &TypeTable,
) {
    // Peel auto-ref wrappers (`&x`, `&mut x`).
    if let ExprKind::Unary {
        op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
        expr: inner,
    } = &body.exprs[arg].kind
    {
        let inner = *inner;
        become_expr(body, arg, inner);
    }

    // Case 1: StructLiteral matching the wrapper's canonical identity → unwrap.
    if let ExprKind::StructLiteral {
        struct_type,
        fields,
        ..
    } = &body.exprs[arg].kind
        && struct_key_of(*struct_type, type_table).as_ref() == Some(&info.struct_key)
        && fields.len() == 1
    {
        let field_value = fields[0].value;
        become_expr(body, arg, field_value);
        return;
    }

    // Case 2: Local(x) whose own param was SROA'd from the same struct.
    if let ExprKind::Local { index, .. } = &body.exprs[arg].kind
        && scalar_param_struct.get(index) == Some(&info.struct_key)
    {
        body.exprs[arg].type_id = info.inner_type_id;
        return;
    }

    // Case 3: general — extract the field via FieldAccess.
    let span = body.exprs[arg].span;
    let orig_ty = body.exprs[arg].type_id;
    let orig_kind = std::mem::replace(&mut body.exprs[arg].kind, ExprKind::Unit);
    let orig = body.exprs.push(ExprNode {
        kind: orig_kind,
        type_id: orig_ty,
        span,
    });
    body.exprs[arg].kind = ExprKind::FieldAccess {
        expr: orig,
        field_index: 0,
        field_name: info.field_name.clone(),
    };
    body.exprs[arg].type_id = info.inner_type_id;
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

/// Same pinning rules DAE uses — see `optimize::dae::is_eligible`.
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
    if func.is_closure_call() {
        return false;
    }
    if func.is_value_copy() {
        return false;
    }
    true
}
