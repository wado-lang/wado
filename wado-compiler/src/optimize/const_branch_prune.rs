//! Constant branch pruning for Wado NIR
//!
//! Simplifies trivial blocks left over after other passes:
//! - `{ expr; }` → `expr`
//! - `label: { break label: val; }` → `val`
//! - Empty blocks → `()`
//!
//! Constant-condition `if` folding is handled by `niri` via the `const_folding`
//! pass; this pass intentionally does *not* duplicate that logic. Copy
//! propagation (including the in-block copies of loop-mutated locals the
//! inliner leaves behind) is `copy_prop`'s job — this pass keys only on block
//! and control-flow *structure*, never on labels or variable names.
//!
//! Runs on the worklist rewrite engine (see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: the
//! expression simplifications are an `apply_expr` peephole and the
//! statement-list flattening / dead-code elimination is an `apply_block`
//! rewrite. The break-target queries are read-only walks over the arena
//! (`arena_query::has_break_to`). All edits go through the engine API
//! (`become_expr`, `replace_expr_kind`, `set_block_stmts`, `alloc_stmt`) so the
//! parent map and use index stay coherent.
//!
//! The in-loop run rides the unified [`super::peephole`] session
//! (`PruneMode::Fixpoint`). Two standalone entry points keep their own engine
//! session for the callers outside that session: [`prune_constant_branches`]
//! (`Fixpoint`, used by the post-globalization cleanup) and
//! [`prune_template_block_wrappers`] (`PostFixpoint`, the final `__tmpl:`
//! flatten after the fixpoint converges).

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
            // value to promote, so leave it untouched (return `false` so the
            // engine does not spin on it).
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
/// flattening unused-label / void wrapper blocks into it. A flattened-in inner
/// block is emptied: the engine may later pop that now-orphaned block, and
/// emptying it prevents re-parenting the (now shared) statement ids back to it.
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
