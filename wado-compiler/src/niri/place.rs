//! What a borrow or lvalue chain names.
//!
//! A place is a local plus the field path reaching into its value. Both the
//! trackability analysis and the frame executor ask in these terms: one to
//! decide which locals stay trackable, the other to find where a write lands.

use crate::nir::NirUnaryOp;
use crate::nir_arena::{Body, ExprKind, Operand};

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
///
/// A cast names the same storage as its operand — `&mut x.repr as
/// &mut Array<u8>` is how a borrow reaches a builtin after monomorphization —
/// so the chain is followed through it.
pub(super) fn place_of(body: &Body, op: Operand) -> Option<(u32, Vec<u32>)> {
    match &body.exprs[op.as_expr()?].kind {
        ExprKind::Local { index, .. } => Some((*index, Vec::new())),
        ExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref,
            expr,
        }
        | ExprKind::Cast { expr, .. } => place_of(body, *expr),
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

/// How many of `places` reach storage `(root, path)` reaches. One place covers
/// another when its path is a prefix: `c` covers `c.repr`, and `c.repr` and
/// `c.used` cover nothing of each other.
pub(super) fn overlapping_places(places: &[(u32, Vec<u32>)], root: u32, path: &[u32]) -> usize {
    places
        .iter()
        .filter(|(other_root, other_path)| {
            *other_root == root && (other_path.starts_with(path) || path.starts_with(other_path))
        })
        .count()
}
