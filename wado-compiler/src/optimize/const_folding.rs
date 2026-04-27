//! Constant folding optimization for Wado TIR.
//!
//! Walks every function body and applies the [`tiri::Interpreter`]
//! rewrite rules at each visited node. All reduction logic
//! (literal folding, integer cast collapsing, short-circuit identity
//! rules, env-aware local lookup) lives in [`crate::tiri`]; this module
//! is only the visitor glue that drives `reduce_local` across function
//! bodies and feeds the interpreter's local-variable env from `Let`
//! statements and assignments.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind};
use crate::tir_visitor::{TirOptVisitor, TirRefVisitor, opt_walk_expr, opt_walk_stmt};
use crate::tiri::{Interpreter, Lattice};

/// Apply constant folding to all functions in the project.
pub fn fold_constants(project: &mut FlatPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    let mut visitor = ConstFoldVisitor {
        interpreter: Interpreter::new(&type_table),
    };
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(ref mut body) = func.body {
            // Local indices are unique per function, not project-wide,
            // so reset the interpreter's env at every function boundary.
            visitor.interpreter.enter_function();
            changed |= visitor.visit_block(body);
        }
    }
    changed
}

struct ConstFoldVisitor<'a> {
    interpreter: Interpreter<'a>,
}

impl TirOptVisitor for ConstFoldVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        // Loops have a back-edge: a value assigned at the bottom of the
        // body re-enters the top on the next iteration. The single-pass
        // visitor walks each body just once, so an env entry that
        // *predates* the loop would otherwise feed the body's first
        // walk an iteration-1 value that no longer holds at the
        // back-edge — the second iteration's `cond`, the third's, and
        // so on. Pre-invalidate every local whose value the loop body
        // can change so the body walks against the meet of all
        // iteration entries (i.e. `NonConst` for any mutated cell).
        if let TirStmtKind::Loop { body } = &stmt.kind {
            self.invalidate_locals_assigned_in(body);
        }

        // Bottom-up: walk children first so the RHS of `let x = …` is
        // already folded by the time we record `x` in env.
        let changed = opt_walk_stmt(self, stmt);
        self.update_env_from_stmt(stmt);
        changed
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: walk every child kind first (`opt_walk_expr` covers
        // If / Block / Match / Call / … in addition to the Binary /
        // Unary / Cast trees that the interpreter recurses into).
        // Then ask the interpreter to apply local rewrites at this node.
        let mut changed = opt_walk_expr(self, expr);
        // Observe assignments to invalidate the LHS local in env. Done
        // *after* walking so the RHS sees the prior binding.
        if let TirExprKind::Assign { target, .. } = &expr.kind
            && let TirExprKind::Local { index, .. } = &target.kind
        {
            self.interpreter.invalidate_local(*index);
        }
        changed |= self.interpreter.reduce_local(expr);
        changed
    }
}

impl ConstFoldVisitor<'_> {
    /// After a statement is walked, capture any introduced binding into
    /// the interpreter's env so subsequent uses can fold against it.
    fn update_env_from_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                is_mut,
                value,
                ..
            } => {
                let lat = if *is_mut {
                    // `let mut x = …` — any later `x = …` would
                    // invalidate the binding anyway, so be conservative
                    // up front. Stage 1 doesn't track flow-sensitive
                    // values for mutable locals.
                    Lattice::NonConst
                } else {
                    self.interpreter.reduce_to_lattice(value)
                };
                self.interpreter.bind_local(*local_index, lat);
            }
            // LetDestructure binds multiple locals via pattern matching
            // (`let [a, b] = tuple`). Tuple-aware lattice values aren't
            // modelled yet, so leave the destructured locals
            // Unevaluated. They'll resolve to NonConst the first time
            // they're observed in env, which is the correct
            // conservative answer.
            TirStmtKind::LetDestructure { .. } => {}
            _ => {}
        }
    }

    /// Walk `block` (and every nested expression / statement) collecting
    /// every `Local` index that appears as the target of an `Assign`,
    /// then invalidate each in env. Conservative — any mutation inside
    /// the loop body is treated as making the local non-constant for
    /// the entire loop, which is the only sound choice without modelling
    /// loop iteration.
    fn invalidate_locals_assigned_in(&mut self, block: &TirBlock) {
        let mut collector = AssignedLocalsCollector {
            targets: IndexSet::default(),
        };
        collector.visit_block(block);
        for idx in collector.targets {
            self.interpreter.invalidate_local(idx);
        }
    }
}

/// Read-only walk that records every `Local` index assigned to inside
/// the visited subtree. Drives the loop back-edge invalidation in
/// [`ConstFoldVisitor::invalidate_locals_assigned_in`].
struct AssignedLocalsCollector {
    targets: IndexSet<u32>,
}

impl TirRefVisitor for AssignedLocalsCollector {
    fn visit_expr(&mut self, expr: &TirExpr) {
        if let TirExprKind::Assign { target, .. } = &expr.kind
            && let TirExprKind::Local { index, .. } = &target.kind
        {
            self.targets.insert(*index);
        }
        self.walk_expr(expr);
    }
}
