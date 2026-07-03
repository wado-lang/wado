//! LabeledBlock-IfVariant fusion rule.
//!
//! Detects the pattern produced by inlining `Option<T>`/`Result<T, E>`-returning
//! functions into if-let call sites, where an intermediate GC allocation is
//! created for the variant result and then immediately unpacked by a
//! `VariantTest`/`VariantPayload` pair (or a two-arm `Match`).
//!
//! ## Pattern detected
//!
//! ```text
//! let temp: Option<T> = L: {
//!     if cond { break L: null; }
//!     let v = ...;
//!     break L: Variant::Some(v);
//! };
//! if variant_test(temp, case=C) {
//!     let b = variant_payload(temp, case=C);
//!     THEN
//! } else {
//!     ELSE
//! }
//! ```
//!
//! ## Transformed output
//!
//! ```text
//! '__fused_L: {
//!     if cond {
//!         ELSE;
//!         break '__fused_L;
//!     }
//!     let v = ...;
//!     let __fused_payload_N = v;
//!     THEN (with variant_payload(temp, C) replaced by __fused_payload_N);
//!     break '__fused_L;
//! }
//! ```
//!
//! This eliminates the GC-allocated `temp: Option<T>` entirely. Subsequent
//! rules (`elide_local`, `copy_prop`, `branch_prune`) clean up the
//! `break '__fused_L;` bookkeeping.
//!
//! ## Architecture
//!
//! Runs as a [`Rule`] on the unified post-inline peephole session (combine
//! migration; formerly the standalone `nir/labeled_block_fusion` pass). The
//! engine seeds every block in post-order, so each `apply_block` only has to
//! find the first `(let-LB, consumer)` adjacent pair in *this* block; the
//! worklist's `set_block_stmts` re-enqueue propagates fusion outwards
//! naturally. All mutations route through the engine edit API so the parent
//! map and use index stay coherent — `engine.clone_block` for THEN/ELSE
//! clones, `engine.replace_expr_kind` for the `VariantPayload → Local`
//! substitution, `engine.alloc_*` for fresh nodes, `engine.set_block_stmts`
//! for the final block-list commit, and `engine.alloc_local` for the fresh
//! `__fused_payload_N` slot.
//!
//! The pre-inline session does not include this rule — the
//! `let temp = LB; if VariantTest(temp, …)` shape it matches is exposed by
//! `inline` copying the helper body into the caller.

use crate::nir::NirLocal;
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatKind, StmtId, StmtKind,
};
use crate::nir_engine::{Engine, Rule};
use crate::nir_visitor::NirRefVisitor;
use crate::tir::TypeId;
use crate::token::Span;

use super::arena_query::{has_break_to, is_local, is_local_operand};

/// `expr_has_break_to` arena adapter.
fn expr_has_break_to(body: &Body, label: &str, e: ExprId) -> bool {
    has_break_to(body, NodeRef::Expr(e), label)
}

/// Block-level fusion rule for the unified post-inline peephole session.
/// The rule keeps no per-function state: every precondition is re-derived from
/// the current body on each `apply_block`, since fusion candidates appear and
/// disappear as neighbouring rewrites land.
pub(super) struct LabeledBlockFusionRule;

/// Build a [`LabeledBlockFusionRule`] for one function. Mirrors the
/// `build_ref_elim` / `build_elide_box_local` constructors so the peephole
/// wiring is uniform; no per-function analysis is needed yet.
pub(super) fn build_labeled_block_fusion() -> LabeledBlockFusionRule {
    LabeledBlockFusionRule
}

