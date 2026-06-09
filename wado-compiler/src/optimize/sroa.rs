//! Scalar Replacement of Aggregates (SROA) optimization for Wado NIR
//!
//! Eliminates struct and tuple allocations when the aggregate is only used for
//! field access. After inlining exposes:
//!
//! ```text
//! let s = MyStruct { x: expr1, y: expr2 };
//! let a = s.x;
//! let b = s.y;
//! ```
//!
//! SROA decomposes the struct into individual scalar locals:
//!
//! ```text
//! let __sroa_s_x = expr1;
//! let __sroa_s_y = expr2;
//! let a = __sroa_s_x;
//! let b = __sroa_s_y;
//! ```
//!
//! Copy propagation then eliminates the trivial copies.
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): candidate collection,
//! escape analysis, and the decomposition rewrite read and mutate the arena
//! `Body` directly. New scalar locals are pushed to `func.locals` (reached via
//! disjoint-field access from `func.body`).

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirFunction, NirLocal};
use crate::nir_arena::{
    ArenaStructField, BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, StmtId, StmtKind,
    StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir::TypeId;
use crate::token::Span;

/// Maps (`module_source`, `func_name`) → set of parameter indices that have `stores` declared.
type StoresLookup = IndexMap<(ModuleSource, String), IndexSet<usize>>;

/// Information about a struct/tuple local that may be decomposable.
struct SroaCandidate {
    local_index: u32,
    local_name: String,
    /// Per-field info: (`field_name`, `field_type_id`).
    fields: Vec<(String, TypeId)>,
    is_mut: bool,
    aggregate_type_id: TypeId,
    /// The struct name (empty for tuples).
    struct_name: String,
}

fn build_stores_lookup(project: &NirPackage) -> StoresLookup {
    let mut lookup = StoresLookup::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if func.stores.is_empty() {
            continue;
        }
        let stored_indices: IndexSet<usize> = func
            .params
            .iter()
            .enumerate()
            .filter(|(_, param)| func.stores.iter().any(|s| s == &param.name))
            .map(|(i, _)| i)
            .collect();
        if !stored_indices.is_empty() {
            lookup.insert(
                (func.module_source.clone(), func.name.clone()),
                stored_indices,
            );
        }
    }
    lookup
}

pub fn scalar_replace_aggregates(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let stores_lookup = build_stores_lookup(project);
    let len = project.functions.len();
    gate.run_gated(GatedPass::Sroa, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        let module_source = func.module_source.clone();
        sroa_in_function(&mut func, &stores_lookup, &module_source)
    })
}

