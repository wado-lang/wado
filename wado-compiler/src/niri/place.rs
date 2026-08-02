//! What a borrow or lvalue chain names.
//!
//! A place is a local plus the field path reaching into its value. Both the
//! trackability analysis and the frame executor ask in these terms: one to
//! decide which locals stay trackable, the other to find where a write lands.

use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprId, ExprKind, Operand};

/// The operand a borrow or a cast wraps. Both name the same storage as their
/// operand — `&mut x.repr as &mut Array<u8>` is how a borrow reaches a builtin
/// after monomorphization — so every walk asking what storage is named looks
/// through them. This is niri's one statement of the wrapper set; the
/// optimizer keeps its own in `optimize/arena_query.rs` (`strip_refs`,
/// `storage_root`), so a new transparent node kind must be taught to both.
fn wrapped_operand(body: &Body, e: ExprId) -> Option<Operand> {
    match &body.exprs[e].kind {
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr,
        }
        | ExprKind::Cast { expr, .. } => Some(*expr),
        _ => None,
    }
}

/// The expression `op` names once every [`wrapped_operand`] layer is peeled.
/// `None` where a layer is a promoted value rather than a skeleton node, which
/// names no storage a walk can record or look up.
pub(super) fn peel_wrappers(body: &Body, op: Operand) -> Option<ExprId> {
    let mut e = op.as_expr()?;
    while let Some(inner) = wrapped_operand(body, e) {
        e = inner.as_expr()?;
    }
    Some(e)
}

/// The local an lvalue or borrow chain roots at: `x`, `x.f`, `x[i]`, `*x`, a
/// cast over any of those (a cast names the same storage as its operand), and
/// any nesting.
pub(super) fn lvalue_root_local(body: &Body, op: Operand) -> Option<u32> {
    match &body.exprs[op.as_expr()?].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. } => lvalue_root_local(body, *inner),
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => lvalue_root_local(body, *inner),
        _ => None,
    }
}

/// The local a borrow or lvalue chain roots at, and the field path reaching
/// into its value. [`lvalue_root_local`] answers only which local is touched;
/// a write also needs to know where inside it lands.
pub(super) fn place_of(body: &Body, op: Operand) -> Option<(u32, Vec<u32>)> {
    let e = op.as_expr()?;
    if let Some(inner) = wrapped_operand(body, e) {
        return place_of(body, inner);
    }
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some((*index, Vec::new())),
        ExprKind::FieldAccess {
            expr, field_index, ..
        } => {
            let (root, mut path) = place_of(body, *expr)?;
            path.push(*field_index);
            Some((root, path))
        }
        _ => None,
    }
}

/// The local a write through `op` lands in: [`place_of`]'s root when the
/// chain spells a borrow (which `lvalue_root_local` does not peel), else
/// [`lvalue_root_local`]'s (which alone handles `Index`). `None` when no
/// local roots the write.
pub(super) fn write_root_local(body: &Body, op: Operand) -> Option<u32> {
    place_of(body, op)
        .map(|(root, _)| root)
        .or_else(|| lvalue_root_local(body, op))
}

/// The borrow `op` spells over a place: whether it is mutable, and the
/// borrowed operand. `None` unless the operand borrows something
/// [`place_of`] can root at a local — `&GLOBAL` and a borrow of an rvalue
/// name no local place.
pub(super) fn borrowed_place_operand(body: &Body, op: Operand) -> Option<(bool, Operand)> {
    let ExprKind::Unary { op: unary, expr } = &body.exprs[op.as_expr()?].kind else {
        return None;
    };
    let is_mut = match unary {
        NirUnaryOp::MutRef => true,
        NirUnaryOp::Ref => false,
        _ => return None,
    };
    place_of(body, *expr)?;
    Some((is_mut, *expr))
}

/// Whether some place in `places` besides `(root, path)` itself reaches the
/// storage it names. One place covers another when its path is a prefix: `c`
/// covers `c.repr`, and `c.repr` and `c.used` cover nothing of each other.
///
/// `places` holds the target's own place too, so a second match is what a
/// second handle looks like.
pub(super) fn place_aliased_by_another(
    places: &[(u32, Vec<u32>)],
    root: u32,
    path: &[u32],
) -> bool {
    places
        .iter()
        .filter(|(other_root, other_path)| {
            *other_root == root && (other_path.starts_with(path) || path.starts_with(other_path))
        })
        .nth(1)
        .is_some()
}