impl Rule for LabeledBlockFusionRule {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let stmts = engine.body.blocks[block].stmts.clone();
        if stmts.len() < 2 {
            return false;
        }
        // The fused block uses value-less `break __fused_L;` to terminate each
        // arm, so the original consumer site cannot be in a position where its
        // value is observed. Refusing fusion here matches the standalone pass's
        // `yields_value && i + 2 == stmts.len()` guard.
        let yields = block_yields_value(engine, block);
        for i in 0..stmts.len() - 1 {
            let Some(info) =
                check_fusion_preconditions(engine.body, stmts[i], stmts[i + 1], engine.locals())
            else {
                continue;
            };
            if yields && i + 2 == stmts.len() {
                continue;
            }
            perform_fusion(engine, block, &stmts, i, info);
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// yields-value walker (replaces the standalone pass's recursive flag)
// ---------------------------------------------------------------------------

/// True iff the tail value of `block` reaches a consumer (a `let` initializer,
/// a function argument, a returned expression, …). Walks up the parent map; the
/// chain is bounded by tree depth. Mirrors the `yields_value` flag the
/// standalone recursive driver threaded through `fuse_in_block`.
fn block_yields_value(engine: &Engine, block: BlockId) -> bool {
    node_yields_value(engine, NodeRef::Block(block))
}

fn node_yields_value(engine: &Engine, node: NodeRef) -> bool {
    let Some(parent) = engine.parent_of(node) else {
        return false;
    };
    match parent {
        NodeRef::Expr(pe) => match &engine.body.exprs[pe].kind {
            // Wrappers / control-flow expressions: yield iff the wrapper
            // itself is in a value-consuming position.
            ExprKind::Block(_)
            | ExprKind::LabeledBlock { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. } => node_yields_value(engine, NodeRef::Expr(pe)),
            // Any other expression parent (Binary, Call, FieldAccess, …):
            // this node's value is consumed by the surrounding expression.
            _ => true,
        },
        NodeRef::Stmt(ps) => match &engine.body.stmts[ps].kind {
            StmtKind::Let { .. }
            | StmtKind::LetDestructure { .. }
            | StmtKind::Return { value: Some(_) }
            | StmtKind::Break { value: Some(_), .. } => true,
            StmtKind::Expr(_) => node_yields_value(engine, NodeRef::Stmt(ps)),
            // For a block under a stmt-form `If` (a branch), mirror the
            // standalone driver's Shape::If propagation: the branch yields iff
            // the If statement itself yields (tail of a yielding outer block).
            // For the condition expression, the value is always consumed.
            StmtKind::If { .. } => match node {
                NodeRef::Expr(_) => true,
                NodeRef::Block(_) => node_yields_value(engine, NodeRef::Stmt(ps)),
                _ => false,
            },
            // Stmt-form Loop / LabeledBlock discard their body's value.
            StmtKind::Loop { .. } | StmtKind::LabeledBlock { .. } => false,
            StmtKind::Break { value: None, .. }
            | StmtKind::Return { value: None }
            | StmtKind::Continue => false,
        },
        NodeRef::Block(pb) => {
            // A stmt under a block yields iff it is the tail AND the block does.
            let stmts = &engine.body.blocks[pb].stmts;
            let s_id = match node {
                NodeRef::Stmt(s) => s,
                _ => return false,
            };
            stmts.last().copied() == Some(s_id) && node_yields_value(engine, NodeRef::Block(pb))
        }
        NodeRef::Pat(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Fusion driver
// ---------------------------------------------------------------------------

/// Information extracted from the two statements during the precondition check.
struct FusionInfo {
    temp_local: u32,
    label: String,
    case_index: u32,
    payload_type: TypeId,
    pattern_payload_binding: Option<u32>,
}

fn check_fusion_preconditions(
    body: &Body,
    let_s: StmtId,
    if_s: StmtId,
    locals: &[NirLocal],
) -> Option<FusionInfo> {
    check_fusion_preconditions_if_variant_test(body, let_s, if_s)
        .or_else(|| check_fusion_preconditions_match(body, let_s, if_s, locals))
}

fn check_fusion_preconditions_if_variant_test(
    body: &Body,
    let_s: StmtId,
    if_s: StmtId,
) -> Option<FusionInfo> {
    // --- Stmt 1: Let { value: LabeledBlock { label, block } } ---
    let StmtKind::Let {
        local_index: temp_local,
        value: let_value,
        ..
    } = &body.stmts[let_s].kind
    else {
        return None;
    };
    let temp_local = *temp_local;
    let lv = let_value.as_expr()?;
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[lv].kind
    else {
        return None;
    };
    let label = label.clone();
    let lb_block = *lb_block;

    // --- Stmt 2: If { condition: VariantTest(Local(X), case=C), then, else } ---
    let StmtKind::If {
        condition,
        then_block,
        else_block,
    } = &body.stmts[if_s].kind
    else {
        return None;
    };
    let condition = *condition;
    let then_block = *then_block;
    let else_block = *else_block;
    let ExprKind::VariantTest {
        expr: vt_expr,
        case_index,
        ..
    } = &body.exprs[condition.as_expr()?].kind
    else {
        return None;
    };
    let case_index = *case_index;
    let ExprKind::Local {
        index: tested_idx, ..
    } = &body.exprs[vt_expr.as_expr()?].kind
    else {
        return None;
    };
    if *tested_idx != temp_local {
        return None;
    }

    // --- LabeledBlock only breaks to L with null or VariantConstruct ---
    let payload_type = check_lb_breaks_and_get_payload(body, lb_block, &label, case_index)?;

    // --- temp is only used as VariantPayload(Local(X), C) in then_block,
    //     and not at all in else_block ---
    let then_uses = count_local_uses_in_block(body, then_block, temp_local);
    let payload_uses =
        count_variant_payload_uses_in_block(body, then_block, temp_local, case_index);
    if then_uses != payload_uses {
        return None;
    }
    if let Some(eb) = else_block
        && count_local_uses_in_block(body, eb, temp_local) > 0
    {
        return None;
    }

    // --- THEN/ELSE blocks must not contain free unlabeled break/continue
    //     when the labeled block being fused contains a loop. ---
    if block_contains_loop(body, lb_block) {
        if block_has_free_unlabeled_loop_exit(body, then_block) {
            return None;
        }
        if let Some(eb) = else_block
            && block_has_free_unlabeled_loop_exit(body, eb)
        {
            return None;
        }
    }

    Some(FusionInfo {
        temp_local,
        label,
        case_index,
        payload_type,
        pattern_payload_binding: None,
    })
}

fn check_fusion_preconditions_match(
    body: &Body,
    let_s: StmtId,
    if_s: StmtId,
    locals: &[NirLocal],
) -> Option<FusionInfo> {
    // --- Stmt 1: Let { value: LabeledBlock { label, block } } ---
    let StmtKind::Let {
        local_index: temp_local,
        value: let_value,
        ..
    } = &body.stmts[let_s].kind
    else {
        return None;
    };
    let temp_local = *temp_local;
    let lv = let_value.as_expr()?;
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[lv].kind
    else {
        return None;
    };
    let label = label.clone();
    let lb_block = *lb_block;

    // --- Stmt 2: Expr(Match { scrut: Local(temp), arms: [Variant, Wildcard] }) ---
    let StmtKind::Expr(Operand::Expr(match_expr)) = &body.stmts[if_s].kind else {
        return None;
    };
    let ExprKind::Match { expr: scrut, arms } = &body.exprs[*match_expr].kind else {
        return None;
    };
    if arms.len() != 2 {
        return None;
    }
    let scrut_e = scrut.as_expr()?;
    let ExprKind::Local {
        index: tested_idx, ..
    } = &body.exprs[scrut_e].kind
    else {
        return None;
    };
    if *tested_idx != temp_local {
        return None;
    }

    let arm0 = &arms[0];
    let arm1 = &arms[1];
    // Both arms must be guard-free; arm0 Variant, arm1 Wildcard.
    let arm0_is_variant = matches!(&body.pats[arm0.pattern].kind, PatKind::Variant { .. });
    let arm1_is_wildcard = matches!(&body.pats[arm1.pattern].kind, PatKind::Wildcard);
    if !(arm0_is_variant && arm1_is_wildcard && arm0.guard.is_none() && arm1.guard.is_none()) {
        return None;
    }
    let variant_arm_body = arm0.body;
    let else_arm_body = arm1.body;

    let PatKind::Variant {
        variant_name,
        bindings,
        ..
    } = &body.pats[arm0.pattern].kind
    else {
        return None;
    };
    let variant_name = variant_name.clone();

    // At most one payload binding slot.
    let pattern_payload_binding: Option<u32> = match bindings.as_slice() {
        [] => None,
        [single] => match &body.pats[*single].kind {
            PatKind::Wildcard => None,
            PatKind::Binding { local_index, .. } => Some(*local_index),
            _ => return None,
        },
        _ => return None,
    };

    // Resolve case_index from the labeled block's breaks.
    let case_index = find_break_case_index_for_name(body, lb_block, &label, &variant_name)?;

    // --- LabeledBlock only breaks to L with null or VariantConstruct ---
    let payload_type = check_lb_breaks_and_get_payload(body, lb_block, &label, case_index)?;

    // --- The reused binding slot must already be declared with payload type. ---
    if let Some(binding) = pattern_payload_binding
        && locals
            .get(binding as usize)
            .is_none_or(|local| local.type_id != payload_type)
    {
        return None;
    }

    // --- temp must not be read outside the Match scrutinee position. ---
    if count_local_uses_in_operand(body, variant_arm_body, temp_local) > 0 {
        return None;
    }
    if count_local_uses_in_operand(body, else_arm_body, temp_local) > 0 {
        return None;
    }

    // --- THEN/ELSE bodies must not contain free unlabeled break/continue
    //     when the labeled block being fused contains a loop. ---
    if block_contains_loop(body, lb_block) {
        // A promoted-value arm body has no skeleton subtree, hence no loop exit.
        if variant_arm_body
            .as_expr()
            .is_some_and(|e| arm_body_has_free_unlabeled_loop_exit(body, e))
        {
            return None;
        }
        if else_arm_body
            .as_expr()
            .is_some_and(|e| arm_body_has_free_unlabeled_loop_exit(body, e))
        {
            return None;
        }
    }

    Some(FusionInfo {
        temp_local,
        label,
        case_index,
        payload_type,
        pattern_payload_binding,
    })
}

/// Walk `block` looking for `break label: VariantConstruct { case_name }`
/// and return the embedded `case_index`.
fn find_break_case_index_for_name(
    body: &Body,
    block: BlockId,
    label: &str,
    variant_name: &str,
) -> Option<u32> {
    for s in &body.blocks[block].stmts {
        if let Some(idx) = find_break_case_index_for_name_in_stmt(body, *s, label, variant_name) {
            return Some(idx);
        }
    }
    None
}

fn find_break_case_index_for_name_in_stmt(
    body: &Body,
    s: StmtId,
    label: &str,
    variant_name: &str,
) -> Option<u32> {
    match &body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value: Some(v),
        } if l == label => {
            // A promoted value break (e.g. a `Null` placeholder) is not a
            // `VariantConstruct` — no case index.
            if let Some(e) = v.as_expr()
                && let ExprKind::VariantConstruct {
                    case_index,
                    case_name,
                    ..
                } = &body.exprs[e].kind
                && case_name == variant_name
            {
                return Some(*case_index);
            }
            None
        }
        StmtKind::LabeledBlock { label: l, .. } if l == label => None,
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => find_break_case_index_for_name(body, *then_block, label, variant_name).or_else(|| {
            else_block.and_then(|eb| find_break_case_index_for_name(body, eb, label, variant_name))
        }),
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            find_break_case_index_for_name(body, *b, label, variant_name)
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            find_break_case_index_for_name_in_operand(body, *value, label, variant_name)
        }
        StmtKind::Expr(expr) => {
            find_break_case_index_for_name_in_operand(body, *expr, label, variant_name)
        }
        StmtKind::Return { value } => value
            .and_then(|v| find_break_case_index_for_name_in_operand(body, v, label, variant_name)),
        StmtKind::Break { value: Some(v), .. } => {
            find_break_case_index_for_name_in_operand(body, *v, label, variant_name)
        }
        StmtKind::Break { value: None, .. } | StmtKind::Continue => None,
    }
}

fn find_break_case_index_for_name_in_operand(
    body: &Body,
    op: Operand,
    label: &str,
    variant_name: &str,
) -> Option<u32> {
    op.as_expr()
        .and_then(|e| find_break_case_index_for_name_in_expr(body, e, label, variant_name))
}

fn find_break_case_index_for_name_in_expr(
    body: &Body,
    e: ExprId,
    label: &str,
    variant_name: &str,
) -> Option<u32> {
    match &body.exprs[e].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            find_break_case_index_for_name(body, *block, label, variant_name)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => find_break_case_index_for_name_in_operand(body, *condition, label, variant_name)
            .or_else(|| find_break_case_index_for_name(body, *then_branch, label, variant_name))
            .or_else(|| {
                else_branch
                    .and_then(|b| find_break_case_index_for_name(body, b, label, variant_name))
            }),
        ExprKind::Match { expr: scrut, arms } => find_break_case_index_for_name_in_operand(
            body,
            *scrut,
            label,
            variant_name,
        )
        .or_else(|| {
            arms.iter().find_map(|arm| {
                find_break_case_index_for_name_in_operand(body, arm.body, label, variant_name)
                    .or_else(|| {
                        arm.guard.and_then(|g| {
                            find_break_case_index_for_name_in_operand(body, g, label, variant_name)
                        })
                    })
            })
        }),
        _ => None,
    }
}

/// Mirrors `block_has_free_unlabeled_loop_exit` but starting from an arm body
/// expression. Walks into `Block` / `LabeledBlock` children only.
fn arm_body_has_free_unlabeled_loop_exit(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            block_has_free_unlabeled_loop_exit(body, *block)
        }
        _ => false,
    }
}