fn sroa_in_function(
    func: &mut NirFunction,
    stores_lookup: &StoresLookup,
    current_module: &ModuleSource,
) -> bool {
    if func.body.is_none() {
        return false;
    }

    // Step 1: identify candidate Let bindings (struct/tuple literals).
    let candidates = collect_candidates(func.body.as_ref().unwrap());
    if candidates.is_empty() {
        return false;
    }

    // Step 2: escape analysis.
    let body_ref = func.body.as_ref().unwrap();
    let escaped = find_escaped_locals(body_ref, &candidates);
    let soft_escaped = find_soft_escaped_locals(
        body_ref,
        &candidates,
        &escaped,
        stores_lookup,
        current_module,
    );

    let mut safe_set: IndexSet<u32> = IndexSet::default();
    let mut reconstruct_set: IndexSet<u32> = IndexSet::default();
    for c in &candidates {
        if func.stores_aliased_locals.contains(&c.local_index) {
            continue;
        }
        if !escaped.contains(&c.local_index) {
            safe_set.insert(c.local_index);
        } else if soft_escaped.contains(&c.local_index) {
            reconstruct_set.insert(c.local_index);
        }
    }

    let all_sroa: IndexSet<u32> = safe_set
        .iter()
        .chain(reconstruct_set.iter())
        .copied()
        .collect();
    if all_sroa.is_empty() {
        return false;
    }

    // Step 3: allocate scalar locals for each field of each SROA'd candidate.
    let mut field_local_map: IndexMap<(u32, u32), u32> = IndexMap::default();
    let mut field_info_map: IndexMap<(u32, u32), (String, TypeId)> = IndexMap::default();
    for candidate in &candidates {
        if !all_sroa.contains(&candidate.local_index) {
            continue;
        }
        let base = func.local_count();
        for (i, (field_name, field_type)) in candidate.fields.iter().enumerate() {
            let new_index = base + i as u32;
            field_local_map.insert((candidate.local_index, i as u32), new_index);
            let new_name = format!("__sroa_{}_{}", candidate.local_name, field_name);
            field_info_map.insert(
                (candidate.local_index, i as u32),
                (new_name.clone(), *field_type),
            );
            func.locals.push(NirLocal {
                name: new_name,
                type_id: *field_type,
                is_mut: candidate.is_mut,
            });
        }
    }

    let mut candidate_mut: IndexMap<u32, bool> = IndexMap::default();
    let mut reconstruct_info: IndexMap<u32, ReconstructInfo> = IndexMap::default();
    for candidate in &candidates {
        if !all_sroa.contains(&candidate.local_index) {
            continue;
        }
        candidate_mut.insert(candidate.local_index, candidate.is_mut);
        if reconstruct_set.contains(&candidate.local_index) {
            reconstruct_info.insert(
                candidate.local_index,
                ReconstructInfo {
                    struct_name: candidate.struct_name.clone(),
                    aggregate_type_id: candidate.aggregate_type_id,
                    fields: candidate.fields.clone(),
                },
            );
        }
    }

    // Step 3b: mark locals referenced via &local in decomposed struct fields.
    {
        let body = func.body.as_ref().unwrap();
        let mut newly_aliased: IndexSet<u32> = IndexSet::default();
        mark_ref_field_locals_as_aliased(body, body.root, &all_sroa, &mut newly_aliased);
        func.stores_aliased_locals.extend(newly_aliased);
    }

    // Step 4: rewrite — expand candidate Lets and replace field accesses.
    let ctx = Rewrite {
        safe_set: &all_sroa,
        field_map: &field_local_map,
        info_map: &field_info_map,
        candidate_mut: &candidate_mut,
        reconstruct_info: &reconstruct_info,
    };
    let body = func.body.as_mut().unwrap();
    let root = body.root;
    rewrite_block(body, root, &ctx);

    true
}

// -----------------------------------------------------------------------
// Step 3b: mark &local field values as stores-aliased
// -----------------------------------------------------------------------

