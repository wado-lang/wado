//! Forward-substitute a single-use pure scalar `let` into its adjacent use, so
//! the backend emits the operand rather than a `local.set` / `local.get` pair.
//! The value must be pure, scalar, and free of the hazards the before-use check
//! cannot cover — `Index`, a global read, unsafe integer `/` `%`. Its one use
//! must be in the next statement, whose earlier effects clear a `ModRef` check.

use cranelift_entity::EntityRef;

use crate::nir::{NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{TypeId, TypeTable};

use super::arena_query::is_pure_expr;
use super::gate::{FunctionGate, GatedPass};
use super::mod_ref::ModRef;

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
        // Maintain the block's live statement list and scan adjacent `(def, use)`
        // pairs. On a fold we drop `def` and re-examine at the same index (the
        // former `use` becomes the new `def`), so a chain resolves in one pass
        // rather than restarting the block head each time (avoids quadratic
        // behaviour on long inliner-generated blocks).
        let mut current = engine.body.blocks[block].stmts.clone();
        let mut changed = false;
        let mut i = 0;
        while i + 1 < current.len() {
            let (def_stmt, use_stmt) = (current[i], current[i + 1]);
            if self.try_fold(engine, block, def_stmt, use_stmt) {
                current.remove(i);
                engine.set_block_stmts(block, current.clone());
                changed = true;
                continue;
            }
            i += 1;
        }
        changed
    }
}

impl ScalarForwardRule<'_> {
    /// Try to forward the single-use pure scalar bound by `def_stmt` into its use
    /// in the adjacent `use_stmt`. Returns whether the fold happened (and `def_stmt`
    /// should be dropped from the block).
    fn try_fold(
        &self,
        engine: &mut Engine,
        block: BlockId,
        def_stmt: StmtId,
        use_stmt: StmtId,
    ) -> bool {
        let Some((local, source, ty)) = forwardable_binding(engine.body, def_stmt) else {
            return false;
        };
        if !self.type_table.is_primitive_like(ty) || !is_forwardable_value(engine.body, source) {
            return false;
        }
        let Some(use_id) = sole_value_use(engine, local) else {
            return false;
        };
        // The use's top-level statement (the direct child of `block` on the path to
        // it) must be the binding's immediate successor. A use nested in a sub-block
        // of `use_stmt` — `let t = a + b; if c { use(t) }` — is admitted only for a
        // trap-free value: sinking a non-trapping pure scalar into a conditional is
        // sound (the result is used only there), but a value that can trap (a null
        // field read) would move its trap from unconditional to conditional.
        // Forwarding into an aggregate literal is fine: this pass runs last, so
        // folding a single-use scalar into a literal only strips a dead temp.
        if top_level_stmt(engine, use_id, block) != Some(use_stmt) {
            return false;
        }
        let value_mr = ModRef::of_expr(engine.body, source);
        let nested = enclosing_stmt(engine, use_id) != Some(use_stmt);
        if nested && value_mr.may_trap {
            return false;
        }
        // Reorder hazard: the value moves into `use_id`'s slot, so it must clear
        // every effect evaluated before it there. A syntactic place check is not
        // enough — a call through a ref-typed local or a write through a captured
        // alias has no `Assign` / `MutRef` node — so summarise with `ModRef`. The
        // outermost op's own effect is sequenced after `use_id`.
        if clobbered_before_use(engine, &value_mr, use_id, use_stmt) {
            return false;
        }
        // Move the pure value into its one use (no clone: single use); the caller
        // drops the now-dead binding statement.
        let kind = std::mem::replace(&mut engine.body.exprs[source].kind, ExprKind::Dead);
        engine.replace_expr_kind(use_id, kind);
        true
    }
}

