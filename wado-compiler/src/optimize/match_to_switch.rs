//! Match → Switch optimization.
//!
//! Rewrites dense-int / dense-enum `Match` expressions into `Switch`, which
//! lowers to a Wasm `br_table`. Faster than the generic match if-chain when
//! the value space is dense and the match has no guards.
//!
//! Before WEP 2026-05-11's "Match canonical" direction, this rewrite ran
//! during TIR → NIR lowering (`lower::translate::switch`). Moving it into
//! the optimizer keeps `lower::translate` emitting a single canonical
//! `Match` shape and treats `Switch` as a codegen-friendly optimised form
//! the optimizer materialises.

use std::cell::RefCell;
use std::rc::Rc;

use crate::module_source::ModuleSource;
use crate::nir::{
    FunctionRef, NirBlock, NirExpr, NirExprKind, NirLiteralPattern, NirMatchArm, NirPattern,
    NirStmt, NirStmtKind,
};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirOptVisitor, opt_walk_expr};
use crate::tir::{PrimitiveType, ResolvedType, TypeTable};
use crate::token::Span;

/// Minimum number of literal arms required for the `br_table` rewrite
/// to be worthwhile.
const SWITCH_MIN_CASES: usize = 8;

/// Minimum density (cases / range) for `br_table` to be worthwhile.
const SWITCH_DENSITY_THRESHOLD: f64 = 0.75;

/// Maximum range size for `br_table` (to avoid huge jump tables).
const SWITCH_MAX_RANGE: i64 = 1024;

/// Walk every NIR expression position and rewrite dense-int / dense-enum
/// `Match` expressions to `Switch`. Returns `true` if any rewrite fired.
pub fn match_to_switch(project: &mut NirPackage) -> bool {
    let mut visitor = MatchToSwitchVisitor {
        type_table: Rc::clone(&project.type_table),
    };
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(mut body) = func.body_block() {
            changed |= visitor.visit_block(&mut body);
            func.set_body_block(body);
        }
        for param in &mut func.params {
            if let Some(ref mut default) = param.default_expr {
                changed |= visitor.visit_expr(default);
            }
        }
    }
    for global in &mut project.globals {
        changed |= visitor.visit_expr(&mut global.initializer);
    }
    for s in &mut project.structs {
        for field in &mut s.fields {
            if let Some(ref mut default) = field.default_expr {
                changed |= visitor.visit_expr(default);
            }
        }
    }
    changed
}

struct MatchToSwitchVisitor {
    type_table: Rc<RefCell<TypeTable>>,
}

impl NirOptVisitor for MatchToSwitchVisitor {
    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        // Descend first so nested `Match` expressions in arm bodies are
        // rewritten before the outer `Match` is analysed.
        let mut changed = opt_walk_expr(self, expr);
        if let NirExprKind::Match {
            expr: scrutinee,
            arms,
        } = &mut expr.kind
        {
            let scrut_resolved = self.type_table.borrow().get(scrutinee.type_id).clone();
            if let Some(analysis) = analyze(&scrut_resolved, arms) {
                expr.kind = build_switch(scrutinee, arms, analysis, expr.span);
                changed = true;
            }
        }
        changed
    }
}

/// Analysis result for converting `Match` to `Switch`.
struct SwitchAnalysis {
    min_value: i64,
    max_value: i64,
    /// `(value, original_arm_index)` for each literal/enum case.
    value_to_arm: Vec<(i64, usize)>,
    /// Index of the wildcard arm, if any.
    default_arm: Option<usize>,
}

