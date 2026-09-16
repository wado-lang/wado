//! A reference names storage: the operand of `&` / `&mut` is a place, and where
//! it is a value instead, the value gets a local of its own for the reference to
//! name.
//!
//! Every place-based analysis downstream — `sroa`'s candidates, `ref_elim`'s
//! referents, `licm`'s invariance, globalization's read-only walk — is written
//! over named storage, so a referent with no name is invisible to all of them.
//! `s.as_str_slice().len()` is the shape: auto-ref takes a reference of the
//! call's result, inlining turns that result into a block, and the view is
//! allocated for one field read nobody can see through. Naming the referent
//! reduces it to `let v = s.as_str_slice(); v.len()`, which those passes already
//! scalarize.

use crate::nir::NirUnaryOp;
use crate::nir_arena::{ExprId, ExprKind, StmtKind};
use crate::nir_engine::{Engine, Rule};

use super::arena_query::storage_root;

/// The prefix every referent local this rule mints carries.
const REFERENT_PREFIX: &str = "$referent_";

pub(super) struct NameReferentRule;

impl Rule for NameReferentRule {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let ExprKind::Unary { op, expr } = &engine.body.exprs[id].kind else {
            return false;
        };
        let (op, value) = (*op, *expr);
        if !matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef) {
            return false;
        }
        // A promoted operand is a value the graph names rather than storage the
        // body holds, so there is nothing to bind and nothing that would read
        // the binding.
        let Some(referent) = value.as_expr() else {
            return false;
        };
        if storage_root(engine.body, referent).is_some() {
            return false;
        }

        let referent_type = engine.body.exprs[referent].type_id;
        let reference_type = engine.body.exprs[id].type_id;
        let span = engine.body.exprs[id].span;

        let index = engine.locals().len() as u32;
        let name = format!("{REFERENT_PREFIX}{index}");
        let allocated = engine.alloc_local(name.clone(), referent_type, /* is_mut */ false);
        assert_eq!(
            allocated, index,
            "a local lands at the index it was named for"
        );

        let bind = engine.alloc_stmt(
            StmtKind::Let {
                name: name.clone(),
                local_index: index,
                is_mut: false,
                is_reactive: false,
                type_id: referent_type,
                value,
                // The referent is the value the reference was already taken of,
                // so nothing else names it and no copy stands between them.
                skip_value_copy: true,
            },
            span,
        );
        let place = engine.alloc_expr(
            ExprKind::Local { index, name },
            referent_type,
            span,
        );
        let reference = engine.alloc_expr(
            ExprKind::Unary {
                op,
                expr: place.into(),
            },
            reference_type,
            span,
        );
        let tail = engine.alloc_stmt(StmtKind::Expr(reference.into()), span);
        let block = engine.alloc_block(vec![bind, tail], span);
        engine.replace_expr_kind(
            id,
            ExprKind::plain_block(block, reference_type, "referent"),
        );
        true
    }
}