/// Unwrap a Match arm body to the block it produced, creating a one-stmt block
/// when the body is not already a `Block`. Engine-routed so the new
/// stmt/block ids are registered in the parent map.
fn arm_body_into_block(engine: &mut Engine, arm_body: ExprId, fallback_span: Span) -> BlockId {
    if let ExprKind::Block(block) = &engine.body.exprs[arm_body].kind {
        *block
    } else {
        let stmt = engine.alloc_stmt(StmtKind::Expr(arm_body.into()), fallback_span);
        engine.alloc_block(vec![stmt], fallback_span)
    }
}

/// Like [`arm_body_into_block`] but accepts an `Operand`: a promoted pure value
/// (e.g. a unit arm body) has no skeleton node, so wrap it as a single-statement
/// block carrying the value operand.
fn arm_body_operand_into_block(
    engine: &mut Engine,
    arm_body: Operand,
    fallback_span: Span,
) -> BlockId {
    if let Some(e) = arm_body.as_expr() {
        arm_body_into_block(engine, e, fallback_span)
    } else {
        let stmt = engine.alloc_stmt(StmtKind::Expr(arm_body), fallback_span);
        engine.alloc_block(vec![stmt], fallback_span)
    }
}

/// Verify that all `break L: v` in `block` have `v` as either `null` or
/// `VariantConstruct`. Returns the payload type of the matching case.
fn check_lb_breaks_and_get_payload(
    body: &Body,
    block: BlockId,
    label: &str,
    case_index: u32,
) -> Option<TypeId> {
    let mut payload_type: Option<TypeId> = None;
    if !check_lb_breaks_in_block(body, block, label, case_index, &mut payload_type) {
        return None;
    }
    payload_type
}

