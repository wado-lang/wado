//! The mutation-witness recognizer behind copy propagation's mutation
//! analyses (consumed via `arena_query::for_each_mutated_root`), plus the
//! callee oracle it consults.
//!
//! The verdict is a callee's declared `&mut` parameter bit (`param_mut`,
//! captured before boxing erases the `&mut`/`&` distinction) — never a
//! body-derived "does not write" proof, which has false negatives for
//! mutations the boxing rewrite hides (`&mut self.payload` becomes
//! `Box { value: self.payload }`), so trusting it would strip a copy the
//! callee then corrupts (wado-lang/wado#1544).
//!
//! Root resolution and the bodyless-callee default stay with each consumer.

use crate::hashmap::IndexMap;
use crate::nir::{FuncId, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, Operand};
use crate::nir_package::NirPackage;

/// Per-callee declared `&mut` parameter bits, captured before boxing erases the
/// `&mut` / `&` distinction. Bodyless callees stay absent (their metadata is not
/// trustworthy here). Consumed by [`MutationOracle`] and the copy-propagation
/// alias analysis.
pub(in crate::optimize) fn build_param_mut(project: &NirPackage) -> IndexMap<FuncId, Vec<bool>> {
    let mut map = IndexMap::default();
    for func in &project.functions {
        let func = func.borrow();
        if func.body.is_none() {
            continue;
        }
        let Some(id) = func.id else { continue };
        map.insert(id, func.params.iter().map(|p| p.is_mut_ref).collect());
    }
    map
}

pub(in crate::optimize) struct MutationOracle<'a> {
    param_mut: &'a IndexMap<FuncId, Vec<bool>>,
}

impl<'a> MutationOracle<'a> {
    pub(in crate::optimize) fn new(param_mut: &'a IndexMap<FuncId, Vec<bool>>) -> Self {
        Self { param_mut }
    }
    /// Whether the callee may write the caller's storage through parameter
    /// `idx` (`self` is 0). `None` for a bodyless callee.
    pub(in crate::optimize) fn arg_mutates(&self, func_id: FuncId, idx: usize) -> Option<bool> {
        self.param_mut
            .get(&func_id)
            .map(|bits| bits.get(idx).copied().unwrap_or(false))
    }
}

/// A mutation-witness site. The consumer applies its own root policy and its
/// own bodyless-callee default (`verdict: None`).
pub(in crate::optimize) enum Witness {
    /// `x = v` — a whole-value rebind of a bare local.
    Rebind(u32),
    /// A write into a non-local place (`x.f = v`, `*r = v`, `x[i] = v`): the
    /// target's inner operand.
    Write(Operand),
    /// `&mut place`.
    MutBorrow(ExprId),
    CalleeArg {
        expr: ExprId,
        verdict: Option<bool>,
        is_mut: bool,
    },
    Receiver {
        expr: ExprId,
        verdict: Option<bool>,
    },
    /// An indirect-call argument: callee unknown.
    IndirectArg(ExprId),
}

/// Report every mutation witness in `id`'s own node; the consumer's walk drives
/// traversal into children.
pub(in crate::optimize) fn expr_witnesses(
    body: &Body,
    id: ExprId,
    oracle: &MutationOracle<'_>,
    sink: &mut impl FnMut(Witness),
) {
    match &body.exprs[id].kind {
        ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
            ExprKind::Local { index, .. } => sink(Witness::Rebind(*index)),
            ExprKind::FieldAccess { expr: inner, .. }
            | ExprKind::Index { expr: inner, .. }
            | ExprKind::VariantPayload { expr: inner, .. }
            | ExprKind::Unary {
                op: NirUnaryOp::Deref,
                expr: inner,
            } => sink(Witness::Write(*inner)),
            // The only other legal target is a global (`GlobalVarGet`),
            // which touches no local storage.
            _ => {}
        },
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => {
            if let Some(e) = inner.as_expr() {
                sink(Witness::MutBorrow(e));
            }
        }
        ExprKind::Call {
            func_id,
            args,
            has_receiver,
            ..
        } => {
            for (i, arg) in args.iter().enumerate() {
                let Some(ae) = arg.expr.as_expr() else {
                    continue;
                };
                let verdict = oracle.arg_mutates(*func_id, i);
                // A receiver reports as `Receiver` so a bodyless callee falls
                // back to the receiver's type rather than to `is_mut`.
                if *has_receiver && i == 0 {
                    sink(Witness::Receiver { expr: ae, verdict });
                } else {
                    sink(Witness::CalleeArg {
                        expr: ae,
                        verdict,
                        is_mut: arg.is_mut,
                    });
                }
            }
        }
        ExprKind::IndirectCall { args, .. } => {
            for &arg in args {
                if let Some(ae) = arg.as_expr() {
                    sink(Witness::IndirectArg(ae));
                }
            }
        }
        _ => {}
    }
}
