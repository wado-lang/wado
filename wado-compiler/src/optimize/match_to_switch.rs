//! Match → Switch: a dense-int or dense-enum guardless `Match` becomes a
//! `Switch`, lowering to a Wasm `br_table` rather than the generic if-chain.
//! Kept in the optimizer rather than lowering (WEP 2026-05-11), so
//! `lower::translate` emits one canonical `Match` shape. The `br_table` sends
//! many offsets to one arm body.

use crate::hashmap;
use crate::module_source::ModuleSource;
use crate::nir::{FuncId, FunctionRef, NirFunction, NirGlobal, NirLiteralPattern, NirLocal};
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, Operand, PatId, PatKind, StmtKind,
};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::primitive::PrimitiveType;
use crate::tir::{ResolvedType, TypeTable};
use crate::token::Span;

/// Minimum values a `br_table` must cover to be worth it — one range arm can
/// reach it alone. It replaces a cascade the predictor gets right with a single
/// indirect branch, so it pays only once that cascade is long.
const SWITCH_MIN_CASES: usize = 12;

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
    globals: &mut [NirGlobal],
    rule: &MatchToSwitchRule,
    type_table: &TypeTable,
    pure_builtin_callees: &hashmap::IndexSet<FuncId>,
    buffers: &mut EngineBuffers,
) -> bool {
    let mut changed = false;
    // Global initializer bodies have no owning function — and no locals (any
    // runtime-computed local is hoisted into `$initialize_module`). Lend an
    // empty scratch list, reused across globals; `MatchToSwitchRule` never
    // allocates, so it stays empty.
    let mut no_locals: Vec<NirLocal> = Vec::new();
    for global in globals {
        let mut engine = Engine::new(
            global.init.slot_expr_mut().body_mut(),
            buffers,
            &mut no_locals,
        );
        engine.set_value_graph_type_table(type_table);
        engine.set_pure_builtin_callees(pure_builtin_callees);
        changed |= engine.run(&[rule]);
    }
    changed
}

pub(super) struct MatchToSwitchRule<'t> {
    type_table: &'t TypeTable,
    cold_path_id: FuncId,
    unreachable_id: FuncId,
}

