//! Single-field parameter SROA for Wado NIR.
//!
//! Rewrites internal functions whose parameter type is
//! `&S` / `&mut S` for some single-field struct `S` (with `Box<T>`
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
//! `FieldAccess(Local(idx), field)` (the scalar read) or an argument at a call
//! position whose callee is ALSO a candidate at that position.
//! Iterates to a fix-point so cascades settle.
//!
//! Rewrite (Phase 3): callee bodies turn `FieldAccess(Local, field)` into the
//! scalar `Local`; call sites unwrap `StructLiteral { field: val }` to `val`
//! (or extract via `FieldAccess`); scalarizing a receiver clears the call's
//! `has_receiver`.
//!
//! The validation walk and both rewrite phases read and mutate the arena `Body`
//! directly. Global initializers are arena bodies too, so the call-site rewrite
//! runs on them as well.

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, ExprNode, NodeRef, Operand};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

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
    // rewrites and the receiver collapse keep the same callee.
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
    body.exprs[id] = body.take_expr(src);
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
    let reachable_writes = transitive_reachable_writes(project);
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
            let aliasing_write = may_write_aliasing_location(
                &reachable_writes[key.index()],
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
                aliasing_write,
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
            let Some(func_rc) = project.functions.get(key.index()) else {
                invalid.insert((*key, *pi));
                continue;
            };
            let func = func_rc.borrow();
            let local_index = func.params[*pi].local_index;
            let body = func
                .body
                .as_ref()
                .expect("is_eligible rejects a body-less function");
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
    let key = struct_key_of(struct_type_id, type_table)?;
    let (field_name, inner_type_id) = single_field.get(&key)?.clone();
    if !is_sroa_eligible_inner_type(inner_type_id) {
        return None;
    }
    Some(SroaInfo {
        struct_key: key,
        inner_type_id,
        field_name,
    })
}

/// A wrapper field that has no Wasm value cannot become a parameter.
fn is_sroa_eligible_inner_type(type_id: TypeId) -> bool {
    type_id != TypeTable::UNIT && type_id != TypeTable::NEVER
}

