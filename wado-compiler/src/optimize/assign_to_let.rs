//! Turn the one assignment of a local no `let` binds into the `let` binding
//! it: `x = v; …x…` becomes `let x = v; …x…` where every mention of `x`
//! follows that statement in its block. A contract check binds a call's earlier
//! arguments by assignment, where the call needs them in place
//! (`lower/contract_checks.rs`), and inlining the call then leaves such a
//! statement in the caller. Every pass that reads a local's definition off its
//! `let` (`sroa` above all) passes the assigned local by, so a struct argument
//! of a checked call stayed a heap object on every call.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirLocal, NirParam};
use crate::nir_arena::{BlockId, Body, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind};

use super::arena_query::{mentions_local_except, operand_read_locals, stmt_mentions_local};

/// Promote every such assignment in `body`. Answers whether any changed.
pub(super) fn promote_lone_assignments(
    body: &mut Body,
    locals: &[NirLocal],
    params: &[NirParam],
) -> bool {
    let lone = lone_assignees(body, params);
    if lone.is_empty() {
        return false;
    }
    let mut blocks: Vec<BlockId> = Vec::new();
    body.for_each_reachable_node(|node| {
        if let NodeRef::Block(b) = node {
            blocks.push(b);
        }
    });
    let mut promoted: Vec<(StmtId, u32, Operand)> = Vec::new();
    for block in blocks {
        let stmts = &body.blocks[block].stmts;
        for (at, &stmt) in stmts.iter().enumerate() {
            if let Some((local, value)) = assignment_statement(body, stmt)
                && lone.contains(&local)
                && !operand_read_locals(body, value).contains(&local)
                && !stmts[..at]
                    .iter()
                    .any(|&s| stmt_mentions_local(body, s, local))
                && !mentions_local_except(
                    body,
                    NodeRef::Block(body.root()),
                    &[NodeRef::Block(block)],
                    local,
                )
            {
                promoted.push((stmt, local, value));
            }
        }
    }
    for &(stmt, local, value) in &promoted {
        let declared = &locals[local as usize];
        body.stmts[stmt].kind = StmtKind::Let {
            name: declared.name.clone(),
            local_index: local,
            is_mut: declared.is_mut,
            type_id: declared.type_id,
            value,
            skip_value_copy: false,
        };
    }
    !promoted.is_empty()
}

/// The locals one assignment writes as a whole and nothing binds: no `let`,
/// pattern or parameter.
fn lone_assignees(body: &Body, params: &[NirParam]) -> IndexSet<u32> {
    let mut bound: IndexSet<u32> = params.iter().map(|p| p.local_index).collect();
    let mut assignments: IndexMap<u32, usize> = IndexMap::default();
    body.for_each_reachable_node(|node| match node {
        NodeRef::Stmt(s) => {
            if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind {
                bound.insert(*local_index);
            }
        }
        NodeRef::Pat(p) => {
            if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                bound.insert(*local_index);
            }
        }
        NodeRef::Expr(e) => {
            if let ExprKind::Assign { target, .. } = &body.exprs[e].kind
                && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
            {
                *assignments.entry(*index).or_default() += 1;
            }
        }
        NodeRef::Block(_) => {}
    });
    assignments
        .into_iter()
        .filter(|&(local, count)| count == 1 && !bound.contains(&local))
        .map(|(local, _)| local)
        .collect()
}

/// The local and value of a statement assigning a local as a whole, `x = v;`.
fn assignment_statement(body: &Body, stmt: StmtId) -> Option<(u32, Operand)> {
    let StmtKind::Expr(Operand::Expr(e)) = body.stmts[stmt].kind else {
        return None;
    };
    let ExprKind::Assign { target, value } = body.exprs[e].kind else {
        return None;
    };
    match body.exprs[target].kind {
        ExprKind::Local { index, .. } => Some((index, value)),
        _ => None,
    }
}
