//! Constant branch pruning for Wado NIR
//!
//! Simplifies trivial blocks left over after other passes:
//! - `{ expr; }` → `expr`
//! - `label: { break label: val; }` → `val`
//! - `label: { let x = y; ... }` → substitute x with y in remaining stmts
//! - Empty blocks → `()`
//!
//! Constant-condition `if` folding is handled by `niri` via the `const_folding`
//! pass; this pass intentionally does *not* duplicate that logic.
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): a bottom-up simplifier,
//! so it walks the arena `Body` children-first (a default bottom-up visitor
//! walk) and mutates in place — block-stmt flattening rebuilds the
//! statement-id list, expression simplification rewrites node kinds, and the
//! break-target queries use the arena `arena_query::has_break_to`.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, StmtId, StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;

use super::arena_query::has_break_to;

#[derive(Clone, Copy, PartialEq, Eq)]
enum PruneMode {
    /// Run inside the optimizer fixpoint loop — preserve `__tmpl:` blocks
    /// for `tmpl_hoist`.
    Fixpoint,
    /// Final cleanup after the fixpoint converges.
    PostFixpoint,
}

/// Prune constant branches and simplify trivial blocks in all functions.
pub fn prune_constant_branches(project: &mut NirPackage) -> bool {
    run(project, PruneMode::Fixpoint)
}

/// Final post-fixpoint pass that flattens any `__tmpl:` wrappers.
pub fn prune_template_block_wrappers(project: &mut NirPackage) -> bool {
    run(project, PruneMode::PostFixpoint)
}

fn run(project: &mut NirPackage, mode: PruneMode) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            let root = body.root;
            changed |= prune_block(body, root, mode);
        }
    }
    changed
}

fn block_has_break_to(body: &Body, block: BlockId, label: &str) -> bool {
    has_break_to(body, NodeRef::Block(block), label)
}

fn stmt_has_break_to(body: &Body, stmt: StmtId, label: &str) -> bool {
    has_break_to(body, NodeRef::Stmt(stmt), label)
}

fn expr_has_break_to(body: &Body, expr: ExprId, label: &str) -> bool {
    has_break_to(body, NodeRef::Expr(expr), label)
}

/// Move `src`'s node content into `id` (the arena form of `*expr = src_expr`);
/// `src` is left as a dead `Unit`.
fn become_expr(body: &mut Body, id: ExprId, src: ExprId) {
    if id == src {
        return;
    }
    let ty = body.exprs[src].type_id;
    let span = body.exprs[src].span;
    let node = std::mem::replace(
        &mut body.exprs[src],
        ExprNode {
            kind: ExprKind::Unit,
            type_id: ty,
            span,
        },
    );
    body.exprs[id] = node;
}

// ---------------------------------------------------------------------------
// Bottom-up traversal
// ---------------------------------------------------------------------------

fn prune_block(body: &mut Body, block: BlockId, mode: PruneMode) -> bool {
    // Bottom-up: prune each statement's children first.
    let stmts = body.blocks[block].stmts.clone();
    let mut changed = false;
    for s in stmts {
        changed |= prune_stmt(body, s, mode);
    }
    changed |= eliminate_dead_stmts(body, block, mode);
    changed
}

fn prune_stmt(body: &mut Body, stmt: StmtId, mode: PruneMode) -> bool {
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
    let mut changed = false;
    for c in kids {
        changed |= prune_node(body, c, mode);
    }
    changed
}

fn prune_node(body: &mut Body, node: NodeRef, mode: PruneMode) -> bool {
    match node {
        NodeRef::Expr(id) => prune_expr(body, id, mode),
        NodeRef::Block(b) => prune_block(body, b, mode),
        NodeRef::Stmt(s) => prune_stmt(body, s, mode),
        NodeRef::Pat(_) => {
            let mut kids = Vec::new();
            body.for_each_child(node, |c| kids.push(c));
            let mut changed = false;
            for c in kids {
                changed |= prune_node(body, c, mode);
            }
            changed
        }
    }
}

fn prune_expr(body: &mut Body, id: ExprId, mode: PruneMode) -> bool {
    // Bottom-up: walk children first.
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
    let mut changed = false;
    for c in kids {
        changed |= prune_node(body, c, mode);
    }
    changed |= inline_labeled_block_copies(body, id);
    changed |= prune_expr_local(body, id, mode);
    changed
}

