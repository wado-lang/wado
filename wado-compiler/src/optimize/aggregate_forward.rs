//! Deliver a freshly built aggregate to its consumer directly, so the binding
//! [`super::sroa`] sees is the literal — its candidate is a `Let` bound to one.
//!
//! `?` leaves two hops in the way. `sroa_variant_return` rewrites a
//! `Result<S, E>`-returning call into slots, so an inlined callee that always
//! succeeds constructs the `Ok` only for the caller to open it again, and the
//! opened payload is then re-bound to the name the body actually uses:
//!
//! ```text
//! __vr   = Option<S>::Some(S { … });
//! __qm_v = __variant_payload(__vr, case=0);
//! seq    = __qm_v;                          // `mut`, so copy_prop declines
//! ```
//!
//! Neither hop is elidable on its own: `elide_local` wants a local nobody reads,
//! and `copy_prop` will not propagate into a binding that is later written. Both
//! collapse here, leaving `seq = S { … }` for SROA to scalarize — which is why
//! every serde container serializer kept its struct on the heap and read each
//! field back through it.

use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};

pub(super) struct AggregateForwardRule;

/// A fresh aggregate: an allocation whose identity nothing has observed yet.
fn is_aggregate_literal(body: &Body, expr: ExprId) -> bool {
    matches!(
        body.exprs[expr].kind,
        ExprKind::StructLiteral { .. } | ExprKind::TupleLiteral { .. }
    )
}

/// The payload `expr` hands back when it is `case_index`'s construct.
fn construct_payload(body: &Body, expr: ExprId, case_index: u32) -> Option<ExprId> {
    let ExprKind::VariantConstruct {
        case_index: built,
        payload: Some(payload),
        ..
    } = &body.exprs[expr].kind
    else {
        return None;
    };
    (*built == case_index).then(|| payload.as_expr())?
}

/// The value `stmt` binds, with the local it binds it to.
fn binding(body: &Body, stmt: StmtId) -> Option<(u32, ExprId)> {
    let StmtKind::Let {
        local_index, value, ..
    } = &body.stmts[stmt].kind
    else {
        return None;
    };
    Some((*local_index, value.as_expr()?))
}

/// Reading `local` back out of `source`: either the whole value, or the payload
/// of the case `source` constructs. Returns the node to overwrite and the
/// expression that should replace it.
fn consumer(body: &Body, stmt: StmtId, local: u32, source: ExprId) -> Option<(ExprId, ExprId)> {
    let (_, read) = binding(body, stmt)?;
    match &body.exprs[read].kind {
        // `let b = a` — a copy, kept alive only because `b` is later written.
        ExprKind::Local { index, .. } if *index == local && is_aggregate_literal(body, source) => {
            Some((read, source))
        }
        // `let b = __variant_payload(a, c)` — the construct/extract pair.
        ExprKind::VariantPayload {
            expr, case_index, ..
        } => {
            let ExprKind::Local { index, .. } = &body.exprs[expr.as_expr()?].kind else {
                return None;
            };
            if *index != local {
                return None;
            }
            Some((read, construct_payload(body, source, *case_index)?))
        }
        _ => None,
    }
}

impl Rule for AggregateForwardRule {
    /// The construct sitting directly inside the extraction — nothing to see
    /// through, so no ordering to preserve.
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let ExprKind::VariantPayload {
            expr, case_index, ..
        } = &engine.body.exprs[id].kind
        else {
            return false;
        };
        let (Some(source), case_index) = (expr.as_expr(), *case_index) else {
            return false;
        };
        let Some(payload) = construct_payload(engine.body, source, case_index) else {
            return false;
        };
        engine.become_expr(id, payload);
        true
    }

    /// The value bound to a local that the next statement reads. The aggregate
    /// moves to where that read stood, so the two statements must be adjacent
    /// and the binding must have this one reader.
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        let found = stmts.windows(2).enumerate().find_map(|(at, pair)| {
            let (local, source) = binding(engine.body, pair[0])?;
            let forward = consumer(engine.body, pair[1], local, source)?;
            (engine.local_reads(local).len() == 1 && engine.local_has_one_version(local))
                .then_some((at, forward))
        });
        let Some((at, (read, forwarded))) = found else {
            return false;
        };
        engine.become_expr(read, forwarded);
        let mut kept = stmts;
        kept.remove(at);
        engine.set_block_stmts(id, kept);
        true
    }
}
