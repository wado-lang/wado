//! Store-to-Load Forwarding optimization for Wado NIR.
//!
//! When a literal value reaches a `Local` read at its program point — with
//! no intervening aliasing write — substitute the literal directly,
//! eliminating the load. Particularly useful after SROA decomposes struct
//! fields into scalar locals.
//!
//! Implementation: a thin wrapper over the engine's ValueGraph. For each
//! `Local` read expression in the body, ask the engine for the read's
//! `ValueId`; if its kind is a literal (`Int`, `Float`, `Bool`, `Char`),
//! clone the original defining literal expression's `ExprKind` and replace
//! the `Local`. The ValueGraph builder is responsible for flow-sensitive
//! reaching-def tracking, branch merging (`Select`), loop invalidation,
//! and heap-write invalidation — this rule contains none of that.
//!
//! Safety carve-out: `Local`s whose address is taken (`&x` / `&mut x`) are
//! excluded — the ValueGraph builder does not yet model writes through
//! pointers, so a literal reaching such a local is unsound to forward.
//! Other invalidation (calls, opaque writes, branches) is already encoded
//! in the ValueGraph's per-point reaching-def state.
//!
//! Runs on the worklist rewrite engine: a per-function standalone session
//! whose `apply_block` fires once at the body root and runs the
//! whole-function substitution pass. The single rewrite point routes through
//! the engine edit API (`replace_expr_kind`) so the parent map and use
//! index stay coherent.

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
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::StoreLoadForward, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        // Canonical alias set: locals whose address is taken (`&x` / `&mut x`)
        // plus those that flow through a `stores` clause. Mirrors `alias.rs`'s
        // seeding; the rule treats their reads as forwarding-ineligible
        // because the ValueGraph builder does not model writes through
        // pointers or `stores`-aliased references.
        let mut unsafe_locals = func.address_taken_locals.clone();
        unsafe_locals.extend(func.stores_aliased_locals.iter().copied());
        let rule = StoreLoadForwardRule {
            applied: Cell::new(false),
            unsafe_locals,
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function literal-forwarding pass at the body root.
pub(super) struct StoreLoadForwardRule {
    applied: Cell<bool>,
    /// Pre-computed forwarding-ineligible locals; see the caller for how
    /// this set is built.
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
        // Skip if this Local is itself an assign target (LHS) — `x = ...`
        // is a write, not a read, and rewriting the target with a literal
        // makes no sense.
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
        // We clone the original literal `ExprKind` so the substituted node
        // keeps its source `repr` and span. When several distinct literal
        // expressions hash-cons to the same `ValueId` (e.g. `0` and `0x0`),
        // `literal_source` returns the first one observed; the other
        // literals' reprs are lost on substitution. This is sound — VN
        // equality implies semantic equality — but may surface in NIR
        // dumps and diagnostic spans for edge-case literals.
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
