//! Fuse a run of sibling `if K == x { … }` statements over one local into a
//! single `Match`, so the dispatch stops at the arm that fired. A derived
//! `Deserialize` routes a field through such a run, unrolled one arm per
//! declared field and left by none of them.

use super::arena_query::local_written_by;
use crate::compiler_trace;
use crate::const_eval::is_signed_int;
use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirLiteralPattern};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::nir_value_graph::{OpaqueSource, ValueKind};
use crate::tir::{ResolvedType, TypeTable};
use crate::token::Span;

pub(super) struct IfChainToMatchRule<'t> {
    type_table: &'t TypeTable,
}

impl<'t> IfChainToMatchRule<'t> {
    pub(super) fn new(type_table: &'t TypeTable) -> Self {
        Self { type_table }
    }
}

/// One recognised `if K == x` statement, and where it sits in its block.
struct Case {
    at: usize,
    span: Span,
    then_block: BlockId,
    key: i128,
}

impl Rule for IfChainToMatchRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        // The clone is for walking while `engine` is borrowed mutably.
        let Some(first) = engine.body.blocks[block]
            .stmts
            .iter()
            .position(|&s| split_guard(engine.body, self.type_table, s).is_some())
        else {
            return false;
        };
        let stmts = engine.body.blocks[block].stmts.clone();
        let mut start = first;
        while start < stmts.len() {
            let Some((local, ..)) = split_guard(engine.body, self.type_table, stmts[start]) else {
                start += 1;
                continue;
            };
            // An escaped local is one any call an arm makes could write.
            if engine.body_address_taken().contains(&local) {
                start += 1;
                continue;
            }
            let mut cases = Vec::new();
            let mut hoisted = Vec::new();
            let mut pending = Vec::new();
            let mut keys = IndexSet::default();
            let mut hoisted_locals = IndexSet::default();
            // `end` is one past the last arm — the run's tail stays where it is.
            let mut end = start;
            let mut cursor = start;
            while cursor < stmts.len() {
                if let Some(case) =
                    case_at(engine.body, self.type_table, stmts[cursor], cursor, local)
                {
                    // A repeated key is an arm the chain runs and a `Match` would not.
                    if !keys.insert(case.key) {
                        break;
                    }
                    hoisted.append(&mut pending);
                    cases.push(case);
                    cursor += 1;
                    end = cursor;
                    continue;
                }
                // `let i = 3; if i == index { … }` — the unroll binds its index
                // between the arms. Hoisting one past a guard that reads it, or
                // past its own shadow, would not be sound.
                if let Some(bound) = constant_let(engine.body, stmts[cursor])
                    && bound != local
                    && hoisted_locals.insert(bound)
                {
                    pending.push(stmts[cursor]);
                    cursor += 1;
                    continue;
                }
                break;
            }
            // One arm is not a chain.
            if cases.len() < 2 {
                start += 1;
                continue;
            }
            // Every run holding that arm is doomed, so resume past it.
            if let Some(writer) = cases.iter().position(|c| {
                subtree_writes_local(engine.body, NodeRef::Block(c.then_block), local)
            }) {
                start = cases[writer].at + 1;
                continue;
            }
            compiler_trace!(
                "if_chain_to_match",
                "fuse {} `K == local {local}` arms in block {block:?}",
                cases.len()
            );
            let fused = build_match(engine, local, &cases);
            let mut new_stmts = Vec::with_capacity(stmts.len() + hoisted.len());
            new_stmts.extend_from_slice(&stmts[..start]);
            new_stmts.extend_from_slice(&hoisted);
            new_stmts.push(fused);
            new_stmts.extend_from_slice(&stmts[end..]);
            engine.set_block_stmts(block, new_stmts);
            return true;
        }
        false
    }
}

/// The block's `at`th statement as `if K == <local> { … }` for the run's local.
fn case_at(body: &Body, table: &TypeTable, stmt: StmtId, at: usize, local: u32) -> Option<Case> {
    let (found, key, then_block) = split_guard(body, table, stmt)?;
    (found == local).then(|| Case {
        at,
        span: body.stmts[stmt].span,
        then_block,
        key,
    })
}

/// Split an `if <int const> == <local> { … }` statement (either operand order,
/// no `else`) into the local's index, the constant, and the arm.
fn split_guard(body: &Body, table: &TypeTable, stmt: StmtId) -> Option<(u32, i128, BlockId)> {
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = body.stmts[stmt].kind
    else {
        return None;
    };
    let (lhs, rhs) = eq_operands(body, condition)?;
    match (
        (local_of(body, lhs), int_key(body, table, rhs)),
        (local_of(body, rhs), int_key(body, table, lhs)),
    ) {
        ((Some(local), Some(key)), _) | (_, (Some(local), Some(key))) => {
            Some((local, key, then_block))
        }
        // Two constants fold away on their own; neither is not a dispatch.
        _ => None,
    }
}

