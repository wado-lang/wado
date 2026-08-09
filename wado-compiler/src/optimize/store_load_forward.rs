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

/// Forwards stores to loads in every function. Used by the post-`field_scalarize`
/// cleanup so the scalarization shadow inits (`__hfs_x = obj.f`) get their fields
/// forwarded to constants — the load→literal fold `field_scalarize` leaves to
/// a later pass, and the only one that runs after it.
pub fn forward_stores_to_loads_all(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= forward_one(
            &mut func,
            &type_table,
            &first_param_types,
            &call_immutability,
            &pure_builtin_callees,
            &mut buffers,
        );
    }
    changed
}

/// Run store→load forwarding over one function body. Returns whether anything
/// changed.
fn forward_one(
    func: &mut NirFunction,
    type_table: &crate::tir::TypeTable,
    first_param_types: &super::alias::FirstParamTypes,
    call_immutability: &super::alias::CallImmutability,
    pure_builtin_callees: &crate::hashmap::IndexSet<crate::nir::FuncId>,
    buffers: &mut EngineBuffers,
) -> bool {
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
        type_table,
        first_param_types,
        call_immutability,
    );
    let mut engine = Engine::new(body, buffers, locals);
    engine.set_alias_sets(aliased, untrackable, mut_escaped);
    engine.set_value_graph_type_table(type_table);
    engine.set_pure_builtin_callees(pure_builtin_callees);
    unsafe_locals.extend(engine.body_address_taken().iter().copied());
    // The complement of the unsafe set: what disqualifies a local is being
    // address-taken or `stores`-aliased, not being an aggregate.
    let forwardable_locals: IndexSet<u32> = (0..engine.locals().len() as u32)
        .filter(|i| !unsafe_locals.contains(i))
        .collect();
    // Born-as-operands (WEP item 3): promote each re-emittable reaching value
    // straight into its operand slot, never through the persisted `value_of`.
    let forwarded: crate::hashmap::IndexMap<ExprId, crate::nir_value_graph::ValueId> = engine
        .scoped_const_reads(&forwardable_locals, /* include_fields */ true)
        .into_iter()
        .collect();
    let rule = StoreLoadForwardRule {
        applied: Cell::new(false),
        unsafe_locals,
        forwarded,
    };
    engine.run(&[&rule])
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function literal-forwarding pass at the body root.
pub(super) struct StoreLoadForwardRule {
    applied: Cell<bool>,
    unsafe_locals: IndexSet<u32>,
    /// Reaching values from the scratch re-walk, promoted directly.
    forwarded: crate::hashmap::IndexMap<ExprId, crate::nir_value_graph::ValueId>,
}

impl Rule for StoreLoadForwardRule {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        forward_at_root(engine, &self.unsafe_locals, &self.forwarded)
    }
}

/// Forward stored literals to later reads at the body root. Its sole caller is
/// [`StoreLoadForwardRule::apply_block`], which runs it once on the standalone
/// session's already-built (graph-preserving) `ValueGraph`.
fn forward_at_root(
    engine: &mut Engine,
    unsafe_locals: &IndexSet<u32>,
    forwarded: &crate::hashmap::IndexMap<ExprId, crate::nir_value_graph::ValueId>,
) -> bool {
    // Snapshot reads before rewriting. `FieldAccess` reads carry `None`:
    // their alias safety is upstream — the builder never seeds an aliased
    // receiver, so such a read never carries a re-emittable `ValueId`.
    let mut reads: Vec<(ExprId, Option<u32>)> = Vec::new();
    collect_candidate_reads(engine.body, &mut reads);

    let mut changed = false;
    for (expr, local_index) in reads {
        if let Some(idx) = local_index
            && unsafe_locals.contains(&idx)
        {
            continue;
        }
        // Skip place-position reads — a `&`/`&mut`/deref referent, a `.field` /
        // `[index]` / method receiver, or an `Assign` LHS. There the storage,
        // not the value, is used; forwarding the stored literal would destroy
        // the place and lose a callee's write-back (`g(&mut obj.f)` → `g(&mut
        // 5)`). Mirrors the sibling `extract::freeze_pure_arith` guard.
        if super::extract::is_place_read(engine, expr) {
            continue;
        }
        // A read not in `forwarded` has no re-emittable value.
        let Some(vid) = forwarded.get(&expr).copied() else {
            continue;
        };
        // The constant promotes into `expr`'s parent operand slot (WEP: The Live
        // ValueGraph).
        let Some(value) = super::extract::extract_const(engine, vid, expr) else {
            continue;
        };
        changed |= engine.replace_expr_with_value(expr, value);
    }
    changed
}

/// Collect value-position `Local` and `FieldAccess` read expressions. A
/// `Local` yields `(id, Some(index))`; a `FieldAccess` yields `(id, None)`.
/// Place-position filtering happens later (`extract::is_place_read`), so write
/// / reference places are gathered here but rejected before rewriting.
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