// ---------------------------------------------------------------------------
// Expression-level simplification
// ---------------------------------------------------------------------------

fn prune_expr_local(body: &mut Body, id: ExprId, mode: PruneMode) -> bool {
    let mut changed = false;

    // `{ expr; }` → `expr` (single-expression unlabeled block)
    if let ExprKind::Block(block) = &body.exprs[id].kind {
        let block = *block;
        if body.blocks[block].stmts.len() == 1
            && let StmtKind::Expr(inner) = body.stmts[body.blocks[block].stmts[0]].kind
        {
            become_expr(body, id, inner);
            changed = true;
        }
    }

    // (C3) `label: { stmts...; break label: val; }` → `{ stmts...; val }`.
    if let ExprKind::LabeledBlock { label, block, .. } = &body.exprs[id].kind {
        let label = label.clone();
        let block = *block;
        let go = (mode == PruneMode::PostFixpoint || label != "__tmpl")
            && body.blocks[block].stmts.last().is_some_and(|&last| {
                matches!(
                    &body.stmts[last].kind,
                    StmtKind::Break { label: Some(bl), value: Some(_) } if *bl == label
                )
            });
        if go {
            let last = *body.blocks[block].stmts.last().unwrap();
            let StmtKind::Break {
                value: Some(brk_value),
                ..
            } = body.stmts[last].kind
            else {
                unreachable!();
            };
            let prefix = &body.blocks[block].stmts[..body.blocks[block].stmts.len() - 1];
            let prefix_clean = !expr_has_break_to(body, brk_value, &label)
                && !prefix.iter().any(|&s| stmt_has_break_to(body, s, &label));
            if prefix_clean {
                // Drop the trailing break; the broken value becomes the tail.
                body.blocks[block].stmts.pop();
                if body.blocks[block].stmts.is_empty() {
                    become_expr(body, id, brk_value);
                } else {
                    let tail_span = body.exprs[brk_value].span;
                    let tail = body.stmts.push(StmtNode {
                        kind: StmtKind::Expr(brk_value),
                        span: tail_span,
                    });
                    body.blocks[block].stmts.push(tail);
                    body.exprs[id].kind = ExprKind::Block(block);
                }
                changed = true;
            }
        }
    }

    // Single-`break` labeled block (covers the `value: None` case too).
    if let ExprKind::LabeledBlock { label, block, .. } = &body.exprs[id].kind {
        let label = label.clone();
        let block = *block;
        let single_break = (mode == PruneMode::PostFixpoint || label != "__tmpl")
            && body.blocks[block].stmts.len() == 1
            && matches!(
                &body.stmts[body.blocks[block].stmts[0]].kind,
                StmtKind::Break { label: Some(bl), .. } if *bl == label
            );
        if single_break {
            let s0 = body.blocks[block].stmts[0];
            let StmtKind::Break { value, .. } = body.stmts[s0].kind else {
                unreachable!();
            };
            let value_ok = value.is_none_or(|v| !expr_has_break_to(body, v, &label));
            if value_ok {
                if let Some(inner) = value {
                    become_expr(body, id, inner);
                }
                changed = true;
            }
        }
    }

    // `[label:] { }` → `()` (empty block, with or without label)
    let is_empty = match &body.exprs[id].kind {
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            body.blocks[*b].stmts.is_empty()
        }
        _ => false,
    };
    if is_empty {
        body.exprs[id].kind = ExprKind::Unit;
        changed = true;
    }

    changed
}

// ---------------------------------------------------------------------------
// Statement-list dead-code elimination / flattening
// ---------------------------------------------------------------------------

fn is_tail_break_only_labeled_block(body: &Body, stmt: StmtId, mode: PruneMode) -> bool {
    let StmtKind::LabeledBlock {
        label,
        block: inner,
    } = &body.stmts[stmt].kind
    else {
        return false;
    };
    let inner = *inner;
    if mode == PruneMode::Fixpoint && label == "__tmpl" {
        return false;
    }
    let Some(&last) = body.blocks[inner].stmts.last() else {
        return false;
    };
    let StmtKind::Break {
        label: Some(brk_label),
        value: None,
    } = &body.stmts[last].kind
    else {
        return false;
    };
    if brk_label != label {
        return false;
    }
    let label = label.clone();
    let n = body.blocks[inner].stmts.len();
    !body.blocks[inner].stmts[..n - 1]
        .iter()
        .any(|&s| stmt_has_break_to(body, s, &label))
}