fn check_lb_breaks_in_block(
    body: &Body,
    block: BlockId,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    body.blocks[block]
        .stmts
        .clone()
        .iter()
        .all(|s| check_lb_breaks_in_stmt(body, *s, label, case_index, payload_type))
}

fn check_lb_breaks_in_stmt(
    body: &Body,
    s: StmtId,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    match &body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value,
        } if l == label => {
            // A break carrying no value, or a promoted `Null` placeholder, is
            // the empty/None break the fusion accepts.
            let Some(e) = value.and_then(super::super::nir_arena::Operand::as_expr) else {
                return value.is_none_or(|v| {
                    v.as_value().is_some_and(|vid| {
                        matches!(
                            body.values.kind(vid),
                            crate::nir_value_graph::ValueKind::Null
                        )
                    })
                });
            };
            match &body.exprs[e].kind {
                ExprKind::VariantConstruct {
                    case_index: ci,
                    payload,
                    ..
                } => {
                    let ci = *ci;
                    let payload = *payload;
                    if let Some(p) = payload
                        && expr_has_break_to_operand(body, label, p)
                    {
                        return false;
                    }
                    if ci == case_index
                        && let Some(p) = payload
                    {
                        *payload_type = Some(body.operand_type(p));
                    }
                    true
                }
                _ => false,
            }
        }
        StmtKind::LabeledBlock { label: l, .. } if l == label => true,
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let condition = *condition;
            let then_block = *then_block;
            let else_block = *else_block;
            check_lb_breaks_in_operand(body, condition, label, case_index, payload_type)
                && check_lb_breaks_in_block(body, then_block, label, case_index, payload_type)
                && else_block.is_none_or(|eb| {
                    check_lb_breaks_in_block(body, eb, label, case_index, payload_type)
                })
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            check_lb_breaks_in_block(body, *b, label, case_index, payload_type)
        }
        StmtKind::Let { value, .. } => {
            check_lb_breaks_in_operand(body, *value, label, case_index, payload_type)
        }
        StmtKind::Break { value, .. } => value
            .is_none_or(|v| check_lb_breaks_in_operand(body, v, label, case_index, payload_type)),
        StmtKind::Return { value } => value
            .is_none_or(|v| check_lb_breaks_in_operand(body, v, label, case_index, payload_type)),
        _ => true,
    }
}

fn check_lb_breaks_in_operand(
    body: &Body,
    op: Operand,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    op.as_expr()
        .is_some_and(|e| check_lb_breaks_in_expr(body, e, label, case_index, payload_type))
}

fn check_lb_breaks_in_expr(
    body: &Body,
    e: ExprId,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    match &body.exprs[e].kind {
        ExprKind::LabeledBlock {
            label: l, block, ..
        } => {
            if l == label {
                true
            } else {
                check_lb_breaks_in_block(body, *block, label, case_index, payload_type)
            }
        }
        ExprKind::Block(block) => {
            check_lb_breaks_in_block(body, *block, label, case_index, payload_type)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let condition = *condition;
            let then_branch = *then_branch;
            let else_branch = *else_branch;
            check_lb_breaks_in_operand(body, condition, label, case_index, payload_type)
                && check_lb_breaks_in_block(body, then_branch, label, case_index, payload_type)
                && else_branch.is_none_or(|eb| {
                    check_lb_breaks_in_block(body, eb, label, case_index, payload_type)
                })
        }
        _ => !expr_has_break_to(body, label, e),
    }
}

struct LocalUseCounter {
    local_idx: u32,
    count: usize,
}

impl NirRefVisitor for LocalUseCounter {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Expr(e) = node
            && is_local(body, e, self.local_idx)
        {
            self.count += 1;
        }
        self.walk_node(body, node);
    }
}

/// Count all occurrences of `Local { index: local_idx }` in a block.
fn count_local_uses_in_block(body: &Body, block: BlockId, local_idx: u32) -> usize {
    let mut v = LocalUseCounter {
        local_idx,
        count: 0,
    };
    v.visit_node(body, NodeRef::Block(block));
    v.count
}

fn count_local_uses_in_operand(body: &Body, op: Operand, local_idx: u32) -> usize {
    let mut v = LocalUseCounter {
        local_idx,
        count: 0,
    };
    if let Operand::Expr(e) = op {
        v.visit_node(body, NodeRef::Expr(e));
    }
    v.count
}

struct VariantPayloadUseCounter {
    local_idx: u32,
    case_index: u32,
    count: usize,
}

impl NirRefVisitor for VariantPayloadUseCounter {
    fn visit_node(&mut self, body: &Body, node: NodeRef) {
        if let NodeRef::Expr(e) = node
            && let ExprKind::VariantPayload {
                expr: inner,
                case_index: ci,
                ..
            } = &body.exprs[e].kind
            && *ci == self.case_index
            && is_local_operand(body, *inner, self.local_idx)
        {
            self.count += 1;
        }
        self.walk_node(body, node);
    }
}

/// Count `VariantPayload { expr: Local(local_idx), case_index }` in a block.
fn count_variant_payload_uses_in_block(
    body: &Body,
    block: BlockId,
    local_idx: u32,
    case_index: u32,
) -> usize {
    let mut v = VariantPayloadUseCounter {
        local_idx,
        case_index,
        count: 0,
    };
    v.visit_node(body, NodeRef::Block(block));
    v.count
}