/// The two sides of an `==`, promoted into the value pool or not.
fn eq_operands(body: &Body, condition: Operand) -> Option<(Operand, Operand)> {
    match condition {
        Operand::Value(v) => match body.values.kind(v) {
            ValueKind::Binary {
                op: NirBinaryOp::Eq,
                lhs,
                rhs,
                ..
            } => Some((Operand::Value(*lhs), Operand::Value(*rhs))),
            _ => None,
        },
        Operand::Expr(e) => match &body.exprs[e].kind {
            ExprKind::Binary {
                left,
                op: NirBinaryOp::Eq,
                right,
            } => Some((*left, *right)),
            _ => None,
        },
    }
}

/// The local an operand reads, directly or through a pooled opaque.
fn local_of(body: &Body, op: Operand) -> Option<u32> {
    match op {
        Operand::Expr(e) => match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => Some(*index),
            _ => None,
        },
        Operand::Value(v) => match body.values.kind(v) {
            ValueKind::Opaque(opaque) => match body.values.opaque_source(*opaque)? {
                OpaqueSource::Local(local) => Some(local),
                OpaqueSource::Expr(_) => None,
            },
            _ => None,
        },
    }
}

/// An operand's integer constant, widened by its own signedness.
fn int_key(body: &Body, table: &TypeTable, op: Operand) -> Option<i128> {
    let Operand::Value(v) = op else {
        return None;
    };
    let ValueKind::Int(bits, ty) = *body.values.kind(v) else {
        return None;
    };
    let ResolvedType::Primitive(prim) = table.get(ty) else {
        return None;
    };
    Some(if is_signed_int(*prim) {
        i128::from(bits as i64)
    } else {
        i128::from(bits)
    })
}

/// The local an immutable `let` of a constant binds — hoistable, reading nothing.
fn constant_let(body: &Body, stmt: StmtId) -> Option<u32> {
    let StmtKind::Let {
        is_mut: false,
        value: Operand::Value(v),
        local_index,
        ..
    } = body.stmts[stmt].kind
    else {
        return None;
    };
    body.values.kind(v).is_constant().then_some(local_index)
}

/// The channel `local_written_by` omits cannot reach a scrutinee: a `&mut self`
/// receiver boxes its local, and `local_of` matches only a bare one.
fn subtree_writes_local(body: &Body, root: NodeRef, local: u32) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if local_written_by(body, node) == Some(local) {
            return true;
        }
        assert!(
            !binds_local(body, node, local),
            "local {local} is bound inside an arm that guards on it: \
             every binding site mints a fresh index, so a collision here is \
             broken local numbering rather than a scrutinee the arms rewrite"
        );
        body.for_each_child(node, |c| stack.push(c));
    }
    false
}

/// Whether `node` binds `local` — a `let` or a pattern binding.
fn binds_local(body: &Body, node: NodeRef, local: u32) -> bool {
    match node {
        NodeRef::Stmt(id) => {
            matches!(&body.stmts[id].kind, StmtKind::Let { local_index, .. } if *local_index == local)
        }
        NodeRef::Pat(id) => {
            matches!(&body.pats[id].kind, PatKind::Binding { local_index, .. } if *local_index == local)
        }
        NodeRef::Expr(_) | NodeRef::Block(_) => false,
    }
}

/// Build `match local { K0 => { … }, …, _ => {} }` as one statement. The chain
/// ran nothing when no key matched, so the wildcard arm is empty.
fn build_match(engine: &mut Engine, local: u32, cases: &[Case]) -> StmtId {
    let span = cases[0].span;
    let (name, local_type) = {
        let l = &engine.locals()[local as usize];
        (l.name.clone(), l.type_id)
    };
    let mut arms = Vec::with_capacity(cases.len() + 1);
    for case in cases {
        let pattern = engine.alloc_pat(PatKind::Literal(NirLiteralPattern::I128(case.key)), span);
        let body = engine.alloc_expr(
            ExprKind::plain_block(case.then_block, TypeTable::UNIT, "arm"),
            TypeTable::UNIT,
            span,
        );
        arms.push(ArmData {
            pattern,
            guard: None,
            body: Operand::Expr(body),
            span,
        });
    }
    let default_block = engine.alloc_block(Vec::new(), span);
    let default_body = engine.alloc_expr(
        ExprKind::plain_block(default_block, TypeTable::UNIT, "default"),
        TypeTable::UNIT,
        span,
    );
    arms.push(ArmData {
        pattern: engine.alloc_pat(PatKind::Wildcard, span),
        guard: None,
        body: Operand::Expr(default_body),
        span,
    });
    let scrutinee = engine.alloc_expr(ExprKind::Local { index: local, name }, local_type, span);
    let matched = engine.alloc_expr(
        ExprKind::Match {
            expr: Operand::Expr(scrutinee),
            arms,
        },
        TypeTable::UNIT,
        span,
    );
    engine.alloc_stmt(StmtKind::Expr(Operand::Expr(matched)), span)
}