fn ends_with_terminator_stmt(body: &Body, stmts: &[StmtId]) -> bool {
    matches!(
        stmts.last().map(|&s| &body.stmts[s].kind),
        Some(StmtKind::Break { .. } | StmtKind::Continue | StmtKind::Return { .. })
    )
}

fn unused_label_flattenable(body: &Body, label: &str, inner: BlockId, mode: PruneMode) -> bool {
    (mode == PruneMode::PostFixpoint || label != "__tmpl")
        && (body.blocks[inner].stmts.is_empty() || !block_has_break_to(body, inner, label))
}

fn stmt_dominated(body: &Body, stmt: StmtId, mode: PruneMode) -> bool {
    let base = match &body.stmts[stmt].kind {
        StmtKind::LabeledBlock { label, block } => {
            unused_label_flattenable(body, label, *block, mode)
        }
        StmtKind::Expr(e) => match &body.exprs[*e].kind {
            ExprKind::Unit | ExprKind::Block(_) => true,
            ExprKind::LabeledBlock { label, block, .. } => {
                unused_label_flattenable(body, label, *block, mode)
            }
            _ => false,
        },
        _ => false,
    };
    base || is_tail_break_only_labeled_block(body, stmt, mode)
}

fn eliminate_dead_stmts(body: &mut Body, block: BlockId, mode: PruneMode) -> bool {
    let stmts = body.blocks[block].stmts.clone();
    let has_dead_after_terminator = stmts.iter().enumerate().any(|(i, &s)| {
        i + 1 < stmts.len()
            && matches!(
                &body.stmts[s].kind,
                StmtKind::Break { .. } | StmtKind::Continue | StmtKind::Return { .. }
            )
    });
    if !has_dead_after_terminator && !stmts.iter().any(|&s| stmt_dominated(body, s, mode)) {
        return false;
    }

    let mut new_stmts: Vec<StmtId> = Vec::with_capacity(stmts.len());
    let mut terminated = false;
    for stmt in stmts {
        if terminated {
            continue;
        }
        // Labeled-block statement with unused label → flatten its stmts in.
        if let StmtKind::LabeledBlock {
            label,
            block: inner,
        } = &body.stmts[stmt].kind
        {
            let inner = *inner;
            if unused_label_flattenable(body, label, inner, mode) {
                let inner_stmts = body.blocks[inner].stmts.clone();
                if ends_with_terminator_stmt(body, &inner_stmts) {
                    terminated = true;
                }
                new_stmts.extend(inner_stmts);
                continue;
            }
        }
        // (C2) Labeled block whose only `break LABEL` is a value-less tail.
        if is_tail_break_only_labeled_block(body, stmt, mode) {
            let StmtKind::LabeledBlock { block: inner, .. } = &body.stmts[stmt].kind else {
                unreachable!();
            };
            let inner = *inner;
            let mut inner_stmts = body.blocks[inner].stmts.clone();
            inner_stmts.pop();
            if ends_with_terminator_stmt(body, &inner_stmts) {
                terminated = true;
            }
            new_stmts.extend(inner_stmts);
            continue;
        }
        // Unit expression statement → drop.
        if let StmtKind::Expr(e) = &body.stmts[stmt].kind
            && matches!(body.exprs[*e].kind, ExprKind::Unit)
        {
            continue;
        }
        // Void block expression statement → flatten.
        if let StmtKind::Expr(e) = &body.stmts[stmt].kind
            && let ExprKind::Block(inner) = &body.exprs[*e].kind
        {
            let inner = *inner;
            let inner_stmts = body.blocks[inner].stmts.clone();
            if ends_with_terminator_stmt(body, &inner_stmts) {
                terminated = true;
            }
            new_stmts.extend(inner_stmts);
            continue;
        }
        // Unused-label labeled-block expression statement → flatten.
        if let StmtKind::Expr(e) = &body.stmts[stmt].kind
            && let ExprKind::LabeledBlock {
                label,
                block: inner,
                ..
            } = &body.exprs[*e].kind
            && unused_label_flattenable(body, label, *inner, mode)
        {
            let inner = *inner;
            let inner_stmts = body.blocks[inner].stmts.clone();
            if ends_with_terminator_stmt(body, &inner_stmts) {
                terminated = true;
            }
            new_stmts.extend(inner_stmts);
            continue;
        }
        if matches!(
            &body.stmts[stmt].kind,
            StmtKind::Break { .. } | StmtKind::Continue | StmtKind::Return { .. }
        ) {
            terminated = true;
        }
        new_stmts.push(stmt);
    }
    body.blocks[block].stmts = new_stmts;
    true
}

