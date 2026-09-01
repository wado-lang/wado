//! Fuse a run of sibling `if K == x { … }` statements over one local into a
//! single `Match`, so `match_to_switch` can reach it and the dispatch stops at
//! the arm that fired.
//!
//! A compile-time-unrolled variadic `for` whose body guards on the loop index —
//! what every `ReflectStruct`-derived `Deserialize` does to route a field to its
//! slot — expands to a flat chain the arms never leave, so a struct pays one
//! comparison per declared field for *every* field on the wire: quadratic in the
//! declaration's width. The guards are mutually exclusive by construction, which
//! is what makes them one `Match`.
//!
//! Recognition reads the skeleton and the value pool, never the value graph. A
//! guard is a constant against a local either way, and the local is what the
//! write scan needs — so the shape answers the question with no analysis behind
//! it, in a body of any size.
//!
//! Every chain is worth fusing: the `Match` lowers to an early-exit `else if`,
//! which executes strictly fewer comparisons than the flat run whatever its
//! length. The threshold that does exist is `match_to_switch`'s, which trades
//! that cascade for one indirect branch and needs the arms to pay for it.

use crate::compiler_trace;
use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirLiteralPattern};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::nir_value_graph::{OpaqueSource, ValueKind};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

pub(super) struct IfChainToMatchRule<'t> {
    type_table: &'t TypeTable,
}

impl<'t> IfChainToMatchRule<'t> {
    pub(super) fn new(type_table: &'t TypeTable) -> Self {
        Self { type_table }
    }
}

/// One recognised `if K == x` statement.
struct Case {
    stmt: StmtId,
    then_block: BlockId,
    key: i128,
}

impl Rule for IfChainToMatchRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let stmts = engine.body.blocks[block].stmts.clone();
        let mut start = 0;
        while start < stmts.len() {
            let Some((local, _)) = split_guard(engine.body, self.type_table, stmts[start]) else {
                start += 1;
                continue;
            };
            // A local whose address escapes can be written by any call an arm
            // makes, so the constants would not stay mutually exclusive.
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
                if let Some(case) = case_at(engine.body, self.type_table, stmts[cursor], local) {
                    // A repeated key is a second arm the chain would still run
                    // and a `Match` would not; stop the run before it.
                    if !keys.insert(case.key) {
                        break;
                    }
                    hoisted.append(&mut pending);
                    cases.push(case);
                    cursor += 1;
                    end = cursor;
                    continue;
                }
                // The unrolled loop binds its index between the arms
                // (`let i = 3; if i == index { … }`), so the arms are adjacent
                // only up to those. A constant binding moves ahead of the whole
                // run: its value depends on nothing an arm writes, and being
                // immutable nothing can reassign it. A second binding of the
                // same local would — the unrolled copies shadow one name — so
                // that ends the run instead.
                if let Some(bound) = constant_let(engine.body, stmts[cursor])
                    && hoisted_locals.insert(bound)
                {
                    pending.push(stmts[cursor]);
                    cursor += 1;
                    continue;
                }
                break;
            }
            // One arm is not a chain — the `Match` would lower back to the very
            // branch it replaced.
            if cases.len() < 2 || writes_local(engine.body, &cases, local) {
                start += 1;
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

/// `stmt` as `if K == <local> { … }` for the run's local.
fn case_at(body: &Body, table: &TypeTable, stmt: StmtId, local: u32) -> Option<Case> {
    let (found, key) = split_guard(body, table, stmt)?;
    if found != local {
        return None;
    }
    let StmtKind::If { then_block, .. } = body.stmts[stmt].kind else {
        return None;
    };
    Some(Case {
        stmt,
        then_block,
        key,
    })
}

/// Split an `if <int const> == <local> { … }` statement (either operand order,
/// no `else`) into the local's index and the constant.
fn split_guard(body: &Body, table: &TypeTable, stmt: StmtId) -> Option<(u32, i128)> {
    let StmtKind::If {
        condition,
        else_block: None,
        ..
    } = body.stmts[stmt].kind
    else {
        return None;
    };
    let (lhs, rhs) = eq_operands(body, condition)?;
    match (
        (local_of(body, lhs), int_key(body, table, rhs)),
        (local_of(body, rhs), int_key(body, table, lhs)),
    ) {
        ((Some(local), Some(key)), _) | (_, (Some(local), Some(key))) => Some((local, key)),
        // Two constants fold away on their own; neither is not a dispatch.
        _ => None,
    }
}

/// The two sides of an `==`, whether the comparison sits in the skeleton or was
/// promoted into the value pool.
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

/// The integer an operand holds, if it is an integer constant. The literal
/// pattern is signed either way — `match_to_switch` reads both `I128` and
/// `U128`, so the arm key only has to round-trip.
fn int_key(body: &Body, table: &TypeTable, op: Operand) -> Option<i128> {
    let Operand::Value(v) = op else {
        return None;
    };
    let ValueKind::Int(bits, ty) = *body.values.kind(v) else {
        return None;
    };
    Some(if is_signed(table, ty) {
        i128::from(bits as i64)
    } else {
        i128::from(bits)
    })
}

fn is_signed(table: &TypeTable, ty: TypeId) -> bool {
    matches!(
        table.get(ty),
        ResolvedType::Primitive(
            PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64
        )
    )
}

/// The local an immutable `let` of a constant binds. A constant reads nothing,
/// so where it is evaluated cannot matter; immutability is what stops the run
/// from reassigning it.
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
    matches!(
        body.values.kind(v),
        ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::Const(..)
    )
    .then_some(local_index)
}

/// Whether any arm assigns `local`. A guard the arms can invalidate is not a
/// guard, and only an unwritten scrutinee makes the constants exclusive.
fn writes_local(body: &Body, cases: &[Case], local: u32) -> bool {
    cases
        .iter()
        .any(|c| node_writes_local(body, NodeRef::Block(c.then_block), local))
}

fn node_writes_local(body: &Body, node: NodeRef, local: u32) -> bool {
    match node {
        NodeRef::Expr(id) => {
            if let ExprKind::Assign { target, .. } = &body.exprs[id].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                && *index == local
            {
                return true;
            }
        }
        NodeRef::Stmt(id) => {
            if let StmtKind::Let { local_index, .. } = &body.stmts[id].kind
                && *local_index == local
            {
                return true;
            }
        }
        _ => {}
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    kids.into_iter().any(|c| node_writes_local(body, c, local))
}

/// Build `match local { K0 => { … }, …, _ => {} }` as one statement. The
/// trailing wildcard is what makes the arms exhaustive: the chain ran nothing
/// when no key matched.
fn build_match(engine: &mut Engine, local: u32, cases: &[Case]) -> StmtId {
    let span = engine.body.stmts[cases[0].stmt].span;
    let (name, local_type) = {
        let l = &engine.locals()[local as usize];
        (l.name.clone(), l.type_id)
    };
    let mut arms = Vec::with_capacity(cases.len() + 1);
    for case in cases {
        let pattern = engine.alloc_pat(PatKind::Literal(NirLiteralPattern::I128(case.key)), span);
        let body = engine.alloc_expr(ExprKind::Block(case.then_block), TypeTable::UNIT, span);
        arms.push(ArmData {
            pattern,
            guard: None,
            body: Operand::Expr(body),
            span,
        });
    }
    let default_block = engine.alloc_block(Vec::new(), span);
    let default_body = engine.alloc_expr(ExprKind::Block(default_block), TypeTable::UNIT, span);
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
