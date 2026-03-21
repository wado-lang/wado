//! Constant branch pruning for Wado TIR
//!
//! Eliminates branches with known boolean conditions and simplifies trivial blocks:
//! - `if true { A } else { B }` → `A`
//! - `if false { A } else { B }` → `B`
//! - `{ expr; }` → `expr`
//! - `label: { break label: val; }` → `val`
//! - Empty blocks → `()`

use crate::project::Project;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind};

use super::visitor::{
    TirVisitor, block_has_break_to, expr_has_break_to, visit_project_functions, walk_block,
    walk_expr,
};

/// Prune constant branches and simplify trivial blocks in all functions.
pub fn prune_constant_branches(project: &mut Project) -> bool {
    let mut visitor = BranchPruner;
    visit_project_functions(project, &mut visitor)
}

struct BranchPruner;

impl TirVisitor for BranchPruner {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: walk children first
        let mut changed = walk_expr(self, expr);
        // Then prune this expression
        changed |= prune_expr(expr);
        changed
    }

    fn visit_block(&mut self, block: &mut TirBlock) -> bool {
        // Bottom-up: walk stmts first
        let mut changed = walk_block(self, block);
        // Then eliminate dead stmts
        changed |= eliminate_dead_stmts(block);
        changed
    }
}

/// Prune constant conditions and simplify trivial blocks at the expression level.
fn prune_expr(expr: &mut TirExpr) -> bool {
    let mut changed = false;

    // Prune expression-level `if` with constant boolean condition
    if let TirExprKind::If { condition, .. } = &expr.kind
        && let TirExprKind::BoolLiteral(value) = condition.kind
    {
        let TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } = std::mem::replace(&mut expr.kind, TirExprKind::Unit)
        else {
            unreachable!();
        };
        if value {
            expr.kind = TirExprKind::Block(then_branch);
        } else if let Some(else_blk) = else_branch {
            expr.kind = TirExprKind::Block(else_blk);
        }
        // false without else: type is Unit, TirExprKind::Unit is already set
        changed = true;
    }

    // Simplify `{ expr; }` → `expr` (single-expression unlabeled block)
    if let TirExprKind::Block(block) = &expr.kind
        && block.stmts.len() == 1
        && let TirStmtKind::Expr(_) = &block.stmts[0].kind
    {
        let TirExprKind::Block(block) = std::mem::replace(&mut expr.kind, TirExprKind::Unit) else {
            unreachable!();
        };
        let mut stmts = block.stmts;
        let TirStmtKind::Expr(inner) = stmts.remove(0).kind else {
            unreachable!();
        };
        *expr = inner;
        changed = true;
    }

    // Simplify `label: { break label: val; }` → `val`
    if let TirExprKind::LabeledBlock { label, block, .. } = &expr.kind
        && block.stmts.len() == 1
        && let TirStmtKind::Break {
            label: Some(brk_label),
            value: brk_value,
        } = &block.stmts[0].kind
        && brk_label == label
        // Only simplify if the break value itself doesn't contain breaks
        // to the same label (e.g., from try-op error paths in nested expressions).
        && !brk_value.as_ref().is_some_and(|v| expr_has_break_to(label, v))
    {
        let TirExprKind::LabeledBlock { block, .. } =
            std::mem::replace(&mut expr.kind, TirExprKind::Unit)
        else {
            unreachable!();
        };
        let mut stmts = block.stmts;
        let TirStmtKind::Break { value, .. } = stmts.remove(0).kind else {
            unreachable!();
        };
        if let Some(inner) = value {
            *expr = inner;
        }
        // else: break without value → Unit is already set
        changed = true;
    }

    // Simplify `[label:] { }` → `()` (empty block, with or without label)
    if matches!(&expr.kind, TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } if b.stmts.is_empty())
    {
        expr.kind = TirExprKind::Unit;
        changed = true;
    }

    changed
}

/// Eliminate dead statements from a block:
/// - `if true { A } [else { B }]` → inline A's statements
/// - `if false { A }` → remove
/// - `if false { A } else { B }` → inline B's statements
/// - `label: { }` (empty labeled block) → remove
/// - `label: { stmts }` (unused label) → flatten stmts into parent
fn eliminate_dead_stmts(block: &mut TirBlock) -> bool {
    let dominated = |s: &TirStmt| {
        matches!(
            &s.kind,
            TirStmtKind::If { condition, .. }
                if matches!(condition.kind, TirExprKind::BoolLiteral(_))
        ) || matches!(
            &s.kind,
            TirStmtKind::LabeledBlock { label, block }
                if block.stmts.is_empty() || !block_has_break_to(label, block)
        ) || matches!(
            &s.kind,
            TirStmtKind::Expr(e) if matches!(e.kind, TirExprKind::Unit | TirExprKind::Block(_))
        ) || matches!(
            &s.kind,
            TirStmtKind::Expr(e) if matches!(&e.kind, TirExprKind::LabeledBlock { label, block, .. } if !block_has_break_to(label, block))
        )
    };
    if !block.stmts.iter().any(dominated) {
        return false;
    }

    let old_stmts = std::mem::take(&mut block.stmts);
    for stmt in old_stmts {
        // Constant `if` → inline taken branch or drop
        if let TirStmtKind::If { ref condition, .. } = stmt.kind
            && let TirExprKind::BoolLiteral(value) = condition.kind
        {
            let TirStmtKind::If {
                then_block,
                else_block,
                ..
            } = stmt.kind
            else {
                unreachable!();
            };
            if value {
                block.stmts.extend(then_block.stmts);
            } else if let Some(else_blk) = else_block {
                block.stmts.extend(else_blk.stmts);
            }
            continue;
        }
        // Labeled block with unused label → flatten stmts into parent
        if let TirStmtKind::LabeledBlock {
            ref label,
            block: ref inner,
        } = stmt.kind
            && !block_has_break_to(label, inner)
        {
            let TirStmtKind::LabeledBlock { block: inner, .. } = stmt.kind else {
                unreachable!();
            };
            block.stmts.extend(inner.stmts);
            continue;
        }
        // Unit expression → drop (side-effect free)
        if let TirStmtKind::Expr(e) = &stmt.kind
            && matches!(e.kind, TirExprKind::Unit)
        {
            continue;
        }
        // Void block expression → flatten stmts into parent
        if let TirStmtKind::Expr(e) = &stmt.kind
            && matches!(e.kind, TirExprKind::Block(_))
        {
            let TirStmtKind::Expr(e) = stmt.kind else {
                unreachable!();
            };
            let TirExprKind::Block(inner) = e.kind else {
                unreachable!();
            };
            block.stmts.extend(inner.stmts);
            continue;
        }
        if let TirStmtKind::Expr(e) = &stmt.kind
            && matches!(&e.kind, TirExprKind::LabeledBlock { label, block, .. } if !block_has_break_to(label, block))
        {
            let TirStmtKind::Expr(e) = stmt.kind else {
                unreachable!();
            };
            let TirExprKind::LabeledBlock { block: inner, .. } = e.kind else {
                unreachable!();
            };
            block.stmts.extend(inner.stmts);
            continue;
        }
        block.stmts.push(stmt);
    }
    true
}
