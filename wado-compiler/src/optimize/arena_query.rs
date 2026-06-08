//! Arena-side structural queries shared by the rewrite-engine rules.
//!
//! `is_local`, `expr_mentions_local`, `stmt_mentions_local`, `is_pure_expr`,
//! `collect_reads`, … read the [`Body`] arena directly, so the ported passes
//! need no `Body ↔ tree` bridge.

use crate::hashmap::IndexSet;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};

/// If `expr` is a place rooted at a local — `x`, `x.f`, `x[i]`, `*x`, and any
/// chain thereof — return that root local index; otherwise `None`. Used by
/// passes that need the local a place projects from (parameter SROA) or that
/// detect mutation of a local through any projection (copy propagation).
pub(super) fn place_root_local(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Index { expr: inner, .. } => {
            place_root_local(body, *inner)
        }
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => place_root_local(body, *inner),
        _ => None,
    }
}

/// Whether the subtree at `node` contains a `Break` targeting `label`. A full
/// subtree search, so nested blocks that rebind the same label are still
/// searched — the conservative behaviour the sync-placement passes rely on.
pub(super) fn has_break_to(body: &Body, node: NodeRef, label: &str) -> bool {
    if let NodeRef::Stmt(s) = node
        && let StmtKind::Break { label: Some(l), .. } = &body.stmts[s].kind
        && l == label
    {
        return true;
    }
    let mut found = false;
    body.for_each_child(node, |c| {
        if !found {
            found = has_break_to(body, c, label);
        }
    });
    found
}

/// Strip outer auto-ref / deref wrappers (`&`, `&mut`, `*`) from an expression,
/// returning the inner value's id.
pub(super) fn strip_refs(body: &Body, id: ExprId) -> ExprId {
    match &body.exprs[id].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        } => strip_refs(body, *inner),
        _ => id,
    }
}

/// Collect every local index that is *read* — every `Local` mention except the
/// bare-`Local` target of an `Assign` (a write). `&local` / `&mut local`,
/// `local.field = …`, and every value-position `Local` count as reads. The
/// arena counterpart of `elide_local`'s tree `ReadCollector` /
/// `collect_reads_in_block`.
pub(super) fn collect_reads(body: &Body, out: &mut IndexSet<u32>) {
    collect_reads_node(body, NodeRef::Block(body.root), out);
}

fn collect_reads_node(body: &Body, node: NodeRef, out: &mut IndexSet<u32>) {
    if let NodeRef::Expr(id) = node {
        match &body.exprs[id].kind {
            ExprKind::Local { index, .. } => {
                out.insert(*index);
                return;
            }
            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                // The bare-`Local` target is a write, not a read; nested write
                // places (`a.field`, `a[i]`) and the assigned value are reads.
                if !matches!(&body.exprs[target].kind, ExprKind::Local { .. }) {
                    collect_reads_node(body, NodeRef::Expr(target), out);
                }
                collect_reads_node(body, NodeRef::Expr(value), out);
                return;
            }
            _ => {}
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_reads_node(body, c, out);
    }
}

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
