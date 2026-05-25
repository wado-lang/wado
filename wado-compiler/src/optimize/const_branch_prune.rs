//! Constant branch pruning for Wado NIR
//!
//! Simplifies trivial blocks left over after other passes:
//! - `{ expr; }` → `expr`
//! - `label: { break label: val; }` → `val`
//! - `label: { let x = y; ... }` → substitute x with y in remaining stmts
//! - Empty blocks → `()`
//!
//! Constant-condition `if` folding (`if true { A } else { B }` → `A`,
//! both at the expression and statement level) is handled by `niri` via
//! the `const_folding` pass — see `docs/wep-2026-04-27-tir-interpreter.md`
//! Stage 2. This pass intentionally does *not* duplicate that logic; if
//! you find yourself adding an `if`-condition rewrite here, push it into
//! `niri` instead so the lattice-driven engine stays the single source
//! of truth.

use crate::hashmap::IndexMap;
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind};
use crate::nir_package::NirPackage;

use crate::nir_visitor::{
    NirMutVisitor, NirOptVisitor, NirRefVisitor, block_has_break_to, expr_has_break_to,
    opt_walk_block, opt_walk_expr, stmt_has_break_to, visit_project_functions,
};

/// Prune constant branches and simplify trivial blocks in all functions.
///
/// Inside the optimizer fixpoint loop. `__tmpl:` labeled blocks are
/// preserved so `tmpl_hoist` can anchor on them later in the same iteration.
pub fn prune_constant_branches(project: &mut NirPackage) -> bool {
    let mut visitor = BranchPruner {
        mode: PruneMode::Fixpoint,
    };
    visit_project_functions(project, &mut visitor)
}

/// Final post-fixpoint pass that flattens any `__tmpl:` wrappers
/// `tmpl_hoist` already finished processing during the fixpoint loop.
pub fn prune_template_block_wrappers(project: &mut NirPackage) -> bool {
    let mut visitor = BranchPruner {
        mode: PruneMode::PostFixpoint,
    };
    visit_project_functions(project, &mut visitor)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PruneMode {
    /// Run inside the optimizer fixpoint loop — preserve `__tmpl:` blocks
    /// for `tmpl_hoist`.
    Fixpoint,
    /// Final cleanup after the fixpoint converges — flatten every
    /// tail-break-only labeled block.
    PostFixpoint,
}

struct BranchPruner {
    mode: PruneMode,
}

impl NirOptVisitor for BranchPruner {
    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        // Bottom-up: walk children first
        let mut changed = opt_walk_expr(self, expr);
        // Inline leading copy bindings inside labeled blocks before pruning
        changed |= inline_labeled_block_copies(expr);
        // Then prune this expression
        changed |= prune_expr(expr, self.mode);
        changed
    }

    fn visit_block(&mut self, block: &mut NirBlock) -> bool {
        // Bottom-up: walk stmts first
        let mut changed = opt_walk_block(self, block);
        // Then eliminate dead stmts
        changed |= eliminate_dead_stmts(block, self.mode);
        changed
    }
}

/// (C2) Recognises a `LabeledBlock { label, inner }` stmt whose only break to
/// `label` is the trailing statement and carries no value. Such labels add no
/// control flow over their straight-line body and the labeled wrapper can be
/// removed by dropping the trailing `break label` and lifting the rest of
/// `inner.stmts` into the parent stmt list.
///
/// `mode` mirrors C3's `PostFixpoint` / Fixpoint distinction: in `Fixpoint`
/// mode the `__tmpl:` carve-out preserves the anchor `tmpl_hoist` needs.
fn is_tail_break_only_labeled_block(stmt: &NirStmt, mode: PruneMode) -> bool {
    let NirStmtKind::LabeledBlock {
        label,
        block: inner,
    } = &stmt.kind
    else {
        return false;
    };
    if mode == PruneMode::Fixpoint && label == "__tmpl" {
        return false;
    }
    let Some(last) = inner.stmts.last() else {
        return false;
    };
    let NirStmtKind::Break {
        label: Some(brk_label),
        value: None,
    } = &last.kind
    else {
        return false;
    };
    if brk_label != label {
        return false;
    }
    !inner.stmts[..inner.stmts.len() - 1]
        .iter()
        .any(|s| stmt_has_break_to(label, s))
}