/// Whether any effect evaluated strictly before `use_id`, within the statement
/// `use_stmt`, may clobber the reads `value_mr` summarises. Walks the path up
/// from `use_id`, testing `ModRef::may_clobber` against each parent's earlier
/// siblings; anything outside `use_stmt` already ran before the binding. A
/// `Loop` on the path is rejected — its back-edge is unseen by a forward walk.
fn clobbered_before_use(
    engine: &Engine,
    value_mr: &ModRef,
    use_id: ExprId,
    use_stmt: StmtId,
) -> bool {
    let mut child = NodeRef::Expr(use_id);
    while child != NodeRef::Stmt(use_stmt) {
        let Some(parent) = engine.parent_of(child) else {
            return false;
        };
        if let NodeRef::Stmt(s) = parent
            && matches!(
                &engine.body.stmts[s].kind,
                StmtKind::Loop { .. } | StmtKind::LabeledBlock { .. }
            )
        {
            return true;
        }
        let mut hazard = false;
        let mut reached = false;
        engine.body.for_each_child(parent, |c| {
            if reached {
                return;
            }
            if c == child {
                reached = true;
                return;
            }
            if node_clobbers(engine, c, value_mr) {
                hazard = true;
            }
        });
        if hazard {
            return true;
        }
        child = parent;
    }
    false
}

/// Whether the subtree at `node` may clobber the reads summarised by `value_mr`.
/// An expression / statement summarises via `ModRef`; a block folds over its
/// statements; a pattern is treated conservatively as a clobber (a binding write
/// the value might read).
fn node_clobbers(engine: &Engine, node: NodeRef, value_mr: &ModRef) -> bool {
    match node {
        NodeRef::Expr(e) => ModRef::of_expr(engine.body, e).may_clobber(value_mr),
        NodeRef::Stmt(s) => ModRef::of_stmt(engine.body, s).may_clobber(value_mr),
        NodeRef::Block(b) => engine.body.blocks[b]
            .stmts
            .iter()
            .any(|&s| ModRef::of_stmt(engine.body, s).may_clobber(value_mr)),
        NodeRef::Pat(_) => true,
    }
}

/// The top-level statement — the direct child of `block` — whose subtree contains
/// `expr`, or `None` if `expr` is not under `block`.
fn top_level_stmt(engine: &Engine, expr: ExprId, block: BlockId) -> Option<StmtId> {
    let mut child = NodeRef::Expr(expr);
    loop {
        match engine.parent_of(child)? {
            NodeRef::Block(b) if b == block => {
                return match child {
                    NodeRef::Stmt(s) => Some(s),
                    _ => None,
                };
            }
            parent => child = parent,
        }
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
/// hazards the `ModRef` before-use check does not otherwise cover: `Index` can
/// trap (OOB), a global read's writers are invisible to the summary, and integer
/// `/` / `%` trap on a zero divisor. A `/` / `%` whose divisor is a provably safe
/// constant never traps, so it is admitted.
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
                    right,
                    ..
                } if !is_safe_const_divisor(body, *right) => return false,
                _ => {}
            }
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    true
}

/// Whether `divisor` is a constant that makes integer `/` / `%` non-trapping: a
/// positive `i32`-range integer literal (never `0`, never `-1`, so neither the
/// divide-by-zero nor the `INT_MIN / -1` overflow trap can fire) or any float
/// constant (Wasm float division never traps). Conservative — a non-constant or
/// out-of-range divisor is treated as possibly-trapping.
fn is_safe_const_divisor(body: &Body, divisor: crate::nir_arena::Operand) -> bool {
    let crate::nir_arena::Operand::Value(vid) = divisor else {
        return false;
    };
    match body.values.kind(vid) {
        // Non-zero and fitting in positive `i32` range (`try_from` admits
        // `0..=i32::MAX`; excluding `0` leaves `1..=i32::MAX`).
        crate::nir_value_graph::ValueKind::Int(v, _) => *v != 0 && i32::try_from(*v).is_ok(),
        crate::nir_value_graph::ValueKind::Float(_, _) => true,
        _ => false,
    }
}

/// The one value-position read of `local`, or `None` unless it is read exactly
/// once, never reassigned, and never address-taken (`&x` / `&mut x` cannot
/// receive a substituted value expression).
fn sole_value_use(engine: &Engine, local: u32) -> Option<ExprId> {
    let mut use_id = None;
    for &mention in engine.local_reads(local) {
        if engine.is_assign_target(mention) || is_addressed(engine, mention) || use_id.is_some() {
            return None;
        }
        use_id = Some(mention);
    }
    use_id
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
