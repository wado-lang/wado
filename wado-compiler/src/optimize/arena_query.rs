//! Arena-side structural queries shared by the rewrite-engine rules.
//!
//! `is_local`, `expr_mentions_local`, `stmt_mentions_local`, `is_pure_expr`,
//! `collect_reads`, … read the [`Body`] arena directly, so the ported passes
//! need no `Body ↔ tree` bridge.

use crate::hashmap::IndexSet;
use crate::nir::NirUnaryOp;
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, PatKind, StmtId, StmtKind,
};

/// Every block reachable from the body root, in DFS pop order (a block precedes
/// the blocks nested under it). The NIR block graph is a tree, so no visited set
/// is needed.
pub(super) fn reachable_blocks(body: &Body) -> Vec<BlockId> {
    let mut out = Vec::new();
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Block(b) = node {
            out.push(b);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    out
}

/// The single optional payload-binding local of a variant arm's `bindings`,
/// as two distinct outcomes callers tell apart (so `?` propagates the reject):
/// `Some(None)` = no binding (`[]` or `[_]`); `Some(Some(idx))` = one `Binding`
/// slot; `None` = reject (multiple bindings, or a nested subpattern the
/// `labeled_block_fusion` payload substitution does not handle).
#[allow(clippy::option_option)]
pub(super) fn single_payload_binding(body: &Body, bindings: &[PatId]) -> Option<Option<u32>> {
    match bindings {
        [] => Some(None),
        [single] => match &body.pats[*single].kind {
            PatKind::Wildcard => Some(None),
            PatKind::Binding { local_index, .. } => Some(Some(*local_index)),
            _ => None,
        },
        _ => None,
    }
}

/// If `expr` is a place rooted at a local — `x`, `x.f`, `x[i]`, `*x`, and any
/// chain thereof — return that root local index; otherwise `None`.
///
/// Deliberately narrower than [`storage_root`]: stopping at `&x` lets
/// `copy_prop`'s mutation collector dispatch on the wrapper (a `&T` receiver is
/// not written through, so it is correctly not marked; `&mut x` is caught by
/// its own arm). Widening through references would over-mark and cost
/// propagations.
pub(super) fn place_root_local(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Index { expr: inner, .. } => {
            inner.as_expr().and_then(|e| place_root_local(body, e))
        }
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => inner.as_expr().and_then(|e| place_root_local(body, e)),
        _ => None,
    }
}

/// A place as its root local plus the field-index chain leading off it.
pub(super) type Place = (u32, Vec<u32>);

/// The [`Place`] of an expression — its root local and field-access chain —
/// seeing through `&`/`&mut`/deref wrappers (so an inlined `self.f` whose
/// receiver became `&mut b` still roots at `b`). `None` at an `Index` or any
/// non-place step: a non-field place can never be a prefix of a pure
/// Local/field place, so it never overlaps one.
pub(super) fn place_path(body: &Body, expr: ExprId) -> Option<Place> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some((*index, Vec::new())),
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let (root, mut fields) = place_path(body, inner.as_expr()?)?;
            fields.push(*field_index);
            Some((root, fields))
        }
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        } => place_path(body, inner.as_expr()?),
        _ => None,
    }
}

/// Whether place `q` is a (non-strict) prefix of place `p`: same root and `q`'s
/// field chain leads `p`'s. Replacing the handle at `q` replaces the object a
/// reference to `p` observes.
pub(super) fn is_place_prefix(q: &Place, p: &Place) -> bool {
    q.0 == p.0 && q.1.len() <= p.1.len() && q.1 == p.1[..q.1.len()]
}

/// Whether two places overlap — one is a prefix of the other — so a write to
/// either may change a read of the other (`a.b` overlaps `a`, `a.b`, and
/// `a.b.c`, but not the sibling `a.c`).
pub(super) fn place_overlaps(a: &Place, b: &Place) -> bool {
    is_place_prefix(a, b) || is_place_prefix(b, a)
}

/// The local whose interior storage `expr` reaches, seeing through the
/// projections that share it: field access, indexing, variant payload, a
/// transparent cast, and `&`/`&mut`/`*`. Arithmetic unaries produce fresh
/// scalars and do not descend. The root-only storage query for the escape /
/// aliasing / mutation-witness analyses; distinct from [`place_root_local`]
/// (narrower, paired with the caller's own wrapper dispatch) and the
/// path-sensitive [`place_path`].
///
/// `None` does *not* mean "fresh": `container.index_value(i)` also returns
/// `None` yet aliases the container, so callers pair this with a freshness
/// gate (`EscapeMap::rvalue_is_fresh`) or treat `None` conservatively.
pub(super) fn storage_root(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr: inner,
        }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. } => storage_root(body, inner.as_expr()?),
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
            // A promoted `Operand::Value` inner cannot be stripped further; the
            // wrapper id is the leaf.
        } => inner.as_expr().map_or(id, |e| strip_refs(body, e)),
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
                if let Some(ve) = value.as_expr() {
                    collect_reads_node(body, NodeRef::Expr(ve), out);
                }
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

/// Whether `op` is a bare `Local(idx)` reference. A promoted constant
/// (`Operand::Value`) is never a local.
pub(super) fn is_local_operand(body: &Body, op: Operand, idx: u32) -> bool {
    op.as_expr().is_some_and(|e| is_local(body, e, idx))
}

/// Whether `idx` appears anywhere in the operand. A promoted constant mentions
/// no local.
pub(super) fn operand_mentions_local(body: &Body, op: Operand, idx: u32) -> bool {
    op.as_expr()
        .is_some_and(|e| expr_mentions_local(body, e, idx))
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

/// [`is_pure_expr`] for an operand: a promoted constant is pure.
pub(super) fn is_pure_operand(body: &Body, op: Operand) -> bool {
    op.as_expr().is_none_or(|e| is_pure_expr(body, e))
}

/// True when the expression at `id` and every sub-expression has no observable
/// effect. The arena counterpart of `elide_local::is_pure_expr`; the two must
/// agree, since both gate the same rewrites.
pub(super) fn is_pure_expr(body: &Body, id: ExprId) -> bool {
    match &body.exprs[id].kind {
        ExprKind::PackedArray(_)
        | ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::EnumConstruct { .. } => true,
        ExprKind::Binary { left, right, .. } => {
            is_pure_operand(body, *left) && is_pure_operand(body, *right)
        }
        ExprKind::Unary { expr: inner, .. } => is_pure_operand(body, *inner),
        ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => is_pure_operand(body, *inner),
        ExprKind::Index { expr: e, index: i } => {
            is_pure_operand(body, *e) && is_pure_operand(body, *i)
        }
        ExprKind::StructLiteral { fields, .. } => {
            fields.iter().all(|f| is_pure_operand(body, f.value))
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            elements.iter().all(|&e| is_pure_operand(body, e))
        }
        ExprKind::VariantConstruct { payload, .. } => {
            payload.is_none_or(|p| is_pure_operand(body, p))
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            is_pure_block(body, *block)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            condition.as_expr().is_none_or(|e| is_pure_expr(body, e))
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
            StmtKind::Expr(e) => is_pure_operand(body, *e),
            StmtKind::Let { value, .. } => is_pure_operand(body, *value),
            _ => false,
        })
}
