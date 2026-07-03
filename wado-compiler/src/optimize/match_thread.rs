//! Match-over-LabeledBlock jump threading.
//!
//! Threads the shape inlining leaves behind when a variant-returning call is
//! a `match` scrutinee — most importantly the `?` desugar:
//!
//! ```text
//! let b = match L: {                     // inlined callee body
//!     if eof { break L: Result::Err(e); }
//!     break L: Result::Ok(v);
//! } {
//!     Ok(x) => x,
//!     Err(e) => { cold; return Err(e); },
//! };
//! ```
//!
//! Every `break L: VariantConstruct(C, …)` statically selects one arm, so the
//! variant value is never observed: each break site becomes the selected
//! arm's body with the payload bound, and the labeled block yields the match
//! result directly:
//!
//! ```text
//! let b = __thread_L: {
//!     if eof { let e' = e; cold; return Err(e'); }
//!     let x = v;
//!     break __thread_L: x;
//! };
//! ```
//!
//! The `Match` expression node is rewritten in place (`replace_expr_kind`),
//! so the rule is position-independent: the labeled block already executes
//! exactly where the scrutinee executed, and a break exit flows straight into
//! the match dispatch with nothing in between, so evaluation order is
//! unchanged in every context (`let` initializer, `return` value, nested
//! operand, statement position).
//!
//! Complements [`super::labeled_block_fusion`], which fuses the
//! value-discarding `let tmp = LB; if VariantTest(tmp) …` / two-statement
//! `match tmp` shapes that if-let desugaring produces; this rule handles the
//! value-producing direct-scrutinee shape. Scope: guard-free arms; `Variant`
//! (payload `[]`, `[_]`, or `[binding]`) and `Wildcard` patterns; every exit
//! a `VariantConstruct`. A `null` / value-less break bails, because mapping it
//! to an arm needs the variant's null-case identity this pass does not carry.
//! This leaves the `Option` `?` operator unthreaded: `reify` lowers `opt?` to a
//! direct `match L: { … break L: Some(v); … break L: null } { Some(x)=>x,
//! None=>… }` whose `None` exit is a bare `null`, so it keeps the intermediate
//! `Option` allocation that `Result` `?` sheds. Closing that seam (threading
//! `null`/unit exits to the payload-less arm) is a deliberate follow-up, not a
//! shape the existing passes already cover.
//!
//! Chained `?` resolves bottom-up without special handling: the engine seeds
//! post-order, so an inner `match LB` threads first, turning into a plain
//! nested `LabeledBlock` the outer walk descends through.