impl<'t> MatchToSwitchRule<'t> {
    pub(super) fn new(
        type_table: &'t TypeTable,
        cold_path_id: FuncId,
        unreachable_id: FuncId,
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
pub(super) fn intern_cold_markers(project: &mut NirPackage) -> (FuncId, FuncId) {
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
    /// Per value from `min_value`, the first arm naming it.
    offset_arm: Vec<Option<usize>>,
    /// Index of the wildcard arm, if any.
    default_arm: Option<usize>,
}

/// The values one arm's pattern names, as `i64` keys.
enum CaseKey {
    /// Each inclusive at both ends, `lo <= hi`. A literal is the one-value span.
    Spans(Vec<(i64, i64)>),
    Wildcard,
}

/// `pat` as a [`CaseKey`], or `None` for one no key dispatch takes: a binding,
/// which would need an arm-local `let` of the scrutinee; a bound past `i64`,
/// where a wrapping cast would corrupt the range; or any destructuring
/// pattern. An or-pattern names the union of its alternatives.
fn case_key(body: &Body, pat: PatId) -> Option<CaseKey> {
    let one = |v| CaseKey::Spans(vec![(v, v)]);
    Some(match &body.pats[pat].kind {
        PatKind::Literal(NirLiteralPattern::I128(v)) => one(i64::try_from(*v).ok()?),
        PatKind::Literal(NirLiteralPattern::U128(v)) => one(i64::try_from(*v).ok()?),
        PatKind::Literal(NirLiteralPattern::Char(c)) => one(i64::from(u32::from(*c))),
        PatKind::Enum { case_index, .. } => one(i64::from(*case_index)),
        PatKind::Range {
            start,
            end,
            inclusive,
            ..
        } => {
            let lo = i64::try_from(*start).ok()?;
            let hi = i64::try_from(*end).ok()?;
            assert!(
                if *inclusive { lo <= hi } else { lo < hi },
                "the elaborator rejects a reversed or empty range, so {lo}..{hi} \
                 (inclusive: {inclusive}) cannot reach here"
            );
            CaseKey::Spans(vec![(lo, if *inclusive { hi } else { hi - 1 })])
        }
        PatKind::Wildcard => CaseKey::Wildcard,
        PatKind::Or(alternatives) => {
            let mut spans = Vec::new();
            for &alt in alternatives {
                match case_key(body, alt)? {
                    CaseKey::Spans(alt_spans) => spans.extend(alt_spans),
                    CaseKey::Wildcard => return Some(CaseKey::Wildcard),
                }
            }
            CaseKey::Spans(spans)
        }
        PatKind::Literal(
            NirLiteralPattern::Bool(_) | NirLiteralPattern::String(_) | NirLiteralPattern::Null,
        )
        | PatKind::Binding { .. }
        | PatKind::Tuple(..)
        | PatKind::Variant { .. }
        | PatKind::Struct { .. }
        | PatKind::ConstantValue { .. } => return None,
    })
}

/// The width in bits of a scrutinee a key dispatch may take — an integer,
/// `char`, or enum — or `None` for any other type.
pub(super) fn scrutinee_bits(scrutinee_type: &ResolvedType) -> Option<u32> {
    match scrutinee_type {
        ResolvedType::Primitive(PrimitiveType::I8 | PrimitiveType::U8) => Some(8),
        ResolvedType::Primitive(PrimitiveType::I16 | PrimitiveType::U16) => Some(16),
        ResolvedType::Primitive(PrimitiveType::I32 | PrimitiveType::U32 | PrimitiveType::Char)
        | ResolvedType::Enum { .. } => Some(32),
        ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => Some(64),
        _ => None,
    }
}

/// One arm's values as `(arm index, lo, hi)`, inclusive at both ends.
pub(super) type ArmSpan = (usize, i64, i64);

/// The arms' spans up to the wildcard, and the wildcard's own index. `match`
/// is first-match-wins, so the arms past a wildcard are dead and none is
/// walked. `None` for a guard, or a pattern no key dispatch takes.
pub(super) fn arm_spans(arms: &[ArmData], body: &Body) -> Option<(Vec<ArmSpan>, Option<usize>)> {
    let mut spans = Vec::new();
    for (i, arm) in arms.iter().enumerate() {
        if arm.guard.is_some() {
            return None;
        }
        match case_key(body, arm.pattern)? {
            CaseKey::Spans(arm_spans) => {
                spans.extend(arm_spans.into_iter().map(|(lo, hi)| (i, lo, hi)))
            }
            CaseKey::Wildcard => return Some((spans, Some(i))),
        }
    }
    Some((spans, None))
}

/// Analyze whether a `Match` can be rewritten into a `Switch`. Accepts
/// integer / `char` / enum scrutinees with guard-less arms whose patterns
/// are integer or `char` literals, enum cases, integer/`char` ranges, or
/// wildcard (the default).
fn analyze(scrutinee_type: &ResolvedType, arms: &[ArmData], body: &Body) -> Option<SwitchAnalysis> {
    scrutinee_bits(scrutinee_type)?;

    let (specs, default_arm) = arm_spans(arms, body)?;
    if specs.is_empty() {
        return None;
    }
    let mut min_value = i64::MAX;
    let mut max_value = i64::MIN;
    for &(_, lo, hi) in &specs {
        min_value = min_value.min(lo);
        max_value = max_value.max(hi);
    }

    let range = i128::from(max_value) - i128::from(min_value) + 1;
    if range > i128::from(SWITCH_MAX_RANGE) {
        return None;
    }

    if specs.len() < 2 {
        return None;
    }

    // `range <= SWITCH_MAX_RANGE`, so an offset table is small enough to
    // materialise. The first spec covering an offset wins (later specs are
    // dead there, matching `build_switch`), so overlapping specs count their
    // shared values once — a value covered twice must not inflate density.
    let range = range as usize;
    let mut offset_arm: Vec<Option<usize>> = vec![None; range];
    for &(arm_idx, lo, hi) in &specs {
        for v in lo..=hi {
            let offset = (v - min_value) as usize;
            offset_arm[offset].get_or_insert(arm_idx);
        }
    }

    let covered = offset_arm.iter().filter(|a| a.is_some()).count();
    if covered < SWITCH_MIN_CASES {
        return None;
    }
    if (covered as f64) / (range as f64) < SWITCH_DENSITY_THRESHOLD {
        return None;
    }

    Some(SwitchAnalysis {
        min_value,
        offset_arm,
        default_arm,
    })
}

/// Build a `Switch` expression kind from the analysis. The scrutinee id is
/// reused directly (it appears once); each reachable arm body is cloned once,
/// however many offsets dispatch to it.
fn build_switch(
    engine: &mut Engine,
    scrutinee: Operand,
    arms: &[ArmData],
    analysis: SwitchAnalysis,
    span: Span,
    cold_path_id: FuncId,
    unreachable_id: FuncId,
) -> ExprKind {
    let mut switch_arm_of: Vec<Option<usize>> = vec![None; arms.len()];
    let mut switch_arms: Vec<BlockId> = Vec::new();
    let table: Vec<Option<usize>> = analysis
        .offset_arm
        .iter()
        .map(|maybe_arm_idx| {
            let arm_idx = (*maybe_arm_idx)?;
            Some(*switch_arm_of[arm_idx].get_or_insert_with(|| {
                switch_arms.push(arm_body_block(
                    engine,
                    arms[arm_idx].body,
                    arms[arm_idx].span,
                ));
                switch_arms.len() - 1
            }))
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
                has_receiver: false,
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
                has_receiver: false,
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
        table,
        arms: switch_arms,
        default: default_block,
    }
}

/// Wrap an arm body in a fresh block holding a single `Expr` statement. A
/// skeleton body is deep-cloned; a promoted constant operand is immutable and
/// shareable, so it flows straight into the statement slot.
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
