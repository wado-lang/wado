//! Forward-substitute a single-use pure scalar `let` into its adjacent use.
//!
//! Inlining a value parameter binds it as `let p = <arg>` at the callee-block
//! head; when the arg is a field read or a cast (`let index = self.used; let
//! value = code as u8; array_set(&mut self.repr, index, value)`) it matches no
//! `copy_prop` copy source and survives as a dead-weight local. This rule folds
//! such a binding back into its one use, so the backend emits the operand
//! directly instead of a `local.set` / `local.get` round-trip.
//!
//! Only a *replacement-free* fold is taken: the binding's value must be pure and
//! scalar (a scalar copy has no aliasing or value-copy timing to preserve), free
//! of traps whose reordering is observable (`Index`, `/`, `%`) and of globals
//! (whose writers the place check cannot see), and its single use must sit in the
//! immediately following statement with nothing there writing a place the value
//! reads. Non-adjacent chains resolve through the engine fixpoint: forwarding the
//! nearer binding makes the next one adjacent.

use cranelift_entity::EntityRef;

use crate::nir::{NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{TypeId, TypeTable};

use super::arena_query::{Place, is_pure_expr, place_overlaps, place_path};
use super::gate::{FunctionGate, GatedPass};

/// Forward single-use pure scalar `let`s into their uses across every function.
///
/// Runs *after* the fixed-point loop and scalarization: forwarding a value's
/// field reads earlier would strip the `let f = agg.n` destructure shape that
/// `sroa` matches to scalarize `agg`, leaving the aggregate materialized. By the
/// end that scalarization is done, so this only cleans up the inliner's leftover
/// value-parameter temps.
pub fn forward_scalar_temps(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.borrow();
    let rule = ScalarForwardRule::new(&type_table);
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::ScalarForward, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        let NirFunction { body, locals, .. } = &mut *func;
        let Some(body) = body.as_mut() else {
            return false;
        };
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

struct ScalarForwardRule<'a> {
    type_table: &'a TypeTable,
}

impl<'a> ScalarForwardRule<'a> {
    fn new(type_table: &'a TypeTable) -> Self {
        Self { type_table }
    }
}

impl Rule for ScalarForwardRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let stmts = engine.body.blocks[block].stmts.clone();
        for pair in stmts.windows(2) {
            let (def_stmt, use_stmt) = (pair[0], pair[1]);
            let Some((local, source, ty)) = forwardable_binding(engine.body, def_stmt) else {
                continue;
            };
            if !self.type_table.is_primitive_like(ty) || !is_forwardable_value(engine.body, source)
            {
                continue;
            }
            let Some(use_id) = sole_value_use(engine, local) else {
                continue;
            };
            // The use's enclosing statement must be the binding's immediate
            // successor (which also rules out a use nested in a sub-block, whose
            // enclosing statement is the inner one). Forwarding into an aggregate
            // literal is fine here: this pass runs last, after every SROA /
            // globalization recognizer has matched its shape, so folding a
            // single-use scalar into a literal element only strips a dead temp.
            if enclosing_stmt(engine, use_id) != Some(use_stmt) {
                continue;
            }
            let mut reads = Vec::new();
            read_places(engine.body, source, &mut reads);
            if writes_overlap(engine.body, use_stmt, &reads) {
                continue;
            }
            // Move the pure value into its one use (no clone: single use), then
            // drop the now-dead binding.
            let kind = std::mem::replace(&mut engine.body.exprs[source].kind, ExprKind::Dead);
            engine.replace_expr_kind(use_id, kind);
            let kept: Vec<StmtId> = stmts.iter().copied().filter(|&s| s != def_stmt).collect();
            engine.set_block_stmts(block, kept);
            return true;
        }
        false
    }
}