use crate::nir::NirLocal;
use crate::nir_arena::{
    ArmData, BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::nir_value_graph::ValueKind;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::arena_query::{has_break_to, single_payload_binding};
use super::labeled_block_fusion::{block_contains_loop, expr_has_free_unlabeled_loop_exit};

pub(super) struct MatchThreadRule;

/// Mirrors the `build_*` constructors of the other peephole rules.
pub(super) fn build_match_thread() -> MatchThreadRule {
    MatchThreadRule
}

impl Rule for MatchThreadRule {
    fn apply_expr(&self, engine: &mut Engine, id: ExprId) -> bool {
        let Some(plan) = plan_threading(engine.body, id, engine.locals()) else {
            return false;
        };
        perform_threading(engine, id, plan);
        true
    }
}

/// One match arm, reduced to what the transform needs.
struct ArmInfo {
    /// `Some(case)` for a `Variant` pattern, `None` for a wildcard.
    case_name: Option<String>,
    /// The payload binding local, when the pattern binds one.
    binding: Option<u32>,
    /// The original arm body; cloned per break site.
    body: Operand,
}

struct ThreadPlan {
    scrut: ExprId,
    label: String,
    lb_block: BlockId,
    arms: Vec<ArmInfo>,
    result_type: TypeId,
    unit_result: bool,
}

fn plan_threading(body: &Body, id: ExprId, locals: &[NirLocal]) -> Option<ThreadPlan> {
    let ExprKind::Match { expr: scrut, arms } = &body.exprs[id].kind else {
        return None;
    };
    let scrut = scrut.as_expr()?;
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[scrut].kind
    else {
        return None;
    };
    let label = label.clone();
    let lb_block = *lb_block;

    let arm_infos: Vec<ArmInfo> = arms
        .iter()
        .map(|arm| arm_info(body, arm))
        .collect::<Option<_>>()?;

    // The labeled block must not fall through: its value must arrive via
    // `break L:` exits only, so the tail has to terminate.
    let last = body.blocks[lb_block].stmts.last()?;
    if !matches!(
        body.stmts[*last].kind,
        StmtKind::Break { .. } | StmtKind::Return { .. } | StmtKind::Continue
    ) {
        return None;
    }

    let result_type = body.exprs[id].type_id;
    let unit_result = result_type == TypeTable::UNIT;

    // Validate every `break L:` exit and record which arms it selects.
    let mut selected = vec![false; arm_infos.len()];
    if !validate_exits_in_block(body, lb_block, &label, &arm_infos, locals, &mut selected) {
        return None;
    }

    let lb_has_loop = block_contains_loop(body, lb_block);
    for (arm, used) in arm_infos.iter().zip(&selected) {
        if !used {
            continue;
        }
        let Some(e) = arm.body.as_expr() else {
            continue;
        };
        // Cloning an arm into the labeled block must not capture a free
        // unlabeled `break`/`continue` into a loop inside it, nor a
        // `break L:` targeting the block being retyped.
        if lb_has_loop && expr_has_free_unlabeled_loop_exit(body, e, 0) {
            return None;
        }
        if has_break_to(body, NodeRef::Expr(e), &label) {
            return None;
        }
        // A non-unit match needs a tail value from every threaded arm.
        if !unit_result && !arm_body_decomposable(body, e) {
            return None;
        }
    }

    Some(ThreadPlan {
        scrut,
        label,
        lb_block,
        arms: arm_infos,
        result_type,
        unit_result,
    })
}

fn arm_info(body: &Body, arm: &ArmData) -> Option<ArmInfo> {
    if arm.guard.is_some() {
        return None;
    }
    match &body.pats[arm.pattern].kind {
        PatKind::Wildcard => Some(ArmInfo {
            case_name: None,
            binding: None,
            body: arm.body,
        }),
        PatKind::Variant {
            variant_name,
            bindings,
            ..
        } => {
            let binding = single_payload_binding(body, bindings)?;
            Some(ArmInfo {
                case_name: Some(variant_name.clone()),
                binding,
                body: arm.body,
            })
        }
        _ => None,
    }
}

/// First arm the case selects: a wildcard, or a `Variant` pattern with the
/// same case name. A `VariantConstruct` of case A never matches a `Variant`
/// pattern of case B, so skipping non-matching variant arms is exact.
fn select_arm(arms: &[ArmInfo], case_name: &str) -> Option<usize> {
    arms.iter()
        .position(|a| a.case_name.as_deref().is_none_or(|n| n == case_name))
}

/// A non-unit threaded arm must decompose into `stmts + tail value` or
/// provably diverge. Value operands and non-block expressions are their own
/// tail; a block must end in an `Expr` tail or a terminator.
fn arm_body_decomposable(body: &Body, e: ExprId) -> bool {
    let ExprKind::Block(b) = &body.exprs[e].kind else {
        return true;
    };
    let Some(last) = body.blocks[*b].stmts.last() else {
        return false;
    };
    matches!(
        body.stmts[*last].kind,
        StmtKind::Expr(_) | StmtKind::Break { .. } | StmtKind::Return { .. } | StmtKind::Continue
    )
}

// ---------------------------------------------------------------------------
// Exit validation (mirrors labeled_block_fusion's break checker, but resolves
// each exit to an arm)
// ---------------------------------------------------------------------------

fn validate_exits_in_block(
    body: &Body,
    block: BlockId,
    label: &str,
    arms: &[ArmInfo],
    locals: &[NirLocal],
    selected: &mut [bool],
) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .all(|s| validate_exits_in_stmt(body, *s, label, arms, locals, selected))
}

