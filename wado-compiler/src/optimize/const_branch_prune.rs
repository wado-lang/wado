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
//! Runs on the worklist rewrite engine (see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: the
//! expression simplifications are an `apply_expr` peephole and the
//! statement-list flattening / dead-code elimination is an `apply_block`
//! rewrite. The break-target and mutation queries are read-only walks over the
//! arena (`arena_query::has_break_to`, [`node_mutates`]). All edits go through
//! the engine API (`become_expr`, `replace_expr_kind`, `set_block_stmts`,
//! `alloc_stmt`) so the parent map and use index stay coherent.
//!
//! Phase ordering: the rule depends on `ref_elim`/`copy_prop`/`sroa` having
//! removed the inliner's leading `let self = &recv` / scalar bindings, so the
//! `let index = arg` parameter copies become *leading* copies that
//! [`inline_labeled_block_copies`] can fold before the label-stripping rules
//! collapse the `__inline:` wrapper. Accordingly the in-loop run is the
//! *pre-inline* peephole session ([`super::peephole`], `PruneMode::Fixpoint`),
//! which sees each body after the previous iteration's cleanup passes — not the
//! post-inline session, where those bindings are still present. Two standalone
//! entry points keep their own engine session for the callers outside that
//! session: [`prune_constant_branches`] (`Fixpoint`, used by the
//! post-globalization cleanup) and [`prune_template_block_wrappers`]
//! (`PostFixpoint`, the final `__tmpl:` flatten after the fixpoint converges).

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;

use super::arena_query::has_break_to;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PruneMode {
    /// Run inside the optimizer fixpoint loop — preserve `__tmpl:` blocks
    /// for `tmpl_hoist`.
    Fixpoint,
    /// Final cleanup after the fixpoint converges.
    PostFixpoint,
}

/// Prune constant branches and simplify trivial blocks in all functions.
/// Standalone engine session for the post-globalization cleanup caller; the
/// in-loop run goes through [`super::peephole`] instead.
pub fn prune_constant_branches(project: &mut NirPackage) -> bool {
    run_rule(project, PruneMode::Fixpoint)
}

/// Final post-fixpoint pass that flattens any `__tmpl:` wrappers.
pub fn prune_template_block_wrappers(project: &mut NirPackage) -> bool {
    run_rule(project, PruneMode::PostFixpoint)
}

fn run_rule(project: &mut NirPackage, mode: PruneMode) -> bool {
    let rule = BranchPruneRule::new(mode);
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            let mut engine = Engine::new(body);
            changed |= engine.run(&[&rule]);
        }
    }
    changed
}

/// Engine rule for constant branch pruning. `mode` controls whether `__tmpl:`
/// labeled blocks are preserved (`Fixpoint`, for `tmpl_hoist`) or flattened
/// (`PostFixpoint`).
pub(super) struct BranchPruneRule {
    mode: PruneMode,
}

impl BranchPruneRule {
    pub(super) fn new(mode: PruneMode) -> Self {
        Self { mode }
    }
}

impl Rule for BranchPruneRule {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        if inline_labeled_block_copies(engine, id) {
            return true;
        }
        prune_expr_local(engine, id, self.mode)
    }

    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        eliminate_dead_stmts(engine, id, self.mode)
    }
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

// ---------------------------------------------------------------------------
// Expression-level simplification
// ---------------------------------------------------------------------------

