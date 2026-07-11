//! The mutation-witness recognizer shared by the value-copy elision, copy
//! propagation, alias, and condition-implication analyses, plus the callee
//! oracle they consult.
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

pub(in crate::optimize) struct MutationOracle<'a> {
    param_mut: &'a IndexMap<FuncId, Vec<bool>>,
}

impl<'a> MutationOracle<'a> {
    pub(in crate::optimize) fn new(param_mut: &'a IndexMap<FuncId, Vec<bool>>) -> Self {
        Self { param_mut }
    }

    /// Whether the callee may write the caller's receiver. `None` for a
    /// bodyless callee.
    pub(in crate::optimize) fn receiver_mutates(&self, func_id: FuncId) -> Option<bool> {
        self.arg_mutates(func_id, 0)
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
            | ExprKind::VariantPayload { expr: inner, .. } => sink(Witness::Write(*inner)),
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
        ExprKind::Call { func_id, args, .. } => {
            for (i, arg) in args.iter().enumerate() {
                if let Some(ae) = arg.expr.as_expr() {
                    sink(Witness::CalleeArg {
                        expr: ae,
                        verdict: oracle.arg_mutates(*func_id, i),
                        is_mut: arg.is_mut,
                    });
                }
            }
        }
        ExprKind::MethodCall {
            receiver,
            func_id,
            args,
            ..
        } => {
            if let Some(re) = receiver.as_expr() {
                sink(Witness::Receiver {
                    expr: re,
                    verdict: oracle.receiver_mutates(*func_id),
                });
            }
            for (i, arg) in args.iter().enumerate() {
                if let Some(ae) = arg.expr.as_expr() {
                    sink(Witness::CalleeArg {
                        expr: ae,
                        verdict: oracle.arg_mutates(*func_id, i + 1),
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
