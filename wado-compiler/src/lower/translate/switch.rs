//! Rewrite `match` on dense integer / enum scrutinees into a `Switch`
//! expression as the TIR → NIR translator walks them.
//!
//! `Switch` lowers to a `br_table` in Wasm, which is faster than the
//! generic match-arm if-chain when the value space is dense and the
//! match has no guards. The translator builds a TIR `Switch` and runs
//! it back through `convert_expr`, which then converts the TIR
//! `Switch` to `NirExprKind::Switch` via the normal passthrough.

use crate::module_source::ModuleSource;
use crate::tir::{
    FunctionRef, PrimitiveType, ResolvedType, TirBlock, TirExpr, TirExprKind, TirLiteralPattern,
    TirMatchArm, TirPattern, TirStmt, TirStmtKind, TypeId, TypeTable,
};
use crate::token::Span;

/// Minimum number of literal arms required for the `br_table` rewrite
/// to be worthwhile.
const SWITCH_MIN_CASES: usize = 8;

/// Minimum density (cases / range) for `br_table` to be worthwhile.
const SWITCH_DENSITY_THRESHOLD: f64 = 0.75;

/// Maximum range size for `br_table` (to avoid huge jump tables).
const SWITCH_MAX_RANGE: i64 = 1024;

/// Analysis result for converting `Match` to `Switch`.
pub(super) struct SwitchAnalysis {
    /// Minimum value in the switch range.
    min_value: i64,
    /// Maximum value in the switch range.
    max_value: i64,
    /// Map from value to arm index in the original arms.
    value_to_arm: Vec<(i64, usize)>,
    /// Index of the default arm (wildcard/binding), if any.
    default_arm: Option<usize>,
}

/// Analyze whether a `Match` expression can be rewritten into a
/// `Switch`. Accepts only integer / enum scrutinees with guard-less
/// arms whose patterns are integer literals, enum cases, or
/// wildcard / binding (the default).
pub(super) fn analyze(
    scrutinee_type: &ResolvedType,
    arms: &[TirMatchArm],
) -> Option<SwitchAnalysis> {
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
            TirPattern::Literal(TirLiteralPattern::I128(v)) => {
                value_to_arm.push((*v as i64, arm_idx));
            }
            TirPattern::Literal(TirLiteralPattern::U128(v)) => {
                value_to_arm.push((*v as i64, arm_idx));
            }
            TirPattern::Enum { case_index, .. } => {
                value_to_arm.push((i64::from(*case_index), arm_idx));
            }
            TirPattern::Wildcard => {
                if default_arm.is_some() {
                    return None;
                }
                default_arm = Some(arm_idx);
            }
            // A `Binding` default arm (`n => use(n)`) needs an
            // arm-local `Let n = scrutinee` that `build` doesn't
            // emit. Rather than special-case the emit, bail out of
            // the Switch rewrite — the normal `Match` lowering path
            // handles the binding correctly. Pre-9.C.1 the same gap
            // was present and accepted Binding here, silently zero-
            // initialising the bound local at runtime for matches
            // dense enough to trigger the Switch rewrite.
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

/// Build a TIR `Switch` expression from the analysis. The caller runs
/// the result back through `convert_expr` so that arm bodies are
/// converted via the normal walk.
pub(super) fn build(
    scrutinee: Box<TirExpr>,
    arms: &[TirMatchArm],
    analysis: SwitchAnalysis,
    result_type_id: TypeId,
    span: Span,
) -> TirExpr {
    let range = (analysis.max_value - analysis.min_value + 1) as usize;

    let mut offset_to_arm: Vec<Option<usize>> = vec![None; range];
    for (value, arm_idx) in &analysis.value_to_arm {
        let offset = (*value - analysis.min_value) as usize;
        offset_to_arm[offset] = Some(*arm_idx);
    }

    let switch_arms: Vec<TirBlock> = offset_to_arm
        .iter()
        .map(|maybe_arm_idx| {
            let arm_idx = maybe_arm_idx.unwrap_or_else(|| {
                // Values not in the literal map fall back to the
                // default arm if any; otherwise arm 0 (an unreachable
                // slot since `default_block` traps for those values).
                analysis.default_arm.unwrap_or(0)
            });
            let arm = &arms[arm_idx];
            TirBlock {
                stmts: vec![TirStmt::new(
                    TirStmtKind::Expr(arm.body.clone()),
                    arm.body.span,
                )],
                span: arm.body.span,
            }
        })
        .collect();

    let default_block = if let Some(default_idx) = analysis.default_arm {
        let arm = &arms[default_idx];
        TirBlock {
            stmts: vec![TirStmt::new(
                TirStmtKind::Expr(arm.body.clone()),
                arm.body.span,
            )],
            span: arm.body.span,
        }
    } else {
        TirBlock {
            stmts: vec![TirStmt::new(
                TirStmtKind::Expr(TirExpr::new(
                    TirExprKind::Call {
                        func: FunctionRef {
                            module_source: ModuleSource::internal(),
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

    TirExpr::new(
        TirExprKind::Switch {
            scrutinee,
            min_value: analysis.min_value,
            arms: switch_arms,
            default: default_block,
        },
        result_type_id,
        span,
    )
}
