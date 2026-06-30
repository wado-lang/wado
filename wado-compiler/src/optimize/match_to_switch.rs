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
//! Global initializers are arena `ExprBody`s, so the same rule runs on each
//! global's body directly. The rule is confluent under the worklist's
//! bottom-up order: nested `Match`es in arm bodies are converted before the
//! outer one, so the cloned bodies hold `Switch`es and re-processing them is a
//! no-op.

use crate::module_source::ModuleSource;
use crate::nir::{FunctionRef, NirFunction, NirLiteralPattern};
use crate::nir_arena::{ArmData, BlockId, Body, ExprId, ExprKind, Operand, PatKind, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
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

/// Ungated: lower every function (and global). Used at `-O0`, where the loop
/// (and thus the gate) is skipped — avoids building a throwaway `FunctionGate`
/// (a full call-graph walk) just to satisfy the gated signature.
pub fn match_to_switch_all(project: &mut NirPackage) -> bool {
    let (cold_path_id, unreachable_id) = intern_cold_markers(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let type_table = project.type_table.borrow();
    let rule = MatchToSwitchRule::new(&type_table, cold_path_id, unreachable_id);
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let NirFunction { body, locals, .. } = &mut *func;
        if let Some(body) = body.as_mut() {
            let mut engine = Engine::new(body, &mut buffers, locals);
            engine.set_value_graph_type_table(&type_table);
            engine.set_pure_builtin_callees(&pure_builtin_callees);
            changed |= engine.run(&[&rule]);
        }
    }
    changed
        | run_globals(
            &mut project.globals,
            &rule,
            &type_table,
            &pure_builtin_callees,
            &mut buffers,
        )
}

/// Lower dense `Match` → `Switch` in global initializer bodies only. Functions
/// are handled inside the unified peephole session (`MatchToSwitchRule` is one
/// of its rules); globals are not, so the fixed-point driver runs this once.
/// Global initializer bodies are not mutated by the function-level loop, so a
/// single pass is equivalent to running it every iteration.
pub(super) fn match_to_switch_globals(project: &mut NirPackage) -> bool {
    let (cold_path_id, unreachable_id) = intern_cold_markers(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let type_table = project.type_table.borrow();
    let rule = MatchToSwitchRule::new(&type_table, cold_path_id, unreachable_id);
    let mut buffers = EngineBuffers::default();
    run_globals(
        &mut project.globals,
        &rule,
        &type_table,
        &pure_builtin_callees,
        &mut buffers,
    )
}

fn run_globals(
    globals: &mut [crate::nir::NirGlobal],
    rule: &MatchToSwitchRule,
    type_table: &TypeTable,
    pure_builtin_callees: &crate::hashmap::IndexSet<crate::nir::FuncId>,
    buffers: &mut EngineBuffers,
) -> bool {
    let mut changed = false;
    // Global initializer bodies have no owning function — and no locals (any
    // runtime-computed local is hoisted into `__initialize_module`). Lend an
    // empty scratch list, reused across globals; `MatchToSwitchRule` never
    // allocates, so it stays empty.
    let mut no_locals: Vec<crate::nir::NirLocal> = Vec::new();
    for global in globals {
        let mut engine = Engine::new(global.initializer.body_mut(), buffers, &mut no_locals);
        engine.set_value_graph_type_table(type_table);
        engine.set_pure_builtin_callees(pure_builtin_callees);
        changed |= engine.run(&[rule]);
    }
    changed
}

pub(super) struct MatchToSwitchRule<'t> {
    type_table: &'t TypeTable,
    cold_path_id: crate::nir::FuncId,
    unreachable_id: crate::nir::FuncId,
}

impl<'t> MatchToSwitchRule<'t> {
    pub(super) fn new(
        type_table: &'t TypeTable,
        cold_path_id: crate::nir::FuncId,
        unreachable_id: crate::nir::FuncId,
    ) -> Self {
        Self {
            type_table,
            cold_path_id,
            unreachable_id,
        }
    }
}

/// Intern the `cold_path` / `unreachable` builtins this pass synthesizes for the
/// default arm of an exhaustive match, returning their `FuncId`s so the
/// synthesized calls are born resolved.
pub(super) fn intern_cold_markers(
    project: &mut NirPackage,
) -> (crate::nir::FuncId, crate::nir::FuncId) {
    let cold_path_id = project.intern_extern(&FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "cold_path".to_string(),
        monomorph_info: None,
        method_info: None,
    });
    let unreachable_id = project.intern_extern(&FunctionRef {
        module_source: ModuleSource::builtin(),
        name: "unreachable".to_string(),
        monomorph_info: None,
        method_info: None,
    });
    (cold_path_id, unreachable_id)
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

        let scrut_type = engine.body.operand_type(scrutinee);
        let scrut_resolved = self.type_table.get(scrut_type);
        let Some(analysis) = analyze(scrut_resolved, &arms, &*engine.body) else {
            return false;
        };

        let span = engine.body.exprs[id].span;
        let new_kind = build_switch(
            engine,
            scrutinee,
            &arms,
            analysis,
            span,
            self.cold_path_id,
            self.unreachable_id,
        );
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

enum CaseSpec {
    Value(i64),
    Range { lo: i64, hi: i64 },
}

/// Analyze whether a `Match` can be rewritten into a `Switch`. Accepts
/// integer / `char` / enum scrutinees with guard-less arms whose patterns
/// are integer or `char` literals, enum cases, integer/`char` ranges, or
/// wildcard (the default).
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
            | PrimitiveType::U8
            | PrimitiveType::Char,
        )
        | ResolvedType::Enum { .. } => {}
        _ => return None,
    }

    let mut specs: Vec<(CaseSpec, usize)> = Vec::new();
    let mut default_arm: Option<usize> = None;
    let mut min_value = i64::MAX;
    let mut max_value = i64::MIN;

    for (arm_idx, arm) in arms.iter().enumerate() {
        if arm.guard.is_some() {
            return None;
        }
        let mut record = |lo: i64, hi: i64, specs: &mut Vec<(CaseSpec, usize)>| {
            min_value = min_value.min(lo);
            max_value = max_value.max(hi);
            specs.push((
                if lo == hi {
                    CaseSpec::Value(lo)
                } else {
                    CaseSpec::Range { lo, hi }
                },
                arm_idx,
            ));
        };
        match &body.pats[arm.pattern].kind {
            // Bail if a literal does not fit in `i64`: the Switch dispatch
            // operates on `i64` case values, so a wrapping cast would corrupt
            // the min/max range analysis.
            PatKind::Literal(NirLiteralPattern::I128(v)) => {
                let v = i64::try_from(*v).ok()?;
                record(v, v, &mut specs);
            }
            PatKind::Literal(NirLiteralPattern::U128(v)) => {
                let v = i64::try_from(*v).ok()?;
                record(v, v, &mut specs);
            }
            PatKind::Literal(NirLiteralPattern::Char(c)) => {
                let v = i64::from(u32::from(*c));
                record(v, v, &mut specs);
            }
            PatKind::Enum { case_index, .. } => {
                let v = i64::from(*case_index);
                record(v, v, &mut specs);
            }
            PatKind::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                let lo = i64::try_from(*start).ok()?;
                let mut hi = i64::try_from(*end).ok()?;
                if !*inclusive {
                    hi -= 1;
                }
                if hi < lo {
                    // Empty range — let the generic lowering handle it.
                    return None;
                }
                record(lo, hi, &mut specs);
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

    if specs.is_empty() {
        return None;
    }

    let range = i128::from(max_value) - i128::from(min_value) + 1;
    if range > i128::from(SWITCH_MAX_RANGE) {
        return None;
    }

    if specs.len() < 2 {
        return None;
    }

    let covered: i128 = specs
        .iter()
        .map(|(spec, _)| match spec {
            CaseSpec::Value(_) => 1,
            CaseSpec::Range { lo, hi } => i128::from(*hi) - i128::from(*lo) + 1,
        })
        .sum();
    if covered < SWITCH_MIN_CASES as i128 {
        return None;
    }
    if (covered as f64) / (range as f64) < SWITCH_DENSITY_THRESHOLD {
        return None;
    }

    let mut value_to_arm: Vec<(i64, usize)> = Vec::new();
    for (spec, arm_idx) in &specs {
        match spec {
            CaseSpec::Value(v) => value_to_arm.push((*v, *arm_idx)),
            CaseSpec::Range { lo, hi } => {
                for v in *lo..=*hi {
                    value_to_arm.push((v, *arm_idx));
                }
            }
        }
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
    scrutinee: Operand,
    arms: &[ArmData],
    analysis: SwitchAnalysis,
    span: Span,
    cold_path_id: crate::nir::FuncId,
    unreachable_id: crate::nir::FuncId,
) -> ExprKind {
    let range = (analysis.max_value - analysis.min_value + 1) as usize;

    let mut offset_to_arm: Vec<Option<usize>> = vec![None; range];
    for (value, arm_idx) in &analysis.value_to_arm {
        let offset = (*value - analysis.min_value) as usize;
        if offset_to_arm[offset].is_none() {
            offset_to_arm[offset] = Some(*arm_idx);
        }
    }

    let switch_arms: Vec<BlockId> = offset_to_arm
        .iter()
        .map(|maybe_arm_idx| {
            let arm_idx = maybe_arm_idx.unwrap_or_else(|| analysis.default_arm.unwrap_or(0));
            arm_body_block(engine, arms[arm_idx].body, arms[arm_idx].span)
        })
        .collect();

    let default_block = if let Some(default_idx) = analysis.default_arm {
        arm_body_block(engine, arms[default_idx].body, arms[default_idx].span)
    } else {
        // The default of an exhaustive match is the unreachable arm. Mark it
        // `cold_path()` so the inliner skips it and codegen hints it unlikely,
        // matching the other compiler-synthesized cold branches.
        let cold_call = engine.alloc_expr(
            ExprKind::Call {
                func_id: cold_path_id,
                args: vec![],
                type_args: vec![],
            },
            TypeTable::UNIT,
            span,
        );
        let cold_stmt = engine.alloc_stmt(StmtKind::Expr(cold_call.into()), span);
        // Call `builtin::unreachable` rather than
        // `core:rt/unreachable`: the former lowers to
        // `WirInstr::Unreachable` directly in `wir_build::calls.rs`
        // and is never DCE'd, so this pass can run after the
        // optimizer's pre-loop DCE without worrying about the
        // synthesised callee being removed.
        let call = engine.alloc_expr(
            ExprKind::Call {
                func_id: unreachable_id,
                args: vec![],
                type_args: vec![],
            },
            TypeTable::NEVER,
            span,
        );
        let stmt = engine.alloc_stmt(StmtKind::Expr(call.into()), span);
        engine.alloc_block(vec![cold_stmt, stmt], span)
    };

    ExprKind::Switch {
        scrutinee,
        min_value: analysis.min_value,
        arms: switch_arms,
        default: default_block,
    }
}

/// Wrap an arm body in a fresh block holding a single `Expr` statement. A
/// skeleton body is deep-cloned (each arm needs its own copy); a promoted
/// constant operand is immutable and shareable, so it flows straight into the
/// statement slot.
fn arm_body_block(engine: &mut Engine, body: Operand, arm_span: Span) -> BlockId {
    let (op, span) = match body {
        Operand::Expr(e) => (
            Operand::Expr(engine.clone_expr(e)),
            engine.body.exprs[e].span,
        ),
        Operand::Value(_) => (body, arm_span),
    };
    let stmt = engine.alloc_stmt(StmtKind::Expr(op), span);
    engine.alloc_block(vec![stmt], span)
}
