//! One-field parameter SROA: a function taking `&S` / `&mut S` and
//! reading exactly one of `S`'s fields — canonically `Box<T>`, but equally an
//! options record whose other fields `param_spec` already folded away — is
//! rewritten to take that field, collapsing the call site's `StructLiteral`
//! allocation to the field's value.
//!
//! Which field that is falls out of the same fixpoint that checks for escape:
//! every use of a candidate param must be a `FieldAccess`, or an argument to
//! another candidate position, and all of them must name the same field.
//!
//! The rewrite **mints a clone** and leaves the original standing. Signatures
//! are not this pass's alone to change: a `#[compiler_item]` marker has
//! peepholes synthesizing calls against it, and `wir_build` writes a forwarding
//! wrapper for each closure functor method after every NIR pass has run. Neither
//! call exists here to retarget, so reshaping in place breaks them — and the set
//! of passes that synthesize calls is not something this one can enumerate and
//! stay correct as more are added. Cloning sidesteps the question: calls this
//! pass can see move to the clone, calls it cannot keep finding the original,
//! and `dce` drops whichever of the two nothing reaches.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirFunction, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, ExprNode, NodeRef, Operand};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::token::Span;

use cranelift_entity::EntityRef;

use super::arena_query::{is_local_operand, is_pure_operand, place_root_local};
use super::gate::{FunctionGate, GatedPass};
use crate::nir::FuncId;

type FnKey = crate::nir::FuncId;

/// Per-candidate metadata captured during Phase 1.
#[derive(Clone)]
struct SroaInfo {
    /// Canonical struct identity — `(struct_name, module_source)`.
    struct_key: (String, ModuleSource),
    /// Type of the field the callee reads — the new scalar parameter type.
    inner_type_id: TypeId,
    /// Name of the field the callee reads.
    field_name: String,
    /// Its declaration index, which is what the rewrites match on: a name can
    /// repeat between the wrapper and the field's own type (`b.value.value`).
    field_index: u32,
    /// How the callee holds the field — see [`param_field_form`].
    form: FieldForm,
    /// The type the scalarized parameter takes: `inner_type_id` in `form`.
    /// Interned by [`resolve_scalar_param_types`] once the fixpoint has settled
    /// which field this is.
    scalar_type_id: TypeId,
}

pub fn sroa_single_field_parameters(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let mut candidates = collect_and_validate(project, gate);
    if candidates.is_empty() {
        return false;
    }
    resolve_scalar_param_types(project, &mut candidates);
    // Interprocedural and scans all functions, but reports the ones it touched
    // (the minted clones + callers whose call sites moved to one) so the gated
    // passes re-examine only those.
    let mut touched: IndexSet<usize> = IndexSet::default();
    let clones = mint_scalarized_clones(project, &candidates, &mut touched);
    if clones.is_empty() {
        return false;
    }
    rewrite_call_sites(project, &candidates, &clones, &mut touched);
    for idx in touched {
        gate.mark_changed(FuncId::new(idx));
    }
    true
}

/// Intern each candidate's scalar parameter type, now that the fixpoint has
/// settled which field it is. Separate from [`collect_and_validate`], which
/// holds the type table shared while it walks every candidate body.
fn resolve_scalar_param_types(
    project: &NirPackage,
    candidates: &mut IndexMap<(FnKey, usize), SroaInfo>,
) {
    let mut type_table = project.type_table.borrow_mut();
    for info in candidates.values_mut() {
        info.scalar_type_id = info.form.of(&mut type_table, info.inner_type_id);
    }
}

/// Move `src`'s node content into `id`; `src` is left as a dead node.
fn become_expr(body: &mut Body, id: ExprId, src: ExprId) {
    if id == src {
        return;
    }
    // Copy rather than take. `take_expr` leaves `ExprKind::Dead` behind, and a
    // promoted operand can still name `src` through `OpaqueSource::Expr` — the
    // value graph then extracts a dead node, which reads as zero. `src` drops
    // out of the tree with the literal that held it, so the duplicated child
    // ids have one live parent either way.
    body.exprs[id] = body.exprs[src].clone();
}

// -----------------------------------------------------------------------
// Phase 1 + 2
// -----------------------------------------------------------------------

/// Every field of each struct, in declaration order.
type FieldTableIndex = IndexMap<(String, ModuleSource), Vec<(String, TypeId)>>;

