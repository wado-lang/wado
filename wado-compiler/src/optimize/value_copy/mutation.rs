//! The shared mutation-witness recognizer and its callee oracle.
//!
//! Four analyses each re-implemented "which locals may this expression
//! mutate" (elide's usage map, `copy_prop`'s `mut_indices` /
//! `has_field_mutation`, alias's mut-escape sets, `condition_implication`'s
//! invalidation), with different callee oracles of *independent* precision:
//! the pre-boxing declared-`&mut` parameter bits ([`super::super::peephole`]'s
//! `param_mut`) and the body-derived receiver-writes fixpoint
//! (`alias::CallImmutability`). This module recognizes the witness shapes
//! once and answers callee questions with the *conjunction* of both oracles —
//! sound (a caller-visible write needs a writable declaration AND an actual
//! write) and strictly more precise than either alone: a declared `&mut self`
//! that never writes, and a by-value `self` that writes its own copy, both
//! stop counting as mutations.
//!
//! Root resolution and the bodyless-callee default stay with each consumer:
//! those differences are load-bearing (see `arena_query::storage_root` /
//! `place_root_local` docs) and direction-sensitive per analysis.

use crate::hashmap::IndexMap;
use crate::nir::{FuncId, NirUnaryOp};
use crate::nir_arena::{Body, ExprId, ExprKind, Operand};

use super::super::alias::CallImmutability;

pub(in crate::optimize) struct MutationOracle<'a> {
    param_mut: &'a IndexMap<FuncId, Vec<bool>>,
    call_immutability: &'a CallImmutability<'a>,
}

impl<'a> MutationOracle<'a> {
    pub(in crate::optimize) fn new(
        param_mut: &'a IndexMap<FuncId, Vec<bool>>,
        call_immutability: &'a CallImmutability<'a>,
    ) -> Self {
        Self {
            param_mut,
            call_immutability,
        }
    }

    /// Whether a method callee may write the caller's storage through its
    /// receiver. `None` for a bodyless callee — the site supplies its own
    /// default. Bodied: declared `&mut self` (pre-boxing bit) AND the
    /// body-derived writes fixpoint.
    pub(in crate::optimize) fn receiver_mutates(&self, func_id: FuncId) -> Option<bool> {
        let declared = self
            .param_mut
            .get(&func_id)?
            .first()
            .copied()
            .unwrap_or(false);
        Some(
            match self.call_immutability.method_writes_receiver(func_id) {
                Some(writes) => declared && writes,
                None => declared,
            },
        )
    }

    /// Whether the callee may write the caller's storage through parameter
    /// `idx` (absolute: `self` is 0). `None` for a bodyless callee.
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