// ---------------------------------------------------------------------------
// Leading copy-binding inlining inside labeled blocks
// ---------------------------------------------------------------------------

fn inline_labeled_block_copies(body: &mut Body, id: ExprId) -> bool {
    let ExprKind::LabeledBlock { block, .. } = &body.exprs[id].kind else {
        return false;
    };
    let block = *block;

    // Collect leading copy bindings: `let x = y` (y a Local, x immutable).
    let mut copies: Vec<(u32, u32, String)> = Vec::new();
    for &stmt in &body.blocks[block].stmts {
        if let StmtKind::Let {
            local_index,
            is_mut,
            value,
            ..
        } = &body.stmts[stmt].kind
            && !*is_mut
            && let ExprKind::Local { index, name } = &body.exprs[*value].kind
        {
            copies.push((*local_index, *index, name.clone()));
        } else {
            break;
        }
    }
    if copies.is_empty() {
        return false;
    }

    // Verify safety: neither target nor source is mutated in the remaining stmts.
    let copy_count = copies.len();
    let remaining: Vec<StmtId> = body.blocks[block].stmts[copy_count..].to_vec();
    let mut locals: IndexSet<u32> = IndexSet::default();
    for (target, source, _) in &copies {
        locals.insert(*target);
        locals.insert(*source);
    }
    if remaining
        .iter()
        .any(|&s| node_mutates(body, NodeRef::Stmt(s), &locals))
    {
        return false;
    }

    // Build substitution map with transitive resolution.
    let mut subs: IndexMap<u32, (u32, String)> = IndexMap::default();
    for (target, source, source_name) in copies {
        let (final_source, final_name) = if let Some((resolved, resolved_name)) = subs.get(&source)
        {
            (*resolved, resolved_name.clone())
        } else {
            (source, source_name)
        };
        subs.insert(target, (final_source, final_name));
    }

    body.blocks[block].stmts.drain(..copy_count);
    substitute_locals(body, block, &subs);
    true
}

/// Whether any local in `locals` is assigned or mutably referenced within the
/// subtree at `node`. Mirrors the tree `MutationChecker`.
fn node_mutates(body: &Body, node: NodeRef, locals: &IndexSet<u32>) -> bool {
    if let NodeRef::Expr(id) = node {
        match &body.exprs[id].kind {
            ExprKind::Assign { target, .. } => {
                if let ExprKind::Local { index, .. } = &body.exprs[*target].kind
                    && locals.contains(index)
                {
                    return true;
                }
                if let ExprKind::FieldAccess { expr: inner, .. } = &body.exprs[*target].kind
                    && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
                    && locals.contains(index)
                {
                    return true;
                }
            }
            ExprKind::Unary {
                op: crate::nir::NirUnaryOp::MutRef,
                expr: inner,
            } => {
                if let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
                    && locals.contains(index)
                {
                    return true;
                }
            }
            ExprKind::MethodCall { receiver, .. } => {
                if let ExprKind::Local { index, .. } = &body.exprs[*receiver].kind
                    && locals.contains(index)
                {
                    return true;
                }
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                        && locals.contains(index)
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    let mut found = false;
    body.for_each_child(node, |c| {
        if !found {
            found = node_mutates(body, c, locals);
        }
    });
    found
}

/// Replace `Local { index: target }` with `Local { index: source, … }`
/// throughout the subtree of `block`.
fn substitute_locals(body: &mut Body, block: BlockId, subs: &IndexMap<u32, (u32, String)>) {
    let mut targets = Vec::new();
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Local { index, .. } = &body.exprs[id].kind
            && subs.contains_key(index)
        {
            targets.push(id);
        }
        body.for_each_child(node, |c| stack.push(c));
    }
    for id in targets {
        if let ExprKind::Local { index, name } = &mut body.exprs[id].kind
            && let Some((src_idx, src_name)) = subs.get(index)
        {
            *index = *src_idx;
            name.clone_from(src_name);
        }
    }
}
