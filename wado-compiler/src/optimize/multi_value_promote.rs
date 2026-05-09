//! Multi-value promotion pass.
//!
//! Converts `let local = TupleLiteral([…])` to `let local = MultiValueLiteral([…])`
//! when `local` is read only via `FieldAccess` / `MultiValueProject` (i.e. it
//! never escapes). The two forms are semantically equivalent at the TIR
//! level — they differ only in WIR ABI:
//!
//! - `TupleLiteral` always lowers to `struct.new` (heap allocation).
//! - `MultiValueLiteral` lowers to `MultiValueStructNew { instr: Seq(…) }`
//!   which the WIR `peephole::elide_multi_value_structs` pass collapses into
//!   `multivalue_bind` when the temp is destructured into N locals via
//!   consecutive `StructGet` reads — eliminating the heap allocation.
//!
//! ## Why a late, conservative pass?
//!
//! Resolver, monomorphize, synthesis, and every TIR optimisation pass
//! continue to assume the heap-resident `TupleLiteral` form. Running this
//! promotion *after* the fixed-point optimisation loop means:
//!
//! 1. Earlier passes don't need to learn about `MultiValueLiteral`'s ABI
//!    differences — they keep the simpler `TupleLiteral` invariant.
//! 2. SROA already scalarises non-escaping tuples whose every field is read
//!    safely. Anything left as `TupleLiteral` post-SROA is a tuple SROA
//!    didn't decompose (e.g. partial-field reads without a SoA-style
//!    rewrite). For those the promotion still pays off because the WIR
//!    peephole catches the destructure shape SROA didn't reach.
//! 3. WIR build is the *only* phase that needs to distinguish the two
//!    forms; everything upstream is unchanged.
//!
//! ## Escape definition
//!
//! A tuple local is *safe to promote* if every use is one of:
//! - `FieldAccess { expr: Local(this), .. }` (heritage-spelling field read)
//! - `MultiValueProject { source: Local(this), .. }` (multi-value spelling)
//! - The defining `Let` itself.
//!
//! Any other use — bare `Local` reference, address taken, closure capture,
//! field assignment, passed to a call, etc. — disqualifies the local. This
//! is intentionally stricter than SROA's "soft escape" concept: SROA can
//! reconstruct a heap struct at escape sites, but promotion has no
//! reconstruction path (and if it did, it would defeat its own purpose).

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{TirExpr, TirExprKind, TirFunction};
use crate::tir_visitor::TirRefVisitor;

/// Promote non-escaping tuple-literal locals across all functions in the
/// project. Returns `true` if any change was made.
pub fn promote_to_multi_value(project: &mut FlatPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= promote_in_function(&mut func);
    }
    changed
}

fn promote_in_function(func: &mut TirFunction) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    // Step 1: collect candidates — local indices whose Let RHS is a
    // `TupleLiteral`. We don't promote `MultiValueLiteral` (already
    // promoted) or non-tuple aggregates.
    let mut candidates: IndexSet<u32> = IndexSet::default();
    collect_tuple_literal_locals(body, &mut candidates);
    if candidates.is_empty() {
        return false;
    }

    // Step 2: escape check — any use of a candidate outside of
    // `FieldAccess` / `MultiValueProject` disqualifies it.
    let mut checker = SafeUseChecker {
        candidates: &candidates,
        escaped: IndexSet::default(),
    };
    checker.visit_block(body);
    let safe: IndexSet<u32> = candidates
        .iter()
        .copied()
        .filter(|idx| !checker.escaped.contains(idx))
        .collect();
    if safe.is_empty() {
        return false;
    }

    // Step 3: rewrite the candidate `TupleLiteral` initialisers to
    // `MultiValueLiteral`. The block-walker only touches `Let` RHS values
    // belonging to safe locals.
    let mut rewriter = Rewriter { safe: &safe };
    rewriter.rewrite_block(body)
}