/// True if the trailing instruction of `stmts` unconditionally exits the
/// enclosing stmt list (`Break` / `Continue` / `Return`). Used by
/// `eliminate_dead_stmts` to keep the `terminated` flag accurate after
/// flatten paths hoist a terminator out of an inner block.
fn ends_with_terminator_stmt(stmts: &[NirStmt]) -> bool {
    matches!(
        stmts.last().map(|s| &s.kind),
        Some(NirStmtKind::Break { .. } | NirStmtKind::Continue | NirStmtKind::Return { .. })
    )
}

/// Simplify trivial blocks at the expression level.
///
/// Constant-condition `if` folding lives in `niri` (Stage 2 of the NIR
/// interpreter); this function deliberately does not handle that case.
fn prune_expr(expr: &mut NirExpr, mode: PruneMode) -> bool {
    let mut changed = false;

    // Simplify `{ expr; }` → `expr` (single-expression unlabeled block)
    if let NirExprKind::Block(block) = &expr.kind
        && block.stmts.len() == 1
        && let NirStmtKind::Expr(_) = &block.stmts[0].kind
    {
        let NirExprKind::Block(block) = std::mem::replace(&mut expr.kind, NirExprKind::Unit) else {
            unreachable!();
        };
        let mut stmts = block.stmts;
        let NirStmtKind::Expr(inner) = stmts.remove(0).kind else {
            unreachable!();
        };
        *expr = inner;
        changed = true;
    }

    // (C3) Simplify `label: { stmts...; break label: val; }` → `{ stmts...; val }`.
    //
    // When the *only* reference to `label` is the trailing `break label: val`
    // statement, the labeled block adds no control flow over its straight-line
    // body and can be unwrapped into a plain block whose tail expression is
    // `val`. Subsequent `prune_expr` iterations collapse `{ val }` → `val`
    // when the prefix becomes empty.
    //
    // `__tmpl` blocks are preserved: `tmpl_hoist` runs later in the optimize
    // loop and anchors on the `__tmpl: block` shape to hoist per-iteration
    // `Formatter` buffer initialisations out of loops. Flattening here would
    // erase that anchor before `tmpl_hoist` gets to scan it.
    if let NirExprKind::LabeledBlock { label, block, .. } = &expr.kind
        && (mode == PruneMode::PostFixpoint || label != "__tmpl")
        && let Some(last) = block.stmts.last()
        && let NirStmtKind::Break {
            label: Some(brk_label),
            value: Some(brk_value),
        } = &last.kind
        && brk_label == label
        && !expr_has_break_to(label, brk_value)
        && !block.stmts[..block.stmts.len() - 1]
            .iter()
            .any(|s| stmt_has_break_to(label, s))
    {
        let NirExprKind::LabeledBlock { block, .. } =
            std::mem::replace(&mut expr.kind, NirExprKind::Unit)
        else {
            unreachable!();
        };
        let block_span = block.span;
        let mut stmts = block.stmts;
        let last = stmts.pop().unwrap();
        let NirStmt { kind, .. } = last;
        let NirStmtKind::Break {
            value: Some(value), ..
        } = kind
        else {
            unreachable!();
        };
        if stmts.is_empty() {
            *expr = value;
        } else {
            // Anchor the synthetic tail Expr stmt on the broken
            // value's own span so diagnostics / hovers point at the
            // value's source location, not the `break label: …`
            // statement.
            let tail_span = value.span;
            stmts.push(NirStmt::new(NirStmtKind::Expr(value), tail_span));
            expr.kind = NirExprKind::Block(NirBlock::new(stmts, block_span));
        }
        changed = true;
    }

    // Also keep the single-stmt rule for when the labeled block has only
    // one break stmt (no prefix). This pattern is structurally identical
    // to C3 with an empty prefix, but the C3 rule above requires
    // `value: Some(_)`, so this rule additionally covers the `value: None`
    // case and any subtle path the N-stmt collapse leaves behind.
    //
    // `__tmpl` carve-out mirrors C3: during the fixpoint loop, leave
    // `__tmpl:` blocks alone so `tmpl_hoist` can still anchor on them
    // even if some other pass shrinks the body to a single break.
    if let NirExprKind::LabeledBlock { label, block, .. } = &expr.kind
        && (mode == PruneMode::PostFixpoint || label != "__tmpl")
        && block.stmts.len() == 1
        && let NirStmtKind::Break {
            label: Some(brk_label),
            value: brk_value,
        } = &block.stmts[0].kind
        && brk_label == label
        && !brk_value
            .as_ref()
            .is_some_and(|v| expr_has_break_to(label, v))
    {
        let NirExprKind::LabeledBlock { block, .. } =
            std::mem::replace(&mut expr.kind, NirExprKind::Unit)
        else {
            unreachable!();
        };
        let mut stmts = block.stmts;
        let NirStmtKind::Break { value, .. } = stmts.remove(0).kind else {
            unreachable!();
        };
        if let Some(inner) = value {
            *expr = inner;
        }
        changed = true;
    }

    // Simplify `[label:] { }` → `()` (empty block, with or without label)
    if matches!(&expr.kind, NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } if b.stmts.is_empty())
    {
        expr.kind = NirExprKind::Unit;
        changed = true;
    }

    changed
}

