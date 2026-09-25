//! Lift `mut` bindings out of `Match` arm and `IfLet` patterns into
//! explicit `Let mut` statements at the arm body / then-block start.
//!
//! Runs as a pre-pass, not inside `translate::pattern`: the fold decides the
//! lifted `Let mut`'s copy, so the statement must exist before it runs.

use crate::flat_package::FlatPackage;
use crate::name::minted_name;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirLocal, TirMatchArm, TirPattern, TirStmt, TirStmtKind,
    TirStructPatternField, TypeId, TypeTable,
};
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr, opt_walk_stmt, remap_local_reads};
use crate::token::Span;

/// Idempotent: a second walk finds no `mut` bindings to lift.
pub fn lift_mut_match_bindings(project: &mut FlatPackage) {
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let local_count = func.local_count;
        let locals = std::mem::take(&mut func.locals);
        let mut lifter = MutBindingLifter {
            local_count,
            locals,
        };
        if let Some(body) = func.body.as_mut() {
            lifter.visit_block(body);
        }
        func.local_count = lifter.local_count;
        func.locals = lifter.locals;
    }
}

struct MutBindingLifter {
    local_count: u32,
    locals: Vec<TirLocal>,
}

impl MutBindingLifter {
    fn alloc_local(&mut self, type_id: TypeId) -> u32 {
        let index = self.local_count;
        self.local_count += 1;
        self.locals.push(TirLocal::synth(index, type_id, false));
        index
    }

    fn local_is_mut(&self, local_index: u32) -> bool {
        self.locals
            .get(local_index as usize)
            .is_some_and(|l| l.is_mut)
    }

    /// The fresh local standing in for `original`. Or-pattern alternatives
    /// bind one name, so every alternative shares the first one's local.
    fn lift_local(
        &mut self,
        name: &str,
        original: u32,
        type_id: TypeId,
        span: Span,
        lifted: &mut LiftedBindings,
    ) -> u32 {
        if let Some(&(_, fresh)) = lifted.fresh.iter().find(|(o, _)| *o == original) {
            return fresh;
        }
        let fresh = self.alloc_local(type_id);
        lifted.fresh.push((original, fresh));
        lifted.lets.push(TirStmt::new(
            TirStmtKind::Let {
                name: name.to_string(),
                local_index: original,
                is_mut: true,
                is_reactive: false,
                type_id,
                value: TirExpr::new(
                    TirExprKind::Local {
                        index: fresh,
                        name: fresh_local_name(fresh),
                    },
                    type_id,
                    span,
                ),
                skip_value_copy: false,
            },
            span,
        ));
        fresh
    }

    /// Replace each `mut` binding in `arm.pattern` with a fresh non-mut local
    /// and prepend `let mut original = fresh` to the arm, which the fold then
    /// decides as it decides any other `Let`.
    fn lift_in_match_arm(&mut self, arm: &mut TirMatchArm) {
        let span = arm.span;
        let mut lifted = LiftedBindings::default();
        self.lift_in_pattern(&mut arm.pattern, span, &mut lifted);
        if lifted.lets.is_empty() {
            return;
        }
        // The guard runs first and its locals persist into the body, so a
        // guard gets the lets and the body shares its binding.
        match arm.guard.as_mut() {
            Some(guard) => prepend_stmts(guard, lifted.lets),
            None => prepend_stmts(&mut arm.body, lifted.lets),
        }
    }

    fn lift_in_pattern(
        &mut self,
        pattern: &mut TirPattern,
        span: Span,
        lifted: &mut LiftedBindings,
    ) {
        match pattern {
            TirPattern::Binding {
                name,
                local_index,
                type_id,
            } => {
                if !self.local_is_mut(*local_index) {
                    return;
                }
                let fresh_index = self.lift_local(name, *local_index, *type_id, span, lifted);
                *pattern = TirPattern::Binding {
                    name: fresh_local_name(fresh_index),
                    local_index: fresh_index,
                    type_id: *type_id,
                };
            }
            TirPattern::Variant { bindings, .. } => {
                for sub in bindings.iter_mut() {
                    self.lift_in_pattern(sub, span, lifted);
                }
            }
            TirPattern::Tuple(sub_patterns, _) => {
                for sub in sub_patterns.iter_mut() {
                    self.lift_in_pattern(sub, span, lifted);
                }
            }
            TirPattern::Struct { fields, .. } => {
                for TirStructPatternField { pattern, .. } in fields.iter_mut() {
                    self.lift_in_pattern(pattern, span, lifted);
                }
            }
            TirPattern::Or(alternatives) => {
                for alt in alternatives.iter_mut() {
                    self.lift_in_pattern(alt, span, lifted);
                }
            }
            TirPattern::Narrow {
                name: Some(name),
                local_index,
                type_id,
                test,
            } => {
                if !self.local_is_mut(*local_index) {
                    return;
                }
                let fresh_index = self.lift_local(name, *local_index, *type_id, span, lifted);
                remap_local_reads(test, *local_index, fresh_index);
                *local_index = fresh_index;
            }
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Narrow { name: None, .. }
            | TirPattern::Range { .. } => {}
        }
    }
}

/// An arm's lifted bindings: each original local's fresh stand-in, and the
/// `let mut original = fresh` statements that rebind them.
#[derive(Default)]
struct LiftedBindings {
    fresh: Vec<(u32, u32)>,
    lets: Vec<TirStmt>,
}

fn fresh_local_name(index: u32) -> String {
    minted_name("match_mut_lift", index)
}

fn prepend_stmts(expr: &mut TirExpr, prefix: Vec<TirStmt>) {
    let span = expr.span;
    let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span);
    let original = std::mem::replace(expr, placeholder);
    let type_id = original.type_id;
    let mut stmts = prefix;
    stmts.push(TirStmt::new(TirStmtKind::Expr(original), span));
    *expr = TirExpr::new(TirExprKind::Block(TirBlock { stmts, span }), type_id, span);
}

impl TirOptVisitor for MutBindingLifter {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        opt_walk_stmt(self, stmt);
        false
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // Pre-process Match arms: lift before recursing into arm
        // bodies. Or-pattern alternatives are handled by the recursion
        // inside `lift_in_pattern`.
        if let TirExprKind::Match { arms, .. } = &mut expr.kind {
            for arm in arms.iter_mut() {
                self.lift_in_match_arm(arm);
            }
        }
        opt_walk_expr(self, expr);
        false
    }
}