fn collect_tuple_literal_locals(block: &crate::tir::TirBlock, out: &mut IndexSet<u32>) {
    use crate::tir::{TirStmt, TirStmtKind};
    fn collect_stmt(stmt: &TirStmt, out: &mut IndexSet<u32>) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                if matches!(value.kind, TirExprKind::TupleLiteral { .. }) {
                    out.insert(*local_index);
                }
                collect_in_expr(value, out);
            }
            TirStmtKind::Expr(e) | TirStmtKind::Return { value: Some(e) } => {
                collect_in_expr(e, out)
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => {}
            TirStmtKind::Break { value, .. } => {
                if let Some(v) = value {
                    collect_in_expr(v, out);
                }
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                collect_in_expr(condition, out);
                collect_tuple_literal_locals(then_block, out);
                if let Some(eb) = else_block {
                    collect_tuple_literal_locals(eb, out);
                }
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                collect_tuple_literal_locals(body, out);
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                collect_in_expr(scrutinee, out);
                collect_tuple_literal_locals(then_block, out);
                if let Some(eb) = else_block {
                    collect_tuple_literal_locals(eb, out);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => collect_in_expr(value, out),
            TirStmtKind::TaskReturn { .. } | TirStmtKind::VariadicForOf { .. } => {}
        }
    }
    fn collect_in_expr(expr: &TirExpr, out: &mut IndexSet<u32>) {
        if let TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } = &expr.kind {
            collect_tuple_literal_locals(b, out);
        } else {
            // For non-block expressions we still need to descend in case
            // they contain nested blocks (e.g. `If`/`Match` in value
            // position) — TirRefVisitor's default walk handles this.
            struct DescendCollector<'a> {
                out: &'a mut IndexSet<u32>,
            }
            impl TirRefVisitor for DescendCollector<'_> {
                fn visit_expr(&mut self, expr: &TirExpr) {
                    if let TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } =
                        &expr.kind
                    {
                        collect_tuple_literal_locals(b, self.out);
                    } else {
                        self.walk_expr(expr);
                    }
                }
            }
            let mut c = DescendCollector { out };
            c.visit_expr(expr);
        }
    }
    for stmt in &block.stmts {
        collect_stmt(stmt, out);
    }
}

/// Visitor that marks a candidate as escaped if it appears outside of a
/// `FieldAccess { expr: Local(candidate), .. }` or
/// `MultiValueProject { source: Local(candidate), .. }` position.
struct SafeUseChecker<'a> {
    candidates: &'a IndexSet<u32>,
    escaped: IndexSet<u32>,
}

impl TirRefVisitor for SafeUseChecker<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            // Safe field access on a candidate: the base local is read but
            // not "exposed" — skip recursion into the Local node itself.
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::MultiValueProject { source: inner, .. } => {
                if matches!(&inner.kind, TirExprKind::Local { index, .. }
                    if self.candidates.contains(index))
                {
                    return;
                }
                self.visit_expr(inner);
            }
            // Bare Local reference of a candidate: escape.
            TirExprKind::Local { index, .. } => {
                if self.candidates.contains(index) {
                    self.escaped.insert(*index);
                }
            }
            // Address taken on a candidate: escape (handled via the bare
            // Local arm above when we recurse into the Unary's inner —
            // visiting the Local marks it as escaped).
            //
            // We deliberately *don't* recurse with a "this is a ref-context"
            // flag: the bare Local arm correctly reports any non-field
            // appearance as an escape, which is the conservative behaviour
            // we want here.
            _ => self.walk_expr(expr),
        }
    }
}

struct Rewriter<'a> {
    safe: &'a IndexSet<u32>,
}