fn mark_ref_field_locals_as_aliased(
    body: &Body,
    block: BlockId,
    decomposed: &IndexSet<u32>,
    stores_aliased: &mut IndexSet<u32>,
) {
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node
            && let StmtKind::Let {
                local_index, value, ..
            } = &body.stmts[s].kind
            && decomposed.contains(local_index)
        {
            collect_ref_locals_in_fields(body, *value, stores_aliased);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
}

fn collect_ref_locals_in_fields(body: &Body, expr: ExprId, stores_aliased: &mut IndexSet<u32>) {
    match &body.exprs[expr].kind {
        ExprKind::StructLiteral { fields, .. } => {
            let vals: Vec<ExprId> = fields.iter().map(|f| f.value).collect();
            for v in vals {
                extract_ref_local(body, v, stores_aliased);
            }
        }
        ExprKind::TupleLiteral { elements, .. } => {
            let elems = elements.clone();
            for e in elems {
                extract_ref_local(body, e, stores_aliased);
            }
        }
        _ => {}
    }
}

fn extract_ref_local(body: &Body, expr: ExprId, stores_aliased: &mut IndexSet<u32>) {
    if let ExprKind::Unary { op, expr: inner } = &body.exprs[expr].kind
        && matches!(
            op,
            crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef
        )
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
    {
        stores_aliased.insert(*index);
    }
}

struct ReconstructInfo {
    struct_name: String,
    aggregate_type_id: TypeId,
    fields: Vec<(String, TypeId)>,
}

// -----------------------------------------------------------------------
// Candidate collection
// -----------------------------------------------------------------------

fn collect_candidates(body: &Body) -> Vec<SroaCandidate> {
    let mut candidates = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Stmt(s) = node {
            candidate_from_stmt(body, s, &mut candidates);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    candidates
}

fn candidate_from_stmt(body: &Body, stmt: StmtId, candidates: &mut Vec<SroaCandidate>) {
    let StmtKind::Let {
        name,
        local_index,
        is_mut,
        value,
        ..
    } = &body.stmts[stmt].kind
    else {
        return;
    };
    let (name, local_index, is_mut, value) = (name.clone(), *local_index, *is_mut, *value);
    let aggregate_type_id = body.exprs[value].type_id;
    match &body.exprs[value].kind {
        ExprKind::StructLiteral {
            struct_name,
            fields,
            ..
        } => {
            let field_info: Vec<(String, TypeId)> = fields
                .iter()
                .map(|f| (f.name.clone(), body.exprs[f.value].type_id))
                .collect();
            candidates.push(SroaCandidate {
                local_index,
                local_name: name,
                fields: field_info,
                is_mut,
                aggregate_type_id,
                struct_name: struct_name.clone(),
            });
        }
        ExprKind::TupleLiteral { elements, .. } => {
            let field_info: Vec<(String, TypeId)> = elements
                .iter()
                .enumerate()
                .map(|(i, e)| (i.to_string(), body.exprs[*e].type_id))
                .collect();
            candidates.push(SroaCandidate {
                local_index,
                local_name: name,
                fields: field_info,
                is_mut,
                aggregate_type_id,
                struct_name: String::new(),
            });
        }
        _ => {}
    }
}

// -----------------------------------------------------------------------
// Escape analysis
// -----------------------------------------------------------------------

fn is_candidate_local(body: &Body, expr: ExprId, candidates: &IndexSet<u32>) -> Option<u32> {
    if let ExprKind::Local { index, .. } = &body.exprs[expr].kind
        && candidates.contains(index)
    {
        return Some(*index);
    }
    None
}

fn is_immut_ref_to_candidate(body: &Body, expr: ExprId, candidates: &IndexSet<u32>) -> bool {
    if let ExprKind::Unary { op, expr: inner } = &body.exprs[expr].kind
        && matches!(op, crate::nir::NirUnaryOp::Ref)
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
        && candidates.contains(index)
    {
        return true;
    }
    false
}

fn find_escaped_locals(body: &Body, candidates: &[SroaCandidate]) -> IndexSet<u32> {
    let candidate_set: IndexSet<u32> = candidates.iter().map(|c| c.local_index).collect();
    let mut escaped = IndexSet::default();
    escape_node(
        body,
        NodeRef::Block(body.root),
        &candidate_set,
        &mut escaped,
    );
    escaped
}

fn escape_node(
    body: &Body,
    node: NodeRef,
    candidates: &IndexSet<u32>,
    escaped: &mut IndexSet<u32>,
) {
    if let NodeRef::Expr(id) = node {
        escape_expr(body, id, candidates, escaped);
    } else {
        body.for_each_child(node, |c| escape_node(body, c, candidates, escaped));
    }
}

fn escape_expr(body: &Body, id: ExprId, candidates: &IndexSet<u32>, escaped: &mut IndexSet<u32>) {
    match &body.exprs[id].kind {
        ExprKind::FieldAccess { expr: inner, .. } => {
            let inner = *inner;
            if is_candidate_local(body, inner, candidates).is_some() {
                return;
            }
            escape_expr(body, inner, candidates, escaped);
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[target].kind
                && is_candidate_local(body, *inner, candidates).is_some()
            {
                escape_expr(body, value, candidates, escaped);
                return;
            }
            escape_expr(body, target, candidates, escaped);
            escape_expr(body, value, candidates, escaped);
        }
        ExprKind::Local { index, .. } => {
            if candidates.contains(index) {
                escaped.insert(*index);
            }
        }
        ExprKind::Unary { op, expr: inner } => {
            let inner = *inner;
            if matches!(
                op,
                crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef
            ) && let ExprKind::Local { index, .. } = &body.exprs[inner].kind
                && candidates.contains(index)
            {
                escaped.insert(*index);
                return;
            }
            escape_expr(body, inner, candidates, escaped);
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                escape_node(body, c, candidates, escaped);
            }
        }
    }
}

fn find_soft_escaped_locals(
    body: &Body,
    candidates: &[SroaCandidate],
    escaped: &IndexSet<u32>,
    stores_lookup: &StoresLookup,
    current_module: &ModuleSource,
) -> IndexSet<u32> {
    let escaped_candidates: IndexSet<u32> = candidates
        .iter()
        .map(|c| c.local_index)
        .filter(|idx| escaped.contains(idx))
        .collect();
    if escaped_candidates.is_empty() {
        return IndexSet::default();
    }

    let mut hard_escaped = IndexSet::default();
    soft_node(
        body,
        NodeRef::Block(body.root),
        &escaped_candidates,
        stores_lookup,
        current_module,
        &mut hard_escaped,
    );

    let mut has_field_access = IndexSet::default();
    field_access_node(
        body,
        NodeRef::Block(body.root),
        &escaped_candidates,
        &mut has_field_access,
    );

    escaped_candidates
        .into_iter()
        .filter(|idx| !hard_escaped.contains(idx) && has_field_access.contains(idx))
        .collect()
}

fn field_access_node(
    body: &Body,
    node: NodeRef,
    candidates: &IndexSet<u32>,
    has_access: &mut IndexSet<u32>,
) {
    if let NodeRef::Expr(id) = node {
        match &body.exprs[id].kind {
            ExprKind::FieldAccess { expr: inner, .. } => {
                let inner = *inner;
                if let Some(idx) = is_candidate_local(body, inner, candidates) {
                    has_access.insert(idx);
                    return;
                }
                field_access_node(body, NodeRef::Expr(inner), candidates, has_access);
            }
            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                if let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[target].kind
                    && let Some(idx) = is_candidate_local(body, *inner, candidates)
                {
                    has_access.insert(idx);
                    field_access_node(body, NodeRef::Expr(value), candidates, has_access);
                    return;
                }
                field_access_node(body, NodeRef::Expr(target), candidates, has_access);
                field_access_node(body, NodeRef::Expr(value), candidates, has_access);
            }
            _ => {
                let mut kids = Vec::new();
                body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
                for c in kids {
                    field_access_node(body, c, candidates, has_access);
                }
            }
        }
    } else {
        body.for_each_child(node, |c| field_access_node(body, c, candidates, has_access));
    }
}

