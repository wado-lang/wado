//! Constant folding optimization for Wado TIR.
//!
//! Walks every function body and applies the [`tiri::Interpreter`]
//! rewrite rules at each visited node. All reduction logic
//! (literal folding, integer cast collapsing, short-circuit identity
//! rules) lives in [`crate::tiri`]; this module is only the visitor
//! glue that drives `reduce_local` across function bodies.

use crate::flat_package::FlatPackage;
use crate::tir::TirExpr;
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr};
use crate::tiri::Interpreter;

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
            changed |= visitor.visit_block(body);
        }
    }
    changed
}

struct ConstFoldVisitor<'a> {
    interpreter: Interpreter<'a>,
}

impl TirOptVisitor for ConstFoldVisitor<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: walk every child kind first (`opt_walk_expr` covers
        // If / Block / Match / Call / … in addition to the Binary /
        // Unary / Cast trees that the interpreter recurses into).
        // Then ask the interpreter to apply local rewrites at this node.
        let mut changed = opt_walk_expr(self, expr);
        changed |= self.interpreter.reduce_local(expr);
        changed
    }
}