fn validate_exits_in_stmt(
    body: &Body,
    s: StmtId,
    label: &str,
    arms: &[ArmInfo],
    locals: &[NirLocal],
    selected: &mut [bool],
) -> bool {
    match &body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value,
        } if l == label => {
            let Some(vc) = value.and_then(Operand::as_expr) else {
                // A value-less or promoted (`null`) break has no static case.
                return false;
            };
            let ExprKind::VariantConstruct {
                case_name, payload, ..
            } = &body.exprs[vc].kind
            else {
                return false;
            };
            if payload.is_some_and(|p| {
                p.as_expr()
                    .is_some_and(|e| has_break_to(body, NodeRef::Expr(e), label))
            }) {
                return false;
            }
            let Some(idx) = select_arm(arms, case_name) else {
                return false;
            };
            if let Some(binding) = arms[idx].binding {
                let Some(p) = payload else {
                    return false;
                };
                if locals
                    .get(binding as usize)
                    .is_none_or(|local| local.type_id != body.operand_type(*p))
                {
                    return false;
                }
            }
            selected[idx] = true;
            true
        }
        StmtKind::LabeledBlock { label: l, .. } if l == label => true,
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            validate_exits_in_operand(body, *condition, label, arms, locals, selected)
                && validate_exits_in_block(body, *then_block, label, arms, locals, selected)
                && else_block.is_none_or(|eb| {
                    validate_exits_in_block(body, eb, label, arms, locals, selected)
                })
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            validate_exits_in_block(body, *b, label, arms, locals, selected)
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            validate_exits_in_operand(body, *value, label, arms, locals, selected)
        }
        StmtKind::Expr(value) => {
            validate_exits_in_operand(body, *value, label, arms, locals, selected)
        }
        StmtKind::Return { value } | StmtKind::Break { value, .. } => {
            value.is_none_or(|v| validate_exits_in_operand(body, v, label, arms, locals, selected))
        }
        StmtKind::Continue => true,
    }
}

fn validate_exits_in_operand(
    body: &Body,
    op: Operand,
    label: &str,
    arms: &[ArmInfo],
    locals: &[NirLocal],
    selected: &mut [bool],
) -> bool {
    op.as_expr()
        .is_none_or(|e| validate_exits_in_expr(body, e, label, arms, locals, selected))
}

fn validate_exits_in_expr(
    body: &Body,
    e: ExprId,
    label: &str,
    arms: &[ArmInfo],
    locals: &[NirLocal],
    selected: &mut [bool],
) -> bool {
    match &body.exprs[e].kind {
        ExprKind::LabeledBlock {
            label: l, block, ..
        } => l == label || validate_exits_in_block(body, *block, label, arms, locals, selected),
        ExprKind::Block(block) => {
            validate_exits_in_block(body, *block, label, arms, locals, selected)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            validate_exits_in_operand(body, *condition, label, arms, locals, selected)
                && validate_exits_in_block(body, *then_branch, label, arms, locals, selected)
                && else_branch.is_none_or(|eb| {
                    validate_exits_in_block(body, eb, label, arms, locals, selected)
                })
        }
        // A nested `match` (over a local, e.g. a non-inlined call's result)
        // and a `Switch` (pre-inline `match_to_switch` output) both host
        // exits in their arms; descend like the threading walk does.
        ExprKind::Match {
            expr: scrut,
            arms: inner_arms,
        } => {
            validate_exits_in_operand(body, *scrut, label, arms, locals, selected)
                && inner_arms.iter().all(|arm| {
                    validate_exits_in_operand(body, arm.body, label, arms, locals, selected)
                        && arm.guard.is_none_or(|g| {
                            validate_exits_in_operand(body, g, label, arms, locals, selected)
                        })
                })
        }
        ExprKind::Switch {
            scrutinee,
            arms: switch_arms,
            default,
            ..
        } => {
            validate_exits_in_operand(body, *scrutinee, label, arms, locals, selected)
                && switch_arms
                    .iter()
                    .all(|b| validate_exits_in_block(body, *b, label, arms, locals, selected))
                && validate_exits_in_block(body, *default, label, arms, locals, selected)
        }
        // Other expression kinds (calls, literals, …) are opaque here: any
        // exit hidden inside bails.
        _ => !has_break_to(body, NodeRef::Expr(e), label),
    }
}

// ---------------------------------------------------------------------------
// Threading (engine-routed)
// ---------------------------------------------------------------------------