fn expr_has_free_unlabeled_loop_exit_operand(body: &Body, op: Operand, loop_depth: u32) -> bool {
    op.as_expr()
        .is_some_and(|e| expr_has_free_unlabeled_loop_exit(body, e, loop_depth))
}
fn expr_has_break_to_operand(body: &Body, label: &str, op: Operand) -> bool {
    op.as_expr()
        .is_some_and(|e| expr_has_break_to(body, label, e))
}
fn expr_contains_loop_operand(body: &Body, op: Operand) -> bool {
    op.as_expr().is_some_and(|e| expr_contains_loop(body, e))
}

// ---------------------------------------------------------------------------
// Fusion (engine-routed)
// ---------------------------------------------------------------------------

fn perform_fusion(
    engine: &mut Engine,
    outer_block: BlockId,
    stmts: &[StmtId],
    i: usize,
    info: FusionInfo,
) {
    let let_s = stmts[i];
    let if_s = stmts[i + 1];
    let span = engine.body.stmts[let_s].span;

    // Extract the LabeledBlock body from the Let statement.
    let StmtKind::Let {
        value: let_value, ..
    } = &engine.body.stmts[let_s].kind
    else {
        unreachable!("guarded by check_fusion_preconditions")
    };
    let lv = let_value
        .as_expr()
        .expect("guarded by check_fusion_preconditions");
    let ExprKind::LabeledBlock {
        block: lb_block, ..
    } = &engine.body.exprs[lv].kind
    else {
        unreachable!("guarded by check_fusion_preconditions")
    };
    let lb_block = *lb_block;

    // Extract the then/else blocks from the consumer statement.
    let (then_block, else_block) = match &engine.body.stmts[if_s].kind {
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => (*then_block, *else_block),
        StmtKind::Expr(match_expr) => {
            let match_expr = match_expr
                .as_expr()
                .expect("match scrutinee is a skeleton expr");
            let ExprKind::Match { arms, .. } = &engine.body.exprs[match_expr].kind else {
                unreachable!()
            };
            let variant_body = arms[0].body;
            let else_body = arms[1].body;
            let then_block = arm_body_operand_into_block(engine, variant_body, span);
            // A unit-valued else arm (the `None` case) contributes no block.
            let else_block = if else_body.as_value().is_some_and(|v| {
                matches!(
                    engine.body.values.kind(v),
                    crate::nir_value_graph::ValueKind::Unit
                )
            }) {
                None
            } else {
                Some(arm_body_operand_into_block(engine, else_body, span))
            };
            (then_block, else_block)
        }
        _ => unreachable!(),
    };

    // Pick the payload local — reuse the Match arm's pattern binding slot if any,
    // else allocate a fresh `__fused_payload_N` through the engine (so the
    // function's local list grows coherently).
    let payload_local = if let Some(b_idx) = info.pattern_payload_binding {
        b_idx
    } else {
        let next = engine.locals().len() as u32;
        engine.alloc_local(
            format!("__fused_payload_{next}"),
            info.payload_type,
            /* is_mut */ false,
        )
    };

    let fused_label = format!("__fused_{}", info.label);

    // Reuse the LB block's stmt ids by clearing its list (the LB block becomes
    // unreachable after `set_block_stmts` on the outer block; freeing the slot
    // here is a Vec take, not an arena free).
    let lb_stmts = std::mem::take(&mut engine.body.blocks[lb_block].stmts);
    let fused_stmts = transform_lb_stmts(
        engine,
        lb_stmts,
        &info.label,
        &fused_label,
        info.case_index,
        info.temp_local,
        payload_local,
        info.payload_type,
        then_block,
        else_block,
        span,
    );

    let fused_body = engine.alloc_block(fused_stmts, span);
    let fused_stmt = engine.alloc_stmt(
        StmtKind::LabeledBlock {
            label: fused_label,
            block: fused_body,
        },
        span,
    );

    // Replace the (let, if/match) pair with the single fused LabeledBlock stmt.
    let mut kept = Vec::with_capacity(stmts.len() - 1);
    kept.extend_from_slice(&stmts[..i]);
    kept.push(fused_stmt);
    kept.extend_from_slice(&stmts[i + 2..]);
    engine.set_block_stmts(outer_block, kept);
}

