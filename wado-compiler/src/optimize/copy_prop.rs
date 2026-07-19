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

use super::arena_query::{MutRefAliases, RootMutation, for_each_mutated_root};
use super::gate::{FunctionGate, GatedPass};
use super::value_copy::mutation::MutationOracle;

#[derive(Debug, Clone)]
struct CopyBinding {
    target_local: u32,
    source: CopySource,
    type_id: TypeId,
    /// Whether the source value is stable across the target's scope: the source
    /// local is never *mutated* (re-assigned, field-mutated, `&mut`-borrowed,
    /// passed as a mutable argument, or written through a `&mut` alias — see
    /// [`MutRefAliases`]) anywhere in the binding block's statements after the
    /// binding, and every read of the target sits after the binding (a
    /// backward read would cross a loop iteration the interval cannot reason
    /// about). The target's uses are confined to that scope, so a stable
    /// source can be propagated even when the source is reassigned elsewhere
    /// in the function (e.g. a loop counter copied inside the loop body).
    /// Always `true` for a promoted-value source.
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
            } else {
                let root_local = field_chain_root(body, ie)?;
                CopySource::RefProjection {
                    root_local,
                    op,
                    projection: ie,
                }
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

/// Per-round analysis inputs: the callee oracle, the function-wide
/// `&mut`-alias map, and the per-block mutation/read index scans, all built
/// once per fixpoint round and shared across every nesting level.
struct AnalysisCtx<'a> {
    type_table: &'a TypeTable,
    oracle: &'a MutationOracle<'a>,
    copy_value_id: Option<FuncId>,
    aliases: MutRefAliases,
    scans: IndexMap<BlockId, BlockScan>,
}

/// Per-block mutation / read positions, keyed by the block's own statement
/// indices.
///
/// `mut_indices`: per-local sorted statement indices whose subtree mutates it.
/// `.last()` is the last mutation — a binding at `k` is source-scope-stable
/// iff its source has none after `k`. The full list additionally lets the
/// projection-source check ask whether any mutation falls in the open
/// interval `(binding, use]`.
///
/// `first_read`: per-local earliest statement index whose subtree reads it.
/// For a single-use projection temp this is its unique use, bounding the
/// interval the projection stability check scans for root mutations; for
/// every source kind it guards against backward (cross-iteration) reads.
#[derive(Default)]
struct BlockScan {
    mut_indices: IndexMap<u32, Vec<usize>>,
    first_read: IndexMap<u32, usize>,
}

/// Build every block's [`BlockScan`] in ONE top-down walk: each read /
/// mutation event is recorded into all enclosing blocks at their current
/// statement index, instead of re-walking full subtrees at every nesting
/// level (O(N²) on deep bodies — #1472 follow-up).
///
/// A method receiver is a mutation only when the callee actually writes
/// through it, and a write through a `&mut` alias is attributed to the
/// aliased root too — both via [`for_each_mutated_root`], the same dispatch
/// `analyze_expr`'s usage marking uses — so a read-only receiver (`x.len()`)
/// does not end the scope-stability interval of `x`-sourced bindings, while
/// `let r = &mut y; …; r.f = v` does.
fn scan_blocks(
    body: &Body,
    type_table: &TypeTable,
    oracle: &MutationOracle<'_>,
    aliases: &MutRefAliases,
) -> IndexMap<BlockId, BlockScan> {
    let mut scans = IndexMap::default();
    let mut frames: Vec<(BlockId, usize)> = Vec::new();
    scan_node(
        body,
        NodeRef::Block(body.root),
        type_table,
        oracle,
        aliases,
        &mut frames,
        &mut scans,
    );
    scans
}

fn scan_node(
    body: &Body,
    node: NodeRef,
    type_table: &TypeTable,
    oracle: &MutationOracle<'_>,
    aliases: &MutRefAliases,
    frames: &mut Vec<(BlockId, usize)>,
    scans: &mut IndexMap<BlockId, BlockScan>,
) {
    if let NodeRef::Block(b) = node {
        scans.entry(b).or_default();
        let stmts = body.blocks[b].stmts.clone();
        for (i, &s) in stmts.iter().enumerate() {
            frames.push((b, i));
            scan_node(
                body,
                NodeRef::Stmt(s),
                type_table,
                oracle,
                aliases,
                frames,
                scans,
            );
            frames.pop();
        }
        return;
    }
    if let NodeRef::Expr(id) = node {
        if let ExprKind::Local { index, .. } = &body.exprs[id].kind {
            for &(b, i) in frames.iter() {
                scans
                    .get_mut(&b)
                    .expect("enclosing block was scanned")
                    .first_read
                    .entry(*index)
                    .or_insert(i);
            }
        }
        for_each_mutated_root(body, id, type_table, oracle, aliases, &mut |rm| {
            let l = rm.local();
            for &(b, i) in frames.iter() {
                let v = scans
                    .get_mut(&b)
                    .expect("enclosing block was scanned")
                    .mut_indices
                    .entry(l)
                    .or_default();
                if v.last() != Some(&i) {
                    v.push(i);
                }
            }
        });
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        scan_node(body, c, type_table, oracle, aliases, frames, scans);
    }
}