fn perform_threading(engine: &mut Engine, match_id: ExprId, plan: ThreadPlan) {
    let fused_label = format!("__thread_{}", plan.label);
    thread_block(engine, plan.lb_block, &plan, &fused_label);
    // Every exit `validate_exits_*` accepted must have been rewritten to the
    // fused label; a surviving `break plan.label` is a walker-divergence bug
    // (the thread walk missed a child position the validate walk descended,
    // e.g. an `if`-statement condition) and would dangle once the label is
    // renamed. Catch it in dev/test builds rather than emitting invalid Wasm.
    debug_assert!(
        !has_break_to(engine.body, NodeRef::Block(plan.lb_block), &plan.label),
        "match_thread: unrewritten `break {}` survived threading",
        plan.label,
    );
    // The scrutinee node's LabeledBlock kind moves onto the match node; kill
    // the vacated node first so the block is never double-claimed.
    engine.replace_expr_kind(plan.scrut, ExprKind::Dead);
    engine.replace_expr_kind(
        match_id,
        ExprKind::LabeledBlock {
            label: fused_label,
            block: plan.lb_block,
            result_type: plan.result_type,
        },
    );
}

fn thread_block(engine: &mut Engine, block: BlockId, plan: &ThreadPlan, fused_label: &str) {
    let stmts = std::mem::take(&mut engine.body.blocks[block].stmts);
    let mut out = Vec::with_capacity(stmts.len());
    for s in stmts {
        thread_stmt(engine, s, plan, fused_label, &mut out);
    }
    engine.set_block_stmts(block, out);
}

fn thread_stmt(
    engine: &mut Engine,
    s: StmtId,
    plan: &ThreadPlan,
    fused_label: &str,
    out: &mut Vec<StmtId>,
) {
    let exit_value = match &engine.body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value,
        } if *l == plan.label => Some(*value),
        _ => None,
    };
    if let Some(value) = exit_value {
        let span = engine.body.stmts[s].span;
        emit_threaded_exit(engine, value, plan, fused_label, span, out);
        return;
    }

    enum Shape {
        Blocks(Vec<BlockId>),
        // An `if` statement's condition operand can host an exit too (a
        // block/if/match condition carrying a `break plan.label`), so it must be
        // threaded alongside the branches — validate_exits_in_stmt descends it.
        CondAndBlocks(Operand, Vec<BlockId>),
        Other,
    }
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::LabeledBlock { label: l, .. } if *l == plan.label => Shape::Blocks(vec![]),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut v = vec![*then_block];
            if let Some(eb) = else_block {
                v.push(*eb);
            }
            Shape::CondAndBlocks(*condition, v)
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            Shape::Blocks(vec![*b])
        }
        _ => Shape::Other,
    };
    match shape {
        Shape::Blocks(blocks) => {
            for b in blocks {
                thread_block(engine, b, plan, fused_label);
            }
        }
        Shape::CondAndBlocks(cond, blocks) => {
            if let Some(ce) = cond.as_expr() {
                thread_expr(engine, ce, plan, fused_label);
            }
            for b in blocks {
                thread_block(engine, b, plan, fused_label);
            }
        }
        Shape::Other => {
            let target = match &engine.body.stmts[s].kind {
                StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
                    Some(*value)
                }
                StmtKind::Expr(value) => Some(*value),
                StmtKind::Return { value } | StmtKind::Break { value, .. } => *value,
                StmtKind::If { .. }
                | StmtKind::Loop { .. }
                | StmtKind::LabeledBlock { .. }
                | StmtKind::Continue => None,
            };
            if let Some(e) = target.and_then(Operand::as_expr) {
                thread_expr(engine, e, plan, fused_label);
            }
        }
    }
    out.push(s);
}

fn thread_expr(engine: &mut Engine, e: ExprId, plan: &ThreadPlan, fused_label: &str) {
    enum Shape {
        Blocks(Vec<BlockId>),
        Operands(Vec<Operand>),
        OperandsAndBlocks(Vec<Operand>, Vec<BlockId>),
        None,
    }
    let shape = match &engine.body.exprs[e].kind {
        ExprKind::LabeledBlock {
            label: l, block, ..
        } => {
            if *l == plan.label {
                Shape::None
            } else {
                Shape::Blocks(vec![*block])
            }
        }
        ExprKind::Block(block) => Shape::Blocks(vec![*block]),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut blocks = vec![*then_branch];
            if let Some(eb) = else_branch {
                blocks.push(*eb);
            }
            Shape::OperandsAndBlocks(vec![*condition], blocks)
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut blocks = arms.clone();
            blocks.push(*default);
            Shape::OperandsAndBlocks(vec![*scrutinee], blocks)
        }
        ExprKind::Match { expr, arms } => {
            let mut ops: Vec<Operand> = vec![*expr];
            for arm in arms {
                ops.push(arm.body);
                if let Some(g) = arm.guard {
                    ops.push(g);
                }
            }
            Shape::Operands(ops)
        }
        _ => Shape::None,
    };
    match shape {
        Shape::Blocks(blocks) => {
            for b in blocks {
                thread_block(engine, b, plan, fused_label);
            }
        }
        Shape::Operands(ops) => {
            for op in ops {
                if let Some(oe) = op.as_expr() {
                    thread_expr(engine, oe, plan, fused_label);
                }
            }
        }
        Shape::OperandsAndBlocks(ops, blocks) => {
            for op in ops {
                if let Some(oe) = op.as_expr() {
                    thread_expr(engine, oe, plan, fused_label);
                }
            }
            for b in blocks {
                thread_block(engine, b, plan, fused_label);
            }
        }
        Shape::None => {}
    }
}

