//! Operand promotion: lift pure literal operands to `Operand::Value` (WEP: The
//! Live ValueGraph).
//!
//! [`promote_literals`] interns every literal expression into the function's
//! [`crate::nir_value_graph::ValuePool`] (`body.values`) and rewrites the operand
//! slots that referenced them to [`Operand::Value`], recording each value's
//! source type for the extractor. The literal `ExprNode`s become unreachable.
//!
//! Constants only for now: a non-constant pure value (`Binary`, `Cast`, …)
//! cannot be materialised from the graph without the skeleton computation behind
//! its `Opaque` operands, which the scheduling extractor provides — a later step.
//!
//! Not yet wired into the pipeline: promotion needs `body.values` to be the one
//! owned pool the value-graph passes also use, so a promoted `ValueId` resolves
//! (the per-pass builder currently makes a fresh pool). That unification lands
//! with build-once; this transform is verified standalone until then.

#![allow(dead_code)]

use crate::hashmap::IndexMap;
use crate::nir_arena::{Body, ExprId, ExprKind, Operand};
use crate::nir_value_graph::{ValueId, ValuePool};
use crate::tir::TypeId;

/// Promote every literal operand in `body` to `Operand::Value`, interning the
/// literal into `body.values` with its source type.
pub fn promote_literals(body: &mut Body) {
    let lits: Vec<(ExprId, ExprKind, TypeId)> = body
        .exprs
        .iter()
        .filter(|(_, n)| is_literal(&n.kind))
        .map(|(id, n)| (id, n.kind.clone(), n.type_id))
        .collect();
    let mut promote: IndexMap<ExprId, ValueId> = IndexMap::default();
    for (id, kind, ty) in lits {
        let v = intern_literal(&mut body.values, &kind);
        body.values.set_type(v, ty);
        promote.insert(id, v);
    }
    body.map_operands(|op| match op {
        Operand::Expr(e) => promote.get(&e).map_or(op, |&v| Operand::Value(v)),
        other => other,
    });
}

fn is_literal(k: &ExprKind) -> bool {
    matches!(
        k,
        ExprKind::IntLiteral { .. }
            | ExprKind::FloatLiteral { .. }
            | ExprKind::BoolLiteral(_)
            | ExprKind::CharLiteral(_)
            | ExprKind::StringLiteral(_)
            | ExprKind::Null
            | ExprKind::Unit
    )
}

fn intern_literal(pool: &mut ValuePool, k: &ExprKind) -> ValueId {
    match k {
        ExprKind::IntLiteral { value, .. } => pool.int(*value),
        ExprKind::FloatLiteral { value, .. } => pool.float(*value),
        ExprKind::BoolLiteral(b) => pool.bool(*b),
        ExprKind::CharLiteral(c) => pool.char(*c),
        ExprKind::StringLiteral(s) => pool.string(s.clone()),
        ExprKind::Null => pool.null(),
        ExprKind::Unit => pool.unit(),
        _ => unreachable!("not a literal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir_arena::{BlockNode, ExprNode, StmtKind, StmtNode};
    use crate::tir::TypeTable;
    use crate::token::Span;

    /// `let x: i32 = 7;` — the literal operand is lifted to `Operand::Value`,
    /// interned into `body.values` with its `i32` source type.
    #[test]
    fn promote_literals_lifts_literal_operands_with_types() {
        let mut body = Body::empty();
        let lit = body.exprs.push(ExprNode {
            kind: ExprKind::IntLiteral {
                value: 7,
                repr: "7".to_string(),
            },
            type_id: TypeTable::I32,
            span: Span::default(),
        });
        let s = body.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name: "x".to_string(),
                local_index: 0,
                is_mut: false,
                is_reactive: false,
                type_id: TypeTable::I32,
                value: Operand::Expr(lit),
                skip_value_copy: false,
            },
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s],
            span: Span::default(),
        });

        promote_literals(&mut body);

        let StmtKind::Let { value, .. } = &body.stmts[s].kind else {
            panic!("expected let");
        };
        let v = value.as_value().expect("literal operand promoted to a value");
        assert_eq!(body.values.kind(v), &crate::nir_value_graph::ValueKind::Int(7));
        assert_eq!(body.values.type_of(v), Some(TypeTable::I32));
    }

    /// A non-literal operand (a `Local` read) is left as `Operand::Expr`.
    #[test]
    fn promote_literals_leaves_non_literals() {
        let mut body = Body::empty();
        let local = body.exprs.push(ExprNode {
            kind: ExprKind::Local {
                index: 0,
                name: "y".to_string(),
            },
            type_id: TypeTable::I32,
            span: Span::default(),
        });
        let s = body.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name: "x".to_string(),
                local_index: 1,
                is_mut: false,
                is_reactive: false,
                type_id: TypeTable::I32,
                value: Operand::Expr(local),
                skip_value_copy: false,
            },
            span: Span::default(),
        });
        body.root = body.blocks.push(BlockNode {
            stmts: vec![s],
            span: Span::default(),
        });

        promote_literals(&mut body);

        let StmtKind::Let { value, .. } = &body.stmts[s].kind else {
            panic!("expected let");
        };
        assert_eq!(*value, Operand::Expr(local));
    }
}