fn analyze_function_body(body: &Body, ctx: &AnalysisCtx<'_>) -> AnalysisResult {
    let mut result = AnalysisResult {
        bindings: Vec::new(),
        usage: IndexMap::default(),
    };
    analyze_block(body, body.root, &mut result, ctx);
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

fn analyze_block(body: &Body, block: BlockId, result: &mut AnalysisResult, ctx: &AnalysisCtx<'_>) {
    let stmts = body.blocks[block].stmts.clone();
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
        if let Some(mut binding) = analyze_copy_binding(body, stmt, ctx.copy_value_id) {
            // The target's uses are confined to this block from `k` onward, so
            // the source is stable iff every read of the target sits after the
            // binding AND the source is not mutated in those statements (a
            // promoted value is unconditionally stable). A `RefProjection` needs
            // the precise capture-at-binding condition — see `refproj_scope_stable`.
            let scan = ctx.scans.get(&block).expect("block was scanned");
            binding.source_scope_stable = match &binding.source {
                CopySource::RefProjection { root_local, .. } => refproj_scope_stable(
                    *root_local,
                    binding.target_local,
                    k,
                    &scan.mut_indices,
                    &scan.first_read,
                ),
                _ => match binding.source.local_index() {
                    Some(src) => {
                        // A target read at or before the binding is a
                        // backward / cross-iteration read the "mutations
                        // after `k`" interval cannot reason about — the same
                        // guard the `RefProjection` path applies.
                        scan.first_read
                            .get(&binding.target_local)
                            .is_none_or(|&u| u > k)
                            && scan
                                .mut_indices
                                .get(&src)
                                .and_then(|v| v.last())
                                .is_none_or(|&i| i <= k)
                    }
                    None => true,
                },
            };
            result.bindings.push(binding);
        }
        analyze_stmt(body, stmt, result, ctx);
    }
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

fn analyze_stmt(body: &Body, stmt: StmtId, result: &mut AnalysisResult, ctx: &AnalysisCtx<'_>) {
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
    for c in kids {
        match c {
            NodeRef::Expr(e) => analyze_expr(body, e, result, ctx),
            NodeRef::Block(b) => analyze_block(body, b, result, ctx),
            NodeRef::Stmt(_) | NodeRef::Pat(_) => {}
        }
    }
}

fn analyze_expr(body: &Body, id: ExprId, result: &mut AnalysisResult, ctx: &AnalysisCtx<'_>) {
    // The shared witness→root dispatch (`arena_query::for_each_mutated_root`)
    // — one root resolution and one bodyless-callee fallback, shared with the
    // scope-stability scan.
    for_each_mutated_root(
        body,
        id,
        ctx.type_table,
        ctx.oracle,
        &ctx.aliases,
        &mut |rm| match rm {
            RootMutation::Rebind(l) => {
                result.usage.entry(l).or_default().is_assigned = true;
            }
            RootMutation::Through(l) => {
                result.usage.entry(l).or_default().has_field_mutation = true;
            }
        },
    );
    match &body.exprs[id].kind {
        ExprKind::Local { index, .. } => {
            result.usage.entry(*index).or_default().read_count += 1;
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            analyze_expr(body, target, result, ctx);
            if let Some(ve) = value.as_expr() {
                analyze_expr(body, ve, result, ctx);
            }
        }
        ExprKind::Unary { op, expr: inner } => {
            let (op, inner) = (*op, *inner);
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && let Some(ie) = inner.as_expr()
                && let ExprKind::Local { index, .. } = &body.exprs[ie].kind
            {
                result.usage.entry(*index).or_default().address_taken = true;
            }
            if let Some(ie) = inner.as_expr() {
                analyze_expr(body, ie, result, ctx);
            }
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                match c {
                    NodeRef::Expr(e) => analyze_expr(body, e, result, ctx),
                    NodeRef::Block(b) => analyze_block(body, b, result, ctx),
                    NodeRef::Stmt(_) | NodeRef::Pat(_) => {}
                }
            }
        }
    }
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
    oracle: &MutationOracle<'_>,
    copy_value_id: Option<FuncId>,
    param_count: usize,
) -> bool {
    let mut ever_changed = false;
    loop {
        let aliases = MutRefAliases::of_body(engine.body, engine.locals(), param_count, type_table);
        let scans = scan_blocks(engine.body, type_table, oracle, &aliases);
        let ctx = AnalysisCtx {
            type_table,
            oracle,
            copy_value_id,
            aliases,
            scans,
        };
        let analysis = analyze_function_body(engine.body, &ctx);
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
    oracle: MutationOracle<'a>,
    copy_value_id: Option<FuncId>,
    /// Locals `0..param_count` are the function's parameters (external
    /// storage the `&mut`-alias map treats as rooting no function local).
    param_count: usize,
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
            &self.oracle,
            self.copy_value_id,
            self.param_count,
        )
    }
}

