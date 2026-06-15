//! Store-to-Load Forwarding optimization for Wado NIR.
//!
//! Replace a value-position read — a `Local` read or a `FieldAccess` read —
//! with the literal that reaches it. The engine's `ValueGraph` handles
//! flow-sensitive reaching-defs, branch merges, loop and heap-write
//! invalidation, and field store→load seeding, so this rule only inspects
//! each read's `ValueKind` and substitutes when it is a literal.
//!
//! For `Local` reads, address-taken and `stores`-aliased locals are excluded
//! here — the builder does not model writes through references for bare
//! locals. For `FieldAccess` reads the alias safety is upstream: the builder
//! seeds a field store only for a non-aliased, non-address-taken receiver, so
//! an aliased field read never carries a literal `ValueId` to begin with.
//! Particularly useful after SROA decomposes struct fields into scalar
//! locals, and now also folds reads of fields whose stored value the builder
//! forwarded (`obj.f = 5; … obj.f …` and `let x = S { f: 5 }; … x.f …`).
//!
//! Runs as a per-function standalone engine session whose `apply_block`
//! fires once at the body root.

use std::cell::Cell;

use crate::hashmap::IndexSet;
use crate::nir::NirFunction;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef};
use crate::nir_engine::{CachedAnalysis, Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

pub fn forward_stores_to_loads(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated_cached(GatedPass::StoreLoadForward, len, |fid, cached| {
        let mut func = project.functions[fid.index()].borrow_mut();
        forward_one(
            &mut func,
            &type_table,
            &first_param_types,
            &call_immutability,
            &mut buffers,
            cached,
        )
    })
}

/// Ungated variant: forwards stores to loads in every function. Used by the
/// post-`field_scalarize` cleanup, which runs once outside the gated loop so
/// the scalarization shadow inits (`__hfs_x = obj.f`) get their fields
/// forwarded to constants — the load→literal fold `field_scalarize` leaves to
/// a later pass, and the only one that runs after it.
pub fn forward_stores_to_loads_all(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= forward_one(
            &mut func,
            &type_table,
            &first_param_types,
            &call_immutability,
            &mut buffers,
            None,
        )
        .0;
    }
    changed
}

/// Run store→load forwarding over one function body. Returns whether anything
/// changed. Shared by the gated loop pass and the ungated post-scalarize run.
fn forward_one(
    func: &mut NirFunction,
    type_table: &crate::tir::TypeTable,
    first_param_types: &crate::hashmap::IndexMap<
        (crate::module_source::ModuleSource, String),
        crate::tir::TypeId,
    >,
    call_immutability: &super::alias::CallImmutability,
    buffers: &mut EngineBuffers,
    cached: Option<CachedAnalysis>,
) -> (bool, Option<CachedAnalysis>) {
    if func.body.is_none() {
        return (false, None);
    }
    // `Local`-read forwarding excludes address-taken / `stores`-aliased
    // locals: the canonical sets plus the engine's body scan — the
    // canonical sets are static elaboration records and go stale after
    // `inline` / `ref_elim` copy `Ref` nodes (see `elide_local::ElideRule`).
    let mut unsafe_locals = func.address_taken_locals.clone();
    unsafe_locals.extend(func.stores_aliased_locals.iter().copied());
    let NirFunction {
        body,
        locals,
        address_taken_locals,
        stores_aliased_locals,
        ..
    } = &mut *func;
    let body = body.as_mut().expect("checked above");
    let mut engine = if let Some(cached) = cached {
        Engine::with_analysis(body, buffers, locals, type_table, cached)
    } else {
        let (aliased, untrackable, mut_escaped) = super::alias::builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            type_table,
            first_param_types,
            call_immutability,
        );
        let mut engine = Engine::new(body, buffers, locals);
        engine.set_alias_sets(aliased, untrackable, mut_escaped);
        engine.set_value_graph_type_table(type_table);
        engine
    };
    unsafe_locals.extend(engine.body_address_taken().iter().copied());
    let rule = StoreLoadForwardRule {
        applied: Cell::new(false),
        unsafe_locals,
    };
    let changed = engine.run(&[&rule]);
    let parked = if changed {
        None
    } else {
        engine.into_analysis()
    };
    (changed, parked)
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function literal-forwarding pass at the body root.
pub(super) struct StoreLoadForwardRule {
    applied: Cell<bool>,
    unsafe_locals: IndexSet<u32>,
}

impl Rule for StoreLoadForwardRule {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        forward_at_root(engine, &self.unsafe_locals)
    }
}

fn forward_at_root(engine: &mut Engine, unsafe_locals: &IndexSet<u32>) -> bool {
    // Snapshot reads before rewriting. `FieldAccess` reads carry `None`:
    // their alias safety is upstream — the builder never seeds an aliased
    // receiver, so such a read never carries a literal `ValueId`.
    let mut reads: Vec<(ExprId, Option<u32>)> = Vec::new();
    collect_candidate_reads(engine.body, &mut reads);

    let mut changed = false;
    for (expr, local_index) in reads {
        if let Some(idx) = local_index
            && unsafe_locals.contains(&idx)
        {
            continue;
        }
        // Skip `Assign` LHS reads — they are writes, not reads. For a
        // `FieldAccess` this also skips the `obj.f` place of `obj.f = …`.
        if is_assign_target(engine, expr) {
            continue;
        }
        let Some(vid) = engine.value(expr) else {
            continue;
        };
        // Resolve through the e-class representative so a value proven equal to
        // a literal by a union (not only by build-time hash-consing) forwards
        // too. With no unions this is the identity. Materialization is the
        // shared extraction primitive: prefer an existing source literal
        // (keeping its `repr` / span), else synthesize from the value kind.
        let vid = engine.value_find(vid);
        let Some(new_kind) = super::extract::materialize_literal(engine, vid, expr) else {
            continue;
        };
        engine.replace_expr_kind(expr, new_kind);
        changed = true;
    }
    changed
}

/// Collect value-position `Local` and `FieldAccess` read expressions. A
/// `Local` yields `(id, Some(index))`; a `FieldAccess` yields `(id, None)`.
/// Assign-target filtering happens later (`is_assign_target`), so write
/// places are gathered here but rejected before rewriting.
fn collect_candidate_reads(body: &Body, out: &mut Vec<(ExprId, Option<u32>)>) {
    collect_candidate_reads_node(body, NodeRef::Block(body.root), out);
}

fn collect_candidate_reads_node(body: &Body, node: NodeRef, out: &mut Vec<(ExprId, Option<u32>)>) {
    if let NodeRef::Expr(id) = node {
        match &body.exprs[id].kind {
            ExprKind::Local { index, .. } => out.push((id, Some(*index))),
            ExprKind::FieldAccess { .. } => out.push((id, None)),
            _ => {}
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_candidate_reads_node(body, c, out);
    }
}

/// True when `expr`'s immediate parent is an `Assign` and `expr` is the
/// assign's `target` (LHS).
fn is_assign_target(engine: &Engine, expr: ExprId) -> bool {
    let Some(NodeRef::Expr(parent)) = engine.parent_of(NodeRef::Expr(expr)) else {
        return false;
    };
    matches!(
        &engine.body.exprs[parent].kind,
        ExprKind::Assign { target, .. } if *target == expr
    )
}