#[allow(clippy::too_many_arguments)]
fn transform_lb_stmts(
    engine: &mut Engine,
    stmts: Vec<StmtId>,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: BlockId,
    else_block: Option<BlockId>,
    span: Span,
) -> Vec<StmtId> {
    let mut out = Vec::new();
    for s in stmts {
        transform_lb_stmt(
            engine,
            s,
            orig_label,
            fused_label,
            case_index,
            temp_local,
            payload_local,
            payload_type,
            then_block,
            else_block,
            span,
            &mut out,
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn transform_lb_stmt(
    engine: &mut Engine,
    s: StmtId,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: BlockId,
    else_block: Option<BlockId>,
    span: Span,
    out: &mut Vec<StmtId>,
) {
    // Check for `break orig_label: v` first.
    let break_value = match &engine.body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value,
        } if l == orig_label => Some(*value),
        _ => None,
    };

    if let Some(value) = break_value {
        let some_case_expr = value.and_then(Operand::as_expr).filter(|&e| {
            matches!(&engine.body.exprs[e].kind,
                ExprKind::VariantConstruct { case_index: ci, .. } if *ci == case_index)
        });

        if let Some(vc_expr) = some_case_expr {
            // Extract payload expression from the VariantConstruct.
            let ExprKind::VariantConstruct { payload, .. } = &engine.body.exprs[vc_expr].kind
            else {
                unreachable!()
            };
            let payload_expr = payload.unwrap_or_else(|| {
                engine.const_operand(crate::nir_value_graph::ValueKind::Unit, payload_type)
            });

            // Emit: let __payload = payload_expr;
            let let_stmt = engine.alloc_stmt(
                StmtKind::Let {
                    name: format!("__fused_payload_{payload_local}"),
                    local_index: payload_local,
                    is_mut: false,
                    is_reactive: false,
                    type_id: payload_type,
                    value: payload_expr,
                    skip_value_copy: false,
                },
                span,
            );
            out.push(let_stmt);

            // Emit a fresh clone of `then_block`'s stmts, with the
            // `VariantPayload(temp, case)` subst applied through the engine.
            let subst_then = engine.clone_block(then_block);
            subst_variant_payload_in_block(
                engine,
                subst_then,
                temp_local,
                case_index,
                payload_local,
            );
            // Move the cloned stmts into `out` and empty the source block.
            // Leaving them parented to `subst_then` AND a new block at the
            // same time double-claims the stmt ids: the engine still
            // enqueues `subst_then` for `apply_block`, and downstream
            // rules (e.g. `const_branch_prune::eliminate_dead_stmts`'s
            // void-block flatten) see the now-orphaned stmts a second
            // time, which can erase live work.
            let cloned_stmts = std::mem::take(&mut engine.body.blocks[subst_then].stmts);
            out.extend(cloned_stmts);
        } else if let Some(eb) = else_block {
            // None / non-matching case → emit a clone of the else block.
            let cloned = engine.clone_block(eb);
            let cloned_stmts = std::mem::take(&mut engine.body.blocks[cloned].stmts);
            out.extend(cloned_stmts);
        }

        // Emit `break fused_label;` unless the last emitted statement already
        // terminates control flow.
        let last_terminates = out.last().is_some_and(|s| {
            matches!(
                engine.body.stmts[*s].kind,
                StmtKind::Break { .. } | StmtKind::Return { .. } | StmtKind::Continue
            )
        });
        if !last_terminates {
            let brk = engine.alloc_stmt(
                StmtKind::Break {
                    label: Some(fused_label.to_owned()),
                    value: None,
                },
                span,
            );
            out.push(brk);
        }
        return;
    }

    // For any other statement, recursively transform nested blocks in place.
    enum Shape {
        Blocks(Vec<BlockId>),
        Other,
    }
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::If {
            then_block: tb,
            else_block: eb,
            ..
        } => {
            let mut v = vec![*tb];
            if let Some(eb) = eb {
                v.push(*eb);
            }
            Shape::Blocks(v)
        }
        StmtKind::Loop { body: b } => Shape::Blocks(vec![*b]),
        StmtKind::LabeledBlock { label: l, block } if l != orig_label => {
            Shape::Blocks(vec![*block])
        }
        _ => Shape::Other,
    };
    match shape {
        Shape::Blocks(blocks) => {
            for b in blocks {
                let inner = std::mem::take(&mut engine.body.blocks[b].stmts);
                let transformed = transform_lb_stmts(
                    engine,
                    inner,
                    orig_label,
                    fused_label,
                    case_index,
                    temp_local,
                    payload_local,
                    payload_type,
                    then_block,
                    else_block,
                    span,
                );
                engine.set_block_stmts(b, transformed);
            }
            out.push(s);
        }
        Shape::Other => {
            transform_lb_in_stmt_kind(
                engine,
                s,
                orig_label,
                fused_label,
                case_index,
                temp_local,
                payload_local,
                payload_type,
                then_block,
                else_block,
                span,
            );
            out.push(s);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn transform_lb_in_stmt_kind(
    engine: &mut Engine,
    s: StmtId,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: BlockId,
    else_block: Option<BlockId>,
    span: Span,
) {
    let target = match &engine.body.stmts[s].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => Some(*value),
        StmtKind::Expr(value) => Some(*value),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => *value,
        StmtKind::If { .. }
        | StmtKind::Loop { .. }
        | StmtKind::LabeledBlock { .. }
        | StmtKind::Continue => None,
    };
    if let Some(e) = target.and_then(Operand::as_expr) {
        transform_lb_in_expr(
            engine,
            e,
            orig_label,
            fused_label,
            case_index,
            temp_local,
            payload_local,
            payload_type,
            then_block,
            else_block,
            span,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn transform_lb_in_expr(
    engine: &mut Engine,
    e: ExprId,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: BlockId,
    else_block: Option<BlockId>,
    span: Span,
) {
    enum Shape {
        Block(BlockId),
        Exprs(Vec<ExprId>),
        ExprsAndBlocks(Vec<ExprId>, Vec<BlockId>),
        None,
    }
    let shape = match &engine.body.exprs[e].kind {
        ExprKind::Block(block) => Shape::Block(*block),
        ExprKind::LabeledBlock {
            label: l, block, ..
        } => {
            if l.as_str() == orig_label {
                Shape::None
            } else {
                Shape::Block(*block)
            }
        }
        ExprKind::Match { expr, arms } => {
            // Promoted-value scrutinee / arm bodies / guards have no skeleton
            // subtree to descend into.
            let mut exprs: Vec<ExprId> = expr.as_expr().into_iter().collect();
            for arm in arms {
                exprs.extend(arm.body.as_expr());
                if let Some(g) = arm.guard {
                    exprs.extend(g.as_expr());
                }
            }
            Shape::Exprs(exprs)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut blocks = vec![*then_branch];
            if let Some(eb) = else_branch {
                blocks.push(*eb);
            }
            Shape::ExprsAndBlocks(condition.as_expr().into_iter().collect(), blocks)
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut blocks = arms.clone();
            blocks.push(*default);
            Shape::ExprsAndBlocks(scrutinee.as_expr().into_iter().collect(), blocks)
        }
        _ => Shape::None,
    };
    match shape {
        Shape::Block(b) => recurse_block(
            engine,
            b,
            orig_label,
            fused_label,
            case_index,
            temp_local,
            payload_local,
            payload_type,
            then_block,
            else_block,
            span,
        ),
        Shape::Exprs(exprs) => {
            for ex in exprs {
                transform_lb_in_expr(
                    engine,
                    ex,
                    orig_label,
                    fused_label,
                    case_index,
                    temp_local,
                    payload_local,
                    payload_type,
                    then_block,
                    else_block,
                    span,
                );
            }
        }
        Shape::ExprsAndBlocks(exprs, blocks) => {
            for ex in exprs {
                transform_lb_in_expr(
                    engine,
                    ex,
                    orig_label,
                    fused_label,
                    case_index,
                    temp_local,
                    payload_local,
                    payload_type,
                    then_block,
                    else_block,
                    span,
                );
            }
            for b in blocks {
                recurse_block(
                    engine,
                    b,
                    orig_label,
                    fused_label,
                    case_index,
                    temp_local,
                    payload_local,
                    payload_type,
                    then_block,
                    else_block,
                    span,
                );
            }
        }
        Shape::None => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn recurse_block(
    engine: &mut Engine,
    b: BlockId,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: BlockId,
    else_block: Option<BlockId>,
    span: Span,
) {
    let inner = std::mem::take(&mut engine.body.blocks[b].stmts);
    let transformed = transform_lb_stmts(
        engine,
        inner,
        orig_label,
        fused_label,
        case_index,
        temp_local,
        payload_local,
        payload_type,
        then_block,
        else_block,
        span,
    );
    engine.set_block_stmts(b, transformed);
}

/// Replace `VariantPayload { expr: Local(temp_local), case_index }` with
/// `Local(payload_local)` throughout a block. Engine-routed so each rewrite
/// updates the use index (the new Local mention is registered, the old
/// `VariantPayload`'s children are orphaned but never queried again).
fn subst_variant_payload_in_block(
    engine: &mut Engine,
    block: BlockId,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    for s in engine.body.blocks[block].stmts.clone() {
        subst_variant_payload_in_stmt(engine, s, temp_local, case_index, payload_local);
    }
}

fn subst_variant_payload_in_stmt(
    engine: &mut Engine,
    s: StmtId,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    enum Shape {
        Expr(ExprId),
        ExprAndBlocks(Option<ExprId>, Vec<BlockId>),
        None,
    }
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            // A promoted-constant value mentions no local — nothing to rewrite.
            value.as_expr().map_or(Shape::None, Shape::Expr)
        }
        StmtKind::Expr(expr) => expr.as_expr().map_or(Shape::None, Shape::Expr),
        StmtKind::Return { value } => match value {
            Some(v) => v.as_expr().map_or(Shape::None, Shape::Expr),
            None => Shape::None,
        },
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut blocks = vec![*then_block];
            if let Some(eb) = else_block {
                blocks.push(*eb);
            }
            Shape::ExprAndBlocks(condition.as_expr(), blocks)
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            Shape::ExprAndBlocks(None, vec![*b])
        }
        StmtKind::Break { value, .. } => match value {
            Some(v) => v.as_expr().map_or(Shape::None, Shape::Expr),
            None => Shape::None,
        },
        StmtKind::Continue => Shape::None,
    };
    match shape {
        Shape::Expr(e) => {
            subst_variant_payload_in_expr(engine, e, temp_local, case_index, payload_local);
        }
        Shape::ExprAndBlocks(cond, blocks) => {
            if let Some(c) = cond {
                subst_variant_payload_in_expr(engine, c, temp_local, case_index, payload_local);
            }
            for b in blocks {
                subst_variant_payload_in_block(engine, b, temp_local, case_index, payload_local);
            }
        }
        Shape::None => {}
    }
}

fn subst_variant_payload_in_expr(
    engine: &mut Engine,
    e: ExprId,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    // Match the target pattern first (top-down, before recursing).
    let is_target = if let ExprKind::VariantPayload {
        expr: inner,
        case_index: ci,
        ..
    } = &engine.body.exprs[e].kind
    {
        *ci == case_index
            && inner.as_expr().is_some_and(|ie| {
                matches!(&engine.body.exprs[ie].kind, ExprKind::Local { index, .. } if *index == temp_local)
            })
    } else {
        false
    };
    if is_target {
        engine.replace_expr_kind(
            e,
            ExprKind::Local {
                index: payload_local,
                name: format!("__fused_payload_{payload_local}"),
            },
        );
        return;
    }

    // Recurse into sub-expressions / sub-blocks (patterns excluded).
    enum Walk {
        Exprs(Vec<ExprId>),
        ExprsAndBlocks(Vec<ExprId>, Vec<BlockId>),
        Block(BlockId),
        None,
    }
    let walk = match &engine.body.exprs[e].kind {
        ExprKind::Binary { left, right, .. }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => Walk::Exprs(
            [*left, *right]
                .into_iter()
                .filter_map(Operand::as_expr)
                .collect(),
        ),
        ExprKind::Assign { target, value } => {
            Walk::Exprs(std::iter::once(*target).chain(value.as_expr()).collect())
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. } => {
            Walk::Exprs(inner.as_expr().into_iter().collect())
        }
        ExprKind::Call { args, .. } => {
            Walk::Exprs(args.iter().filter_map(|a| a.expr.as_expr()).collect())
        }
        ExprKind::CmRawCall { args, .. } => {
            Walk::Exprs(args.iter().filter_map(|o| o.as_expr()).collect())
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let mut v: Vec<ExprId> = receiver.as_expr().into_iter().collect();
            v.extend(args.iter().filter_map(|a| a.expr.as_expr()));
            Walk::Exprs(v)
        }
        ExprKind::IndirectCall { callee, args } => {
            let mut v: Vec<ExprId> = callee.as_expr().into_iter().collect();
            v.extend(args.iter().filter_map(|o| o.as_expr()));
            Walk::Exprs(v)
        }
        ExprKind::StructLiteral { fields, .. } => {
            Walk::Exprs(fields.iter().filter_map(|f| f.value.as_expr()).collect())
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            Walk::Exprs(elements.iter().filter_map(|o| o.as_expr()).collect())
        }
        ExprKind::VariantConstruct { payload, .. } => {
            Walk::Exprs(payload.iter().filter_map(|o| o.as_expr()).collect())
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => Walk::Block(*block),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut blocks = vec![*then_branch];
            if let Some(eb) = else_branch {
                blocks.push(*eb);
            }
            Walk::ExprsAndBlocks(condition.as_expr().into_iter().collect(), blocks)
        }
        ExprKind::Match { expr, arms } => {
            let mut exprs: Vec<ExprId> = expr.as_expr().into_iter().collect();
            for arm in arms {
                if let Some(b) = arm.body.as_expr() {
                    exprs.push(b);
                }
                if let Some(g) = arm.guard.and_then(Operand::as_expr) {
                    exprs.push(g);
                }
            }
            Walk::Exprs(exprs)
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut blocks = arms.clone();
            blocks.push(*default);
            Walk::ExprsAndBlocks(scrutinee.as_expr().into_iter().collect(), blocks)
        }
        _ => Walk::None,
    };
    match walk {
        Walk::Exprs(v) => {
            for id in v {
                subst_variant_payload_in_expr(engine, id, temp_local, case_index, payload_local);
            }
        }
        Walk::ExprsAndBlocks(exprs, blocks) => {
            for id in exprs {
                subst_variant_payload_in_expr(engine, id, temp_local, case_index, payload_local);
            }
            for b in blocks {
                subst_variant_payload_in_block(engine, b, temp_local, case_index, payload_local);
            }
        }
        Walk::Block(b) => {
            subst_variant_payload_in_block(engine, b, temp_local, case_index, payload_local);
        }
        Walk::None => {}
    }
}

/// Returns `true` if `block` contains a `Loop` statement at any nesting depth.
pub(super) fn block_contains_loop(body: &Body, block: BlockId) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .any(|s| stmt_contains_loop(body, *s))
}

fn stmt_contains_loop(body: &Body, s: StmtId) -> bool {
    match &body.stmts[s].kind {
        StmtKind::Loop { .. } => true,
        StmtKind::LabeledBlock { block, .. } => block_contains_loop(body, *block),
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            block_contains_loop(body, *then_block)
                || else_block.is_some_and(|b| block_contains_loop(body, b))
        }
        StmtKind::Let { value, .. }
        | StmtKind::LetDestructure { value, .. }
        | StmtKind::Return { value: Some(value) } => expr_contains_loop_operand(body, *value),
        StmtKind::Expr(value) => expr_contains_loop_operand(body, *value),
        _ => false,
    }
}

fn expr_contains_loop(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            block_contains_loop(body, *block)
        }
        _ => false,
    }
}

