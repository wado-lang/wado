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

use super::arena_query::operand_read_locals;

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
    let mut census = Census {
        lone: &lone,
        path: Vec::new(),
        mentions: IndexMap::default(),
        assignments: Vec::new(),
    };
    census.walk(body, NodeRef::Block(body.root()));
    let mut promoted: Vec<(StmtId, u32, Operand)> = Vec::new();
    for (stmt, local, value, at) in &census.assignments {
        let [enclosing @ .., (block, index)] = at.as_slice() else {
            unreachable!("a statement sits in a block");
        };
        // Every mention follows the assignment in its block.
        let after = |path: &Vec<(BlockId, usize)>| {
            path.len() >= at.len()
                && path[..enclosing.len()] == *enclosing
                && path[enclosing.len()].0 == *block
                && path[enclosing.len()].1 >= *index
        };
        if !operand_read_locals(body, *value).contains(local)
            && census.mentions[local].iter().all(after)
        {
            promoted.push((*stmt, *local, *value));
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

/// Where each lone local is mentioned and assigned, as the statements
/// enclosing the mention: each a block and the statement's index in it. One
/// walk answers for every assignment, so a body of many costs one walk.
struct Census<'a> {
    lone: &'a IndexSet<u32>,
    path: Vec<(BlockId, usize)>,
    mentions: IndexMap<u32, Vec<Vec<(BlockId, usize)>>>,
    assignments: Vec<(StmtId, u32, Operand, Vec<(BlockId, usize)>)>,
}

impl Census<'_> {
    fn walk(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Block(b) = node {
            for (index, &stmt) in body.blocks[b].stmts.iter().enumerate() {
                self.path.push((b, index));
                if let Some((local, value)) = assignment_statement(body, stmt)
                    && self.lone.contains(&local)
                {
                    self.assignments
                        .push((stmt, local, value, self.path.clone()));
                }
                self.walk(body, NodeRef::Stmt(stmt));
                self.path.pop();
            }
            return;
        }
        let mut read = IndexSet::default();
        if let NodeRef::Expr(e) = node
            && let ExprKind::Local { index, .. } = &body.exprs[e].kind
        {
            read.insert(*index);
        }
        body.for_each_operand(node, |op| {
            if let Some(v) = op.as_value() {
                body.values.collect_opaque_locals(v, &mut read);
            }
        });
        for local in read {
            if self.lone.contains(&local) {
                self.mentions
                    .entry(local)
                    .or_default()
                    .push(self.path.clone());
            }
        }
        body.for_each_child(node, |c| self.walk(body, c));
    }
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
