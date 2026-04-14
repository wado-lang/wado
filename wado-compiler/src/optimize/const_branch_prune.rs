//! Constant branch pruning for Wado TIR
//!
//! Eliminates branches with known boolean conditions and simplifies trivial blocks:
//! - `if true { A } else { B }` → `A`
//! - `if false { A } else { B }` → `B`
//! - `{ expr; }` → `expr`
//! - `label: { break label: val; }` → `val`
//! - `label: { let x = y; ... }` → substitute x with y in remaining stmts
//! - Empty blocks → `()`

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind};

use crate::tir_visitor::{
    TirMutVisitor, TirOptVisitor, TirRefVisitor, block_has_break_to, expr_has_break_to,
    opt_walk_block, opt_walk_expr, visit_project_functions,
};

/// Prune constant branches and simplify trivial blocks in all functions.
pub fn prune_constant_branches(project: &mut FlatPackage) -> bool {
    let mut visitor = BranchPruner;
    visit_project_functions(project, &mut visitor)
}

struct BranchPruner;

impl TirOptVisitor for BranchPruner {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: walk children first
        let mut changed = opt_walk_expr(self, expr);
        // Inline leading copy bindings inside labeled blocks before pruning
        changed |= inline_labeled_block_copies(expr);
        // Then prune this expression
        changed |= prune_expr(expr);
        changed
    }

    fn visit_block(&mut self, block: &mut TirBlock) -> bool {
        // Bottom-up: walk stmts first
        let mut changed = opt_walk_block(self, block);
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

/// Inline leading copy bindings inside labeled block expressions.
///
/// Pattern: `label: { let x = y; ... break label: expr; }` where `y` is a Local.
/// When neither `x` nor `y` is assigned within the remaining statements, substitute
/// all occurrences of `x` with `y` in the block body and remove the let binding.
///
/// This handles residual copies left by function inlining (e.g., inlined parameter
/// bindings like `let index = p`).
fn inline_labeled_block_copies(expr: &mut TirExpr) -> bool {
    let TirExprKind::LabeledBlock { block, .. } = &mut expr.kind else {
        return false;
    };

    // Collect leading copy bindings: `let x = y` where y is a Local and x is immutable.
    // Mutable targets must not be propagated because the value copy serves as a snapshot
    // that may be mutated independently (e.g., `fn f(mut s: String) { s.push_str("!"); }`).
    let mut copies: Vec<(u32, u32, String)> = Vec::new(); // (target_idx, source_idx, source_name)
    for stmt in &block.stmts {
        if let TirStmtKind::Let {
            local_index,
            is_mut,
            value,
            ..
        } = &stmt.kind
            && !is_mut
            && let TirExprKind::Local { index, name } = &value.kind
        {
            copies.push((*local_index, *index, name.clone()));
        } else {
            break;
        }
    }

    if copies.is_empty() {
        return false;
    }

    // Verify safety: neither target nor source is assigned or mutably referenced
    // within the remaining stmts. Mutable references must also be checked because
    // modifications through `&mut y` would not be caught as direct assignments.
    let remaining = &block.stmts[copies.len()..];
    let mut checker = MutationChecker::default();
    for (target, source, _) in &copies {
        checker.locals.insert(*target);
        checker.locals.insert(*source);
    }
    for stmt in remaining {
        checker.visit_stmt(stmt);
    }
    if checker.found {
        return false;
    }

    // Build substitution map with transitive resolution.
    // If copies are [a→x, b→a], resolve b→a to b→x so that removing `a`
    // doesn't leave dangling references.
    let copy_count = copies.len();
    let mut subs: IndexMap<u32, (u32, String)> = IndexMap::default();
    for (target, source, source_name) in copies {
        // Resolve through existing substitutions
        let (final_source, final_name) = if let Some((resolved, resolved_name)) = subs.get(&source)
        {
            (*resolved, resolved_name.clone())
        } else {
            (source, source_name)
        };
        subs.insert(target, (final_source, final_name));
    }

    block.stmts.drain(..copy_count);

    let mut substituter = LocalSubstituter { subs };
    substituter.visit_block(block);

    true
}

/// Checks whether any local in `locals` is assigned or mutably referenced
/// within the visited tree. Catches both:
/// - Direct assignment: `y = ...`
/// - Field assignment: `y.field = ...`
/// - Mutable reference: `&mut y` (could be used to modify y indirectly)
#[derive(Default)]
struct MutationChecker {
    locals: indexmap::IndexSet<u32>,
    found: bool,
}

impl TirRefVisitor for MutationChecker {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if self.found {
            return;
        }
        match &expr.kind {
            // Direct assignment: `y = ...`
            TirExprKind::Assign { target, .. } => {
                if let TirExprKind::Local { index, .. } = &target.kind
                    && self.locals.contains(index)
                {
                    self.found = true;
                    return;
                }
                // Field assignment: `y.field = ...`
                if let TirExprKind::FieldAccess { expr: inner, .. } = &target.kind
                    && let TirExprKind::Local { index, .. } = &inner.kind
                    && self.locals.contains(index)
                {
                    self.found = true;
                    return;
                }
            }
            // Mutable reference: `&mut y`
            TirExprKind::Unary {
                op: crate::tir::TirUnaryOp::MutRef,
                expr: inner,
            } => {
                if let TirExprKind::Local { index, .. } = &inner.kind
                    && self.locals.contains(index)
                {
                    self.found = true;
                    return;
                }
            }
            // Method call receiver may be mutated by the callee (`s.push_str(...)`).
            // Be conservative and treat receiver use as a potential mutation.
            TirExprKind::MethodCall { receiver, .. } => {
                if let TirExprKind::Local { index, .. } = &receiver.kind
                    && self.locals.contains(index)
                {
                    self.found = true;
                    return;
                }
            }
            // Mutable call arguments can mutate the tracked local.
            TirExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let TirExprKind::Local { index, .. } = &arg.expr.kind
                        && self.locals.contains(index)
                    {
                        self.found = true;
                        return;
                    }
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }

    fn visit_stmt(&mut self, stmt: &TirStmt) {
        if self.found {
            return;
        }
        self.walk_stmt(stmt);
    }
}

/// Substitutes local variable references: replaces `Local { index: target }` with
/// `Local { index: source, name: source_name }`.
struct LocalSubstituter {
    subs: IndexMap<u32, (u32, String)>,
}

impl TirMutVisitor for LocalSubstituter {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if let TirExprKind::Local { index, name } = &mut expr.kind
            && let Some((src_idx, src_name)) = self.subs.get(index)
        {
            *index = *src_idx;
            name.clone_from(src_name);
            return;
        }
        self.walk_expr(expr);
    }
}
