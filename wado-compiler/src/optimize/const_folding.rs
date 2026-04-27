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
use crate::tir::{TirExpr, TirExprKind, TirStmt, TirStmtKind};
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr, opt_walk_stmt};
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
}