#[allow(clippy::too_many_arguments)]
fn soft_node(
    body: &Body,
    node: NodeRef,
    candidates: &IndexSet<u32>,
    stores_lookup: &StoresLookup,
    current_module: &ModuleSource,
    hard_escaped: &mut IndexSet<u32>,
) {
    match node {
        NodeRef::Stmt(s) => {
            // Return / Break value's top expression is a soft context.
            if let StmtKind::Return { value: Some(v) } | StmtKind::Break { value: Some(v), .. } =
                &body.stmts[s].kind
            {
                let v = *v;
                soft_expr(
                    body,
                    v,
                    true,
                    candidates,
                    stores_lookup,
                    current_module,
                    hard_escaped,
                );
            } else {
                body.for_each_child(NodeRef::Stmt(s), |c| {
                    soft_node(
                        body,
                        c,
                        candidates,
                        stores_lookup,
                        current_module,
                        hard_escaped,
                    );
                });
            }
        }
        NodeRef::Expr(id) => soft_expr(
            body,
            id,
            false,
            candidates,
            stores_lookup,
            current_module,
            hard_escaped,
        ),
        _ => body.for_each_child(node, |c| {
            soft_node(
                body,
                c,
                candidates,
                stores_lookup,
                current_module,
                hard_escaped,
            );
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn soft_expr(
    body: &Body,
    id: ExprId,
    soft: bool,
    candidates: &IndexSet<u32>,
    stores_lookup: &StoresLookup,
    current_module: &ModuleSource,
    hard_escaped: &mut IndexSet<u32>,
) {
    match &body.exprs[id].kind {
        ExprKind::FieldAccess { expr: inner, .. } => {
            let inner = *inner;
            if is_candidate_local(body, inner, candidates).is_some() {
                return;
            }
            soft_expr(
                body,
                inner,
                false,
                candidates,
                stores_lookup,
                current_module,
                hard_escaped,
            );
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[target].kind
                && is_candidate_local(body, *inner, candidates).is_some()
            {
                soft_expr(
                    body,
                    value,
                    false,
                    candidates,
                    stores_lookup,
                    current_module,
                    hard_escaped,
                );
                return;
            }
            soft_expr(
                body,
                target,
                false,
                candidates,
                stores_lookup,
                current_module,
                hard_escaped,
            );
            soft_expr(
                body,
                value,
                false,
                candidates,
                stores_lookup,
                current_module,
                hard_escaped,
            );
        }
        ExprKind::Local { index, .. } => {
            if candidates.contains(index) && !soft {
                hard_escaped.insert(*index);
            }
        }
        ExprKind::Unary { op, expr: inner } => {
            let inner = *inner;
            if matches!(
                op,
                crate::nir::NirUnaryOp::Ref | crate::nir::NirUnaryOp::MutRef
            ) && let ExprKind::Local { index, .. } = &body.exprs[inner].kind
                && candidates.contains(index)
            {
                hard_escaped.insert(*index);
                return;
            }
            soft_expr(
                body,
                inner,
                false,
                candidates,
                stores_lookup,
                current_module,
                hard_escaped,
            );
        }
        ExprKind::Call { func, args, .. } => {
            let func = func.clone();
            let arg_exprs: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for (i, arg) in arg_exprs.into_iter().enumerate() {
                if is_immut_ref_to_candidate(body, arg, candidates)
                    && !callee_stores_param_at(&func, i, current_module, stores_lookup)
                {
                    continue;
                }
                soft_expr(
                    body,
                    arg,
                    false,
                    candidates,
                    stores_lookup,
                    current_module,
                    hard_escaped,
                );
            }
        }
        ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let receiver = *receiver;
            let func = func.clone();
            let arg_exprs: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            if !is_immut_ref_to_candidate(body, receiver, candidates)
                || callee_stores_param_at(&func, 0, current_module, stores_lookup)
            {
                soft_expr(
                    body,
                    receiver,
                    false,
                    candidates,
                    stores_lookup,
                    current_module,
                    hard_escaped,
                );
            }
            for (i, arg) in arg_exprs.into_iter().enumerate() {
                if is_immut_ref_to_candidate(body, arg, candidates)
                    && !callee_stores_param_at(&func, i + 1, current_module, stores_lookup)
                {
                    continue;
                }
                soft_expr(
                    body,
                    arg,
                    false,
                    candidates,
                    stores_lookup,
                    current_module,
                    hard_escaped,
                );
            }
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                soft_node(
                    body,
                    c,
                    candidates,
                    stores_lookup,
                    current_module,
                    hard_escaped,
                );
            }
        }
    }
}

fn callee_stores_param_at(
    func_ref: &FunctionRef,
    param_index: usize,
    current_module: &ModuleSource,
    stores_lookup: &StoresLookup,
) -> bool {
    let target_module = if func_ref.module_source.is_entry_point() {
        current_module.clone()
    } else {
        func_ref.module_source.clone()
    };
    let key = (target_module, func_ref.name.clone());
    match stores_lookup.get(&key) {
        Some(stored_indices) => stored_indices.contains(&param_index),
        None => false,
    }
}

// -----------------------------------------------------------------------
// Rewrite
// -----------------------------------------------------------------------

struct Rewrite<'a> {
    safe_set: &'a IndexSet<u32>,
    field_map: &'a IndexMap<(u32, u32), u32>,
    info_map: &'a IndexMap<(u32, u32), (String, TypeId)>,
    candidate_mut: &'a IndexMap<u32, bool>,
    reconstruct_info: &'a IndexMap<u32, ReconstructInfo>,
}

fn rewrite_block(body: &mut Body, block: BlockId, ctx: &Rewrite) {
    let old_stmts = body.blocks[block].stmts.clone();
    let mut new_stmts: Vec<StmtId> = Vec::with_capacity(old_stmts.len());
    for stmt in old_stmts {
        let candidate = match &body.stmts[stmt].kind {
            StmtKind::Let { local_index, .. } if ctx.safe_set.contains(local_index) => {
                Some(*local_index)
            }
            _ => None,
        };
        if let Some(local_idx) = candidate {
            let span = body.stmts[stmt].span;
            let is_mut = ctx.candidate_mut.get(&local_idx).copied().unwrap_or(false);
            let StmtKind::Let { value, .. } = &body.stmts[stmt].kind else {
                unreachable!("candidate must be Let statement");
            };
            let value = *value;
            expand_struct_let(body, value, local_idx, is_mut, span, ctx, &mut new_stmts);
            continue;
        }
        rewrite_node(body, NodeRef::Stmt(stmt), ctx);
        new_stmts.push(stmt);
    }
    body.blocks[block].stmts = new_stmts;
}

fn rewrite_node(body: &mut Body, node: NodeRef, ctx: &Rewrite) {
    match node {
        NodeRef::Expr(id) => rewrite_expr(body, id, ctx),
        NodeRef::Block(b) => rewrite_block(body, b, ctx),
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(node, |c| kids.push(c));
            for c in kids {
                rewrite_node(body, c, ctx);
            }
        }
    }
}

