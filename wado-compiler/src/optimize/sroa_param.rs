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
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{ArenaCallArg, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use cranelift_entity::EntityRef;

use super::arena_query::place_root_local;
use super::gate::{FunctionGate, GatedPass};
use crate::nir::FuncId;

type FnKey = crate::nir::FuncId;

/// Per-candidate metadata captured during Phase 1.
#[derive(Clone)]
struct SroaInfo {
    /// Canonical struct identity — `(struct_name, module_source)`.
    struct_key: (String, ModuleSource),
    /// Type of the wrapper's sole field — the new scalar parameter type.
    inner_type_id: TypeId,
    /// Field name of the wrapper struct's sole field.
    field_name: String,
}

pub fn sroa_single_field_parameters(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let candidates = collect_and_validate(project, gate);
    if candidates.is_empty() {
        return false;
    }
    // Interprocedural and scans all functions, but reports the ones it touched
    // (param-scalarized callees + callers whose call sites were rewritten) so
    // the gated passes re-examine only those. The call graph is unaffected: arg
    // rewrites and the `MethodCall` → `Call` collapse keep the same callee.
    let mut touched: IndexSet<usize> = IndexSet::default();
    rewrite_callees(project, &candidates, &mut touched);
    rewrite_call_sites(project, &candidates, &mut touched);
    for idx in touched {
        gate.mark_changed(FuncId::new(idx));
    }
    true
}

/// Move `src`'s node content into `id`; `src` is left as a dead node.
fn become_expr(body: &mut Body, id: ExprId, src: ExprId) {
    if id == src {
        return;
    }
    let ty = body.exprs[src].type_id;
    let span = body.exprs[src].span;
    let node = std::mem::replace(
        &mut body.exprs[src],
        ExprNode {
            kind: ExprKind::Dead,
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

fn collect_and_validate(
    project: &NirPackage,
    gate: &mut FunctionGate,
) -> IndexMap<(FnKey, usize), SroaInfo> {
    let type_table = project.type_table.borrow();
    let single_field = build_single_field_index(project);
    let struct_fields = build_struct_fields_index(project);
    let global_writes = transitive_global_writes(project);
    let global_types = global_type_index(project);

    let mut candidates: IndexMap<(FnKey, usize), SroaInfo> = IndexMap::default();
    for fid in gate.dirty_funcs(GatedPass::SroaParam, project.functions.len()) {
        let func = project.functions[fid.index()].borrow();
        if !is_eligible(&func) {
            continue;
        }
        let Some(key) = func.id else { continue };
        let is_trait_method = func.is_trait_method();
        for (pi, param) in func.params.iter().enumerate() {
            // A trait method's `self` receiver stays pinned. Scalarizing a
            // single-field-struct receiver (e.g. a `SequenceLiteralBuilder`
            // wrapper like `SeqVec { items: List<T> }`) changes the receiver
            // shape that later collapse passes match on. Non-receiver params —
            // notably serde's boxed `value: &T` — are still unwrapped, which is
            // where the box-per-scalar win comes from.
            if is_trait_method && pi == 0 && param.name == "self" {
                continue;
            }
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
            let writes_global = writes_aliasing_global(
                &global_writes[key.index()],
                &info.struct_key,
                &global_types,
                &type_table,
                &struct_fields,
            );
            if param_snapshot_unsound(
                &func,
                pi,
                &info.struct_key,
                &type_table,
                &struct_fields,
                writes_global,
            ) {
                continue;
            }
            candidates.insert((key, pi), info);
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
                invalid.insert((*key, *pi));
                continue;
            };
            let func = func_rc.borrow();
            let local_index = func.params[*pi].local_index;
            let body = func.body.as_ref().unwrap();
            if !body_uses_param_safely(body, local_index, &candidates) {
                invalid.insert((*key, *pi));
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

/// Reject a reference candidate whose call-time value snapshot could be
/// invalidated during the callee's execution — because some other access path
/// the callee holds can mutate the pointee `*p`:
///
/// - a mutable-global write (the pointee may live in a global), or
/// - a `&mut` sibling param whose pointee type *transitively contains* the
///   wrapper struct (a write through it can reach a wrapper-typed location that
///   the same call site may have aliased with `p`, e.g. `f(&s.m, &mut s)` where
///   `S { m: M }`). This subsumes the same-type sibling case (`X == W` contains
///   `W` trivially).
///
/// A by-value struct candidate is already a copy, so it is never affected.
fn param_snapshot_unsound(
    func: &NirFunction,
    pi: usize,
    struct_key: &(String, ModuleSource),
    type_table: &TypeTable,
    struct_fields: &StructFieldsIndex,
    writes_global: bool,
) -> bool {
    let candidate_is_ref = func
        .params
        .get(pi)
        .map(|p| reference_param_struct_key(p.type_id, type_table))
        == Some(Some(struct_key.clone()));
    if !candidate_is_ref {
        return false;
    }
    if writes_global {
        return true;
    }
    func.params.iter().enumerate().any(|(pj, other)| {
        if pj == pi {
            return false;
        }
        let ResolvedType::MutRef(inner) = type_table.get(other.type_id) else {
            return false;
        };
        let mut visited = IndexSet::default();
        type_transitively_contains(*inner, struct_key, type_table, struct_fields, &mut visited)
    })
}

/// Map a struct's identity → its field types, for transitive-containment queries.
type StructFieldsIndex = IndexMap<(String, ModuleSource), Vec<TypeId>>;

fn build_struct_fields_index(project: &NirPackage) -> StructFieldsIndex {
    let mut out: StructFieldsIndex = IndexMap::default();
    for s in &project.structs {
        out.insert(
            (s.name.clone(), s.module_source.clone()),
            s.fields.iter().map(|f| f.type_id).collect(),
        );
    }
    out
}

/// Whether `ty` can reach a location of the `target` struct type — directly, or
/// transitively through fields, references, newtypes, arrays, or generic type
/// arguments. `visited` breaks cycles over recursive types.
fn type_transitively_contains(
    ty: TypeId,
    target: &(String, ModuleSource),
    type_table: &TypeTable,
    struct_fields: &StructFieldsIndex,
    visited: &mut IndexSet<TypeId>,
) -> bool {
    if !visited.insert(ty) {
        return false;
    }
    match type_table.get(ty) {
        ResolvedType::Struct {
            name,
            module_source,
            ..
        } => {
            let key = (name.clone(), module_source.clone());
            if &key == target {
                return true;
            }
            let Some(fields) = struct_fields.get(&key) else {
                return false;
            };
            fields.iter().any(|&ft| {
                type_transitively_contains(ft, target, type_table, struct_fields, visited)
            })
        }
        ResolvedType::Ref(inner)
        | ResolvedType::MutRef(inner)
        | ResolvedType::Reactive(inner)
        | ResolvedType::BuiltinArray(inner)
        | ResolvedType::Newtype {
            base_type: inner, ..
        } => type_transitively_contains(*inner, target, type_table, struct_fields, visited),
        ResolvedType::GenericInstance { type_args, .. }
        | ResolvedType::GenericResource { type_args, .. } => type_args
            .iter()
            .any(|&t| type_transitively_contains(t, target, type_table, struct_fields, visited)),
        _ => false,
    }
}

#[derive(Clone)]
enum GlobalWrites {
    /// An indirect call whose target cannot be resolved: any global is possible.
    Any,
    Known(IndexSet<(ModuleSource, String)>),
}

impl GlobalWrites {
    fn absorb(&mut self, other: &GlobalWrites) -> bool {
        match (&mut *self, other) {
            (GlobalWrites::Any, _) => false,
            (slot, GlobalWrites::Any) => {
                *slot = GlobalWrites::Any;
                true
            }
            (GlobalWrites::Known(acc), GlobalWrites::Known(more)) => {
                let before = acc.len();
                for g in more {
                    acc.insert(g.clone());
                }
                acc.len() != before
            }
        }
    }
}

/// The global a write-target place is rooted at, seeing through projections, so
/// an in-place `G.field = …` counts as a write to `G`, not only a `GlobalVarSet`.
fn global_place_root(body: &Body, target: ExprId) -> Option<(ModuleSource, String)> {
    match &body.exprs[target].kind {
        ExprKind::GlobalVarGet {
            module_source,
            name,
        } => Some((module_source.clone(), name.clone())),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => inner.as_expr().and_then(|e| global_place_root(body, e)),
        _ => None,
    }
}

/// Per-function global-write summary (indexed by function position): a caller
/// inherits every global its callees may write. A monotone worklist over the
/// reverse call graph.
fn transitive_global_writes(project: &NirPackage) -> Vec<GlobalWrites> {
    let n = project.functions.len();
    let mut writes: Vec<GlobalWrites> = Vec::with_capacity(n);
    let mut callers: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        let func = project.functions[i].borrow();
        let mut direct: IndexSet<(ModuleSource, String)> = IndexSet::default();
        let mut indirect = false;
        if let Some(body) = func.body.as_ref() {
            for node in body.exprs.values() {
                match &node.kind {
                    ExprKind::GlobalVarSet {
                        module_source,
                        name,
                        ..
                    } => {
                        direct.insert((module_source.clone(), name.clone()));
                    }
                    ExprKind::Assign { target, .. } => {
                        if let Some(g) = global_place_root(body, *target) {
                            direct.insert(g);
                        }
                    }
                    ExprKind::Call { func_id, .. } | ExprKind::MethodCall { func_id, .. } => {
                        let c = func_id.index();
                        if c < n {
                            callers[c].push(i);
                        }
                    }
                    ExprKind::IndirectCall { .. } => indirect = true,
                    _ => {}
                }
            }
        }
        writes.push(if indirect {
            GlobalWrites::Any
        } else {
            GlobalWrites::Known(direct)
        });
    }
    let mut queued = vec![true; n];
    let mut work: Vec<usize> = (0..n).collect();
    while let Some(c) = work.pop() {
        queued[c] = false;
        let callee_set = writes[c].clone();
        for k in 0..callers[c].len() {
            let caller = callers[c][k];
            if writes[caller].absorb(&callee_set) && !queued[caller] {
                queued[caller] = true;
                work.push(caller);
            }
        }
    }
    writes
}

fn global_type_index(project: &NirPackage) -> IndexMap<(ModuleSource, String), TypeId> {
    let mut out: IndexMap<(ModuleSource, String), TypeId> = IndexMap::default();
    for g in &project.globals {
        out.insert((g.module_source.clone(), g.name.clone()), g.ty);
    }
    out
}

/// Whether `writes` includes a global whose type can alias a `&target` pointee —
/// the only writes that can stale a call-time snapshot of that reference param.
fn writes_aliasing_global(
    writes: &GlobalWrites,
    target: &(String, ModuleSource),
    global_types: &IndexMap<(ModuleSource, String), TypeId>,
    type_table: &TypeTable,
    struct_fields: &StructFieldsIndex,
) -> bool {
    match writes {
        GlobalWrites::Any => true,
        GlobalWrites::Known(set) => set.iter().any(|g| match global_types.get(g) {
            Some(&ty) => {
                let mut visited = IndexSet::default();
                type_transitively_contains(ty, target, type_table, struct_fields, &mut visited)
            }
            None => true,
        }),
    }
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
            if inner.as_expr().is_some_and(
                |e| matches!(&body.exprs[e].kind, ExprKind::Local { index, .. } if *index == idx),
            ) {
                return true;
            }
            check_operand(body, inner, idx, candidates)
        }
        ExprKind::Call { func_id, args, .. } => {
            let key = *func_id;
            let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
            args.iter()
                .enumerate()
                .all(|(i, &a)| check_call_arg(body, key, i, a, idx, candidates))
        }
        ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } => {
            let key = *func_id;
            let receiver = *receiver;
            let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
            check_call_arg(body, key, 0, receiver, idx, candidates)
                && args
                    .iter()
                    .enumerate()
                    .all(|(i, &a)| check_call_arg(body, key, i + 1, a, idx, candidates))
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if place_root_local(body, target) == Some(idx) {
                return false;
            }
            check_expr(body, target, idx, candidates) && check_operand(body, value, idx, candidates)
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
    callee: FnKey,
    pos: usize,
    arg: Operand,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> bool {
    // A promoted constant arg never references the SROA candidate local.
    let Some(arg) = arg.as_expr() else {
        return true;
    };
    if matches!(&body.exprs[arg].kind, ExprKind::Local { index, .. } if *index == idx) {
        // The candidate local is passed directly; safe only if the callee SROAs
        // this position too.
        return candidates.contains_key(&(callee, pos));
    }
    check_expr(body, arg, idx, candidates)
}

/// [`check_expr`] for an operand: a promoted constant never references the SROA
/// candidate local, so it never blocks.
fn check_operand(
    body: &Body,
    op: Operand,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> bool {
    op.as_expr()
        .is_none_or(|e| check_expr(body, e, idx, candidates))
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
        let Some(key) = func.id else { continue };
        let mut affected: Vec<u32> = Vec::new();
        for pi in 0..func.params.len() {
            if let Some(info) = candidates.get(&(key, pi)) {
                let local_index = func.params[pi].local_index;
                affected.push(local_index);
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

/// Pre-order: replace `FieldAccess(Local(idx), field_index: 0)` for a SROA'd
/// param `idx` with the bare scalar `Local`, before children are reshaped. The
/// wrapper is a single-field struct, so its sole field is index 0 — matching by
/// index (not name) avoids over-stripping a same-named field of the inner type
/// (e.g. `b.value.value` where the inner `.value` belongs to a different struct).
fn rewrite_param_reads(body: &mut Body, node: NodeRef, affected: &[u32]) {
    if let NodeRef::Expr(id) = node {
        // The SROA'd field access whose inner `Local` should replace it, if any.
        let local_inner = if let ExprKind::FieldAccess {
            expr: inner,
            field_index: 0,
            ..
        } = &body.exprs[id].kind
        {
            inner.as_expr().filter(|&e| {
                matches!(&body.exprs[e].kind, ExprKind::Local { index, .. }
                if affected.contains(index))
            })
        } else {
            None
        };
        if let Some(inner) = local_inner {
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
            .entry(*key)
            .or_default()
            .insert(*pi, info.clone());
    }

    let type_table_rc = project.type_table.clone();
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        let Some(key) = func.id else { continue };
        let mut scalar_param_struct: IndexMap<u32, (String, ModuleSource)> = IndexMap::default();
        for (pi, param) in func.params.iter().enumerate() {
            if let Some(info) = candidates.get(&(key, pi)) {
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
        let body = global.init.slot_expr_mut().body_mut();
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
        ExprKind::Call { func_id, args, .. } => {
            let Some(positions) = sroa_positions.get(func_id).cloned() else {
                return false;
            };
            let args: Vec<Option<ExprId>> = args.iter().map(|a| a.expr.as_expr()).collect();
            for (pi, info) in &positions {
                if let Some(Some(arg)) = args.get(*pi).copied() {
                    rewrite_arg(body, arg, info, scalar_param_struct, type_table);
                }
            }
            true
        }
        ExprKind::MethodCall { func_id, .. } => {
            let Some(positions) = sroa_positions.get(func_id).cloned() else {
                return false;
            };
            if positions.contains_key(&0) {
                // Receiver SROA'd: collapse `MethodCall` → `Call`.
                let ExprKind::MethodCall {
                    receiver,
                    func_id,
                    type_args,
                    args,
                    ..
                } = std::mem::replace(&mut body.exprs[id].kind, ExprKind::Dead)
                else {
                    unreachable!();
                };
                if let Some(info) = positions.get(&0) {
                    rewrite_arg_operand(body, receiver, info, scalar_param_struct, type_table);
                }
                for (pi, info) in &positions {
                    if *pi == 0 {
                        continue;
                    }
                    let arg_idx = *pi - 1;
                    if arg_idx < args.len()
                        && let Some(arg_e) = args[arg_idx].expr.as_expr()
                    {
                        rewrite_arg(body, arg_e, info, scalar_param_struct, type_table);
                    }
                }
                let mut new_args = Vec::with_capacity(args.len() + 1);
                new_args.push(ArenaCallArg {
                    expr: receiver,
                    is_mut: false,
                });
                new_args.extend(args);
                body.exprs[id].kind = ExprKind::Call {
                    func_id,
                    type_args,
                    args: new_args,
                };
            } else {
                let ExprKind::MethodCall { args, .. } = &body.exprs[id].kind else {
                    unreachable!();
                };
                let args: Vec<Option<ExprId>> = args.iter().map(|a| a.expr.as_expr()).collect();
                for (pi, info) in &positions {
                    let arg_idx = pi.saturating_sub(1);
                    if *pi >= 1
                        && let Some(Some(arg)) = args.get(arg_idx).copied()
                    {
                        rewrite_arg(body, arg, info, scalar_param_struct, type_table);
                    }
                }
            }
            true
        }
        _ => false,
    }
}

fn rewrite_arg_operand(
    body: &mut Body,
    op: Operand,
    info: &SroaInfo,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
    type_table: &TypeTable,
) {
    if let Some(e) = op.as_expr() {
        rewrite_arg(body, e, info, scalar_param_struct, type_table);
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
        if let Some(inner_e) = inner.as_expr() {
            become_expr(body, arg, inner_e);
        }
    }

    // Case 1: StructLiteral matching the wrapper's canonical identity → unwrap to
    // its single field. Only a skeleton field is lifted in place; a promoted
    // constant field falls through to Case 3's `FieldAccess` (`(Wrapper{V}).f`,
    // folded later) since it has no node to become (WEP: The Live ValueGraph).
    if let ExprKind::StructLiteral {
        struct_type,
        fields,
        ..
    } = &body.exprs[arg].kind
        && struct_key_of(*struct_type, type_table).as_ref() == Some(&info.struct_key)
        && fields.len() == 1
        && let Some(fe) = fields[0].value.as_expr()
    {
        become_expr(body, arg, fe);
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
    let orig_kind = std::mem::replace(&mut body.exprs[arg].kind, ExprKind::Dead);
    let orig = body.exprs.push(ExprNode {
        kind: orig_kind,
        type_id: orig_ty,
        span,
    });
    body.exprs[arg].kind = ExprKind::FieldAccess {
        expr: orig.into(),
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
    use cranelift_entity::EntityRef;
    project.functions.get(key.index())
}

/// Pinning rules, shared with DAE via [`super::dae::is_dae_sroa_eligible`].
/// `relax_closure_call = false` keeps closure `__call` functors pinned (their
/// function-table wrapper snapshots the signature); unlike DAE there is no
/// relaxation here. `sroa_param` adds one pin the shared predicate does not
/// carry — a `$value_copy$T` helper is never a rewrite target.
///
/// Concrete trait-impl methods are eligible: after monomorphization every call
/// site carries a resolved `func_id` and `rewrite_call_sites` rewrites them all,
/// so scalarizing a single-field-struct parameter (and its call-site
/// allocation) is sound. This unwraps the `Box<Scalar>` that `&T` reference
/// parameters box the value into — e.g. every scalar `serde` field / element
/// (`SerializeStruct::field<i32>`, `SerializeSeq::element<f64>`).
fn is_eligible(func: &NirFunction) -> bool {
    super::dae::is_dae_sroa_eligible(func, false) && !func.is_value_copy()
}