/// Eliminate dead statements from a block:
/// - `label: { }` (empty labeled block) → remove
/// - `label: { stmts }` (unused label) → flatten stmts into parent
/// - Trivial Unit / Block / unused-LabeledBlock expression stmts → flatten
/// - Anything after a `break` / `continue` / `return` in the same stmt list
///   is structurally dead and dropped (C1). The terminator itself stays.
///
/// Constant-condition `if` statement folding (`if true { … }` →
/// inline branch) lives in `niri::Interpreter::reduce_local_block`
/// and runs as part of the `const_folding` pass.
fn eliminate_dead_stmts(block: &mut NirBlock, mode: PruneMode) -> bool {
    // In `Fixpoint` mode the unused-label flatten still has to keep `__tmpl:`
    // wrappers alive: even if no `break __tmpl` survives, `tmpl_hoist` still
    // anchors on the labeled-block shape itself.
    let unused_label_flattenable = |label: &str, inner: &NirBlock| {
        (mode == PruneMode::PostFixpoint || label != "__tmpl")
            && (inner.stmts.is_empty() || !block_has_break_to(label, inner))
    };
    let dominated = |s: &NirStmt| {
        matches!(
            &s.kind,
            NirStmtKind::LabeledBlock { label, block }
                if unused_label_flattenable(label, block)
        ) || matches!(
            &s.kind,
            NirStmtKind::Expr(e) if matches!(e.kind, NirExprKind::Unit | NirExprKind::Block(_))
        ) || matches!(
            &s.kind,
            NirStmtKind::Expr(e) if matches!(&e.kind, NirExprKind::LabeledBlock { label, block, .. } if unused_label_flattenable(label, block))
        ) || is_tail_break_only_labeled_block(s, mode)
    };
    let has_dead_after_terminator = block.stmts.iter().enumerate().any(|(i, s)| {
        i + 1 < block.stmts.len()
            && matches!(
                &s.kind,
                NirStmtKind::Break { .. } | NirStmtKind::Continue | NirStmtKind::Return { .. }
            )
    });
    if !has_dead_after_terminator && !block.stmts.iter().any(dominated) {
        return false;
    }

    let old_stmts = std::mem::take(&mut block.stmts);
    let mut terminated = false;
    for stmt in old_stmts {
        if terminated {
            // Drop everything after a terminator in the same stmt list.
            continue;
        }
        // Labeled block with unused label → flatten stmts into parent
        if let NirStmtKind::LabeledBlock {
            ref label,
            block: ref inner,
        } = stmt.kind
            && unused_label_flattenable(label, inner)
        {
            let NirStmtKind::LabeledBlock { block: inner, .. } = stmt.kind else {
                unreachable!();
            };
            // If the hoisted-out inner stmts end with a terminator, mark the
            // outer list as terminated so the rest of `old_stmts` is dropped
            // in this same pass instead of waiting for the next fixpoint
            // iteration.
            if ends_with_terminator_stmt(&inner.stmts) {
                terminated = true;
            }
            block.stmts.extend(inner.stmts);
            continue;
        }
        // (C2) Labeled block whose only `break LABEL` is a value-less tail
        // statement → drop the trailing break, flatten the rest into parent.
        if is_tail_break_only_labeled_block(&stmt, mode) {
            let NirStmtKind::LabeledBlock {
                block: mut inner, ..
            } = stmt.kind
            else {
                unreachable!();
            };
            inner.stmts.pop();
            if ends_with_terminator_stmt(&inner.stmts) {
                terminated = true;
            }
            block.stmts.extend(inner.stmts);
            continue;
        }
        // Unit expression → drop (side-effect free)
        if let NirStmtKind::Expr(e) = &stmt.kind
            && matches!(e.kind, NirExprKind::Unit)
        {
            continue;
        }
        // Void block expression → flatten stmts into parent
        if let NirStmtKind::Expr(e) = &stmt.kind
            && matches!(e.kind, NirExprKind::Block(_))
        {
            let NirStmtKind::Expr(e) = stmt.kind else {
                unreachable!();
            };
            let NirExprKind::Block(inner) = e.kind else {
                unreachable!();
            };
            if ends_with_terminator_stmt(&inner.stmts) {
                terminated = true;
            }
            block.stmts.extend(inner.stmts);
            continue;
        }
        if let NirStmtKind::Expr(e) = &stmt.kind
            && matches!(&e.kind, NirExprKind::LabeledBlock { label, block, .. } if unused_label_flattenable(label, block))
        {
            let NirStmtKind::Expr(e) = stmt.kind else {
                unreachable!();
            };
            let NirExprKind::LabeledBlock { block: inner, .. } = e.kind else {
                unreachable!();
            };
            if ends_with_terminator_stmt(&inner.stmts) {
                terminated = true;
            }
            block.stmts.extend(inner.stmts);
            continue;
        }
        if matches!(
            &stmt.kind,
            NirStmtKind::Break { .. } | NirStmtKind::Continue | NirStmtKind::Return { .. }
        ) {
            terminated = true;
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
fn inline_labeled_block_copies(expr: &mut NirExpr) -> bool {
    let NirExprKind::LabeledBlock { block, .. } = &mut expr.kind else {
        return false;
    };

    // Collect leading copy bindings: `let x = y` where y is a Local and x is immutable.
    // Mutable targets must not be propagated because the value copy serves as a snapshot
    // that may be mutated independently (e.g., `fn f(mut s: String) { s.push_str("!"); }`).
    let mut copies: Vec<(u32, u32, String)> = Vec::new(); // (target_idx, source_idx, source_name)
    for stmt in &block.stmts {
        if let NirStmtKind::Let {
            local_index,
            is_mut,
            value,
            ..
        } = &stmt.kind
            && !is_mut
            && let NirExprKind::Local { index, name } = &value.kind
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

impl NirRefVisitor for MutationChecker {
    fn visit_expr(&mut self, expr: &NirExpr) {
        if self.found {
            return;
        }
        match &expr.kind {
            // Direct assignment: `y = ...`
            NirExprKind::Assign { target, .. } => {
                if let NirExprKind::Local { index, .. } = &target.kind
                    && self.locals.contains(index)
                {
                    self.found = true;
                    return;
                }
                // Field assignment: `y.field = ...`
                if let NirExprKind::FieldAccess { expr: inner, .. } = &target.kind
                    && let NirExprKind::Local { index, .. } = &inner.kind
                    && self.locals.contains(index)
                {
                    self.found = true;
                    return;
                }
            }
            // Mutable reference: `&mut y`
            NirExprKind::Unary {
                op: crate::nir::NirUnaryOp::MutRef,
                expr: inner,
            } => {
                if let NirExprKind::Local { index, .. } = &inner.kind
                    && self.locals.contains(index)
                {
                    self.found = true;
                    return;
                }
            }
            // Method call receiver may be mutated by the callee (`s.push_str(...)`).
            // Be conservative and treat receiver use as a potential mutation.
            NirExprKind::MethodCall { receiver, .. } => {
                if let NirExprKind::Local { index, .. } = &receiver.kind
                    && self.locals.contains(index)
                {
                    self.found = true;
                    return;
                }
            }
            // Mutable call arguments can mutate the tracked local.
            NirExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
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

    fn visit_stmt(&mut self, stmt: &NirStmt) {
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

impl NirMutVisitor for LocalSubstituter {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        if let NirExprKind::Local { index, name } = &mut expr.kind
            && let Some((src_idx, src_name)) = self.subs.get(index)
        {
            *index = *src_idx;
            name.clone_from(src_name);
            return;
        }
        self.walk_expr(expr);
    }
}