fn rewrite_expr(body: &mut Body, id: ExprId, ctx: &Rewrite) {
    // Field read: candidate.field -> scalar local.
    if let ExprKind::FieldAccess {
        expr: inner,
        field_index,
        ..
    } = &body.exprs[id].kind
    {
        let (inner, field_index) = (*inner, *field_index);
        if let Some(local_idx) = is_candidate_local(body, inner, ctx.safe_set) {
            let key = (local_idx, field_index);
            if let Some(&new_local) = ctx.field_map.get(&key) {
                let new_name = ctx.info_map[&key].0.clone();
                body.exprs[id].kind = ExprKind::Local {
                    index: new_local,
                    name: new_name,
                };
                return;
            }
        }
    }

    // Field write: candidate.field = value -> scalar_local = value.
    if let ExprKind::Assign { target, value } = &body.exprs[id].kind {
        let (target, value) = (*target, *value);
        if let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &body.exprs[target].kind
        {
            let (inner, field_index) = (*inner, *field_index);
            if let Some(local_idx) = is_candidate_local(body, inner, ctx.safe_set) {
                let key = (local_idx, field_index);
                if let Some(&new_local) = ctx.field_map.get(&key) {
                    let new_name = ctx.info_map[&key].0.clone();
                    body.exprs[target].kind = ExprKind::Local {
                        index: new_local,
                        name: new_name,
                    };
                    rewrite_expr(body, value, ctx);
                    return;
                }
            }
        }
    }

    // Reconstruct: bare Local of a soft-escape candidate -> re-materialize.
    if let ExprKind::Local { index, .. } = &body.exprs[id].kind {
        let index = *index;
        if ctx.reconstruct_info.contains_key(&index) {
            reconstruct_aggregate(body, id, index, ctx);
            return;
        }
    }

    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
    for c in kids {
        rewrite_node(body, c, ctx);
    }
}

