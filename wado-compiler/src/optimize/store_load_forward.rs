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
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

pub fn forward_stores_to_loads(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::StoreLoadForward, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
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
        let (aliased, untrackable, mut_escaped) = super::alias::builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            &type_table,
            &first_param_types,
            &call_immutability,
        );
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.set_alias_sets(aliased, untrackable, mut_escaped);
        unsafe_locals.extend(engine.body_address_taken().iter().copied());
        let rule = StoreLoadForwardRule {
            applied: Cell::new(false),
            unsafe_locals,
        };
        engine.run(&[&rule])
    })
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
        if !matches!(
            engine.value_kind(vid),
            ValueKind::Int(_) | ValueKind::Float(_) | ValueKind::Bool(_) | ValueKind::Char(_)
        ) {
            continue;
        }
        let Some(src) = engine.literal_source(vid) else {
            continue;
        };
        // Cloning the source `ExprKind` keeps the substituted node's
        // `repr` and span. When distinct literal exprs hash-cons to one
        // `ValueId` (e.g. `0` vs `0x0`), `literal_source` returns the
        // first one — the others' reprs are lost on substitution. Sound
        // (VN equality ⇒ semantic equality), but visible in NIR dumps and
        // diagnostic spans for these edge-case literals.
        let new_kind = engine.body.exprs[src].kind.clone();
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
