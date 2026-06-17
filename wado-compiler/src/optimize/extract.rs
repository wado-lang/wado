//! Extraction: materialize pure values from the live ValueGraph back into the
//! SkelTree.
//!
//! In the live-ValueGraph design (see
//! `docs/wep-2026-06-15-live-value-graph.md`) pure-value rewrites prove
//! equivalences by unioning e-classes rather than editing the skeleton. A read
//! of an expression's value therefore goes through the class representative
//! ([`Engine::value_find`]); the extractor walks the skeleton and lowers each
//! pure operand to the representative's chosen concrete form.
//!
//! This is the literal case — the keystone every union-based pure-value rule
//! reuses: an expression whose representative is a constant (`Int` / `Float` /
//! `Bool` / `Char`) is replaced by that literal. The share-vs-duplicate cost
//! model for non-literal multi-use values is a later step; constants are always
//! cheaper to rematerialize than to share, so they need no cost decision.
//!
//! [`materialize_literal`] is the shared materialization primitive, used in
//! production by `store_load_forward`. [`ExtractLiteralRule`] (the worklist
//! rule form) is exercised by unit tests until the first union-producing
//! pure-value pass wires it into a combined session.
#![allow(dead_code)]

use crate::nir_arena::{ExprId, ExprKind, NodeRef};
use crate::nir_engine::{Engine, Rule};
use crate::nir_value_graph::{ValueId, ValueKind};

/// Rewrite a pure expression whose ValueGraph representative is a literal into
/// that literal. Idempotent: an expression already holding the target literal
/// is left untouched, so the worklist retry terminates.
pub(super) struct ExtractLiteralRule;

impl Rule for ExtractLiteralRule {
    fn apply_expr(&self, e: &mut Engine, id: ExprId) -> bool {
        // An assign target is a place, not a value — never materialize it.
        if is_assign_target(e, id) {
            return false;
        }
        let Some(vid) = e.value(id) else {
            return false;
        };
        let rep = e.value_find(vid);
        let Some(value) = extract_const(e, rep, id) else {
            return false;
        };
        // Promote the node to the pooled constant in its parent slot (WEP: The
        // Live ValueGraph). Idempotent: a promoted (orphaned) node has no parent
        // slot, so the retry reports no change and the worklist terminates.
        e.replace_expr_with_value(id, value)
    }
}

/// The constant [`Value`] for `rep` if its representative kind is a scalar
/// constant, using `at`'s NIR type for integer width. `None` for a non-constant
/// representative or when folding is disabled (no type table).
pub(super) fn extract_const(
    e: &mut Engine,
    rep: ValueId,
    at: ExprId,
) -> Option<crate::const_eval::Value> {
    if !matches!(
        e.value_kind(rep),
        ValueKind::Int(_) | ValueKind::Float(_) | ValueKind::Bool(_) | ValueKind::Char(_)
    ) {
        return None;
    }
    let vk = e.value_kind(rep).clone();
    let type_id = e.body.exprs[at].type_id;
    let prim = e
        .value_graph_type_table()
        .and_then(|tt| crate::const_eval::prim_of(type_id, tt));
    crate::nir_value_graph::value_kind_to_const(&vk, prim)
}

/// True when `expr`'s immediate parent is an `Assign` and `expr` is its target.
fn is_assign_target(e: &Engine, expr: ExprId) -> bool {
    let Some(NodeRef::Expr(parent)) = e.parent_of(NodeRef::Expr(expr)) else {
        return false;
    };
    matches!(
        &e.body.exprs[parent].kind,
        ExprKind::Assign { target, .. } if *target == expr
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirLocal};
    use crate::nir_arena::{BlockNode, Body, ExprNode, StmtKind, StmtNode};
    use crate::nir_engine::EngineBuffers;
    use crate::tir::TypeTable;
    use crate::token::Span;

    fn e(body: &mut Body, kind: ExprKind) -> ExprId {
        ei(body, kind, TypeTable::UNIT)
    }

    fn ei(body: &mut Body, kind: ExprKind, type_id: crate::tir::TypeId) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id,
            span: Span::default(),
        })
    }

    #[test]
    fn materializes_a_value_unioned_to_a_literal() {
        use crate::nir_arena::Operand;
        use crate::nir_value_graph::ValueKind;
        // { a + b; 5; } — union the sum's class with the constant 5, then the
        // extractor promotes the sum statement's operand to the pooled `5`.
        let mut body = Body::empty();
        let a = ei(
            &mut body,
            ExprKind::Local {
                index: 0,
                name: "a".into(),
            },
            TypeTable::I32,
        );
        let b = ei(
            &mut body,
            ExprKind::Local {
                index: 1,
                name: "b".into(),
            },
            TypeTable::I32,
        );
        let sum = ei(
            &mut body,
            ExprKind::Binary {
                left: a.into(),
                op: NirBinaryOp::Add,
                right: b.into(),
            },
            TypeTable::I32,
        );
        // The constant `5` is a pooled value, born as `Operand::Value`.
        let five_v = body.values.alloc_unshared(ValueKind::Int(5), TypeTable::I32);
        let s0 = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(sum.into()),
            span: Span::default(),
        });
        let s1 = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(Operand::Value(five_v)),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s0, s1],
            span: Span::default(),
        });

        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        let v_sum = eng.value(sum).unwrap();
        // The sum statement still holds the skeleton expression.
        assert!(matches!(
            eng.body.stmts[s0].kind,
            StmtKind::Expr(Operand::Expr(_))
        ));
        // Prove sum ≡ 5 and extract.
        eng.value_union(v_sum, five_v);
        eng.rebuild_value_congruence();
        let rule = ExtractLiteralRule;
        let rules: Vec<&dyn Rule> = vec![&rule];
        eng.run(&rules);
        // The sum statement's operand is now the pooled constant 5.
        let StmtKind::Expr(Operand::Value(v)) = eng.body.stmts[s0].kind else {
            panic!("sum statement operand was not promoted to a value");
        };
        assert!(matches!(eng.body.values.kind(v), ValueKind::Int(5)));
    }

    #[test]
    fn leaves_non_constant_values_untouched() {
        // { a + b; } with no union — nothing to materialize.
        let mut body = Body::empty();
        let a = e(
            &mut body,
            ExprKind::Local {
                index: 0,
                name: "a".into(),
            },
        );
        let b = e(
            &mut body,
            ExprKind::Local {
                index: 1,
                name: "b".into(),
            },
        );
        let sum = e(
            &mut body,
            ExprKind::Binary {
                left: a.into(),
                op: NirBinaryOp::Add,
                right: b.into(),
            },
        );
        let s0 = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(sum.into()),
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s0],
            span: Span::default(),
        });

        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        let rule = ExtractLiteralRule;
        let rules: Vec<&dyn Rule> = vec![&rule];
        let changed = eng.run(&rules);
        assert!(!changed);
        assert!(matches!(eng.body.exprs[sum].kind, ExprKind::Binary { .. }));
    }
}