impl Rewriter<'_> {
    fn rewrite_block(&mut self, block: &mut crate::tir::TirBlock) -> bool {
        use crate::tir::{TirStmt, TirStmtKind};
        let mut changed = false;
        for stmt in &mut block.stmts {
            changed |= self.rewrite_stmt(stmt);
        }
        // Suppress unused; helps with future refactors keeping types alive.
        let _ = std::mem::size_of::<TirStmt>();
        let _ = std::mem::size_of::<TirStmtKind>();
        changed
    }

    fn rewrite_stmt(&mut self, stmt: &mut crate::tir::TirStmt) -> bool {
        use crate::tir::TirStmtKind;
        match &mut stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                let mut changed = false;
                if self.safe.contains(local_index)
                    && let TirExprKind::TupleLiteral { elements } = &mut value.kind
                {
                    let elements = std::mem::take(elements);
                    value.kind = TirExprKind::MultiValueLiteral { elements };
                    changed = true;
                }
                changed |= self.rewrite_expr(value);
                changed
            }
            TirStmtKind::Expr(e) | TirStmtKind::Return { value: Some(e) } => self.rewrite_expr(e),
            TirStmtKind::Return { value: None } | TirStmtKind::Continue => false,
            TirStmtKind::Break { value, .. } => {
                value.as_mut().is_some_and(|v| self.rewrite_expr(v))
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut changed = self.rewrite_expr(condition);
                changed |= self.rewrite_block(then_block);
                if let Some(eb) = else_block {
                    changed |= self.rewrite_block(eb);
                }
                changed
            }
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.rewrite_block(body)
            }
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                let mut changed = self.rewrite_expr(scrutinee);
                changed |= self.rewrite_block(then_block);
                if let Some(eb) = else_block {
                    changed |= self.rewrite_block(eb);
                }
                changed
            }
            TirStmtKind::LetDestructure { value, .. } => self.rewrite_expr(value),
            TirStmtKind::TaskReturn { .. } | TirStmtKind::VariadicForOf { .. } => false,
        }
    }

    /// Recurse into nested blocks within an expression. We only rewrite
    /// `Let`-bound `TupleLiteral`s, so the leaf-level expression doesn't
    /// need any rewriting on its own — only the blocks it contains do.
    fn rewrite_expr(&mut self, expr: &mut TirExpr) -> bool {
        match &mut expr.kind {
            TirExprKind::Block(b) | TirExprKind::LabeledBlock { block: b, .. } => {
                self.rewrite_block(b)
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut changed = self.rewrite_expr(condition);
                changed |= self.rewrite_block(then_branch);
                if let Some(eb) = else_branch {
                    changed |= self.rewrite_block(eb);
                }
                changed
            }
            TirExprKind::Match { expr: inner, arms } => {
                let mut changed = self.rewrite_expr(inner);
                for arm in arms {
                    if let Some(g) = &mut arm.guard {
                        changed |= self.rewrite_expr(g);
                    }
                    changed |= self.rewrite_expr(&mut arm.body);
                }
                changed
            }
            TirExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                let mut changed = self.rewrite_expr(scrutinee);
                for arm in arms {
                    changed |= self.rewrite_block(arm);
                }
                changed |= self.rewrite_block(default);
                changed
            }
            TirExprKind::Closure { body, .. } => self.rewrite_expr(body),
            // For non-block expression kinds, descend through children.
            _ => {
                let mut changed = false;
                expr_for_each_child_mut(expr, &mut |child| {
                    changed |= self.rewrite_expr(child);
                });
                changed
            }
        }
    }
}

/// Walk each direct child expression of `expr`, invoking `f` on each.
///
/// This mirrors the structural recursion in `tir_visitor::opt_walk_expr`
/// but exposes a closure-based API so the rewriter doesn't need to
/// duplicate the dispatch table.
fn expr_for_each_child_mut(expr: &mut TirExpr, f: &mut dyn FnMut(&mut TirExpr)) {
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. } => f(inner),
        TirExprKind::MultiValueProject { source, .. } => f(source),
        TirExprKind::Assign { target, value }
        | TirExprKind::Index {
            expr: target,
            index: value,
        } => {
            f(target);
            f(value);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                f(&mut arg.expr);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                f(arg);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            f(receiver);
            for arg in args {
                f(&mut arg.expr);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            f(callee);
            for arg in args {
                f(arg);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                f(&mut field.value);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::MultiValueLiteral { elements } => {
            for elem in elements {
                f(elem);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                f(p);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => f(value),
        // Leaf nodes / handled by the outer Rewriter.
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. }
        | TirExprKind::Block(_)
        | TirExprKind::LabeledBlock { .. }
        | TirExprKind::If { .. }
        | TirExprKind::Match { .. }
        | TirExprKind::Switch { .. }
        | TirExprKind::Closure { .. }
        | TirExprKind::TemplateString { .. }
        | TirExprKind::WithHandler { .. }
        | TirExprKind::Resume { .. } => {}
    }
}