/// Returns `true` if `block` contains a "free" unlabeled `break;` or `continue`
/// — one not nested inside a `loop {}` within the block itself.
fn block_has_free_unlabeled_loop_exit(body: &Body, block: BlockId) -> bool {
    stmts_have_free_unlabeled_loop_exit(body, block, 0)
}

fn stmts_have_free_unlabeled_loop_exit(body: &Body, block: BlockId, loop_depth: u32) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .any(|s| stmt_has_free_unlabeled_loop_exit(body, *s, loop_depth))
}

fn stmt_has_free_unlabeled_loop_exit(body: &Body, s: StmtId, loop_depth: u32) -> bool {
    match &body.stmts[s].kind {
        StmtKind::Break { label: None, .. } | StmtKind::Continue => loop_depth == 0,
        StmtKind::Loop { body: b } => stmts_have_free_unlabeled_loop_exit(body, *b, loop_depth + 1),
        StmtKind::LabeledBlock { block, .. } => {
            stmts_have_free_unlabeled_loop_exit(body, *block, loop_depth)
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *condition, loop_depth)
                || stmts_have_free_unlabeled_loop_exit(body, *then_block, loop_depth)
                || else_block
                    .is_some_and(|b| stmts_have_free_unlabeled_loop_exit(body, b, loop_depth))
        }
        StmtKind::Let { value, .. }
        | StmtKind::LetDestructure { value, .. }
        | StmtKind::Return { value: Some(value) }
        | StmtKind::Break {
            value: Some(value), ..
        } => expr_has_free_unlabeled_loop_exit_operand(body, *value, loop_depth),
        StmtKind::Expr(value) => {
            expr_has_free_unlabeled_loop_exit_operand(body, *value, loop_depth)
        }
        _ => false,
    }
}

