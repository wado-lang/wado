//! Arena-side structural queries shared by the rewrite-engine rules.
//!
//! These mirror the tree-shaped helpers in `nir_visitor` / `elide_local`
//! (`is_local`, `expr_mentions_local`, `stmt_mentions_local`, `is_pure_expr`)
//! but read the [`Body`] arena directly, so passes ported onto the worklist
//! engine need no `Body ↔ tree` bridge. The tree helpers stay for the passes
//! that have not yet ported; this module is the single arena counterpart they
//! converge onto.

use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};

/// Whether `id` is a bare `Local(idx)` reference.
pub(super) fn is_local(body: &Body, id: ExprId, idx: u32) -> bool {
    matches!(&body.exprs[id].kind, ExprKind::Local { index, .. } if *index == idx)
}

/// Whether `idx` appears anywhere in the expression subtree at `id`. Matches
/// the coverage of the tree `expr_mentions_local` (every nested statement,
/// block, and `ConstantValue` pattern expression is walked).
pub(super) fn expr_mentions_local(body: &Body, id: ExprId, idx: u32) -> bool {
    node_mentions_local(body, NodeRef::Expr(id), idx)
}

/// Whether `idx` appears anywhere in the statement subtree at `id`.
pub(super) fn stmt_mentions_local(body: &Body, id: StmtId, idx: u32) -> bool {
    node_mentions_local(body, NodeRef::Stmt(id), idx)
}

fn node_mentions_local(body: &Body, node: NodeRef, idx: u32) -> bool {
    if let NodeRef::Expr(id) = node
        && is_local(body, id, idx)
    {
        return true;
    }
    let mut found = false;
    body.for_each_child(node, |c| {
        if !found {
            found = node_mentions_local(body, c, idx);
        }
    });
    found
}

/// True when the expression at `id` and every sub-expression has no observable
/// effect. The arena counterpart of `elide_local::is_pure_expr`; the two must
/// agree, since both gate the same rewrites.
pub(super) fn is_pure_expr(body: &Body, id: ExprId) -> bool {
    match &body.exprs[id].kind {
        ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::StringLiteral(_)
        | ExprKind::BytesLiteral(_)
        | ExprKind::Null
        | ExprKind::Unit
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::EnumConstruct { .. } => true,
        ExprKind::Binary { left, right, .. } => {
            is_pure_expr(body, *left) && is_pure_expr(body, *right)
        }
        ExprKind::Unary { expr: inner, .. } => is_pure_expr(body, *inner),
        ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => is_pure_expr(body, *inner),
        ExprKind::Index { expr: e, index: i } => is_pure_expr(body, *e) && is_pure_expr(body, *i),
        ExprKind::StructLiteral { fields, .. } => {
            fields.iter().all(|f| is_pure_expr(body, f.value))
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            elements.iter().all(|e| is_pure_expr(body, *e))
        }
        ExprKind::VariantConstruct { payload, .. } => payload.is_none_or(|p| is_pure_expr(body, p)),
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            is_pure_block(body, *block)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            is_pure_expr(body, *condition)
                && is_pure_block(body, *then_branch)
                && else_branch.is_none_or(|b| is_pure_block(body, b))
        }
        // Calls, mutations, closures, control-flow exits, and anything that
        // could suspend are conservatively impure.
        _ => false,
    }
}

fn is_pure_block(body: &Body, block: BlockId) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .all(|s| match &body.stmts[*s].kind {
            StmtKind::Expr(e) | StmtKind::Let { value: e, .. } => is_pure_expr(body, *e),
            _ => false,
        })
}
