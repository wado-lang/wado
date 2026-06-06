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
//!
//! This is the second peephole pass ported to the worklist rewrite engine
//! (Phase 4 stage C; see `docs/wep-2026-06-05-nir-rewrite-engine-design.md`):
//! it runs as a [`Rule`] over each function's arena `Body`. The arm bodies are
//! deep-cloned via the engine's edit API because the same arm can appear at
//! multiple `br_table` offsets, and the arena is a tree (one parent per node).
//! Param defaults, globals, and struct-field defaults are still tree-shaped
//! NIR, so they reuse the same rule through a wrap-in-`Body` helper rather than
//! a second copy of the logic. The rule is confluent under the worklist's
//! bottom-up order: nested `Match`es in arm bodies are converted before the
//! outer one, so the cloned bodies hold `Switch`es and re-processing them is a
//! no-op.

use crate::module_source::ModuleSource;
use crate::nir::{
    FunctionRef, NirBlock, NirExpr, NirExprKind, NirLiteralPattern, NirStmt, NirStmtKind,
};
use crate::nir_arena::{ArmData, BlockId, Body, ExprId, ExprKind, PatKind, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::nir_package::NirPackage;
use crate::tir::{PrimitiveType, ResolvedType, TypeTable};
use crate::token::Span;

/// Minimum number of literal arms required for the `br_table` rewrite
/// to be worthwhile.
const SWITCH_MIN_CASES: usize = 8;

/// Minimum density (cases / range) for `br_table` to be worthwhile.
const SWITCH_DENSITY_THRESHOLD: f64 = 0.75;

/// Maximum range size for `br_table` (to avoid huge jump tables).
const SWITCH_MAX_RANGE: i64 = 1024;

/// Rewrite dense-int / dense-enum `Match` expressions to `Switch`, driven by
/// the rewrite engine. Returns `true` if any rewrite fired.
pub fn match_to_switch(project: &mut NirPackage) -> bool {
    let type_table = project.type_table.borrow();
    let rule = MatchToSwitchRule {
        type_table: &type_table,
    };
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            let mut engine = Engine::new(body);
            changed |= engine.run(&[&rule]);
        }
        for param in &mut func.params {
            if let Some(ref mut default) = param.default_expr {
                changed |= run_rule_on_tree_expr(default, &rule);
            }
        }
    }
    for global in &mut project.globals {
        changed |= run_rule_on_tree_expr(&mut global.initializer, &rule);
    }
    for s in &mut project.structs {
        for field in &mut s.fields {
            if let Some(ref mut default) = field.default_expr {
                changed |= run_rule_on_tree_expr(default, &rule);
            }
        }
    }
    changed
}

/// Run an engine rule over a standalone tree-shaped NIR expression by wrapping
/// it in a temporary single-statement `Body`, running the engine, and
/// unwrapping the result. Used for the NIR positions that are not yet arena
/// bodies — param defaults, global initializers, struct-field defaults — so the
/// rule logic lives in exactly one place.
fn run_rule_on_tree_expr(expr: &mut NirExpr, rule: &dyn Rule) -> bool {
    let span = expr.span;
    let placeholder = NirExpr::new(NirExprKind::Unit, TypeTable::UNIT, span);
    let owned = std::mem::replace(expr, placeholder);
    let block = NirBlock::new(vec![NirStmt::new(NirStmtKind::Expr(owned), span)], span);
    let mut body = Body::from_block(&block);
    let changed = {
        let mut engine = Engine::new(&mut body);
        engine.run(&[rule])
    };
    let new_block = body.to_block();
    let stmt = new_block
        .stmts
        .into_iter()
        .next()
        .expect("wrapper block has one statement");
    let NirStmtKind::Expr(new_expr) = stmt.kind else {
        unreachable!("wrapper statement is an expression statement");
    };
    *expr = new_expr;
    changed
}

struct MatchToSwitchRule<'t> {
    type_table: &'t TypeTable,
}

