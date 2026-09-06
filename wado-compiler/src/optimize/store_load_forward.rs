//! Store-to-Load Forwarding: replace a value-position `Local` or `FieldAccess`
//! read with the literal reaching it. The `ValueGraph` handles reaching-defs,
//! branch merges, and heap-write invalidation, so this rule only inspects each
//! read's `ValueKind`. Address-taken and `stores`-aliased locals are excluded
//! from `Local` forwarding; a field read is safe already, its store seeded for
//! any receiver but a `stores`-aliased one and versioned against writes.

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
    let ctfe_builtins = crate::niri::build_ctfe_builtin_map(project);
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
            &ctfe_builtins,
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
    ctfe_builtins: &crate::niri::CtfeBuiltinMap,
    buffers: &mut EngineBuffers,
) -> bool {
    if func.body.is_none() {
        return false;
    }
    // Nothing else ever queries a value in a CM export wrapper, so it reaches
    // this pass with no `ValueGraph` and `scoped_const_reads` — which only
    // grows one that exists — forwards nothing into it. Only wrappers: building
    // it everywhere forwards into the hot paths too, which trades shared locals
    // for duplicated constants and measures slower.
    let is_cm_export = func.is_cm_export;
    let param_locals: Vec<u32> = func.params.iter().map(|p| p.local_index).collect();
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
    engine.set_ctfe_builtins(ctfe_builtins);
    if is_cm_export {
        // `ensure_value_graph` never rebuilds, so what is seeded here is what
        // licm and `promote_fields` inherit.
        engine.set_param_locals(param_locals);
    }
    // Before the first query: `scoped_const_reads` only *grows* a graph that
    // already exists, and this session is the one that set the real alias sets,
    // so a body reaching it without one forwarded nothing at all.
    engine.build_value_graph_now();
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
    // Snapshot reads before rewriting. A `FieldAccess` carries no local index,
    // so the `unsafe_locals` filter below never applies to one: a field read's
    // safety is the value graph's, and it is by version rather than by refusing
    // to seed. A `stores`-aliased receiver is the one the builder never seeds;
    // a reference-aliased receiver is seeded and versioned against the escaped
    // generation, which every impure call bumps, so a forward that survives to
    // here is one no write could have reached.
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
        let field = match &engine.body.exprs[expr].kind {
            ExprKind::FieldAccess { field_name, .. } => Some(field_name.clone()),
            _ => None,
        };
        let field = field.filter(|n| n == "width");
        if super::extract::is_place_read(engine, expr) {
            if field.is_some() {
                crate::compiler_trace!("vg_field", "forward width {expr:?}: a place read");
            }
            continue;
        }
        // A read not in `forwarded` has no re-emittable value.
        let Some(vid) = forwarded.get(&expr).copied() else {
            if field.is_some() {
                crate::compiler_trace!("vg_field", "forward width {expr:?}: no re-emittable value");
            }
            continue;
        };
        // The constant promotes into `expr`'s parent operand slot (WEP: The Live
        // ValueGraph).
        let Some(value) = super::extract::extract_const(engine, vid, expr) else {
            if field.is_some() {
                crate::compiler_trace!("vg_field", "forward width {expr:?}: {vid:?} no extract");
            }
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
            // A call carries a value only where the graph was taught its
            // builtin — an array length, which the capacity guard tests.
            ExprKind::FieldAccess { .. } | ExprKind::Call { .. } => out.push((id, None)),
            _ => {}
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_candidate_reads_node(body, c, out);
    }
}
