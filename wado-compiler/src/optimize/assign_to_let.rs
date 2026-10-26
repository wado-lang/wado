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

/// Promote every such assignment in `body`. Answers whether any changed.
pub(super) fn promote_lone_assignments(
    body: &mut Body,
    locals: &[NirLocal],
    params: &[NirParam],
) -> bool {
    let census = Census::of(body);
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
            let Some((local, value)) = assignment_statement(body, stmt) else {
                continue;
            };
            let lone = census.assignments.get(&local) == Some(&1)
                && !census.bound.contains(&local)
                && params.iter().all(|p| p.local_index != local);
            // The target is a mention too, and the only one ahead of the rest.
            if lone && mentions(body, &stmts[at + 1..], local) + 1 == census.mentions[&local] {
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

/// What one walk of the body says about each local.
#[derive(Default)]
struct Census {
    /// Bound by a `let` or a pattern.
    bound: IndexSet<u32>,
    /// How many assignments write each local as a whole.
    assignments: IndexMap<u32, usize>,
    /// How many times each local is mentioned: by a node naming it, or by an
    /// operand slot whose promoted value reads it.
    mentions: IndexMap<u32, usize>,
}

impl Census {
    fn of(body: &Body) -> Self {
        let mut census = Census::default();
        body.for_each_reachable_node(|node| {
            match node {
                NodeRef::Stmt(s) => {
                    if let StmtKind::Let { local_index, .. } = &body.stmts[s].kind {
                        census.bound.insert(*local_index);
                    }
                }
                NodeRef::Pat(p) => {
                    if let PatKind::Binding { local_index, .. } = &body.pats[p].kind {
                        census.bound.insert(*local_index);
                    }
                }
                NodeRef::Expr(e) => {
                    if let ExprKind::Assign { target, .. } = &body.exprs[e].kind
                        && let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                    {
                        *census.assignments.entry(*index).or_default() += 1;
                    }
                }
                NodeRef::Block(_) => {}
            }
            count_mentions(body, node, &mut census.mentions);
        });
        census
    }
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

/// How many times `stmts` mention `local`, as [`Census::mentions`] counts.
fn mentions(body: &Body, stmts: &[StmtId], local: u32) -> usize {
    let mut counts = IndexMap::default();
    for &stmt in stmts {
        body.for_each_node_under(NodeRef::Stmt(stmt), |node| {
            count_mentions(body, node, &mut counts)
        });
    }
    counts.get(&local).copied().unwrap_or(0)
}

/// Count the locals `node` names, and those each of its promoted operands
/// reads, once per slot.
fn count_mentions(body: &Body, node: NodeRef, counts: &mut IndexMap<u32, usize>) {
    if let NodeRef::Expr(e) = node
        && let ExprKind::Local { index, .. } = &body.exprs[e].kind
    {
        *counts.entry(*index).or_default() += 1;
    }
    let mut leaves = IndexSet::default();
    body.for_each_operand(node, |op| {
        if let Operand::Value(v) = op
            && body.values.names_a_local(v)
        {
            leaves.clear();
            body.values.collect_opaque_locals(v, &mut leaves);
            for &local in &leaves {
                *counts.entry(local).or_default() += 1;
            }
        }
    });
}