pub fn propagate_copies(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let copy_value_id = project.builtin_func_id("copy_value");
    let type_table = project.type_table.borrow();
    let param_mut = super::value_copy::mutation::build_param_mut(project);
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::CopyProp, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = CopyPropRule {
            type_table: &type_table,
            oracle: MutationOracle::new(&param_mut),
            copy_value_id,
            param_count: func.params.len(),
            applied: Cell::new(false),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_source::ModuleSource;
    use crate::nir::NirLocal;
    use crate::nir_arena::{BlockNode, ExprNode, StmtNode};
    use crate::nir_value_graph::ValueKind;
    use crate::token::Span;

    struct Setup {
        body: Body,
        locals: Vec<NirLocal>,
        type_table: TypeTable,
        mut_ref_ty: TypeId,
    }

    impl Setup {
        fn new(local_types: &[LocalKind]) -> Self {
            let mut type_table = TypeTable::new();
            let struct_ty = type_table.make_struct("P".to_string(), ModuleSource::prelude());
            let mut_ref_ty = type_table.make_mut_ref(struct_ty);
            let locals = local_types
                .iter()
                .enumerate()
                .map(|(i, k)| NirLocal {
                    name: format!("__l{i}"),
                    type_id: match k {
                        LocalKind::Struct => struct_ty,
                        LocalKind::MutRef => mut_ref_ty,
                    },
                    is_mut: true,
                })
                .collect();
            Self {
                body: Body::empty(),
                locals,
                type_table,
                mut_ref_ty,
            }
        }

        fn local(&mut self, index: u32) -> ExprId {
            let type_id = self.locals[index as usize].type_id;
            self.expr(
                ExprKind::Local {
                    index,
                    name: format!("__l{index}"),
                },
                type_id,
            )
        }

        fn expr(&mut self, kind: ExprKind, type_id: TypeId) -> ExprId {
            self.body.exprs.push(ExprNode {
                kind,
                type_id,
                span: Span::default(),
            })
        }

        fn let_stmt(&mut self, index: u32, value: impl Into<Operand>) -> StmtId {
            let type_id = self.locals[index as usize].type_id;
            self.body.stmts.push(StmtNode {
                kind: StmtKind::Let {
                    name: format!("__l{index}"),
                    local_index: index,
                    is_mut: true,
                    is_reactive: false,
                    type_id,
                    value: value.into(),
                    skip_value_copy: false,
                },
                span: Span::default(),
            })
        }

        fn mut_borrow(&mut self, index: u32) -> ExprId {
            let inner = self.local(index);
            self.expr(
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: inner.into(),
                },
                self.mut_ref_ty,
            )
        }

        /// `<ref_local>.f = 0;` as an expression statement.
        fn field_write_stmt(&mut self, ref_local: u32) -> StmtId {
            let recv = self.local(ref_local);
            let target = self.expr(
                ExprKind::FieldAccess {
                    expr: recv.into(),
                    field_index: 0,
                    field_name: "f".to_string(),
                },
                TypeTable::I32,
            );
            let zero = Operand::Value(
                self.body
                    .values
                    .alloc_unshared(ValueKind::Int(0, TypeTable::I32), TypeTable::I32),
            );
            let assign = self.expr(
                ExprKind::Assign {
                    target,
                    value: zero,
                },
                TypeTable::UNIT,
            );
            self.body.stmts.push(StmtNode {
                kind: StmtKind::Expr(assign.into()),
                span: Span::default(),
            })
        }

        fn read_stmt(&mut self, index: u32) -> StmtId {
            let l = self.local(index);
            self.body.stmts.push(StmtNode {
                kind: StmtKind::Expr(l.into()),
                span: Span::default(),
            })
        }

        fn finish(&mut self, stmts: Vec<StmtId>) {
            let root = self.body.blocks.push(BlockNode {
                stmts,
                span: Span::default(),
            });
            self.body.root = root;
        }

        fn analyze(&self) -> AnalysisResult {
            let param_mut = IndexMap::default();
            let oracle = MutationOracle::new(&param_mut);
            let aliases = MutRefAliases::of_body(&self.body, &self.locals, 0, &self.type_table);
            let scans = scan_blocks(&self.body, &self.type_table, &oracle, &aliases);
            let ctx = AnalysisCtx {
                type_table: &self.type_table,
                oracle: &oracle,
                copy_value_id: None,
                aliases,
                scans,
            };
            analyze_function_body(&self.body, &ctx)
        }
    }

    enum LocalKind {
        Struct,
        MutRef,
    }
    use LocalKind::{MutRef, Struct};

    fn binding_for(analysis: &AnalysisResult, target: u32) -> &CopyBinding {
        analysis
            .bindings
            .iter()
            .find(|b| b.target_local == target)
            .expect("copy binding for target")
    }

    fn propagates(setup: &Setup, analysis: &AnalysisResult, target: u32) -> bool {
        can_propagate_copy(
            binding_for(analysis, target),
            &analysis.usage,
            &setup.type_table,
            &IndexSet::default(),
        )
    }

    // `let r = &mut y; let x = y; r.f = 0; use(x)` — the write through the
    // alias lands AFTER the copy binding, so the binding must NOT be
    // scope-stable, and the coarse `has_field_mutation` gate must see `y`.
    #[test]
    fn alias_write_after_binding_blocks_propagation() {
        let mut s = Setup::new(&[Struct, MutRef, Struct]);
        let (y, r, x) = (0, 1, 2);
        let borrow = s.mut_borrow(y);
        let s0 = s.let_stmt(r, borrow);
        let ysrc = s.local(y);
        let s1 = s.let_stmt(x, ysrc);
        let s2 = s.field_write_stmt(r);
        let s3 = s.read_stmt(x);
        s.finish(vec![s0, s1, s2, s3]);

        let analysis = s.analyze();
        assert!(
            !binding_for(&analysis, x).source_scope_stable,
            "write through the &mut alias after the binding must end the stability interval"
        );
        assert!(
            analysis.usage.get(&y).is_some_and(|u| u.has_field_mutation),
            "through-alias write must mark the aliased root"
        );
        assert!(!propagates(&s, &analysis, x));
    }

    // Same shape but the alias is never written through: the borrow is a
    // point event before the binding, so the copy still propagates
    // (precision pin: a bare `&mut` borrow must not poison the source).
    #[test]
    fn unwritten_alias_before_binding_keeps_propagation() {
        let mut s = Setup::new(&[Struct, MutRef, Struct]);
        let (y, r, x) = (0, 1, 2);
        let borrow = s.mut_borrow(y);
        let s0 = s.let_stmt(r, borrow);
        let ysrc = s.local(y);
        let s1 = s.let_stmt(x, ysrc);
        let s2 = s.read_stmt(x);
        s.finish(vec![s0, s1, s2]);

        let analysis = s.analyze();
        assert!(binding_for(&analysis, x).source_scope_stable);
        assert!(propagates(&s, &analysis, x));
    }

    // The alias flows through a ref-to-ref copy (`let r2 = r`): the fixpoint
    // must attribute a write through `r2` back to `y`.
    #[test]
    fn alias_copy_chain_write_blocks_propagation() {
        let mut s = Setup::new(&[Struct, MutRef, Struct, MutRef]);
        let (y, r, x, r2) = (0, 1, 2, 3);
        let borrow = s.mut_borrow(y);
        let s0 = s.let_stmt(r, borrow);
        let rsrc = s.local(r);
        let s1 = s.let_stmt(r2, rsrc);
        let ysrc = s.local(y);
        let s2 = s.let_stmt(x, ysrc);
        let s3 = s.field_write_stmt(r2);
        let s4 = s.read_stmt(x);
        s.finish(vec![s0, s1, s2, s3, s4]);

        let analysis = s.analyze();
        assert!(!binding_for(&analysis, x).source_scope_stable);
        assert!(
            analysis.usage.get(&y).is_some_and(|u| u.has_field_mutation),
            "write through the copied alias must reach y through the fixpoint"
        );
    }

    // A target read at or before the binding index (the shape a
    // cross-iteration read produces) must reject the fast path — the same
    // guard the `RefProjection` source already had.
    #[test]
    fn backward_target_read_rejects_stability() {
        let mut s = Setup::new(&[Struct, Struct]);
        let (y, x) = (0, 1);
        let s0 = s.read_stmt(x);
        let ysrc = s.local(y);
        let s1 = s.let_stmt(x, ysrc);
        let s2 = s.read_stmt(x);
        s.finish(vec![s0, s1, s2]);

        let analysis = s.analyze();
        assert!(
            !binding_for(&analysis, x).source_scope_stable,
            "a read of the target before its binding is a cross-iteration read"
        );
    }
}