/// Expand a candidate `Let` value into one per-field `Let`, rewriting each
/// field expression as it goes.
fn expand_struct_let(
    body: &mut Body,
    value: ExprId,
    local_idx: u32,
    is_mut: bool,
    span: Span,
    ctx: &Rewrite,
    new_stmts: &mut Vec<StmtId>,
) {
    // (field_index, value_expr) pairs in field-index order.
    let mut pairs: Vec<(u32, ExprId)> = match &body.exprs[value].kind {
        ExprKind::StructLiteral { fields, .. } => {
            fields.iter().map(|f| (f.field_index, f.value)).collect()
        }
        ExprKind::TupleLiteral { elements, .. } => elements
            .iter()
            .enumerate()
            .map(|(i, e)| (i as u32, *e))
            .collect(),
        _ => unreachable!("candidate must be struct or tuple literal"),
    };
    pairs.sort_by_key(|(fi, _)| *fi);
    for (field_index, field_value) in pairs {
        rewrite_expr(body, field_value, ctx);
        push_field_let(
            body,
            (local_idx, field_index),
            is_mut,
            span,
            field_value,
            ctx,
            new_stmts,
        );
    }
}

fn push_field_let(
    body: &mut Body,
    key: (u32, u32),
    is_mut: bool,
    span: Span,
    value: ExprId,
    ctx: &Rewrite,
    new_stmts: &mut Vec<StmtId>,
) {
    let new_local = ctx.field_map[&key];
    let (new_name, field_type) = ctx.info_map[&key].clone();
    let stmt = body.stmts.push(StmtNode {
        kind: StmtKind::Let {
            name: new_name,
            local_index: new_local,
            is_mut,
            is_reactive: false,
            type_id: field_type,
            value,
            // The original literal was a fresh value, so its fields don't need
            // value_copy — see the original pass comment.
            skip_value_copy: true,
        },
        span,
    });
    new_stmts.push(stmt);
}

