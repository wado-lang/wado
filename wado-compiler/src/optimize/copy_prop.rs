//! Copy propagation optimization for Wado NIR.
//!
//! Eliminates trivial copy bindings like `let x = y`, `let x = 42`,
//! `let x = &y`, or `let x = &mut y` by propagating the source value to all
//! uses of the target variable. See `can_propagate_copy` for the safety gates.
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
use crate::module_source::ModuleSource;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use super::arena_query::{place_root_local, projection_root_local};
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
    /// body). Always `true` for literal sources.
    source_scope_stable: bool,
}

impl CopySource {
    /// The source local index for the index-bearing sources; `None` for
    /// literals (which are always stable).
    fn local_index(&self) -> Option<u32> {
        match self {
            CopySource::Local { index, .. }
            | CopySource::Ref { index, .. }
            | CopySource::MutRef { index, .. } => Some(*index),
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
}

#[derive(Debug, Default)]
struct LocalUsage {
    read_count: u32,
    is_assigned: bool,
    has_field_mutation: bool,
    address_taken: bool,
}

/// If `expr` is `builtin::copy_value::<T>(inner)`, return `inner`; else `expr`.
fn unwrap_copy_value(body: &Body, expr: ExprId) -> ExprId {
    if let ExprKind::Call { func, args, .. } = &body.exprs[expr].kind
        && func.module_source.is_core_builtin()
        && func.name == "copy_value"
        && args.len() == 1
    {
        return args[0].expr.as_expr().expect("skeleton operand");
    }
    expr
}

fn analyze_copy_binding(body: &Body, stmt: StmtId) -> Option<CopyBinding> {
    let StmtKind::Let {
        local_index,
        value,
        skip_value_copy,
        ..
    } = &body.stmts[stmt].kind
    else {
        return None;
    };
    let (local_index, value, skip_value_copy) = (*local_index, *value, *skip_value_copy);
    if skip_value_copy {
        return None;
    }
    // A promoted constant binding is not a copy of another place.
    let value = unwrap_copy_value(body, value.as_expr()?);
    let value_type = body.exprs[value].type_id;

    let source = match &body.exprs[value].kind {
        ExprKind::Local { index, name } => CopySource::Local {
            index: *index,
            name: name.clone(),
        },
        ExprKind::Unary { op, expr: inner }
            if matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef) =>
        {
            let inner = *inner;
            let is_ref = matches!(op, NirUnaryOp::Ref);
            if let Some(ie) = inner.as_expr()
                && let ExprKind::Local { index, name } = &body.exprs[ie].kind
            {
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

/// Whether `local`'s value is mutated anywhere in the subtree at `node`:
/// re-assignment, field/element assignment, `&mut` borrow, mutable method
/// receiver, or a mutable call argument — through any field/index projection
/// (`local.f.g = …`, `local[i] = …`, `local.f.method()`), not just one level.
/// Conservative (treats any method call on a place rooted at `local` as a
/// mutation), matching and generalizing the in-block copy-inlining check this
/// subsumes.
fn subtree_mutates_local(body: &Body, node: NodeRef, local: u32) -> bool {
    if let NodeRef::Expr(id) = node {
        match &body.exprs[id].kind {
            ExprKind::Assign { target, .. } => {
                if place_root_local(body, *target) == Some(local) {
                    return true;
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                if place_root_local(body, inner.as_expr().expect("skeleton operand")) == Some(local)
                {
                    return true;
                }
            }
            ExprKind::MethodCall { receiver, .. } => {
                if receiver.as_expr().and_then(|e| place_root_local(body, e)) == Some(local) {
                    return true;
                }
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && arg.expr.as_expr().and_then(|e| place_root_local(body, e)) == Some(local)
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    let mut found = false;
    body.for_each_child(node, |c| {
        if !found {
            found = subtree_mutates_local(body, c, local);
        }
    });
    found
}

struct AnalysisResult {
    bindings: Vec<CopyBinding>,
    usage: IndexMap<u32, LocalUsage>,
}

type FirstParamTypes = IndexMap<(ModuleSource, String), TypeId>;

fn analyze_function_body(
    body: &Body,
    type_table: &TypeTable,
    first_param_types: &FirstParamTypes,
) -> AnalysisResult {
    let mut result = AnalysisResult {
        bindings: Vec::new(),
        usage: IndexMap::default(),
    };
    analyze_block(body, body.root, &mut result, type_table, first_param_types);
    // A local read only through a promoted `Operand::Value` (`Opaque(Local)`) is
    // invisible to the skeleton walk above; count it so copy-prop does not treat
    // the local as dead / single-use and propagate or eliminate it out from under
    // the promoted read. Empty (behavior-neutral) until operand promotion runs.
    for idx in promoted_reads_set(body) {
        result.usage.entry(idx).or_default().read_count += 2;
    }
    result
}

/// Locals read through a promoted `Operand::Value`. Under early promotion
/// (`WADO_PROMOTE_EARLY`) the precise live-slot walk has a known gap (a still-read
/// local whose `Opaque(Local)` source it doesn't reach — see WEP / `elide_local`),
/// so use the over-conservative pool-wide set: sound (keeps more locals live,
/// never propagates/eliminates a still-read one), at the cost of a few missed
/// copies. Flag-off keeps the precise walk, so the default is unchanged.
fn promoted_reads_set(body: &crate::nir_arena::Body) -> crate::hashmap::IndexSet<u32> {
    if crate::optimize::promote_early_enabled() {
        body.values.opaque_local_sources().collect()
    } else {
        body.locals_read_via_promotion()
    }
}

fn analyze_block(
    body: &Body,
    block: BlockId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
) {
    let stmts = body.blocks[block].stmts.clone();
    for (k, &stmt) in stmts.iter().enumerate() {
        if let Some(mut binding) = analyze_copy_binding(body, stmt) {
            // The target's uses are confined to this block from `k` onward, so
            // the source is stable for the propagation iff it is not mutated in
            // those statements (literals are unconditionally stable).
            binding.source_scope_stable = match binding.source.local_index() {
                Some(src) => !stmts[k + 1..]
                    .iter()
                    .any(|&s| subtree_mutates_local(body, NodeRef::Stmt(s), src)),
                None => true,
            };
            result.bindings.push(binding);
        }
        analyze_stmt(body, stmt, result, type_table, fpt);
    }
}

fn analyze_stmt(
    body: &Body,
    stmt: StmtId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
) {
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
    for c in kids {
        match c {
            NodeRef::Expr(e) => analyze_expr(body, e, result, type_table, fpt),
            NodeRef::Block(b) => analyze_block(body, b, result, type_table, fpt),
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
) {
    if let Some(e) = op.as_expr() {
        analyze_expr(body, e, result, type_table, fpt);
    }
}

fn analyze_expr(
    body: &Body,
    id: ExprId,
    result: &mut AnalysisResult,
    type_table: &TypeTable,
    fpt: &FirstParamTypes,
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
                && let ExprKind::Local { index, .. } =
                    &body.exprs[inner.as_expr().expect("skeleton operand")].kind
            {
                result.usage.entry(*index).or_default().has_field_mutation = true;
            }
            analyze_expr(body, target, result, type_table, fpt);
            if let Some(ve) = value.as_expr() {
                analyze_expr(body, ve, result, type_table, fpt);
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
            analyze_expr_operand(body, inner, result, type_table, fpt);
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
                analyze_expr(body, arg, result, type_table, fpt);
            }
        }
        ExprKind::MethodCall {
            receiver,
            func,
            args,
            ..
        } => {
            let receiver = *receiver;
            let arg_data: Vec<(ExprId, bool)> = args
                .iter()
                .filter_map(|a| a.expr.as_expr().map(|e| (e, a.is_mut)))
                .collect();
            // Copy propagation: a callee absent from `fpt` is assumed *not* to
            // mutate the receiver (`conservative_on_unknown = false`); the
            // receiver-type guard below still protects value receivers.
            if super::alias::method_mutates_receiver(
                body,
                receiver.as_expr().expect("skeleton operand"),
                func,
                fpt,
                type_table,
                false,
            ) {
                mark_potentially_mutated_local_operand(body, receiver, result);
            }
            analyze_expr_operand(body, receiver, result, type_table, fpt);
            for (arg, is_mut) in arg_data {
                if is_mut && may_mutate_through_arg(body, arg, type_table) {
                    mark_potentially_mutated_local(body, arg, result);
                }
                analyze_expr(body, arg, result, type_table, fpt);
            }
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                match c {
                    NodeRef::Expr(e) => analyze_expr(body, e, result, type_table, fpt),
                    NodeRef::Block(b) => analyze_block(body, b, result, type_table, fpt),
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
    if let Some(root) = projection_root_local(body, expr) {
        result.usage.entry(root).or_default().has_field_mutation = true;
    }
}

fn may_mutate_through_arg(body: &Body, expr: ExprId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(body.exprs[expr].type_id),
        ResolvedType::MutRef(_)
    )
}

fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. }
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
) -> bool {
    let mut ever_changed = false;
    loop {
        let analysis = analyze_function_body(engine.body, type_table, first_param_types);
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
        propagate_at_root(engine, self.type_table, self.first_param_types)
    }
}

pub fn propagate_copies(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
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
            applied: Cell::new(false),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}