pub(super) fn expr_has_free_unlabeled_loop_exit(body: &Body, e: ExprId, loop_depth: u32) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            stmts_have_free_unlabeled_loop_exit(body, *block, loop_depth)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *condition, loop_depth)
                || stmts_have_free_unlabeled_loop_exit(body, *then_branch, loop_depth)
                || else_branch
                    .is_some_and(|b| stmts_have_free_unlabeled_loop_exit(body, b, loop_depth))
        }
        ExprKind::Binary { left, right, .. } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *left, loop_depth)
                || expr_has_free_unlabeled_loop_exit_operand(body, *right, loop_depth)
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *inner, loop_depth)
        }
        ExprKind::Assign { target, value } => {
            expr_has_free_unlabeled_loop_exit(body, *target, loop_depth)
                || expr_has_free_unlabeled_loop_exit_operand(body, *value, loop_depth)
        }
        ExprKind::Call { args, .. } => args
            .iter()
            .any(|a| expr_has_free_unlabeled_loop_exit_operand(body, a.expr, loop_depth)),
        ExprKind::MethodCall { receiver, args, .. } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *receiver, loop_depth)
                || args
                    .iter()
                    .any(|a| expr_has_free_unlabeled_loop_exit_operand(body, a.expr, loop_depth))
        }
        ExprKind::IndirectCall { callee, args } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *callee, loop_depth)
                || args
                    .iter()
                    .any(|a| expr_has_free_unlabeled_loop_exit_operand(body, *a, loop_depth))
        }
        ExprKind::CmRawCall { args, .. } => args
            .iter()
            .any(|a| expr_has_free_unlabeled_loop_exit_operand(body, *a, loop_depth)),
        ExprKind::Index { expr: inner, index } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *inner, loop_depth)
                || expr_has_free_unlabeled_loop_exit_operand(body, *index, loop_depth)
        }
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| expr_has_free_unlabeled_loop_exit_operand(body, f.value, loop_depth)),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .any(|e| expr_has_free_unlabeled_loop_exit_operand(body, *e, loop_depth)),
        ExprKind::VariantConstruct { payload, .. } => {
            payload.is_some_and(|p| expr_has_free_unlabeled_loop_exit_operand(body, p, loop_depth))
        }
        ExprKind::Match { expr, arms } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *expr, loop_depth)
                || arms.iter().any(|arm| {
                    expr_has_free_unlabeled_loop_exit_operand(body, arm.body, loop_depth)
                })
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_free_unlabeled_loop_exit_operand(body, *scrutinee, loop_depth)
                || arms
                    .iter()
                    .any(|b| stmts_have_free_unlabeled_loop_exit(body, *b, loop_depth))
                || stmts_have_free_unlabeled_loop_exit(body, *default, loop_depth)
        }
        _ => false,
    }
}
