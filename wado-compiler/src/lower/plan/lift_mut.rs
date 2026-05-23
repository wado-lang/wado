//! Lift `mut` value-semantic bindings out of `Match` arm and `IfLet`
//! patterns into explicit `Let mut` statements at the start of the
//! corresponding arm body / then-block.
//!
//! Why this exists separately from pattern lowering: the lift produces
//! `let mut original_local = fresh_local;` statements that
//! [`super::value_copy::analyze::collect_seed_types`] records (and the
//! TIR → NIR fold then wraps with a `$value_copy$T(...)` call). Pattern
//! lowering itself runs inside the translator (WEP 2026-05-11 Phase 10
//! Step 2b), so the lift has to happen *before* `value_copy`'s seed
//! walker runs — otherwise the lifted Lets would be invisible to the
//! walker. The lifted `Let mut` is exactly the shape `value_copy::analyze`
//! keys on.
//!
//! Mirror of the previous `PatternLowerer::lift_mut_payload_bindings` /
//! `lift_mut_in_pattern` (the latter recurses through compound patterns
//! so or-pattern alternatives and nested destructures all get the same
//! lift).

use crate::flat_package::FlatPackage;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirLocal, TirMatchArm, TirPattern, TirStmt, TirStmtKind,
    TirStructPatternField, TypeId, TypeTable,
};
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr, opt_walk_stmt};
use crate::token::Span;

/// Walk every function body and lift `mut` payload bindings inside
/// `Match` arms and `IfLet` patterns. After this runs, every `mut`
/// binding inside a pattern has been replaced with a fresh non-mut
/// binding, and a `let mut original = fresh` statement is prepended to
/// the corresponding arm body / then-block. Idempotent — running twice
/// is a no-op because the second walk finds no `mut` bindings to lift.
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

    /// Walk `arm.pattern` looking for `Binding`s whose local is `mut`.
    /// For each, allocate a fresh non-mut local of the same type, swap
    /// it into the pattern slot, and prepend `let mut original_local =
    /// fresh_local` to the arm body. The fresh local lands in the
    /// pattern slot so `wir_build::pattern_match::emit_pattern_bindings`
    /// writes the variant payload into a private slot; the `Let mut`
    /// is then picked up by `value_copy::analyze` (the fold wraps it
    /// in `$value_copy$T(...)`) and lands in the user-visible binding.
    fn lift_in_match_arm(&mut self, arm: &mut TirMatchArm) {
        let span = arm.span;
        let mut prefix_stmts: Vec<TirStmt> = Vec::new();
        self.lift_in_pattern(&mut arm.pattern, span, &mut prefix_stmts);
        if prefix_stmts.is_empty() {
            return;
        }
        let body_span = arm.body.span;
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, body_span);
        let original_body = std::mem::replace(&mut arm.body, placeholder);
        let original_type = original_body.type_id;
        let mut stmts = prefix_stmts;
        stmts.push(TirStmt::new(TirStmtKind::Expr(original_body), body_span));
        arm.body = TirExpr::new(
            TirExprKind::Block(TirBlock {
                stmts,
                span: body_span,
            }),
            original_type,
            body_span,
        );
    }

    fn lift_in_pattern(
        &mut self,
        pattern: &mut TirPattern,
        span: Span,
        prefix_stmts: &mut Vec<TirStmt>,
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
                let fresh_index = self.alloc_local(*type_id);
                let original_name = name.clone();
                let original_index = *local_index;
                let original_type = *type_id;
                let fresh_name = format!("__match_mut_lift_{fresh_index}");
                prefix_stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: original_name,
                        local_index: original_index,
                        is_mut: true,
                        is_reactive: false,
                        type_id: original_type,
                        value: TirExpr::new(
                            TirExprKind::Local {
                                index: fresh_index,
                                name: fresh_name.clone(),
                            },
                            original_type,
                            span,
                        ),
                        skip_value_copy: false,
                    },
                    span,
                ));
                *pattern = TirPattern::Binding {
                    name: fresh_name,
                    local_index: fresh_index,
                    type_id: original_type,
                };
            }
            TirPattern::Variant { bindings, .. } => {
                for sub in bindings.iter_mut() {
                    self.lift_in_pattern(sub, span, prefix_stmts);
                }
            }
            TirPattern::Tuple(sub_patterns, _) => {
                for sub in sub_patterns.iter_mut() {
                    self.lift_in_pattern(sub, span, prefix_stmts);
                }
            }
            TirPattern::Struct { fields, .. } => {
                for TirStructPatternField { pattern, .. } in fields.iter_mut() {
                    self.lift_in_pattern(pattern, span, prefix_stmts);
                }
            }
            TirPattern::Or(alternatives) => {
                for alt in alternatives.iter_mut() {
                    self.lift_in_pattern(alt, span, prefix_stmts);
                }
            }
            TirPattern::Wildcard
            | TirPattern::Literal(_)
            | TirPattern::Enum { .. }
            | TirPattern::ConstantValue { .. }
            | TirPattern::Range { .. } => {}
        }
    }
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