fn struct_key_of(type_id: TypeId, type_table: &TypeTable) -> Option<(String, ModuleSource)> {
    match type_table.get(type_id) {
        ResolvedType::Struct { module_source, .. } => {
            Some((type_table.struct_list_name(type_id)?, module_source.clone()))
        }
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
/// - `aliasing_write` — a write the walk cannot rule out: an aliasing global, or
///   anything behind an unresolved indirect call, or
/// - a sibling param from which a write can *reach* the wrapper. The sibling
///   need not be the handle itself, only carry one: `f(&s.m, &mut s)` where
///   `S { m: M }`, or `f(&x, Holder { r: &mut x })` where the `&mut` hides in a
///   by-value struct field.
///
/// A genuine by-value struct candidate is already a copy, so it is never
/// affected. A *boxed* reference is not one: `boxing::prepare_types` collapses
/// `&T` / `&mut T` onto a by-value `Box<T>`, and `&x` of an address-taken local
/// lowers to a read of that one box, so `f(&mut x, &x)` hands the same box to
/// both params and the snapshot must be refused.
///
/// The split is load-bearing: a `fn mut()` sibling is covered only by the first
/// clause, because the walk cannot see its captures — see
/// [`ReachableWrites::Opaque`].
fn param_snapshot_unsound(
    func: &NirFunction,
    pi: usize,
    struct_key: &(String, ModuleSource),
    type_table: &TypeTable,
    struct_fields: &StructFieldsIndex,
    aliasing_write: bool,
) -> bool {
    let param_type = func.params[pi].type_id;
    let candidate_is_ref =
        reference_param_struct_key(param_type, type_table).as_ref() == Some(struct_key);
    let candidate_is_boxed_ref = type_table.box_payload_of(param_type).is_some();
    if !candidate_is_ref && !candidate_is_boxed_ref {
        return false;
    }
    if aliasing_write {
        return true;
    }
    func.params.iter().enumerate().any(|(pj, other)| {
        if pj == pi {
            return false;
        }
        let mut visited = IndexSet::default();
        mut_reachable_contains(
            other.type_id,
            param_root_writable(other.type_id, type_table),
            struct_key,
            type_table,
            struct_fields,
            &mut visited,
        )
    })
}

/// Whether a parameter's *own* storage can be written through: the pointee of a
/// plain `MutRef`, or a `&mut T` boxing collapsed onto `Box<T>`. Everything else
/// arrives as a copy or a read-only handle — but its interior may still hand out
/// a mutable reference, which [`mut_reachable_contains`] picks up.
fn param_root_writable(param_type: TypeId, type_table: &TypeTable) -> bool {
    matches!(type_table.get(param_type), ResolvedType::MutRef(_))
        || type_table.is_mut_box(param_type)
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

/// A shared `&T` collapsed onto `Box<T>`: the wrapper may be reachable, but
/// `*p` cannot be written through it, so its payload is sealed.
fn is_shared_box(ty: TypeId, type_table: &TypeTable) -> bool {
    type_table.box_payload_of(ty).is_some() && !type_table.is_mut_box(ty)
}

/// Whether a write can land on a `target`-typed location reachable from `ty`.
///
/// `writable` says the location being visited is one the callee can write. That
/// is a property of the location, not of the walk's depth: a `&mut` pointee and
/// a mutable box are writable wherever they appear, a shared pointee never is,
/// and a component of a writable aggregate inherits it. So a by-value
/// `Holder { r: &mut i32 }` is a copy whose `r` still writes the caller's box,
/// while a by-value `M` writes nothing.
fn mut_reachable_contains(
    ty: TypeId,
    writable: bool,
    target: &(String, ModuleSource),
    type_table: &TypeTable,
    struct_fields: &StructFieldsIndex,
    visited: &mut IndexSet<(TypeId, bool)>,
) -> bool {
    if !visited.insert((ty, writable)) {
        return false;
    }
    let (inner, inner_writable) = match type_table.get(ty) {
        ResolvedType::Struct {
            decl_name,
            module_source,
            type_args,
        } => {
            let key = (
                type_table.struct_rendered_name(decl_name, type_args),
                module_source.clone(),
            );
            let writable_here = writable || type_table.is_mut_box(ty);
            if writable_here && &key == target {
                return true;
            }
            let Some(fields) = struct_fields.get(&key) else {
                return false;
            };
            let fields_writable = writable_here && !is_shared_box(ty, type_table);
            return fields.iter().any(|&ft| {
                mut_reachable_contains(
                    ft,
                    fields_writable,
                    target,
                    type_table,
                    struct_fields,
                    visited,
                )
            });
        }
        ResolvedType::MutRef(inner) => (*inner, true),
        ResolvedType::Ref(inner) => (*inner, false),
        ResolvedType::Reactive(inner)
        | ResolvedType::BuiltinArray(inner)
        | ResolvedType::Newtype {
            base_type: inner, ..
        } => (*inner, writable),
        ResolvedType::GenericInstance { type_args, .. }
        | ResolvedType::GenericResource { type_args, .. } => {
            let args = type_args.clone();
            return args.iter().any(|&t| {
                mut_reachable_contains(t, writable, target, type_table, struct_fields, visited)
            });
        }
        _ => return false,
    };
    mut_reachable_contains(
        inner,
        inner_writable,
        target,
        type_table,
        struct_fields,
        visited,
    )
}

/// What a function may write, as far as this pass can attribute it to a name.
#[derive(Clone)]
enum ReachableWrites {
    /// An unresolved indirect call: any global **and** any captured location.
    ///
    /// This is the pass's only account of closure captures. A `fn mut()` value
    /// carries them in a functor struct its *type* does not mention, so no
    /// type-directed query can see them, and invoking it is the only way to
    /// reach them. Every query over `Opaque` MUST therefore answer
    /// conservatively; narrowing it un-guards every capture.
    Opaque,
    Named(IndexSet<(ModuleSource, String)>),
}

impl ReachableWrites {
    fn absorb(&mut self, other: &ReachableWrites) -> bool {
        match (&mut *self, other) {
            (ReachableWrites::Opaque, _) => false,
            (slot, ReachableWrites::Opaque) => {
                *slot = ReachableWrites::Opaque;
                true
            }
            (ReachableWrites::Named(acc), ReachableWrites::Named(more)) => {
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

/// Per-function write summary (indexed by function position): a caller inherits
/// everything its callees may write, so an indirect call anywhere below it makes
/// it [`Opaque`](ReachableWrites::Opaque). A monotone worklist over the reverse
/// call graph.
fn transitive_reachable_writes(project: &NirPackage) -> Vec<ReachableWrites> {
    let n = project.functions.len();
    let mut writes: Vec<ReachableWrites> = Vec::with_capacity(n);
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
                    ExprKind::Call { func_id, .. } => {
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
            ReachableWrites::Opaque
        } else {
            ReachableWrites::Named(direct)
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

/// Whether `writes` can land on a location aliasing a `&target` pointee, and so
/// stale a call-time snapshot of that reference param.
/// [`Opaque`](ReachableWrites::Opaque) answers `true` unconditionally.
fn may_write_aliasing_location(
    writes: &ReachableWrites,
    target: &(String, ModuleSource),
    global_types: &IndexMap<(ModuleSource, String), TypeId>,
    type_table: &TypeTable,
    struct_fields: &StructFieldsIndex,
) -> bool {
    match writes {
        ReachableWrites::Opaque => true,
        ReachableWrites::Named(set) => set.iter().any(|g| match global_types.get(g) {
            Some(&ty) => {
                let mut visited = IndexSet::default();
                mut_reachable_contains(ty, true, target, type_table, struct_fields, &mut visited)
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
        return check_expr(body, id, idx, candidates);
    }
    let mut ok = true;
    body.for_each_child(node, |c| {
        ok = ok && check_node(body, c, idx, candidates);
    });
    ok
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
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if place_root_local(body, target) == Some(idx) {
                return false;
            }
            check_expr(body, target, idx, candidates) && check_operand(body, value, idx, candidates)
        }
        _ => {
            let mut ok = true;
            body.for_each_child(NodeRef::Expr(id), |c| {
                ok = ok && check_node(body, c, idx, candidates);
            });
            ok
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
    let ExprKind::Call {
        func_id,
        args,
        has_receiver,
        ..
    } = &body.exprs[id].kind
    else {
        return false;
    };
    let Some(positions) = sroa_positions.get(func_id).cloned() else {
        return false;
    };
    // Scalarizing position 0 replaces a method's receiver with a plain scalar,
    // so the call stops being one.
    let receiver_scalarized = *has_receiver && positions.contains_key(&0);
    let span = body.exprs[id].span;
    let arg_ops: Vec<Operand> = args.iter().map(|a| a.expr).collect();
    let mut rewritten: Vec<(usize, Operand)> = Vec::with_capacity(positions.len());
    for (pi, info) in &positions {
        let op = scalarized_arg(&arg_ops, *pi);
        rewritten.push((
            *pi,
            rewrite_arg_operand(body, op, info, scalar_param_struct, type_table, span),
        ));
    }
    let ExprKind::Call {
        args, has_receiver, ..
    } = &mut body.exprs[id].kind
    else {
        unreachable!("matched a Call above")
    };
    for (pi, op) in rewritten {
        args[pi].expr = op;
    }
    if receiver_scalarized {
        *has_receiver = false;
    }
    true
}

/// The argument operand at a scalarized parameter position. The position exists
/// because the callee declares a parameter there, so a call reaching the rewrite
/// supplies one.
fn scalarized_arg(args: &[Operand], arg_idx: usize) -> Operand {
    *args.get(arg_idx).unwrap_or_else(|| {
        panic!("sroa_param: call has no argument at scalarized position {arg_idx}")
    })
}

/// Rewrite the argument at a scalarized parameter position, returning the
/// operand the call should carry from now on.
///
/// A skeleton argument is rewritten in place and handed back unchanged. A
/// promoted constant has no node to rewrite, so it takes the same field
/// projection [`rewrite_arg`]'s general case builds — `(<value>).f` over the
/// operand itself, moving to the call site the read the callee used to perform.
/// Leaving it alone is not an option: the callee's signature already names the
/// inner scalar.
fn rewrite_arg_operand(
    body: &mut Body,
    op: Operand,
    info: &SroaInfo,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
    type_table: &TypeTable,
    span: Span,
) -> Operand {
    let Some(arg) = op.as_expr() else {
        return Operand::Expr(body.exprs.push(ExprNode {
            kind: ExprKind::FieldAccess {
                expr: op,
                field_index: 0,
                field_name: info.field_name.clone(),
            },
            type_id: info.inner_type_id,
            span,
        }));
    };
    rewrite_arg(body, arg, info, scalar_param_struct, type_table);
    op
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
    let moved = body.take_expr(arg);
    let orig = body.exprs.push(moved);
    body.exprs[arg].kind = ExprKind::FieldAccess {
        expr: orig.into(),
        field_index: 0,
        field_name: info.field_name.clone(),
    };
    body.exprs[arg].type_id = info.inner_type_id;
}

// -----------------------------------------------------------------------
// Pinning
// -----------------------------------------------------------------------

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
