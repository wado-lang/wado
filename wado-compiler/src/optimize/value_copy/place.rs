//! Resolve an lvalue expression to the storage-root local it projects from.

use crate::nir_arena::{Body, ExprId, ExprKind};

/// Find the root local that `expr` reads from, descending through projections
/// and indexing that share storage with the container they read from
/// (`FieldAccess`, `VariantPayload`, `Cast`, `Unary`, `Index`). Returns `None`
/// when no local root is reachable — a call result, a constant, or a bare
/// literal. `None` does *not* by itself mean "fresh": an accessor call such as
/// `container.index_value(i)` also returns `None` yet aliases the container's
/// element, so callers pair it with a freshness gate
/// (`EscapeMap::rvalue_is_fresh` or `yields_subexpression`).
///
/// This follows every projection, including references and variant payloads.
/// `arena_query::{place_root_local, projection_root_local}` deliberately follow
/// narrower subsets for their callers; consolidating all three behind an
/// explicit projection policy is future work (Phase 2 of the value-copy
/// refactor).
pub(in crate::optimize) fn arg_source_root(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. } => {
            inner.as_expr().and_then(|e| arg_source_root(body, e))
        }
        _ => None,
    }
}