/// One peephole simplification at `id`, applied through the engine edit API.
/// Returns `true` after the first rewrite; the engine re-tries the node, so a
/// sequence of collapses (e.g. C3 → `{ expr; }` → `expr`) still runs to a
/// local fixed point.
fn prune_expr_local(engine: &mut Engine, id: ExprId, mode: PruneMode) -> bool {
    // `{ expr; }` → `expr` (single-expression unlabeled block)
    if let ExprKind::Block(block) = &engine.body.exprs[id].kind {
        let block = *block;
        if engine.body.blocks[block].stmts.len() == 1
            && let StmtKind::Expr(inner) =
                engine.body.stmts[engine.body.blocks[block].stmts[0]].kind
        {
            engine.become_expr(id, inner);
            return true;
        }
    }

    // (C3) `label: { stmts...; break label: val; }` → `{ stmts...; val }`.
    if let ExprKind::LabeledBlock { label, block, .. } = &engine.body.exprs[id].kind {
        let label = label.clone();
        let block = *block;
        let go = (mode == PruneMode::PostFixpoint || label != "__tmpl")
            && engine.body.blocks[block].stmts.last().is_some_and(|&last| {
                matches!(
                    &engine.body.stmts[last].kind,
                    StmtKind::Break { label: Some(bl), value: Some(_) } if *bl == label
                )
            });
        if go {
            let last = *engine.body.blocks[block].stmts.last().unwrap();
            let StmtKind::Break {
                value: Some(brk_value),
                ..
            } = engine.body.stmts[last].kind
            else {
                unreachable!();
            };
            let n = engine.body.blocks[block].stmts.len();
            let prefix = &engine.body.blocks[block].stmts[..n - 1];
            let prefix_clean = !expr_has_break_to(engine.body, brk_value, &label)
                && !prefix
                    .iter()
                    .any(|&s| stmt_has_break_to(engine.body, s, &label));
            if prefix_clean {
                // Drop the trailing break; the broken value becomes the tail.
                let mut stmts = engine.body.blocks[block].stmts.clone();
                stmts.pop();
                if stmts.is_empty() {
                    engine.become_expr(id, brk_value);
                } else {
                    let tail_span = engine.body.exprs[brk_value].span;
                    let tail = engine.alloc_stmt(StmtKind::Expr(brk_value), tail_span);
                    stmts.push(tail);
                    engine.set_block_stmts(block, stmts);
                    engine.replace_expr_kind(id, ExprKind::Block(block));
                }
                return true;
            }
        }
    }

    // Single-`break` labeled block delivering a value.
    if let ExprKind::LabeledBlock { label, block, .. } = &engine.body.exprs[id].kind {
        let label = label.clone();
        let block = *block;
        let single_break = (mode == PruneMode::PostFixpoint || label != "__tmpl")
            && engine.body.blocks[block].stmts.len() == 1
            && matches!(
                &engine.body.stmts[engine.body.blocks[block].stmts[0]].kind,
                StmtKind::Break { label: Some(bl), .. } if *bl == label
            );
        if single_break {
            let s0 = engine.body.blocks[block].stmts[0];
            let StmtKind::Break { value, .. } = engine.body.stmts[s0].kind else {
                unreachable!();
            };
            // A value-less `label: { break label }` yields unit but carries no
            // value to promote; the original pass left it untouched, so do the
            // same (returning `false` keeps the engine from spinning on it).
            if let Some(inner) = value
                && !expr_has_break_to(engine.body, inner, &label)
            {
                engine.become_expr(id, inner);
                return true;
            }
        }
    }

    // `[label:] { }` → `()` (empty block, with or without label)
    let is_empty = match &engine.body.exprs[id].kind {
        ExprKind::Block(b) | ExprKind::LabeledBlock { block: b, .. } => {
            engine.body.blocks[*b].stmts.is_empty()
        }
        _ => false,
    };
    if is_empty {
        engine.replace_expr_kind(id, ExprKind::Unit);
        return true;
    }

    false
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

/// Rebuild `block`'s statement list, dropping code after a terminator and
/// flattening unused-label / void wrapper blocks into it. Inner blocks whose
/// statements are flattened in are emptied so that — unlike the original
/// tree-walk, which never revisited a consumed block — the engine re-seeing the
/// now-orphaned inner block cannot re-parent the shared statement ids back to
/// it.
fn eliminate_dead_stmts(engine: &mut Engine, block: BlockId, mode: PruneMode) -> bool {
    let stmts = engine.body.blocks[block].stmts.clone();
    let has_dead_after_terminator = stmts.iter().enumerate().any(|(i, &s)| {
        i + 1 < stmts.len()
            && matches!(
                &engine.body.stmts[s].kind,
                StmtKind::Break { .. } | StmtKind::Continue | StmtKind::Return { .. }
            )
    });
    if !has_dead_after_terminator && !stmts.iter().any(|&s| stmt_dominated(engine.body, s, mode)) {
        return false;
    }

    let mut new_stmts: Vec<StmtId> = Vec::with_capacity(stmts.len());
    let mut consumed_inner: Vec<BlockId> = Vec::new();
    let mut terminated = false;
    for stmt in stmts {
        if terminated {
            continue;
        }
        // Labeled-block statement with unused label → flatten its stmts in.
        if let StmtKind::LabeledBlock {
            label,
            block: inner,
        } = &engine.body.stmts[stmt].kind
        {
            let inner = *inner;
            if unused_label_flattenable(engine.body, label, inner, mode) {
                let inner_stmts = engine.body.blocks[inner].stmts.clone();
                if ends_with_terminator_stmt(engine.body, &inner_stmts) {
                    terminated = true;
                }
                new_stmts.extend(inner_stmts);
                consumed_inner.push(inner);
                continue;
            }
        }
        // (C2) Labeled block whose only `break LABEL` is a value-less tail.
        if is_tail_break_only_labeled_block(engine.body, stmt, mode) {
            let StmtKind::LabeledBlock { block: inner, .. } = &engine.body.stmts[stmt].kind else {
                unreachable!();
            };
            let inner = *inner;
            let mut inner_stmts = engine.body.blocks[inner].stmts.clone();
            inner_stmts.pop();
            if ends_with_terminator_stmt(engine.body, &inner_stmts) {
                terminated = true;
            }
            new_stmts.extend(inner_stmts);
            consumed_inner.push(inner);
            continue;
        }
        // Unit expression statement → drop.
        if let StmtKind::Expr(e) = &engine.body.stmts[stmt].kind
            && matches!(engine.body.exprs[*e].kind, ExprKind::Unit)
        {
            continue;
        }
        // Void block expression statement → flatten.
        if let StmtKind::Expr(e) = &engine.body.stmts[stmt].kind
            && let ExprKind::Block(inner) = &engine.body.exprs[*e].kind
        {
            let inner = *inner;
            let inner_stmts = engine.body.blocks[inner].stmts.clone();
            if ends_with_terminator_stmt(engine.body, &inner_stmts) {
                terminated = true;
            }
            new_stmts.extend(inner_stmts);
            consumed_inner.push(inner);
            continue;
        }
        // Unused-label labeled-block expression statement → flatten.
        if let StmtKind::Expr(e) = &engine.body.stmts[stmt].kind
            && let ExprKind::LabeledBlock {
                label,
                block: inner,
                ..
            } = &engine.body.exprs[*e].kind
            && unused_label_flattenable(engine.body, label, *inner, mode)
        {
            let inner = *inner;
            let inner_stmts = engine.body.blocks[inner].stmts.clone();
            if ends_with_terminator_stmt(engine.body, &inner_stmts) {
                terminated = true;
            }
            new_stmts.extend(inner_stmts);
            consumed_inner.push(inner);
            continue;
        }
        if matches!(
            &engine.body.stmts[stmt].kind,
            StmtKind::Break { .. } | StmtKind::Continue | StmtKind::Return { .. }
        ) {
            terminated = true;
        }
        new_stmts.push(stmt);
    }
    engine.set_block_stmts(block, new_stmts);
    // Empty each consumed inner block: its statements now live (and are
    // re-parented) in `block`, so the orphaned inner must not keep claiming
    // them if the engine pops it again.
    for inner in consumed_inner {
        engine.set_block_stmts(inner, Vec::new());
    }
    true
}

// ---------------------------------------------------------------------------
// Leading copy-binding inlining inside labeled blocks
// ---------------------------------------------------------------------------

fn inline_labeled_block_copies(engine: &mut Engine, id: ExprId) -> bool {
    let ExprKind::LabeledBlock { block, .. } = &engine.body.exprs[id].kind else {
        return false;
    };
    let block = *block;

    // Collect leading copy bindings: `let x = y` (y a Local, x immutable).
    let mut copies: Vec<(u32, u32, String)> = Vec::new();
    for &stmt in &engine.body.blocks[block].stmts {
        if let StmtKind::Let {
            local_index,
            is_mut,
            value,
            ..
        } = &engine.body.stmts[stmt].kind
            && !*is_mut
            && let ExprKind::Local { index, name } = &engine.body.exprs[*value].kind
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
    let remaining: Vec<StmtId> = engine.body.blocks[block].stmts[copy_count..].to_vec();
    let mut locals: IndexSet<u32> = IndexSet::default();
    for (target, source, _) in &copies {
        locals.insert(*target);
        locals.insert(*source);
    }
    if remaining
        .iter()
        .any(|&s| node_mutates(engine.body, NodeRef::Stmt(s), &locals))
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

    let mut stmts = engine.body.blocks[block].stmts.clone();
    stmts.drain(..copy_count);
    engine.set_block_stmts(block, stmts);
    substitute_locals(engine, block, &subs);
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
/// throughout the subtree of `block`, routing each rewrite through the engine
/// so the use index follows the renamed reads.
fn substitute_locals(engine: &mut Engine, block: BlockId, subs: &IndexMap<u32, (u32, String)>) {
    let mut targets = Vec::new();
    let mut stack = vec![NodeRef::Block(block)];
    while let Some(node) = stack.pop() {
        if let NodeRef::Expr(id) = node
            && let ExprKind::Local { index, .. } = &engine.body.exprs[id].kind
            && subs.contains_key(index)
        {
            targets.push(id);
        }
        engine.body.for_each_child(node, |c| stack.push(c));
    }
    for id in targets {
        let ExprKind::Local { index, .. } = &engine.body.exprs[id].kind else {
            continue;
        };
        if let Some((src_idx, src_name)) = subs.get(index) {
            let new_kind = ExprKind::Local {
                index: *src_idx,
                name: src_name.clone(),
            };
            engine.replace_expr_kind(id, new_kind);
        }
    }
}