fn build_field_table_index(project: &NirPackage) -> FieldTableIndex {
    let mut out: FieldTableIndex = IndexMap::default();
    for s in &project.structs {
        out.insert(
            (s.name.clone(), s.module_source.clone()),
            s.fields
                .iter()
                .map(|f| (f.name.clone(), f.type_id))
                .collect(),
        );
    }
    out
}

/// What a candidate parameter is used for. A parameter qualifies exactly when
/// every use agrees on one field, so this is that agreement or its failure —
/// resolved to a fixpoint, since a use may be "whatever the position I forward
/// to resolves to".
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldUse {
    /// Nothing decided yet: unread, or only forwarded to positions that are
    /// themselves still unresolved.
    Unresolved,
    /// Every use so far reads this field.
    Field(u32),
    /// Escapes whole, is assigned through, or reads two different fields.
    Invalid,
}

impl FieldUse {
    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Invalid, _) | (_, Self::Invalid) => Self::Invalid,
            (Self::Unresolved, x) | (x, Self::Unresolved) => x,
            (Self::Field(a), Self::Field(b)) if a == b => Self::Field(a),
            (Self::Field(_), Self::Field(_)) => Self::Invalid,
        }
    }
}

fn collect_and_validate(
    project: &NirPackage,
    gate: &mut FunctionGate,
) -> IndexMap<(FnKey, usize), SroaInfo> {
    let type_table = project.type_table.borrow();
    let field_table = build_field_table_index(project);
    let struct_fields = build_struct_fields_index(project);
    let reachable_writes = transitive_reachable_writes(project);
    let global_types = global_type_index(project);

    let mut candidates: IndexMap<(FnKey, usize), SroaInfo> = IndexMap::default();
    for fid in gate.dirty_funcs(GatedPass::SroaParam, project.functions.len()) {
        // This pass's own output. A clone is already scalarized in every
        // position found for it, and unwrapping it again chains `$scalar$scalar`
        // names whose depth depends on how many fixpoint iterations ran.
        if project.sroa_param_clones.contains(&fid) {
            continue;
        }
        let func = project.functions[fid.index()].borrow();
        if !is_eligible(&func) {
            continue;
        }
        let Some(key) = func.id else { continue };
        let is_trait_method = func.is_trait_method();
        for (pi, param) in func.params.iter().enumerate() {
            // A trait method's `self` stays put: its shape is the vtable slot's.
            // Every other receiver is fair game, because the original function
            // survives this pass — see `mint_scalarized_clones`.
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
            let Some(mut info) = candidate_info_for(param.type_id, &type_table, &field_table)
            else {
                continue;
            };
            info.form = func.body.as_ref().map_or(FieldForm::Value, |body| {
                param_field_form(body, param.local_index)
            });
            // A shared borrow of the field would make the call site pass
            // `&place.f`, and `sroa` mishandles that shape: it decomposes the
            // caller's struct, rewrites the argument, and leaves the reads the
            // inliner had already planted behind — dropping the binding while
            // they still name it. Left alone until that is fixed, so these call
            // sites keep the form they had.
            if info.form == FieldForm::Shared {
                continue;
            }
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

    // Phase 2: resolve each param to the one field it reads, dropping the ones
    // that escape or disagree — one fixpoint, since a param forwarded to another
    // candidate position takes its answer from there.
    loop {
        let mut invalid: IndexSet<(FnKey, usize)> = IndexSet::default();
        let mut resolved: Vec<((FnKey, usize), u32)> = Vec::new();
        for ((key, pi), info) in &candidates {
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
            match param_field_use(body, local_index, &candidates) {
                FieldUse::Invalid => {
                    invalid.insert((*key, *pi));
                }
                FieldUse::Field(fi) if fi != info.field_index => {
                    resolved.push(((*key, *pi), fi));
                }
                FieldUse::Field(_) | FieldUse::Unresolved => {}
            }
        }
        for (k, fi) in &resolved {
            let Some(info) = candidates.get_mut(k) else {
                continue;
            };
            let Some(fields) = field_table.get(&info.struct_key) else {
                invalid.insert(*k);
                continue;
            };
            let Some((name, ty)) = fields.get(*fi as usize).cloned() else {
                invalid.insert(*k);
                continue;
            };
            info.field_index = *fi;
            info.field_name = name;
            info.inner_type_id = ty;
        }
        if invalid.is_empty() && resolved.is_empty() {
            // Converged, so a param still holding no field never names one, and
            // a field the rewrite cannot make a parameter is out too. Both are
            // dropped inside the loop, not after it: a param that forwards to a
            // dropped position loses the answer it was taking from there, and
            // must be re-checked. `TreeMap::get(&self)` reads `self.root` and
            // hands `self` to `search_in_node(&self)`, which only ever forwards
            // to itself and so resolves to nothing — dropping it after the
            // fixpoint left `get` scalarized around a position that no longer
            // existed.
            let before = candidates.len();
            candidates.retain(|_, info| {
                info.field_index != u32::MAX && is_sroa_eligible_inner_type(info.inner_type_id)
            });
            if candidates.len() == before {
                break;
            }
            continue;
        }
        for k in &invalid {
            candidates.swap_remove(k);
        }
    }

    candidates
}

/// A provisional candidate: the struct is known, the field is not. A one-field
/// struct resolves immediately; anything wider waits for the use fixpoint,
/// which is also what rejects a param that reads more than one.
fn candidate_info_for(
    param_type: TypeId,
    type_table: &TypeTable,
    field_table: &FieldTableIndex,
) -> Option<SroaInfo> {
    let struct_type_id = match type_table.get(param_type) {
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
        ResolvedType::Struct { .. } => param_type,
        _ => return None,
    };
    let key = struct_key_of(struct_type_id, type_table)?;
    let fields = field_table.get(&key)?;
    if fields.is_empty() {
        return None;
    }
    // Seed on field 0; a wider struct has this overwritten once its uses agree.
    let (field_name, inner_type_id) = fields[0].clone();
    Some(SroaInfo {
        struct_key: key,
        inner_type_id,
        field_name,
        field_index: if fields.len() == 1 { 0 } else { u32::MAX },
        form: FieldForm::Value,
        scalar_type_id: inner_type_id,
    })
}

/// A wrapper field that has no Wasm value cannot become a parameter.
fn is_sroa_eligible_inner_type(type_id: TypeId) -> bool {
    type_id != TypeTable::UNIT && type_id != TypeTable::NEVER
}

fn struct_key_of(type_id: TypeId, type_table: &TypeTable) -> Option<(String, ModuleSource)> {
    match type_table.get(type_id) {
        ResolvedType::Struct { def, .. } => Some((
            type_table.struct_list_name(type_id)?,
            type_table.struct_head_module(*def).clone(),
        )),
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

/// Reject a reference candidate whose call-time snapshot the callee could
/// invalidate through another access path: an `aliasing_write` the walk cannot
/// rule out, or a sibling param a write can *reach* the wrapper through —
/// `f(&s.m, &mut s)`, or a `&mut` hidden in a by-value field. A boxed reference
/// counts, `&x` of an address-taken local lowering to a read of the one box.
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
        ResolvedType::Struct { def, type_args } => {
            let module_source = &type_table.struct_head_module(*def).clone();
            let key = (
                type_table.struct_rendered_name(*def, type_args),
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

/// How the callee holds the field, and so how the scalar parameter is typed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldForm {
    /// Read as a value — the canonical `Box<T>` case, where a reference would
    /// only re-box what the unwrap just removed.
    Value,
    Shared,
    Mutable,
}

impl FieldForm {
    fn of(self, type_table: &mut TypeTable, inner: TypeId) -> TypeId {
        match self {
            Self::Value => inner,
            Self::Shared => type_table.make_ref(inner),
            Self::Mutable => type_table.make_mut_ref(inner),
        }
    }

    fn borrow_op(self) -> Option<NirUnaryOp> {
        match self {
            Self::Value => None,
            Self::Shared => Some(NirUnaryOp::Ref),
            Self::Mutable => Some(NirUnaryOp::MutRef),
        }
    }
}

/// The form in which the body holds param `idx`'s field.
///
/// The pass runs after `value_copy` has placed the copies value semantics call
/// for, so a by-value scalar parameter gets none: handing the callee `p.f`
/// hands it the caller's storage. That is what the pass means and what codegen
/// does — `&F` and `F` are the same `ref` once lowered — but not what a
/// by-value signature *says*, and `niri` reads the signature, duly copying.
///
/// Matching the callee's own borrow also keeps the rewrite from leaving one
/// behind. Rewriting `self.f` to the param turns `&self.f` into `&param`, a
/// borrow of what is already a reference; that makes the local address-taken,
/// which stops `copy_prop` folding it back and strands a temporary at every
/// call site. Taking the borrow into the parameter's type lets
/// [`borrow_of_scalarized`] absorb it instead.
/// Only a borrow of the field *itself* counts — `&p.f`, not `&p.f.g`, which
/// borrows a sub-field and leaves `p.f` read as a value. This has to agree
/// exactly with what [`borrow_of_scalarized`] absorbs: a form claiming a borrow
/// the rewrite then fails to find leaves the parameter typed as a reference
/// while the body still reads it as a value.
fn param_field_form(body: &Body, idx: u32) -> FieldForm {
    let mut form = FieldForm::Value;
    for node in body.exprs.values() {
        let ExprKind::Unary { op, expr: inner } = &node.kind else {
            continue;
        };
        let borrows_field = inner.as_expr().is_some_and(|e| {
            matches!(&body.exprs[e].kind, ExprKind::FieldAccess { expr: recv, .. }
                if is_local_operand(body, *recv, idx))
        });
        if !borrows_field {
            continue;
        }
        match op {
            NirUnaryOp::MutRef => return FieldForm::Mutable,
            NirUnaryOp::Ref => form = FieldForm::Shared,
            _ => {}
        }
    }
    form
}

/// The one field every use of param `local_index` reads, or why there isn't one.
fn param_field_use(
    body: &Body,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> FieldUse {
    check_node(body, NodeRef::Block(body.root), idx, candidates)
}

fn check_node(
    body: &Body,
    node: NodeRef,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> FieldUse {
    if let NodeRef::Expr(id) = node {
        return check_expr(body, id, idx, candidates);
    }
    let mut use_ = FieldUse::Unresolved;
    body.for_each_child(node, |c| {
        use_ = use_.meet(check_node(body, c, idx, candidates));
    });
    use_
}

fn check_expr(
    body: &Body,
    id: ExprId,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> FieldUse {
    match &body.exprs[id].kind {
        // Bare local read reaching here (not consumed by a borrowing parent)
        // is an unwrapped use → invalid.
        ExprKind::Local { index, .. } if *index == idx => FieldUse::Invalid,
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let (inner, field_index) = (*inner, *field_index);
            if is_local_operand(body, inner, idx) {
                return FieldUse::Field(field_index);
            }
            check_operand(body, inner, idx, candidates)
        }
        ExprKind::Call { func_id, args, .. } => {
            let key = *func_id;
            let args: Vec<Operand> = args.iter().map(|a| a.expr).collect();
            args.iter()
                .enumerate()
                .fold(FieldUse::Unresolved, |acc, (i, &a)| {
                    acc.meet(check_call_arg(body, key, i, a, idx, candidates))
                })
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if place_root_local(body, target) == Some(idx) {
                return FieldUse::Invalid;
            }
            check_expr(body, target, idx, candidates)
                .meet(check_operand(body, value, idx, candidates))
        }
        _ => {
            let mut use_ = FieldUse::Unresolved;
            body.for_each_child(NodeRef::Expr(id), |c| {
                use_ = use_.meet(check_node(body, c, idx, candidates));
            });
            use_
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
) -> FieldUse {
    if is_local_operand(body, arg, idx) {
        // Passed on whole: this use reads whatever the callee's position reads,
        // which the fixpoint may not have settled yet.
        return match candidates.get(&(callee, pos)) {
            Some(info) if info.field_index != u32::MAX => FieldUse::Field(info.field_index),
            Some(_) => FieldUse::Unresolved,
            None => FieldUse::Invalid,
        };
    }
    // A promoted constant that is not the candidate local cannot reference it.
    let Some(arg) = arg.as_expr() else {
        return FieldUse::Unresolved;
    };
    check_expr(body, arg, idx, candidates)
}

/// [`check_expr`] for an operand. Reaching the candidate local here means it was
/// read whole without a borrowing parent consuming it — the same unwrapped use
/// the `Local` arm rejects, in the promoted form. Any other promoted value
/// cannot reference it, so it constrains nothing.
fn check_operand(
    body: &Body,
    op: Operand,
    idx: u32,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
) -> FieldUse {
    if is_local_operand(body, op, idx) {
        return FieldUse::Invalid;
    }
    op.as_expr().map_or(FieldUse::Unresolved, |e| {
        check_expr(body, e, idx, candidates)
    })
}

// -----------------------------------------------------------------------
// Phase 3a: callee body rewrite (arena)
// -----------------------------------------------------------------------

/// Mint one scalarized clone per function with a candidate parameter, leaving
/// the original untouched. Returns the original → clone map the call-site
/// rewrite retargets through.
fn mint_scalarized_clones(
    project: &mut NirPackage,
    candidates: &IndexMap<(FnKey, usize), SroaInfo>,
    touched: &mut IndexSet<usize>,
) -> IndexMap<FnKey, FnKey> {
    use cranelift_entity::EntityRef as _;

    let mut by_fn: IndexMap<FnKey, Vec<usize>> = IndexMap::default();
    for (key, pi) in candidates.keys() {
        by_fn.entry(*key).or_default().push(*pi);
    }

    let mut clones: IndexMap<FnKey, FnKey> = IndexMap::default();
    let mut minted: Vec<Rc<RefCell<NirFunction>>> = Vec::new();
    let mut next_id = project.next_func_id().index();
    for (key, positions) in &by_fn {
        let Some(original) = project.functions.get(key.index()) else {
            continue;
        };
        let mut clone = original.borrow().clone();
        let origin = (clone.module_source.clone(), clone.name.clone());
        let name = crate::name::sroa_param_name(&clone.name);
        clone.name.clone_from(&name);
        if let Some(info) = &mut clone.method_info {
            info.method_name = crate::name::sroa_param_name(&info.method_name);
        }
        // This pass runs once per fixpoint iteration, so a clone it minted on an
        // earlier one already stands under this name. Reuse it rather than mint
        // a second function with the same identity.
        let func_key =
            FunctionRef::from_resolved(&clone, clone.module_source.clone()).function_id();
        if let Some(&existing) = project.func_index.get(&func_key) {
            clones.insert(*key, existing);
            continue;
        }
        let id = FuncId::new(next_id);
        next_id += 1;
        clone.id = Some(id);
        clone.visibility = crate::ast::Visibility::Private;
        clone.is_export = false;
        clone.export_name = None;
        // The marker names the one canonical function behind a compiler item, and
        // that is the original — peepholes resolve it to synthesize calls wearing
        // the original signature, which the clone no longer has.
        clone.compiler_item = None;

        let mut affected: Vec<Scalarized> = Vec::new();
        for pi in positions {
            let info = &candidates[&(*key, *pi)];
            let local_index = clone.params[*pi].local_index;
            affected.push(Scalarized {
                local: local_index,
                field_index: info.field_index,
                name: clone.params[*pi].name.clone(),
                borrow: info.form.borrow_op(),
            });
            clone.params[*pi].type_id = info.scalar_type_id;
            if let Some(local) = clone.locals.get_mut(local_index as usize) {
                local.type_id = info.scalar_type_id;
            }
        }
        if let Some(body) = clone.body.as_mut() {
            let root = body.root;
            rewrite_param_reads(body, NodeRef::Block(root), &affected);
        }

        project.func_index.insert(func_key, id);
        project.sroa_param_clone_fields.insert(
            id,
            positions
                .iter()
                .map(|pi| {
                    (
                        clone.params[*pi].local_index,
                        candidates[&(*key, *pi)].struct_key.clone(),
                    )
                })
                .collect(),
        );
        copy_function_strings(project, &origin, (clone.module_source.clone(), name));
        clones.insert(*key, id);
        minted.push(Rc::new(RefCell::new(clone)));
    }
    for f in minted {
        let id = FuncId::new(project.functions.len());
        touched.insert(project.functions.len());
        project.sroa_param_clones.insert(id);
        project.functions.push(f);
    }
    clones
}

/// Give the clone its own entry in `function_strings`, which is name-keyed: DCE
/// reads it to decide which string literals survive, so without one the clone's
/// literals are pruned out from under it.
fn copy_function_strings(
    project: &mut NirPackage,
    origin: &(ModuleSource, String),
    clone: (ModuleSource, String),
) {
    if let Some(strings) = project.function_strings.get(origin).cloned() {
        project.function_strings.insert(clone, strings);
    }
}

/// A parameter this clone scalarized: the local it occupies, the field it now
/// stands for, and whether it holds that field by reference.
struct Scalarized {
    local: u32,
    field_index: u32,
    name: String,
    /// The borrow the param's type already carries, if any.
    borrow: Option<NirUnaryOp>,
}

/// The affected param a `&`/`&mut` of a scalarized field access borrows, when
/// the param already holds that field by reference.
///
/// `&mut self.repr` on a `repr: &mut Array<u8>` param is `&mut (&mut …)`. The
/// extra borrow is not just noise: it makes the local address-taken, which
/// stops `copy_prop` folding it back into the call and leaves a temporary per
/// call site where the field read used to sit inline.
fn borrow_of_scalarized<'a>(
    body: &Body,
    id: ExprId,
    affected: &'a [Scalarized],
) -> Option<&'a Scalarized> {
    let ExprKind::Unary {
        op: op @ (NirUnaryOp::Ref | NirUnaryOp::MutRef),
        expr: inner,
    } = &body.exprs[id].kind
    else {
        return None;
    };
    let ExprKind::FieldAccess {
        expr: recv,
        field_index,
        ..
    } = &body.exprs[inner.as_expr()?].kind
    else {
        return None;
    };
    let (op, recv, field_index) = (*op, *recv, *field_index);
    affected.iter().find(|s| {
        s.borrow == Some(op)
            && s.field_index == field_index
            && is_local_operand(body, recv, s.local)
    })
}

/// Pre-order: replace the SROA'd param's `FieldAccess` with the bare scalar
/// `Local`, before children are reshaped. Matching on the field's index rather
/// than its name avoids over-stripping a same-named field of the field's own
/// type (e.g. `b.value.value`, whose inner `.value` belongs to another struct).
///
/// A `Local` read left standing — the param forwarded whole to another
/// scalarized position — is retyped in place. Leaving the node claiming the
/// wrapper's type makes every later reader of it wrong, and the call-site
/// rewrite is one: it would take the stale type as licence to project the
/// wrapper's field onto a value that is already the field.
fn rewrite_param_reads(body: &mut Body, node: NodeRef, affected: &[Scalarized]) {
    if let NodeRef::Expr(id) = node {
        // `&mut self.f` where the param is already `&mut F`: the whole borrow
        // becomes the param, not just its operand.
        if let Some(s) = borrow_of_scalarized(body, id, affected) {
            body.exprs[id].kind = ExprKind::Local {
                index: s.local,
                name: s.name.clone(),
            };
            return;
        }
        // The SROA'd field access, and the param it reads. The receiver is
        // matched as an *operand*: a promoted value that extracts back to the
        // local reads it just as a skeleton `Local` does, and skipping that form
        // leaves a `f.buf` behind on a param whose type is now the field's.
        let read = if let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &body.exprs[id].kind
        {
            let (inner, field_index) = (*inner, *field_index);
            affected
                .iter()
                .find(|s| s.field_index == field_index && is_local_operand(body, inner, s.local))
        } else {
            None
        };
        if let Some(s) = read {
            // The node keeps its (field-scalar) type_id / span; its kind becomes
            // the read of the scalarized param.
            body.exprs[id].kind = ExprKind::Local {
                index: s.local,
                name: s.name.clone(),
            };
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
    clones: &IndexMap<FnKey, FnKey>,
    touched: &mut IndexSet<usize>,
) {
    let mut sroa_positions: IndexMap<FnKey, (FnKey, IndexMap<usize, SroaInfo>)> =
        IndexMap::default();
    for ((key, pi), info) in candidates {
        let Some(clone) = clones.get(key) else {
            continue;
        };
        sroa_positions
            .entry(*key)
            .or_insert_with(|| (*clone, IndexMap::default()))
            .1
            .insert(*pi, info.clone());
    }

    let type_table_rc = project.type_table.clone();
    let clone_fields = project.sroa_param_clone_fields.clone();
    for (i, func_rc) in project.functions.iter().enumerate() {
        let mut func = func_rc.borrow_mut();
        let Some(key) = func.id else { continue };
        // Inside a clone the scalarized params already hold the field, so an
        // onward call at another candidate position passes them straight
        // through. Read from the package, not from this run's `clones`: a clone
        // minted on an earlier fixpoint iteration is still a clone, and losing
        // that fact projects the wrapper's field onto it a second time.
        let scalar_param_struct = clone_fields.get(&key).cloned().unwrap_or_default();
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
    sroa_positions: &IndexMap<FnKey, (FnKey, IndexMap<usize, SroaInfo>)>,
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
    sroa_positions: &IndexMap<FnKey, (FnKey, IndexMap<usize, SroaInfo>)>,
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
    let Some((clone, positions)) = sroa_positions.get(func_id).cloned() else {
        return false;
    };
    let span = body.exprs[id].span;
    let arg_ops: Vec<Operand> = args.iter().map(|a| a.expr).collect();
    // Both checks run before anything is mutated: a call the rewrite cannot make
    // safely is left calling the original.
    if !hoist_order_preserved(body, &arg_ops, &positions, scalar_param_struct)
        || !positions.iter().all(|(pi, info)| {
            arg_rewritable(
                body,
                scalarized_arg(&arg_ops, *pi),
                info,
                scalar_param_struct,
                type_table,
            )
        })
    {
        return false;
    }
    // Scalarizing position 0 replaces a method's receiver with a plain scalar,
    // so the call stops being one.
    let receiver_scalarized = *has_receiver && positions.contains_key(&0);
    let mut rewritten: Vec<(usize, Operand)> = Vec::with_capacity(positions.len());
    for (pi, info) in &positions {
        let op = scalarized_arg(&arg_ops, *pi);
        rewritten.push((
            *pi,
            rewrite_arg_operand(body, op, info, scalar_param_struct, type_table, span),
        ));
    }
    let ExprKind::Call {
        func_id,
        args,
        has_receiver,
        ..
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
    // The original keeps its shape for whatever this pass cannot see.
    *func_id = clone;
    true
}

/// Whether reading the field at the call site keeps the order the callee had.
///
/// The callee read `p.f` after every argument was evaluated; hoisting the read
/// into position `pi` moves it ahead of the arguments to its right, so any
/// effect among those could have changed the field the callee would have seen.
/// `consume(&mut c, bump_val(&mut c))` is the shape: `bump_val` writes `c.x`,
/// which `consume` then reads.
///
/// A call this rejects is left alone, and so keeps calling the original — the
/// reason the rewrite mints a clone rather than reshaping in place.
fn hoist_order_preserved(
    body: &Body,
    args: &[Operand],
    positions: &IndexMap<usize, SroaInfo>,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
) -> bool {
    // Only a position that actually reads `place.f` here moves a read earlier.
    // An argument already holding the field — a caller param this pass
    // scalarized — is handed over untouched, so nothing is resequenced. That
    // exemption is also what keeps a clone consistent: its own body forwards
    // scalarized params, and those calls *must* retarget, the original callee
    // no longer taking the type the clone now holds.
    let Some(first) = positions
        .iter()
        .filter(|(pi, info)| {
            scalarized_from(body, scalarized_arg(args, **pi), scalar_param_struct).as_ref()
                != Some(&info.struct_key)
        })
        .map(|(pi, _)| *pi)
        .min()
    else {
        return true;
    };
    args.iter()
        .skip(first + 1)
        .all(|&op| is_pure_operand(body, op))
}

/// Whether the argument at a scalarized position is one the field projection
/// applies to — its type must be the wrapper struct, however it is wrapped.
///
/// A clone's own scalarized parameter is the reason to ask. Unwrapping chains:
/// `Formatter::write(&mut self)` reading only `self.buf` becomes
/// `write$scalar(buf: &mut String)`, and a later round scalarizes that `String`
/// in turn. A caller already holding the inner `String` must not have
/// `Formatter`'s `buf` projected onto it a second time.
fn arg_rewritable(
    body: &Body,
    op: Operand,
    info: &SroaInfo,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
    type_table: &TypeTable,
) -> bool {
    // Already the field: a caller param scalarized from the same struct passes
    // straight through. Matched as an operand, since the promoted form of that
    // local reads it just as the skeleton one does.
    if scalarized_from(body, op, scalar_param_struct).as_ref() == Some(&info.struct_key) {
        return true;
    }
    let Some(arg) = op.as_expr() else {
        // A promoted operand has no node to inspect, so take the value graph's
        // type for it and require the same proof as any other argument.
        return op
            .as_value()
            .and_then(|v| body.values.type_of(v))
            .is_some_and(|ty| denotes_struct(ty, &info.struct_key, type_table));
    };
    denotes_struct(
        body.exprs[peel_one_ref(body, arg)].type_id,
        &info.struct_key,
        type_table,
    )
}

/// The struct a caller's own scalarized parameter came from, when `op` reads one.
fn scalarized_from(
    body: &Body,
    op: Operand,
    scalar_param_struct: &IndexMap<u32, (String, ModuleSource)>,
) -> Option<(String, ModuleSource)> {
    scalar_param_struct
        .iter()
        .find(|(local, _)| is_local_operand(body, op, **local))
        .map(|(_, key)| key.clone())
}

/// The single auto-ref wrapper [`rewrite_arg`] peels, so validation and rewrite
/// look at the same node.
fn peel_one_ref(body: &Body, arg: ExprId) -> ExprId {
    match &body.exprs[arg].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } => inner.as_expr().unwrap_or(arg),
        _ => arg,
    }
}

/// Whether `ty` names the wrapper struct, directly or behind a reference or box.
fn denotes_struct(ty: TypeId, key: &(String, ModuleSource), type_table: &TypeTable) -> bool {
    struct_key_of(ty, type_table).as_ref() == Some(key)
        || reference_param_struct_key(ty, type_table).as_ref() == Some(key)
        || type_table
            .box_payload_of(ty)
            .is_some_and(|p| struct_key_of(p, type_table).as_ref() == Some(key))
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
    // The promoted form of Case 2: the local already holds the field, so the
    // call carries it unchanged.
    if scalarized_from(body, op, scalar_param_struct).as_ref() == Some(&info.struct_key) {
        return op;
    }
    let Some(arg) = op.as_expr() else {
        return Operand::Expr(body.exprs.push(ExprNode {
            kind: ExprKind::FieldAccess {
                expr: op,
                field_index: info.field_index,
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

    // Case 1: StructLiteral matching the wrapper's canonical identity → unwrap
    // to the field the callee reads. Only a skeleton field is lifted in place; a
    // promoted constant field falls through to Case 3's `FieldAccess`
    // (`(Wrapper{V}).f`, folded later) since it has no node to become.
    //
    // The other fields are dropped with the literal, so each must be something
    // whose evaluation nothing observes — a constant, or a local read. That is
    // the ordinary case for a record built to carry options at a call.
    if let ExprKind::StructLiteral {
        struct_type,
        fields,
        ..
    } = &body.exprs[arg].kind
        && struct_key_of(*struct_type, type_table).as_ref() == Some(&info.struct_key)
        && let Some(used) = fields.iter().find(|f| f.field_index == info.field_index)
        && let Some(fe) = used.value.as_expr()
        && fields
            .iter()
            .all(|f| f.field_index == info.field_index || discardable_field(body, f.value))
    {
        become_expr(body, arg, fe);
        return;
    }

    // Case 2: a local whose own param was SROA'd from the same struct.
    if scalarized_from(body, Operand::Expr(arg), scalar_param_struct).as_ref()
        == Some(&info.struct_key)
    {
        body.exprs[arg].type_id = info.scalar_type_id;
        return;
    }

    // Case 3: general — extract the field via FieldAccess, re-borrowed when the
    // callee writes through it. The `&mut` peeled above was the whole struct's;
    // this one is the field's.
    let moved = body.take_expr(arg);
    let orig = body.exprs.push(moved);
    let span = body.exprs[arg].span;
    let field = ExprNode {
        kind: ExprKind::FieldAccess {
            expr: orig.into(),
            field_index: info.field_index,
            field_name: info.field_name.clone(),
        },
        type_id: info.inner_type_id,
        span,
    };
    body.exprs[arg] = field;
    if let Some(op) = info.form.borrow_op() {
        let moved_field = body.take_expr(arg);
        let inner = body.exprs.push(moved_field);
        body.exprs[arg] = ExprNode {
            kind: ExprKind::Unary {
                op,
                expr: inner.into(),
            },
            type_id: info.scalar_type_id,
            span,
        };
    }
}

/// Whether dropping this field initializer with its literal is unobservable.
fn discardable_field(body: &Body, value: Operand) -> bool {
    value.as_expr().is_none_or(|e| {
        matches!(
            &body.exprs[e].kind,
            ExprKind::Local { .. } | ExprKind::PackedArray(_) | ExprKind::GlobalVarGet { .. }
        )
    })
}

// -----------------------------------------------------------------------
// Pinning
// -----------------------------------------------------------------------

/// Pinning rules, shared with DAE via [`super::dae::is_dae_sroa_eligible`].
/// `relax_closure_call = false` keeps closure `__call` functors pinned, their
/// function-table wrapper having snapshotted the signature, and one pin is added
/// here: a `$value_copy$T` helper is never a rewrite target. A concrete
/// trait-impl method is eligible, every post-mono call site being resolved.
fn is_eligible(func: &NirFunction) -> bool {
    super::dae::is_dae_sroa_eligible(func, false) && !func.is_value_copy()
}
