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
//! Exercised by unit tests until the first union-producing pure-value pass is
//! migrated onto the live graph and wires this in as the consumer.
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
        let Some(kind) = materialize_literal(e, rep, id) else {
            return false;
        };
        if same_literal(&e.body.exprs[id].kind, &kind) {
            return false;
        }
        e.replace_expr_kind(id, kind);
        true
    }
}

/// The literal `ExprKind` for `rep` if its representative kind is a constant,
/// using `at`'s NIR type for integer width / repr. Prefers an existing source
/// literal (keeping its `repr` / span); otherwise synthesizes one from the
/// value kind, byte-identically to niri's CTFE path. `None` for a non-literal
/// representative or when folding is disabled (no type table).
pub(super) fn materialize_literal(e: &mut Engine, rep: ValueId, at: ExprId) -> Option<ExprKind> {
    if !matches!(
        e.value_kind(rep),
        ValueKind::Int(_) | ValueKind::Float(_) | ValueKind::Bool(_) | ValueKind::Char(_)
    ) {
        return None;
    }
    if let Some(src) = e.literal_source(rep) {
        return Some(e.body.exprs[src].kind.clone());
    }
    let vk = e.value_kind(rep).clone();
    let type_id = e.body.exprs[at].type_id;
    let prim = e
        .value_graph_type_table()
        .and_then(|tt| crate::const_eval::prim_of(type_id, tt));
    let value = crate::nir_value_graph::value_kind_to_const(&vk, prim)?;
    Some(crate::const_eval::value_to_arena_kind(value))
}

/// True when two `ExprKind`s are the same literal (so re-materializing is a
/// no-op). Compares only the literal payload, not `repr` — a differing `repr`
/// for the same value is not worth re-churning.
fn same_literal(a: &ExprKind, b: &ExprKind) -> bool {
    match (a, b) {
        (ExprKind::IntLiteral { value: x, .. }, ExprKind::IntLiteral { value: y, .. }) => x == y,
        (ExprKind::FloatLiteral { value: x, .. }, ExprKind::FloatLiteral { value: y, .. }) => {
            x.to_bits() == y.to_bits()
        }
        (ExprKind::BoolLiteral(x), ExprKind::BoolLiteral(y)) => x == y,
        (ExprKind::CharLiteral(x), ExprKind::CharLiteral(y)) => x == y,
        _ => false,
    }
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
    use crate::nir_arena::{Body, BlockNode, ExprNode, StmtKind, StmtNode};
    use crate::nir_engine::EngineBuffers;
    use crate::tir::TypeTable;
    use crate::token::Span;

    fn e(body: &mut Body, kind: ExprKind) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id: TypeTable::UNIT,
            span: Span::default(),
        })
    }

    #[test]
    fn materializes_a_value_unioned_to_a_literal() {
        // { a + b; 5; } — union the sum's class with the literal 5, then the
        // extractor rewrites the sum expression into `5`.
        let mut body = Body::empty();
        let a = e(&mut body, ExprKind::Local { index: 0, name: "a".into() });
        let b = e(&mut body, ExprKind::Local { index: 1, name: "b".into() });
        let sum = e(&mut body, ExprKind::Binary { left: a, op: NirBinaryOp::Add, right: b });
        let five = e(
            &mut body,
            ExprKind::IntLiteral { value: 5, repr: "5".into() },
        );
        let s0 = body.stmts.push(StmtNode { kind: StmtKind::Expr(sum), span: Span::default() });
        let s1 = body.stmts.push(StmtNode { kind: StmtKind::Expr(five), span: Span::default() });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s0, s1],
            span: Span::default(),
        });

        let mut buf = EngineBuffers::default();
        let mut locals: Vec<NirLocal> = Vec::new();
        let mut eng = Engine::new(&mut body, &mut buf, &mut locals);
        let v_sum = eng.value(sum).unwrap();
        let v_five = eng.value(five).unwrap();
        // The sum is not a literal yet.
        assert!(!matches!(
            eng.body.exprs[sum].kind,
            ExprKind::IntLiteral { .. }
        ));
        // Prove sum ≡ 5 and extract.
        eng.value_union(v_sum, v_five);
        eng.rebuild_value_congruence();
        let rule = ExtractLiteralRule;
        let rules: Vec<&dyn Rule> = vec![&rule];
        eng.run(&rules);
        // The sum expression is now the literal 5.
        assert!(matches!(
            eng.body.exprs[sum].kind,
            ExprKind::IntLiteral { value: 5, .. }
        ));
    }

    #[test]
    fn leaves_non_constant_values_untouched() {
        // { a + b; } with no union — nothing to materialize.
        let mut body = Body::empty();
        let a = e(&mut body, ExprKind::Local { index: 0, name: "a".into() });
        let b = e(&mut body, ExprKind::Local { index: 1, name: "b".into() });
        let sum = e(&mut body, ExprKind::Binary { left: a, op: NirBinaryOp::Add, right: b });
        let s0 = body.stmts.push(StmtNode { kind: StmtKind::Expr(sum), span: Span::default() });
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