fn emit_threaded_exit(
    engine: &mut Engine,
    value: Option<Operand>,
    plan: &ThreadPlan,
    fused_label: &str,
    span: Span,
    out: &mut Vec<StmtId>,
) {
    let vc = value
        .and_then(Operand::as_expr)
        .expect("guarded by plan_threading");
    let ExprKind::VariantConstruct {
        case_name, payload, ..
    } = &engine.body.exprs[vc].kind
    else {
        unreachable!("guarded by plan_threading");
    };
    let payload = *payload;
    let arm_idx = select_arm(&plan.arms, case_name).expect("guarded by plan_threading");
    let arm = &plan.arms[arm_idx];
    let arm_body = arm.body;
    let binding = arm.binding;

    // Bind or preserve the payload. An unbound effectful payload still runs
    // (source order: the payload evaluates at the break, before dispatch).
    if let Some(b_local) = binding {
        let payload_op = payload.expect("guarded by plan_threading");
        let local = &engine.locals()[b_local as usize];
        let (name, type_id) = (local.name.clone(), local.type_id);
        let let_stmt = engine.alloc_stmt(
            StmtKind::Let {
                name,
                local_index: b_local,
                is_mut: false,
                is_reactive: false,
                type_id,
                value: payload_op,
                skip_value_copy: false,
            },
            span,
        );
        out.push(let_stmt);
    } else if let Some(p) = payload
        && p.as_expr().is_some()
    {
        out.push(engine.alloc_stmt(StmtKind::Expr(p), span));
    }

    // Clone the arm body and split it into leading statements plus the tail
    // value the fused break carries.
    let (body_stmts, tail): (Vec<StmtId>, Option<Operand>) = match arm_body {
        Operand::Value(v) => (vec![], Some(Operand::Value(v))),
        Operand::Expr(e) => {
            if let ExprKind::Block(b) = engine.body.exprs[e].kind {
                let cloned = engine.clone_block(b);
                // Move the cloned stmts out so the source block never
                // double-claims them (same discipline as labeled_block_fusion).
                let mut stmts = std::mem::take(&mut engine.body.blocks[cloned].stmts);
                let tail = match stmts.last().map(|s| &engine.body.stmts[*s].kind) {
                    Some(StmtKind::Expr(op)) => {
                        let op = *op;
                        stmts.pop();
                        Some(op)
                    }
                    Some(StmtKind::Break { .. } | StmtKind::Return { .. } | StmtKind::Continue) => {
                        None
                    }
                    _ => Some(engine.const_operand(ValueKind::Unit, TypeTable::UNIT)),
                };
                (stmts, tail)
            } else {
                (vec![], Some(Operand::Expr(engine.clone_expr(e))))
            }
        }
    };
    out.extend(body_stmts);

    match tail {
        Some(t) if !plan.unit_result => {
            out.push(engine.alloc_stmt(
                StmtKind::Break {
                    label: Some(fused_label.to_owned()),
                    value: Some(t),
                },
                span,
            ));
        }
        Some(t) => {
            // Unit match: evaluate an effectful tail as a statement and break
            // value-less, keeping the fused block unit-typed.
            if t.as_expr().is_some() {
                out.push(engine.alloc_stmt(StmtKind::Expr(t), span));
            }
            out.push(engine.alloc_stmt(
                StmtKind::Break {
                    label: Some(fused_label.to_owned()),
                    value: None,
                },
                span,
            ));
        }
        // A divergent arm already terminates; no fused break needed.
        None => {}
    }
}
