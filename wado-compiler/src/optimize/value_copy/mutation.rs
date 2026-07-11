//! The shared mutation-witness recognizer and its callee oracle.
//!
//! Four analyses each re-implemented "which locals may this expression
//! mutate" (elide's usage map, `copy_prop`'s `mut_indices` /
//! `has_field_mutation`, alias's mut-escape sets, `condition_implication`'s
//! invalidation). This module recognizes the witness shapes once and answers
//! callee questions from one oracle: the pre-boxing declared-`&mut` parameter
//! bits ([`super::super::peephole`]'s `param_mut`), which stay precise where
//! the boxing rewrite has erased the `&mut`/`&` distinction from the parameter
//! type.
//!
//! The verdict is by *declaration*, not by whether the body actually writes.
//! A body-derived receiver-writes proof (`alias::CallImmutability`) is not
//! sound to elide against: it has false negatives for mutations the boxing
//! rewrite hides — `if let Some(v) = &mut self.payload { v.push(x) }` lowers to
//! `Box { value: self.payload }` matched and pushed through, a shape the
//! syntactic self-write scan misses — so trusting `writes == false` would strip
//! a copy the callee's mutation then corrupts (wado-lang/wado#1544). A declared
//! `&mut` is conservatively assumed to mutate.
//!
//! Root resolution and the bodyless-callee default stay with each consumer:
//! those differences are load-bearing (see `arena_query::storage_root` /
//! `place_root_local` docs) and direction-sensitive per analysis.

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

    /// Whether a method callee may write the caller's storage through its
    /// receiver: its declared `&mut self` bit. `None` for a bodyless callee —
    /// the site supplies its own default.
    pub(in crate::optimize) fn receiver_mutates(&self, func_id: FuncId) -> Option<bool> {
        self.arg_mutates(func_id, 0)
    }

    /// Whether the callee may write the caller's storage through parameter
    /// `idx` (absolute: `self` is 0): its declared `&mut` bit. `None` for a
    /// bodyless callee.
    pub(in crate::optimize) fn arg_mutates(&self, func_id: FuncId, idx: usize) -> Option<bool> {
        self.param_mut
            .get(&func_id)
            .map(|bits| bits.get(idx).copied().unwrap_or(false))
    }
}

/// One mutation-witness site inside an expression. The place operand is
/// reported as-is; the consumer applies its own root policy and its own
/// bodyless-callee default (`verdict: None`).
pub(in crate::optimize) enum Witness {
    /// `x = v` — a whole-value rebind of a bare local.
    Rebind(u32),
    /// A write into a non-local place (`x.f = v`, `*r = v`, `x[i] = v`):
    /// the target place's inner operand.
    Write(Operand),
    /// `&mut place`.
    MutBorrow(ExprId),
    /// A call / method argument. `verdict` is the oracle's answer (`None`
    /// = bodyless callee); `is_mut` is the site's `mut` flag.
    CalleeArg {
        expr: ExprId,
        verdict: Option<bool>,
        is_mut: bool,
    },
    /// A method receiver, with the oracle's answer.
    Receiver { expr: ExprId, verdict: Option<bool> },
    /// An indirect-call argument: no callee identity, always unknown.
    IndirectArg(ExprId),
}

/// Report every mutation witness in `id`'s own node (not its subtree — the
/// consumer's walk drives traversal, matching the existing collectors).
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
