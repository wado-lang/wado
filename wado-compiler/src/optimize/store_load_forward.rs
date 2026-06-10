//! Store-to-Load Forwarding optimization for Wado NIR.
//!
//! Replace `Local` reads with the literal that reaches them. The engine's
//! `ValueGraph` handles flow-sensitive reaching-defs, branch merges, loop
//! and heap-write invalidation, so this rule only inspects each read's
//! `ValueKind` and substitutes when it is a literal.
//!
//! Address-taken and `stores`-aliased locals are excluded — the builder
//! does not model writes through references. Particularly useful after
//! SROA decomposes struct fields into scalar locals.
//!
//! Runs as a per-function standalone engine session whose `apply_block`
//! fires once at the body root.

use std::cell::Cell;

use crate::hashmap::IndexSet;
use crate::nir::{NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueKind;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

pub fn forward_stores_to_loads(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::StoreLoadForward, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        // Forwarding-ineligible locals: the canonical `address_taken_locals`
        // / `stores_aliased_locals` sets, plus a body re-scan for live
        // `&x` / `&mut x` over `Local`. The canonical sets are static
        // records from elaboration and are stale after `inline` /
        // `ref_elim` may have copied `Ref` / `MutRef` nodes for remapped
        // callee locals (see the comment on `elide_local::ElideRule`).
        // The body scan catches those transient post-inline aliases.
        let mut unsafe_locals = func.address_taken_locals.clone();
        unsafe_locals.extend(func.stores_aliased_locals.iter().copied());
        let body_ref = func.body.as_ref().expect("checked above");
        collect_address_taken_in_body(body_ref, &mut unsafe_locals);
        let rule = StoreLoadForwardRule {
            applied: Cell::new(false),
            unsafe_locals: unsafe_locals.clone(),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        // Suppress field store→load seeding on the same aliased locals this
        // rule excludes from forwarding, so the `ValueGraph` does not hand
        // back a forwarded field value for an aliased object.
        engine.set_alias_unsafe_locals(unsafe_locals);
        engine.run(&[&rule])
    })
}

/// Scan `body` for `Unary::Ref(Local)` / `Unary::MutRef(Local)` and insert
/// every targeted local into `out`. Mirrors the live-`&local` source used
/// by `elide_local`; catches locals whose address-taken status is not in
/// the function's static `address_taken_locals` set (e.g. callee locals
/// that the inliner remapped into this body without updating the set).
fn collect_address_taken_in_body(body: &Body, out: &mut IndexSet<u32>) {
    collect_address_taken_node(body, NodeRef::Block(body.root), out);
}

fn collect_address_taken_node(body: &Body, node: NodeRef, out: &mut IndexSet<u32>) {
    if let NodeRef::Expr(id) = node
        && let ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr: inner,
        } = &body.exprs[id].kind
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
    {
        out.insert(*index);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_address_taken_node(body, c, out);
    }
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
    // Collect every `Local` read expression first; iterating while the
    // engine rewrites would invalidate body/expr indices we're walking.
    let mut local_reads: Vec<(ExprId, u32)> = Vec::new();
    collect_local_reads(engine.body, &mut local_reads);

    let mut changed = false;
    for (expr, local_index) in local_reads {
        if unsafe_locals.contains(&local_index) {
            continue;
        }
        // Skip `Assign` LHS reads — they are writes, not reads.
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

fn collect_local_reads(body: &Body, out: &mut Vec<(ExprId, u32)>) {
    collect_local_reads_node(body, NodeRef::Block(body.root), out);
}

fn collect_local_reads_node(body: &Body, node: NodeRef, out: &mut Vec<(ExprId, u32)>) {
    if let NodeRef::Expr(id) = node
        && let ExprKind::Local { index, .. } = &body.exprs[id].kind
    {
        out.push((id, *index));
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_local_reads_node(body, c, out);
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
