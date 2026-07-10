//! Copy propagation optimization for Wado NIR.
//!
//! Eliminates trivial copy bindings like `let x = y`, `let x = &y`,
//! `let x = &mut y`, or a copy of a promoted operand by propagating the source
//! to all uses of the target. See `can_propagate_copy` for the safety gates.
//!
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and runs the analyse/substitute fixpoint to convergence in
//! one shot. The substitute-and-remove rewrites route through
//! `engine.replace_expr_kind`, `engine.alloc_expr`, and
//! `engine.set_block_stmts` so the parent map and use index stay coherent.

use std::cell::Cell;

use cranelift_entity::EntityRef;

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{FuncId, NirFunction, NirUnaryOp};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::arena_query::{place_root_local, storage_root};
use super::gate::{FunctionGate, GatedPass};

#[derive(Debug, Clone)]
struct CopyBinding {
    target_local: u32,
    source: CopySource,
    type_id: TypeId,
    /// Whether the source value is stable across the target's scope: the source
    /// local is never *mutated* (re-assigned, field-mutated, `&mut`-borrowed, or
    /// passed as a mutable argument) anywhere in the binding block's statements
    /// after the binding. The target's uses are confined to that scope, so a
    /// stable source can be propagated even when the source is reassigned
    /// elsewhere in the function (e.g. a loop counter copied inside the loop
    /// body). Always `true` for a promoted-value source.
    source_scope_stable: bool,
}