/// Analyze whether a `Match` can be rewritten into a `Switch`. Accepts
/// only integer / enum scrutinees with guard-less arms whose patterns
/// are integer literals, enum cases, or wildcard (the default).
fn analyze(scrutinee_type: &ResolvedType, arms: &[NirMatchArm]) -> Option<SwitchAnalysis> {
    match scrutinee_type {
        ResolvedType::Primitive(
            PrimitiveType::I32
            | PrimitiveType::U32
            | PrimitiveType::I64
            | PrimitiveType::U64
            | PrimitiveType::I16
            | PrimitiveType::U16
            | PrimitiveType::I8
            | PrimitiveType::U8,
        )
        | ResolvedType::Enum { .. } => {}
        _ => return None,
    }

    let mut value_to_arm: Vec<(i64, usize)> = Vec::new();
    let mut default_arm: Option<usize> = None;

    for (arm_idx, arm) in arms.iter().enumerate() {
        if arm.guard.is_some() {
            return None;
        }
        match &arm.pattern {
            NirPattern::Literal(NirLiteralPattern::I128(v)) => {
                // Bail if the literal does not fit in `i64`: the Switch
                // dispatch operates on `i64` case values, so a wrapping
                // cast would corrupt the min/max range analysis.
                let v = i64::try_from(*v).ok()?;
                value_to_arm.push((v, arm_idx));
            }
            NirPattern::Literal(NirLiteralPattern::U128(v)) => {
                let v = i64::try_from(*v).ok()?;
                value_to_arm.push((v, arm_idx));
            }
            NirPattern::Enum { case_index, .. } => {
                value_to_arm.push((i64::from(*case_index), arm_idx));
            }
            NirPattern::Wildcard => {
                if default_arm.is_some() {
                    return None;
                }
                default_arm = Some(arm_idx);
            }
            // A `Binding` default arm (`n => use(n)`) would need an
            // arm-local `Let n = scrutinee` that `build_switch` doesn't
            // emit. The normal `Match` lowering path handles bindings
            // correctly, so bail out of the Switch rewrite here.
            _ => return None,
        }
    }

    if value_to_arm.len() < SWITCH_MIN_CASES {
        return None;
    }

    let min_value = value_to_arm.iter().map(|(v, _)| *v).min().unwrap();
    let max_value = value_to_arm.iter().map(|(v, _)| *v).max().unwrap();
    let range = max_value - min_value + 1;

    if range > SWITCH_MAX_RANGE {
        return None;
    }

    let density = value_to_arm.len() as f64 / range as f64;
    if density < SWITCH_DENSITY_THRESHOLD {
        return None;
    }

    Some(SwitchAnalysis {
        min_value,
        max_value,
        value_to_arm,
        default_arm,
    })
}

/// Build a `Switch` expression kind from the analysis. The scrutinee is
/// moved out of the original `Match` node; arm bodies are cloned because
/// the same arm can appear at multiple offsets (when there is no default
/// arm, holes fall back to arm 0, which is unreachable for those values).
fn build_switch(
    scrutinee: &mut Box<NirExpr>,
    arms: &[NirMatchArm],
    analysis: SwitchAnalysis,
    span: Span,
) -> NirExprKind {
    let range = (analysis.max_value - analysis.min_value + 1) as usize;

    let mut offset_to_arm: Vec<Option<usize>> = vec![None; range];
    for (value, arm_idx) in &analysis.value_to_arm {
        let offset = (*value - analysis.min_value) as usize;
        offset_to_arm[offset] = Some(*arm_idx);
    }

    let switch_arms: Vec<NirBlock> = offset_to_arm
        .iter()
        .map(|maybe_arm_idx| {
            let arm_idx = maybe_arm_idx.unwrap_or_else(|| analysis.default_arm.unwrap_or(0));
            let arm = &arms[arm_idx];
            NirBlock {
                stmts: vec![NirStmt::new(
                    NirStmtKind::Expr(arm.body.clone()),
                    arm.body.span,
                )],
                span: arm.body.span,
            }
        })
        .collect();

    let default_block = if let Some(default_idx) = analysis.default_arm {
        let arm = &arms[default_idx];
        NirBlock {
            stmts: vec![NirStmt::new(
                NirStmtKind::Expr(arm.body.clone()),
                arm.body.span,
            )],
            span: arm.body.span,
        }
    } else {
        // Call `builtin::unreachable` rather than
        // `core:internal/unreachable`: the former lowers to
        // `WirInstr::Unreachable` directly in `wir_build::calls.rs`
        // and is never DCE'd, so this pass can run after the
        // optimizer's pre-loop DCE without worrying about the
        // synthesised callee being removed.
        NirBlock {
            stmts: vec![NirStmt::new(
                NirStmtKind::Expr(NirExpr::new(
                    NirExprKind::Call {
                        func: FunctionRef {
                            module_source: ModuleSource::builtin(),
                            name: "unreachable".to_string(),
                            monomorph_info: None,
                            method_info: None,
                        },
                        args: vec![],
                        type_args: vec![],
                    },
                    TypeTable::NEVER,
                    span,
                )),
                span,
            )],
            span,
        }
    };

    // Steal the scrutinee out of the original `Match` node. The
    // placeholder is dropped along with the old `Match`-kind container
    // once the caller assigns the returned `NirExprKind::Switch` into
    // `expr.kind`.
    let placeholder = NirExpr::new(NirExprKind::Unit, TypeTable::UNIT, span);
    let moved_scrutinee = std::mem::replace(scrutinee.as_mut(), placeholder);

    NirExprKind::Switch {
        scrutinee: Box::new(moved_scrutinee),
        min_value: analysis.min_value,
        arms: switch_arms,
        default: default_block,
    }
}