impl Rule for MatchToSwitchRule<'_> {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let ExprKind::Match {
            expr: scrutinee,
            arms,
        } = &engine.body.exprs[id].kind
        else {
            return false;
        };
        let scrutinee = *scrutinee;
        let arms = arms.clone();

        let scrut_type = engine.body.exprs[scrutinee].type_id;
        let scrut_resolved = self.type_table.get(scrut_type);
        let Some(analysis) = analyze(scrut_resolved, &arms, &*engine.body) else {
            return false;
        };

        let span = engine.body.exprs[id].span;
        let new_kind = build_switch(engine, scrutinee, &arms, analysis, span);
        engine.replace_expr_kind(id, new_kind);
        true
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
fn analyze(scrutinee_type: &ResolvedType, arms: &[ArmData], body: &Body) -> Option<SwitchAnalysis> {
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
        match &body.pats[arm.pattern].kind {
            PatKind::Literal(NirLiteralPattern::I128(v)) => {
                // Bail if the literal does not fit in `i64`: the Switch
                // dispatch operates on `i64` case values, so a wrapping
                // cast would corrupt the min/max range analysis.
                let v = i64::try_from(*v).ok()?;
                value_to_arm.push((v, arm_idx));
            }
            PatKind::Literal(NirLiteralPattern::U128(v)) => {
                let v = i64::try_from(*v).ok()?;
                value_to_arm.push((v, arm_idx));
            }
            PatKind::Enum { case_index, .. } => {
                value_to_arm.push((i64::from(*case_index), arm_idx));
            }
            PatKind::Wildcard => {
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

/// Build a `Switch` expression kind from the analysis. The scrutinee id is
/// reused directly (it appears once); arm bodies are deep-cloned because the
/// same arm can appear at multiple offsets (when there is no default arm,
/// holes fall back to arm 0, which is unreachable for those values).
fn build_switch(
    engine: &mut Engine,
    scrutinee: ExprId,
    arms: &[ArmData],
    analysis: SwitchAnalysis,
    span: Span,
) -> ExprKind {
    let range = (analysis.max_value - analysis.min_value + 1) as usize;

    let mut offset_to_arm: Vec<Option<usize>> = vec![None; range];
    for (value, arm_idx) in &analysis.value_to_arm {
        let offset = (*value - analysis.min_value) as usize;
        offset_to_arm[offset] = Some(*arm_idx);
    }

    let switch_arms: Vec<BlockId> = offset_to_arm
        .iter()
        .map(|maybe_arm_idx| {
            let arm_idx = maybe_arm_idx.unwrap_or_else(|| analysis.default_arm.unwrap_or(0));
            arm_body_block(engine, arms[arm_idx].body)
        })
        .collect();

    let default_block = if let Some(default_idx) = analysis.default_arm {
        arm_body_block(engine, arms[default_idx].body)
    } else {
        // The default of an exhaustive match is the unreachable arm. Mark it
        // `cold_path()` so the inliner skips it and codegen hints it unlikely,
        // matching the other compiler-synthesized cold branches.
        let cold_call = engine.alloc_expr(
            ExprKind::Call {
                func: FunctionRef {
                    module_source: ModuleSource::builtin(),
                    name: "cold_path".to_string(),
                    monomorph_info: None,
                    method_info: None,
                },
                args: vec![],
                type_args: vec![],
            },
            TypeTable::UNIT,
            span,
        );
        let cold_stmt = engine.alloc_stmt(StmtKind::Expr(cold_call), span);
        // Call `builtin::unreachable` rather than
        // `core:internal/unreachable`: the former lowers to
        // `WirInstr::Unreachable` directly in `wir_build::calls.rs`
        // and is never DCE'd, so this pass can run after the
        // optimizer's pre-loop DCE without worrying about the
        // synthesised callee being removed.
        let call = engine.alloc_expr(
            ExprKind::Call {
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
        );
        let stmt = engine.alloc_stmt(StmtKind::Expr(call), span);
        engine.alloc_block(vec![cold_stmt, stmt], span)
    };

    ExprKind::Switch {
        scrutinee,
        min_value: analysis.min_value,
        arms: switch_arms,
        default: default_block,
    }
}

/// Deep-clone an arm body expression and wrap it in a fresh single-statement
/// block, mirroring the `NirBlock { stmts: [Expr(body)] }` the tree pass built.
fn arm_body_block(engine: &mut Engine, body: ExprId) -> BlockId {
    let clone = engine.clone_expr(body);
    let span = engine.body.exprs[clone].span;
    let stmt = engine.alloc_stmt(StmtKind::Expr(clone), span);
    engine.alloc_block(vec![stmt], span)
}