/// A `let x = <expr>` binding as `(local, value-expr, declared-type)`; `None` for
/// a destructure, a promoted-constant value (already an operand), or a non-`Let`.
fn forwardable_binding(body: &Body, stmt: StmtId) -> Option<(u32, ExprId, TypeId)> {
    match &body.stmts[stmt].kind {
        StmtKind::Let {
            local_index,
            value,
            type_id,
            ..
        } => value.as_expr().map(|e| (*local_index, e, *type_id)),
        _ => None,
    }
}

/// A value is forwardable when it is pure (no effects) and free of the reorder
/// hazards the adjacency-plus-place check does not otherwise cover: `Index` and
/// `/` / `%` can trap, and a global read's writers are invisible to the place
/// check.
fn is_forwardable_value(body: &Body, root: ExprId) -> bool {
    if !is_pure_expr(body, root) {
        return false;
    }
    let mut stack = vec![NodeRef::Expr(root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            match &body.exprs[id].kind {
                ExprKind::Index { .. } | ExprKind::GlobalVarGet { .. } => return false,
                ExprKind::Binary {
                    op: NirBinaryOp::Div | NirBinaryOp::Mod,
                    ..
                } => return false,
                _ => {}
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    true
}

/// The one value-position read of `local`, or `None` unless it is read exactly
/// once, never reassigned, and never address-taken (`&x` / `&mut x` cannot
/// receive a substituted value expression).
fn sole_value_use(engine: &Engine, local: u32) -> Option<ExprId> {
    let mut use_id = None;
    for &mention in engine.local_reads(local) {
        if is_assign_target(engine, mention) || is_addressed(engine, mention) || use_id.is_some() {
            return None;
        }
        use_id = Some(mention);
    }
    use_id
}

fn is_assign_target(engine: &Engine, mention: ExprId) -> bool {
    matches!(engine.parent_of(NodeRef::Expr(mention)), Some(NodeRef::Expr(p))
        if matches!(&engine.body.exprs[p].kind, ExprKind::Assign { target, .. } if *target == mention))
}

fn is_addressed(engine: &Engine, mention: ExprId) -> bool {
    matches!(engine.parent_of(NodeRef::Expr(mention)), Some(NodeRef::Expr(p))
        if matches!(&engine.body.exprs[p].kind, ExprKind::Unary { op: NirUnaryOp::Ref | NirUnaryOp::MutRef, .. }))
}

/// The innermost statement containing `expr`. Adjacency to the binding requires
/// this to be its immediate successor, which also rules out a use nested in a
/// sub-block (whose enclosing statement is the inner one).
fn enclosing_stmt(engine: &Engine, expr: ExprId) -> Option<StmtId> {
    let mut node = NodeRef::Expr(expr);
    loop {
        match engine.parent_of(node)? {
            NodeRef::Stmt(s) => return Some(s),
            parent => node = parent,
        }
    }
}

/// The places a forwardable value reads: each maximal `Local` / field-access
/// chain, plus the operands of the pure scalar ops above them.
fn read_places(body: &Body, expr: ExprId, out: &mut Vec<Place>) {
    if matches!(
        &body.exprs[expr].kind,
        ExprKind::Local { .. } | ExprKind::FieldAccess { .. }
    ) && let Some(place) = place_path(body, expr)
    {
        out.push(place);
        return;
    }
    body.for_each_child(NodeRef::Expr(expr), |c| {
        if let NodeRef::Expr(e) = c {
            read_places(body, e, out);
        }
    });
}

/// Whether a statement writes — by `Assign` or `&mut` borrow — a place that
/// overlaps one the forwarded value reads.
fn writes_overlap(body: &Body, stmt: StmtId, reads: &[Place]) -> bool {
    let mut stack = vec![NodeRef::Stmt(stmt)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node {
            let written = match &body.exprs[id].kind {
                ExprKind::Assign { target, .. } => place_path(body, *target),
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: inner,
                } => inner.as_expr().and_then(|e| place_path(body, e)),
                _ => None,
            };
            if let Some(w) = written
                && reads.iter().any(|r| place_overlaps(&w, r))
            {
                return true;
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}