/// Build a reconstructed struct or tuple literal from SROA'd scalar locals,
/// replacing the bare-`Local` node `id` in place (keeping its `type_id` / span).
fn reconstruct_aggregate(body: &mut Body, id: ExprId, local_idx: u32, ctx: &Rewrite) {
    let info = &ctx.reconstruct_info[&local_idx];
    let span = body.exprs[id].span;
    let is_tuple = info.struct_name.is_empty();
    let field_specs: Vec<(String, TypeId)> = info.fields.clone();
    let struct_name = info.struct_name.clone();
    let struct_type = info.aggregate_type_id;

    if is_tuple {
        let mut elements: Vec<ExprId> = Vec::with_capacity(field_specs.len());
        for (i, (_, type_id)) in field_specs.iter().enumerate() {
            let key = (local_idx, i as u32);
            let field_local = ctx.field_map[&key];
            let field_name = ctx.info_map[&key].0.clone();
            let e = body.exprs.push(ExprNode {
                kind: ExprKind::Local {
                    index: field_local,
                    name: field_name,
                },
                type_id: *type_id,
                span,
            });
            elements.push(e);
        }
        body.exprs[id].kind = ExprKind::TupleLiteral { elements };
    } else {
        let mut fields: Vec<ArenaStructField> = Vec::with_capacity(field_specs.len());
        for (i, (name, type_id)) in field_specs.iter().enumerate() {
            let key = (local_idx, i as u32);
            let field_local = ctx.field_map[&key];
            let field_name = ctx.info_map[&key].0.clone();
            let value = body.exprs.push(ExprNode {
                kind: ExprKind::Local {
                    index: field_local,
                    name: field_name,
                },
                type_id: *type_id,
                span,
            });
            fields.push(ArenaStructField {
                name: name.clone(),
                value,
                field_index: i as u32,
            });
        }
        body.exprs[id].kind = ExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        };
    }
}