impl CopySource {
    /// The source local index for the index-bearing sources; `None` for a
    /// promoted-value source (always stable — a pooled `ValueId` is immutable).
    fn local_index(&self) -> Option<u32> {
        match self {
            CopySource::Local { index, .. }
            | CopySource::Ref { index, .. }
            | CopySource::MutRef { index, .. } => Some(*index),
            CopySource::RefProjection { root_local, .. } => Some(*root_local),
            CopySource::Promoted(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
enum CopySource {
    Local {
        index: u32,
        name: String,
    },
    Ref {
        index: u32,
        name: String,
        inner_type_id: TypeId,
    },
    MutRef {
        index: u32,
        name: String,
        inner_type_id: TypeId,
    },
    /// `let x = Operand::Value(v)` — a copy of a promoted operand. `x`'s reads
    /// forward to `Operand::Value(v)` directly; the pooled value is immutable so
    /// the copy is unconditionally stable.
    Promoted(crate::nir_value_graph::ValueId),
    /// `let x = &place` / `&mut place` (a pure `FieldAccess` chain rooted at
    /// `root_local`), re-materialized at `x`'s single use. Single-use +
    /// `source_scope_stable` keep capture-at-binding semantics: a root reassigned
    /// before the use blocks the propagation.
    RefProjection {
        root_local: u32,
        op: NirUnaryOp,
        projection: ExprId,
    },
}

#[derive(Debug, Default)]
struct LocalUsage {
    read_count: u32,
    /// Number of `let` statements defining this local. More than one means
    /// multiple reaching defs on disjoint paths (the shape
    /// `labeled_block_fusion` produces reusing one binding slot across break
    /// sites); forwarding every read to one source would leak it across paths.
    def_count: u32,
    is_assigned: bool,
    has_field_mutation: bool,
    address_taken: bool,
}

/// If `expr` is `builtin::copy_value::<T>(inner)`, return `inner`; else `expr`.
/// `copy_value_id` is the builtin's `FuncId` (resolved once at the pass top);
/// identity is an integer compare against the call node's `func_id`.
fn unwrap_copy_value(body: &Body, expr: ExprId, copy_value_id: Option<FuncId>) -> ExprId {
    if let ExprKind::Call { func_id, args, .. } = &body.exprs[expr].kind
        && copy_value_id == Some(*func_id)
        && args.len() == 1
    {
        return args[0].expr.as_expr().unwrap_or(expr);
    }
    expr
}

/// Root local of a pure `FieldAccess` chain (`local.f.g`), else `None`.
fn field_chain_root(body: &Body, e: ExprId) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. } => field_chain_root(body, inner.as_expr()?),
        _ => None,
    }
}

fn analyze_copy_binding(
    body: &Body,
    stmt: StmtId,
    copy_value_id: Option<FuncId>,
) -> Option<CopyBinding> {
    let StmtKind::Let {
        local_index,
        value,
        skip_value_copy,
        type_id: let_type_id,
        ..
    } = &body.stmts[stmt].kind
    else {
        return None;
    };
    let (local_index, value, skip_value_copy, let_type_id) =
        (*local_index, *value, *skip_value_copy, *let_type_id);
    // `let x = Operand::Value(v)`: a copy of a promoted operand. Forward `x`'s
    // reads to it (the value is pooled-immutable, so it is always stable). This
    // is independent of `skip_value_copy` — the source is a value, not a place.
    if let Operand::Value(v) = value {
        return Some(CopyBinding {
            target_local: local_index,
            source: CopySource::Promoted(v),
            type_id: let_type_id,
            source_scope_stable: true,
        });
    }
    if skip_value_copy {
        return None;
    }
    let value = unwrap_copy_value(body, value.as_expr()?, copy_value_id);
    let value_type = body.exprs[value].type_id;

    let source = match &body.exprs[value].kind {
        ExprKind::Local { index, name } => CopySource::Local {
            index: *index,
            name: name.clone(),
        },
        ExprKind::Unary { op, expr: inner }
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef) =>
        {
            let op = *op;
            let is_ref = matches!(op, NirUnaryOp::Ref);
            let ie = inner.as_expr()?;
            if let ExprKind::Local { index, name } = &body.exprs[ie].kind {
                let inner_type_id = body.exprs[ie].type_id;
                if is_ref {
                    CopySource::Ref {
                        index: *index,
                        name: name.clone(),
                        inner_type_id,
                    }
                } else {
                    CopySource::MutRef {
                        index: *index,
                        name: name.clone(),
                        inner_type_id,
                    }
                }
            } else if let Some(root_local) = field_chain_root(body, ie) {
                CopySource::RefProjection {
                    root_local,
                    op,
                    projection: ie,
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    Some(CopyBinding {
        target_local: local_index,
        source,
        type_id: value_type,
        // Filled in by `analyze_block`, which knows the binding's position.
        source_scope_stable: false,
    })
}

struct AnalysisResult {
    bindings: Vec<CopyBinding>,
    usage: IndexMap<u32, LocalUsage>,
}

type FirstParamTypes = super::alias::FirstParamTypes;

fn analyze_function_body(
    body: &Body,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
    copy_value_id: Option<FuncId>,
) -> AnalysisResult {
    let mut result = AnalysisResult {
        bindings: Vec::new(),
        usage: IndexMap::default(),
    };
    analyze_block(
        body,
        body.root,
        &mut result,
        type_table,
        first_param_types,
        copy_value_id,
    );
    // A local read only through a promoted `Operand::Value` (`Opaque(Local)`) is
    // invisible to the skeleton walk above; count it so copy-prop does not treat
    // the local as dead / single-use and eliminate it out from under the promoted
    // read. Empty (behavior-neutral) until operand promotion runs.
    for idx in promoted_reads_set(body) {
        result.usage.entry(idx).or_default().read_count += 2;
    }
    result
}

/// Locals read through a promoted `Operand::Value`. The pool-wide set is
/// over-conservative (only ever keeps too many) — sound, costs a few copies.
fn promoted_reads_set(body: &crate::nir_arena::Body) -> crate::hashmap::IndexSet<u32> {
    body.values.opaque_local_sources().collect()
}

fn analyze_block(
    body: &Body,
    block: BlockId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
    copy_value_id: Option<FuncId>,
) {
    let stmts = body.blocks[block].stmts.clone();
    // Per-local sorted statement indices whose subtree mutates it (one pass over
    // the block; ascending, since we scan in order). `.last()` is the last
    // mutation — a binding at `k` is source-scope-stable iff its source has none
    // after `k`. The full list additionally lets the projection-source check ask
    // whether any mutation falls in the open interval `(binding, use]`.
    // Precomputing this avoids the per-binding `stmts[k+1..]` rescan, which is
    // O(N²) when a block holds many copy bindings (e.g. a large sequence-literal
    // builder chain — #1472 follow-up).
    let mut mut_indices: IndexMap<u32, Vec<usize>> = IndexMap::default();
    for (i, &stmt) in stmts.iter().enumerate() {
        collect_mutated_locals(
            body,
            NodeRef::Stmt(stmt),
            &mut mut_indices,
            i,
            type_table,
            fpt,
        );
    }
    // Per-local earliest statement index whose subtree reads it. For a
    // single-use projection temp this is its unique use, bounding the interval
    // the projection stability check scans for root mutations.
    let mut first_read: IndexMap<u32, usize> = IndexMap::default();
    for (i, &stmt) in stmts.iter().enumerate() {
        collect_first_reads(body, NodeRef::Stmt(stmt), &mut first_read, i);
    }
    for (k, &stmt) in stmts.iter().enumerate() {
        match &body.stmts[stmt].kind {
            StmtKind::Let { local_index, .. } => {
                result.usage.entry(*local_index).or_default().def_count += 1;
            }
            StmtKind::LetDestructure { pattern, .. } => {
                count_pattern_defs(body, *pattern, result);
            }
            _ => {}
        }
        if let Some(mut binding) = analyze_copy_binding(body, stmt, copy_value_id) {
            // The target's uses are confined to this block from `k` onward, so
            // the source is stable iff it is not mutated in those statements (a
            // promoted value is unconditionally stable). A `RefProjection` needs
            // the precise capture-at-binding condition — see `refproj_scope_stable`.
            binding.source_scope_stable = match &binding.source {
                CopySource::RefProjection { root_local, .. } => refproj_scope_stable(
                    *root_local,
                    binding.target_local,
                    k,
                    &mut_indices,
                    &first_read,
                ),
                _ => match binding.source.local_index() {
                    Some(src) => mut_indices
                        .get(&src)
                        .and_then(|v| v.last())
                        .is_none_or(|&i| i <= k),
                    None => true,
                },
            };
            result.bindings.push(binding);
        }
        analyze_stmt(body, stmt, result, type_table, fpt, copy_value_id);
    }
}

/// Record, into `mut_indices`, statement index `idx` for every local whose
/// subtree `node` mutates. Called with ascending `idx`, so each local's list
/// stays sorted; `.last()` is its final mutation and the list supports interval
/// queries, collecting all roots in one walk instead of testing a single local.
///
/// A method receiver is a mutation only when the callee actually writes
/// through it (`method_mutates_receiver`, the same oracle `analyze_expr`'s
/// `has_field_mutation` marking uses) — a read-only receiver (`x.len()`)
/// must not end the scope-stability interval of `x`-sourced bindings.
fn collect_mutated_locals(
    body: &Body,
    node: NodeRef,
    mut_indices: &mut IndexMap<u32, Vec<usize>>,
    idx: usize,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
) {
    let mut note = |l: u32| {
        let v = mut_indices.entry(l).or_default();
        if v.last() != Some(&idx) {
            v.push(idx);
        }
    };
    if let NodeRef::Expr(id) = node {
        match &body.exprs[id].kind {
            ExprKind::Assign { target, .. } => {
                if let Some(l) = place_root_local(body, *target) {
                    note(l);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                if let Some(l) = inner.as_expr().and_then(|ie| place_root_local(body, ie)) {
                    note(l);
                }
            }
            ExprKind::MethodCall {
                receiver, func_id, ..
            } => {
                if let Some(re) = receiver.as_expr()
                    && super::alias::method_mutates_receiver(
                        body, re, *func_id, fpt, type_table, false, None,
                    )
                    && let Some(l) = place_root_local(body, re)
                {
                    note(l);
                }
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let Some(l) = arg.expr.as_expr().and_then(|e| place_root_local(body, e))
                    {
                        note(l);
                    }
                }
            }
            _ => {}
        }
    }
    body.for_each_child(node, |c| {
        collect_mutated_locals(body, c, mut_indices, idx, type_table, fpt);
    });
}

/// Record, into `first_read`, the earliest statement index `idx` whose subtree
/// reads each local. `or_insert` keeps the first (smallest) index seen.
fn collect_first_reads(
    body: &Body,
    node: NodeRef,
    first_read: &mut IndexMap<u32, usize>,
    idx: usize,
) {
    if let NodeRef::Expr(id) = node
        && let ExprKind::Local { index, .. } = &body.exprs[id].kind
    {
        first_read.entry(*index).or_insert(idx);
    }
    body.for_each_child(node, |c| collect_first_reads(body, c, first_read, idx));
}

/// Stability for a `RefProjection` source rooted at `root`, bound at statement
/// `k`, whose target is re-materialized at its single use. Sound iff no mutation
/// of `root` falls in `(k, use]`: mutations at or before the binding predate the
/// captured object, and mutations after the single use cannot reach it. Without
/// a recorded read the target is dead — treat as stable and let DCE remove it.
fn refproj_scope_stable(
    root: u32,
    target: u32,
    k: usize,
    mut_indices: &IndexMap<u32, Vec<usize>>,
    first_read: &IndexMap<u32, usize>,
) -> bool {
    let Some(&use_at) = first_read.get(&target) else {
        return true;
    };
    // A use recorded at or before the binding is a backward/cross-iteration read
    // the `(k, use]` interval cannot reason about; be conservative.
    if use_at <= k {
        return false;
    }
    mut_indices
        .get(&root)
        .is_none_or(|v| !v.iter().any(|&m| m > k && m <= use_at))
}

/// Count every `Binding` local a destructuring pattern introduces as a def.
fn count_pattern_defs(body: &Body, pat: PatId, result: &mut AnalysisResult) {
    match &body.pats[pat].kind {
        PatKind::Binding { local_index, .. } => {
            result.usage.entry(*local_index).or_default().def_count += 1;
        }
        PatKind::Tuple(pats, _) | PatKind::Or(pats) => {
            for p in pats {
                count_pattern_defs(body, *p, result);
            }
        }
        PatKind::Variant { bindings, .. } => {
            for p in bindings {
                count_pattern_defs(body, *p, result);
            }
        }
        PatKind::Struct { fields, .. } => {
            for f in fields {
                count_pattern_defs(body, f.pattern, result);
            }
        }
        PatKind::Wildcard
        | PatKind::Literal(_)
        | PatKind::Enum { .. }
        | PatKind::ConstantValue { .. }
        | PatKind::Range { .. } => {}
    }
}

fn analyze_stmt(
    body: &Body,
    stmt: StmtId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
    copy_value_id: Option<FuncId>,
) {
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
    for c in kids {
        match c {
            NodeRef::Expr(e) => analyze_expr(body, e, result, type_table, fpt, copy_value_id),
            NodeRef::Block(b) => analyze_block(body, b, result, type_table, fpt, copy_value_id),
            _ => {}
        }
    }
}

fn analyze_expr_operand(
    body: &Body,
    op: Operand,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
    copy_value_id: Option<FuncId>,
) {
    if let Some(e) = op.as_expr() {
        analyze_expr(body, e, result, type_table, fpt, copy_value_id);
    }
}

fn analyze_expr(
    body: &Body,
    id: ExprId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
    copy_value_id: Option<FuncId>,
) {
    match &body.exprs[id].kind {
        ExprKind::Local { index, .. } => {
            result.usage.entry(*index).or_default().read_count += 1;
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if let ExprKind::Local { index, .. } = &body.exprs[target].kind {
                result.usage.entry(*index).or_default().is_assigned = true;
            }
            if let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[target].kind
                && let Some(ExprKind::Local { index, .. }) =
                    inner.as_expr().map(|e| &body.exprs[e].kind)
            {
                result.usage.entry(*index).or_default().has_field_mutation = true;
            }
            analyze_expr(body, target, result, type_table, fpt, copy_value_id);
            if let Some(ve) = value.as_expr() {
                analyze_expr(body, ve, result, type_table, fpt, copy_value_id);
            }
        }
        ExprKind::Unary { op, expr: inner } => {
            let (op, inner) = (*op, *inner);
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && let Some(ie) = inner.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[ie].kind
            {
                let index = *index;
                result.usage.entry(index).or_default().address_taken = true;
                if matches!(op, NirUnaryOp::MutRef) {
                    result.usage.entry(index).or_default().has_field_mutation = true;
                }
            }
            analyze_expr_operand(body, inner, result, type_table, fpt, copy_value_id);
        }
        ExprKind::Call { args, .. } => {
            let arg_data: Vec<(ExprId, bool)> = args
                .iter()
                .filter_map(|a| a.expr.as_expr().map(|e| (e, a.is_mut)))
                .collect();
            for (arg, is_mut) in arg_data {
                if is_mut && may_mutate_through_arg(body, arg, type_table) {
                    mark_potentially_mutated_local(body, arg, result);
                }
                analyze_expr(body, arg, result, type_table, fpt, copy_value_id);
            }
        }
        ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } => {
            let receiver = *receiver;
            let func_id = *func_id;
            let arg_data: Vec<(ExprId, bool)> = args
                .iter()
                .filter_map(|a| a.expr.as_expr().map(|e| (e, a.is_mut)))
                .collect();
            // Copy propagation: a callee absent from `fpt` is assumed *not* to
            // mutate the receiver (`conservative_on_unknown = false`).
            if let Some(recv_e) = receiver.as_expr()
                && super::alias::method_mutates_receiver(
                    body, recv_e, func_id, fpt, type_table, false, None,
                )
            {
                mark_potentially_mutated_local_operand(body, receiver, result);
            }
            analyze_expr_operand(body, receiver, result, type_table, fpt, copy_value_id);
            for (arg, is_mut) in arg_data {
                if is_mut && may_mutate_through_arg(body, arg, type_table) {
                    mark_potentially_mutated_local(body, arg, result);
                }
                analyze_expr(body, arg, result, type_table, fpt, copy_value_id);
            }
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                match c {
                    NodeRef::Expr(e) => {
                        analyze_expr(body, e, result, type_table, fpt, copy_value_id);
                    }
                    NodeRef::Block(b) => {
                        analyze_block(body, b, result, type_table, fpt, copy_value_id);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn mark_potentially_mutated_local_operand(body: &Body, op: Operand, result: &mut AnalysisResult) {
    if let Some(e) = op.as_expr() {
        mark_potentially_mutated_local(body, e, result);
    }
}

fn mark_potentially_mutated_local(body: &Body, expr: ExprId, result: &mut AnalysisResult) {
    if let Some(root) = storage_root(body, expr) {
        result.usage.entry(root).or_default().has_field_mutation = true;
    }
}

fn may_mutate_through_arg(body: &Body, expr: ExprId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(body.exprs[expr].type_id),
        ResolvedType::MutRef(_)
    )
}

/// Over-approximation of `lower::plan::value_copy::needs_value_copy`;
/// a `true` here can only cost a missed propagation.
fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Struct { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::BuiltinArray(_)
    )
}

fn can_propagate_copy(
    binding: &CopyBinding,
    usage: &IndexMap<u32, LocalUsage>,
    type_table: &TypeTable,
    promoted_reads: &IndexSet<u32>,
) -> bool {
    // A target read through a promoted `Opaque(Local)` value cannot be
    // propagated: `apply_in_expr` substitutes only skeleton reads, and the
    // binding's `let` is then removed (`dead_locals`), so the promoted read
    // would dangle on a deleted local. Leave the copy in place.
    if promoted_reads.contains(&binding.target_local) {
        return false;
    }
    let Some(target_usage) = usage.get(&binding.target_local) else {
        return true;
    };
    // Multi-def target: a single source cannot cover every reaching def (see
    // `LocalUsage::def_count`).
    if target_usage.def_count > 1 {
        return false;
    }
    if target_usage.is_assigned {
        return false;
    }
    if target_usage.has_field_mutation && needs_value_copy(binding.type_id, type_table) {
        return false;
    }
    if target_usage.address_taken {
        return false;
    }

    match &binding.source {
        CopySource::Local { index, .. } => {
            // A scope-stable source is provably unchanged across every use of
            // the target (the target's reads are confined to the binding's
            // scope), so the coarse whole-function source gates below — which
            // would otherwise reject e.g. a loop counter copied inside the loop
            // — do not apply.
            if binding.source_scope_stable {
                return true;
            }
            let source_usage = usage.get(index);
            if let Some(su) = source_usage
                && su.is_assigned
            {
                return false;
            }
            let is_value_type = needs_value_copy(binding.type_id, type_table);
            if is_value_type
                && target_usage.read_count == 1
                && let Some(su) = source_usage
                && su.has_field_mutation
            {
                return false;
            }
            let single_use_value_copy = is_value_type && target_usage.read_count == 1;
            if let Some(su) = source_usage
                && !single_use_value_copy
                && su.address_taken
            {
                return false;
            }
            if is_value_type
                && !single_use_value_copy
                && let Some(su) = source_usage
                && (su.read_count > 1 || su.address_taken)
            {
                return false;
            }
            true
        }
        CopySource::Ref { index, .. } | CopySource::MutRef { index, .. } => {
            if target_usage.read_count != 1 {
                return false;
            }
            if let Some(su) = usage.get(index)
                && su.is_assigned
            {
                return false;
            }
            true
        }
        CopySource::RefProjection { .. } => {
            target_usage.read_count == 1 && binding.source_scope_stable
        }
        // A pooled value is immutable, so forwarding it to every read is always
        // sound; the read becomes `Operand::Value(v)`, re-emitted by the extractor.
        CopySource::Promoted(_) => true,
    }
}

fn apply_in_block(
    engine: &mut Engine,
    block: BlockId,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    let kept: Vec<StmtId> = engine.body.blocks[block]
        .stmts
        .iter()
        .copied()
        .filter(|s| match &engine.body.stmts[*s].kind {
            StmtKind::Let { local_index, .. } => !dead_locals.contains(local_index),
            _ => true,
        })
        .collect();
    engine.set_block_stmts(block, kept);

    let stmts = engine.body.blocks[block].stmts.clone();
    for stmt in stmts {
        apply_in_node(engine, NodeRef::Stmt(stmt), substitutions, dead_locals);
    }
}

fn apply_in_node(
    engine: &mut Engine,
    node: NodeRef,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    match node {
        NodeRef::Expr(id) => apply_in_expr(engine, id, substitutions, dead_locals),
        NodeRef::Block(b) => apply_in_block(engine, b, substitutions, dead_locals),
        NodeRef::Stmt(s) => {
            let mut kids = Vec::new();
            engine
                .body
                .for_each_child(NodeRef::Stmt(s), |c| kids.push(c));
            for c in kids {
                apply_in_node(engine, c, substitutions, dead_locals);
            }
        }
        NodeRef::Pat(_) => {}
    }
}

fn apply_in_expr(
    engine: &mut Engine,
    id: ExprId,
    substitutions: &IndexMap<u32, CopySource>,
    dead_locals: &IndexSet<u32>,
) {
    let sub = if let ExprKind::Local { index, .. } = &engine.body.exprs[id].kind {
        substitutions.get(index).cloned()
    } else {
        None
    };
    if let Some(source) = sub {
        match source {
            CopySource::Local { index, name } => {
                engine.replace_expr_kind(id, ExprKind::Local { index, name });
            }
            CopySource::Ref {
                index,
                name,
                inner_type_id,
            } => emit_ref(engine, id, NirUnaryOp::Ref, index, name, inner_type_id),
            CopySource::MutRef {
                index,
                name,
                inner_type_id,
            } => emit_ref(engine, id, NirUnaryOp::MutRef, index, name, inner_type_id),
            CopySource::RefProjection { op, projection, .. } => {
                let cloned = engine.clone_expr(projection);
                engine.replace_expr_kind(
                    id,
                    ExprKind::Unary {
                        op,
                        expr: cloned.into(),
                    },
                );
            }
            CopySource::Promoted(v) => {
                engine.redirect_expr(id, Operand::Value(v));
            }
        }
        return;
    }

    let mut kids = Vec::new();
    engine
        .body
        .for_each_child(NodeRef::Expr(id), |c| kids.push(c));
    for c in kids {
        apply_in_node(engine, c, substitutions, dead_locals);
    }
}

/// Replace expression `id` with `&src` / `&mut src` (the propagated ref source),
/// keeping `id`'s own `type_id` / span.
fn emit_ref(
    engine: &mut Engine,
    id: ExprId,
    op: NirUnaryOp,
    index: u32,
    name: String,
    inner_type_id: TypeId,
) {
    let span = engine.body.exprs[id].span;
    let inner = engine.alloc_expr(ExprKind::Local { index, name }, inner_type_id, span);
    engine.replace_expr_kind(
        id,
        ExprKind::Unary {
            op,
            expr: inner.into(),
        },
    );
}

/// Whole-function copy-propagation fixpoint driven from the engine session
/// root. Mirrors the standalone driver's loop: analyse → filter → substitute
/// until no further bindings can be propagated.
fn propagate_at_root(
    engine: &mut Engine,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
    copy_value_id: Option<FuncId>,
) -> bool {
    let mut ever_changed = false;
    loop {
        let analysis =
            analyze_function_body(engine.body, type_table, first_param_types, copy_value_id);
        if analysis.bindings.is_empty() {
            break;
        }
        let promoted_reads = promoted_reads_set(engine.body);
        let eliminable: Vec<CopyBinding> = analysis
            .bindings
            .into_iter()
            .filter(|b| can_propagate_copy(b, &analysis.usage, type_table, &promoted_reads))
            .collect();
        if eliminable.is_empty() {
            break;
        }
        let target_set: IndexSet<u32> = eliminable.iter().map(|b| b.target_local).collect();
        let mut substitutions: IndexMap<u32, CopySource> = IndexMap::default();
        let mut has_deferred = false;
        for binding in eliminable {
            let source_conflicts = match &binding.source {
                CopySource::Local { index, .. }
                | CopySource::Ref { index, .. }
                | CopySource::MutRef { index, .. } => target_set.contains(index),
                CopySource::RefProjection { root_local, .. } => target_set.contains(root_local),
                // A promoted value has no source local to conflict.
                CopySource::Promoted(_) => false,
            };
            if source_conflicts {
                has_deferred = true;
            } else {
                substitutions.insert(binding.target_local, binding.source);
            }
        }
        if substitutions.is_empty() {
            break;
        }
        let dead_locals: IndexSet<u32> = substitutions.keys().copied().collect();
        let root = engine.body.root;
        apply_in_block(engine, root, &substitutions, &dead_locals);
        ever_changed = true;
        if !has_deferred {
            break;
        }
    }
    ever_changed
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function copy-propagation fixpoint at the body root.
pub(super) struct CopyPropRule<'a> {
    type_table: &'a TypeTable,
    first_param_types: &'a FirstParamTypes,
    copy_value_id: Option<FuncId>,
    applied: Cell<bool>,
}

impl Rule for CopyPropRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        propagate_at_root(
            engine,
            self.type_table,
            self.first_param_types,
            self.copy_value_id,
        )
    }
}

pub fn propagate_copies(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let copy_value_id = project.builtin_func_id("copy_value");
    let type_table = project.type_table.borrow();
    let first_param_types: FirstParamTypes = super::alias::first_param_types(project);
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::CopyProp, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = CopyPropRule {
            type_table: &type_table,
            first_param_types: &first_param_types,
            copy_value_id,
            applied: Cell::new(false),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}
