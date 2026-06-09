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
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, PatKind, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::tir::TypeId;
use crate::token::Span;

use super::arena_query::{has_break_to, is_local};

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
            | StmtKind::Break {
                value: Some(_), ..
            } => true,
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
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[*let_value].kind
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
    } = &body.exprs[condition].kind
    else {
        return None;
    };
    let case_index = *case_index;
    let ExprKind::Local {
        index: tested_idx, ..
    } = &body.exprs[*vt_expr].kind
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
    let ExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &body.exprs[*let_value].kind
    else {
        return None;
    };
    let label = label.clone();
    let lb_block = *lb_block;

    // --- Stmt 2: Expr(Match { scrut: Local(temp), arms: [Variant, Wildcard] }) ---
    let StmtKind::Expr(match_expr) = &body.stmts[if_s].kind else {
        return None;
    };
    let ExprKind::Match { expr: scrut, arms } = &body.exprs[*match_expr].kind else {
        return None;
    };
    if arms.len() != 2 {
        return None;
    }
    let ExprKind::Local {
        index: tested_idx, ..
    } = &body.exprs[*scrut].kind
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
    if count_local_uses_in_expr(body, variant_arm_body, temp_local) > 0 {
        return None;
    }
    if count_local_uses_in_expr(body, else_arm_body, temp_local) > 0 {
        return None;
    }

    // --- THEN/ELSE bodies must not contain free unlabeled break/continue
    //     when the labeled block being fused contains a loop. ---
    if block_contains_loop(body, lb_block) {
        if arm_body_has_free_unlabeled_loop_exit(body, variant_arm_body) {
            return None;
        }
        if arm_body_has_free_unlabeled_loop_exit(body, else_arm_body) {
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
            if let ExprKind::VariantConstruct {
                case_index,
                case_name,
                ..
            } = &body.exprs[*v].kind
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
            find_break_case_index_for_name_in_expr(body, *value, label, variant_name)
        }
        StmtKind::Expr(expr) => {
            find_break_case_index_for_name_in_expr(body, *expr, label, variant_name)
        }
        StmtKind::Return { value } => {
            value.and_then(|v| find_break_case_index_for_name_in_expr(body, v, label, variant_name))
        }
        StmtKind::Break { value: Some(v), .. } => {
            find_break_case_index_for_name_in_expr(body, *v, label, variant_name)
        }
        StmtKind::Break { value: None, .. } | StmtKind::Continue => None,
    }
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
        } => find_break_case_index_for_name_in_expr(body, *condition, label, variant_name)
            .or_else(|| find_break_case_index_for_name(body, *then_branch, label, variant_name))
            .or_else(|| {
                else_branch
                    .and_then(|b| find_break_case_index_for_name(body, b, label, variant_name))
            }),
        ExprKind::Match { expr: scrut, arms } => find_break_case_index_for_name_in_expr(
            body,
            *scrut,
            label,
            variant_name,
        )
        .or_else(|| {
            arms.iter().find_map(|arm| {
                find_break_case_index_for_name_in_expr(body, arm.body, label, variant_name).or_else(
                    || {
                        arm.guard.and_then(|g| {
                            find_break_case_index_for_name_in_expr(body, g, label, variant_name)
                        })
                    },
                )
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
        } if l == label => match value.map(|v| &body.exprs[v].kind) {
            None | Some(ExprKind::Null) => true,
            Some(ExprKind::VariantConstruct {
                case_index: ci,
                payload,
                ..
            }) => {
                let ci = *ci;
                let payload = *payload;
                if let Some(p) = payload
                    && expr_has_break_to(body, label, p)
                {
                    return false;
                }
                if ci == case_index
                    && let Some(p) = payload
                {
                    *payload_type = Some(body.exprs[p].type_id);
                }
                true
            }
            _ => false,
        },
        StmtKind::LabeledBlock { label: l, .. } if l == label => true,
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let condition = *condition;
            let then_block = *then_block;
            let else_block = *else_block;
            check_lb_breaks_in_expr(body, condition, label, case_index, payload_type)
                && check_lb_breaks_in_block(body, then_block, label, case_index, payload_type)
                && else_block.is_none_or(|eb| {
                    check_lb_breaks_in_block(body, eb, label, case_index, payload_type)
                })
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            check_lb_breaks_in_block(body, *b, label, case_index, payload_type)
        }
        StmtKind::Let { value, .. } => {
            check_lb_breaks_in_expr(body, *value, label, case_index, payload_type)
        }
        StmtKind::Break { value, .. } => {
            value.is_none_or(|v| check_lb_breaks_in_expr(body, v, label, case_index, payload_type))
        }
        StmtKind::Return { value } => {
            value.is_none_or(|v| check_lb_breaks_in_expr(body, v, label, case_index, payload_type))
        }
        _ => true,
    }
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
            check_lb_breaks_in_expr(body, condition, label, case_index, payload_type)
                && check_lb_breaks_in_block(body, then_branch, label, case_index, payload_type)
                && else_branch.is_none_or(|eb| {
                    check_lb_breaks_in_block(body, eb, label, case_index, payload_type)
                })
        }
        _ => !expr_has_break_to(body, label, e),
    }
}

/// Count all occurrences of `Local { index: local_idx }` in a block.
fn count_local_uses_in_block(body: &Body, block: BlockId, local_idx: u32) -> usize {
    body.blocks[block]
        .stmts
        .iter()
        .map(|s| count_local_uses_in_stmt(body, *s, local_idx))
        .sum()
}

fn count_local_uses_in_stmt(body: &Body, s: StmtId, local_idx: u32) -> usize {
    match &body.stmts[s].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            count_local_uses_in_expr(body, *value, local_idx)
        }
        StmtKind::Expr(expr) => count_local_uses_in_expr(body, *expr, local_idx),
        StmtKind::Return { value } => {
            value.map_or(0, |v| count_local_uses_in_expr(body, v, local_idx))
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            count_local_uses_in_expr(body, *condition, local_idx)
                + count_local_uses_in_block(body, *then_block, local_idx)
                + else_block.map_or(0, |eb| count_local_uses_in_block(body, eb, local_idx))
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            count_local_uses_in_block(body, *b, local_idx)
        }
        StmtKind::Break { value, .. } => {
            value.map_or(0, |v| count_local_uses_in_expr(body, v, local_idx))
        }
        StmtKind::Continue => 0,
    }
}

fn count_local_uses_in_expr(body: &Body, e: ExprId, local_idx: u32) -> usize {
    match &body.exprs[e].kind {
        ExprKind::Local { .. } => usize::from(is_local(body, e, local_idx)),
        ExprKind::Binary { left, right, .. } => {
            count_local_uses_in_expr(body, *left, local_idx)
                + count_local_uses_in_expr(body, *right, local_idx)
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. } => {
            count_local_uses_in_expr(body, *inner, local_idx)
        }
        ExprKind::Assign { target, value } => {
            count_local_uses_in_expr(body, *target, local_idx)
                + count_local_uses_in_expr(body, *value, local_idx)
        }
        ExprKind::Index { expr: inner, index } => {
            count_local_uses_in_expr(body, *inner, local_idx)
                + count_local_uses_in_expr(body, *index, local_idx)
        }
        ExprKind::Call { args, .. } => args
            .iter()
            .map(|a| count_local_uses_in_expr(body, a.expr, local_idx))
            .sum(),
        ExprKind::CmRawCall { args, .. } => args
            .iter()
            .map(|a| count_local_uses_in_expr(body, *a, local_idx))
            .sum(),
        ExprKind::MethodCall { receiver, args, .. } => {
            count_local_uses_in_expr(body, *receiver, local_idx)
                + args
                    .iter()
                    .map(|a| count_local_uses_in_expr(body, a.expr, local_idx))
                    .sum::<usize>()
        }
        ExprKind::IndirectCall { callee, args } => {
            count_local_uses_in_expr(body, *callee, local_idx)
                + args
                    .iter()
                    .map(|a| count_local_uses_in_expr(body, *a, local_idx))
                    .sum::<usize>()
        }
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .map(|f| count_local_uses_in_expr(body, f.value, local_idx))
            .sum(),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .map(|e| count_local_uses_in_expr(body, *e, local_idx))
            .sum(),
        ExprKind::VariantConstruct { payload, .. } => {
            payload.map_or(0, |p| count_local_uses_in_expr(body, p, local_idx))
        }
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            count_local_uses_in_block(body, *block, local_idx)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            count_local_uses_in_expr(body, *condition, local_idx)
                + count_local_uses_in_block(body, *then_branch, local_idx)
                + else_branch.map_or(0, |eb| count_local_uses_in_block(body, eb, local_idx))
        }
        ExprKind::Match { expr, arms } => {
            count_local_uses_in_expr(body, *expr, local_idx)
                + arms
                    .iter()
                    .map(|arm| {
                        count_local_uses_in_expr(body, arm.body, local_idx)
                            + arm
                                .guard
                                .map_or(0, |g| count_local_uses_in_expr(body, g, local_idx))
                    })
                    .sum::<usize>()
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_local_uses_in_expr(body, *scrutinee, local_idx)
                + arms
                    .iter()
                    .map(|arm| count_local_uses_in_block(body, *arm, local_idx))
                    .sum::<usize>()
                + count_local_uses_in_block(body, *default, local_idx)
        }
        _ => 0,
    }
}

/// Count `VariantPayload { expr: Local(local_idx), case_index }` in a block.
fn count_variant_payload_uses_in_block(
    body: &Body,
    block: BlockId,
    local_idx: u32,
    case_index: u32,
) -> usize {
    body.blocks[block]
        .stmts
        .iter()
        .map(|s| count_variant_payload_uses_in_stmt(body, *s, local_idx, case_index))
        .sum()
}

fn count_variant_payload_uses_in_stmt(
    body: &Body,
    s: StmtId,
    local_idx: u32,
    case_index: u32,
) -> usize {
    match &body.stmts[s].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            count_variant_payload_uses_in_expr(body, *value, local_idx, case_index)
        }
        StmtKind::Expr(expr) => {
            count_variant_payload_uses_in_expr(body, *expr, local_idx, case_index)
        }
        StmtKind::Return { value } => value.map_or(0, |v| {
            count_variant_payload_uses_in_expr(body, v, local_idx, case_index)
        }),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            count_variant_payload_uses_in_expr(body, *condition, local_idx, case_index)
                + count_variant_payload_uses_in_block(body, *then_block, local_idx, case_index)
                + else_block.map_or(0, |eb| {
                    count_variant_payload_uses_in_block(body, eb, local_idx, case_index)
                })
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            count_variant_payload_uses_in_block(body, *b, local_idx, case_index)
        }
        StmtKind::Break { value, .. } => value.map_or(0, |v| {
            count_variant_payload_uses_in_expr(body, v, local_idx, case_index)
        }),
        StmtKind::Continue => 0,
    }
}

fn count_variant_payload_uses_in_expr(
    body: &Body,
    e: ExprId,
    local_idx: u32,
    case_index: u32,
) -> usize {
    match &body.exprs[e].kind {
        ExprKind::VariantPayload {
            expr: inner,
            case_index: ci,
            ..
        } if *ci == case_index => {
            let inner = *inner;
            if is_local(body, inner, local_idx) {
                return 1;
            }
            count_variant_payload_uses_in_expr(body, inner, local_idx, case_index)
        }
        ExprKind::Local { .. } => 0,
        ExprKind::Binary { left, right, .. } => {
            count_variant_payload_uses_in_expr(body, *left, local_idx, case_index)
                + count_variant_payload_uses_in_expr(body, *right, local_idx, case_index)
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. } => {
            count_variant_payload_uses_in_expr(body, *inner, local_idx, case_index)
        }
        ExprKind::Assign { target, value } => {
            count_variant_payload_uses_in_expr(body, *target, local_idx, case_index)
                + count_variant_payload_uses_in_expr(body, *value, local_idx, case_index)
        }
        ExprKind::Index { expr: inner, index } => {
            count_variant_payload_uses_in_expr(body, *inner, local_idx, case_index)
                + count_variant_payload_uses_in_expr(body, *index, local_idx, case_index)
        }
        ExprKind::Call { args, .. } => args
            .iter()
            .map(|a| count_variant_payload_uses_in_expr(body, a.expr, local_idx, case_index))
            .sum(),
        ExprKind::CmRawCall { args, .. } => args
            .iter()
            .map(|a| count_variant_payload_uses_in_expr(body, *a, local_idx, case_index))
            .sum(),
        ExprKind::MethodCall { receiver, args, .. } => {
            count_variant_payload_uses_in_expr(body, *receiver, local_idx, case_index)
                + args
                    .iter()
                    .map(|a| {
                        count_variant_payload_uses_in_expr(body, a.expr, local_idx, case_index)
                    })
                    .sum::<usize>()
        }
        ExprKind::IndirectCall { callee, args } => {
            count_variant_payload_uses_in_expr(body, *callee, local_idx, case_index)
                + args
                    .iter()
                    .map(|a| count_variant_payload_uses_in_expr(body, *a, local_idx, case_index))
                    .sum::<usize>()
        }
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .map(|f| count_variant_payload_uses_in_expr(body, f.value, local_idx, case_index))
            .sum(),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .map(|e| count_variant_payload_uses_in_expr(body, *e, local_idx, case_index))
            .sum(),
        ExprKind::VariantConstruct { payload, .. } => payload.map_or(0, |p| {
            count_variant_payload_uses_in_expr(body, p, local_idx, case_index)
        }),
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            count_variant_payload_uses_in_block(body, *block, local_idx, case_index)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            count_variant_payload_uses_in_expr(body, *condition, local_idx, case_index)
                + count_variant_payload_uses_in_block(body, *then_branch, local_idx, case_index)
                + else_branch.map_or(0, |eb| {
                    count_variant_payload_uses_in_block(body, eb, local_idx, case_index)
                })
        }
        ExprKind::Match { expr, arms } => {
            count_variant_payload_uses_in_expr(body, *expr, local_idx, case_index)
                + arms
                    .iter()
                    .map(|arm| {
                        count_variant_payload_uses_in_expr(body, arm.body, local_idx, case_index)
                            + arm.guard.map_or(0, |g| {
                                count_variant_payload_uses_in_expr(body, g, local_idx, case_index)
                            })
                    })
                    .sum::<usize>()
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_variant_payload_uses_in_expr(body, *scrutinee, local_idx, case_index)
                + arms
                    .iter()
                    .map(|arm| {
                        count_variant_payload_uses_in_block(body, *arm, local_idx, case_index)
                    })
                    .sum::<usize>()
                + count_variant_payload_uses_in_block(body, *default, local_idx, case_index)
        }
        _ => 0,
    }
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
    let ExprKind::LabeledBlock {
        block: lb_block, ..
    } = &engine.body.exprs[*let_value].kind
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
            let ExprKind::Match { arms, .. } = &engine.body.exprs[*match_expr].kind else {
                unreachable!()
            };
            let variant_body = arms[0].body;
            let else_body = arms[1].body;
            let then_block = arm_body_into_block(engine, variant_body, span);
            let else_block = match &engine.body.exprs[else_body].kind {
                ExprKind::Unit => None,
                _ => Some(arm_body_into_block(engine, else_body, span)),
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
        let is_some_case = match value {
            Some(v) => matches!(&engine.body.exprs[v].kind,
                ExprKind::VariantConstruct { case_index: ci, .. } if *ci == case_index),
            None => false,
        };

        if is_some_case {
            // Extract payload expression from the VariantConstruct.
            let v = value.unwrap();
            let ExprKind::VariantConstruct { payload, .. } = &engine.body.exprs[v].kind else {
                unreachable!()
            };
            let payload_expr = payload.unwrap_or_else(|| {
                engine.alloc_expr(ExprKind::Unit, payload_type, span)
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
            let cloned_stmts = engine.body.blocks[subst_then].stmts.clone();
            out.extend(cloned_stmts);
        } else if let Some(eb) = else_block {
            // None / non-matching case → emit a clone of the else block.
            let cloned = engine.clone_block(eb);
            let cloned_stmts = engine.body.blocks[cloned].stmts.clone();
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
        StmtKind::Let { value, .. }
        | StmtKind::LetDestructure { value, .. }
        | StmtKind::Expr(value) => Some(*value),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => *value,
        StmtKind::If { .. }
        | StmtKind::Loop { .. }
        | StmtKind::LabeledBlock { .. }
        | StmtKind::Continue => None,
    };
    if let Some(v) = target {
        transform_lb_in_expr(
            engine,
            v,
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
            let mut exprs = vec![*expr];
            for arm in arms {
                exprs.push(arm.body);
                if let Some(g) = arm.guard {
                    exprs.push(g);
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
            Shape::ExprsAndBlocks(vec![*condition], blocks)
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut blocks = arms.clone();
            blocks.push(*default);
            Shape::ExprsAndBlocks(vec![*scrutinee], blocks)
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
/// VariantPayload's children are orphaned but never queried again).
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
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => Shape::Expr(*value),
        StmtKind::Expr(expr) => Shape::Expr(*expr),
        StmtKind::Return { value } => match value {
            Some(v) => Shape::Expr(*v),
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
            Shape::ExprAndBlocks(Some(*condition), blocks)
        }
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => {
            Shape::ExprAndBlocks(None, vec![*b])
        }
        StmtKind::Break { value, .. } => match value {
            Some(v) => Shape::Expr(*v),
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
            && matches!(&engine.body.exprs[*inner].kind, ExprKind::Local { index, .. } if *index == temp_local)
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
        | ExprKind::Assign {
            target: left,
            value: right,
        }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => Walk::Exprs(vec![*left, *right]),
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. } => Walk::Exprs(vec![*inner]),
        ExprKind::Call { args, .. } => Walk::Exprs(args.iter().map(|a| a.expr).collect()),
        ExprKind::CmRawCall { args, .. } => Walk::Exprs(args.clone()),
        ExprKind::MethodCall { receiver, args, .. } => {
            let mut v = vec![*receiver];
            v.extend(args.iter().map(|a| a.expr));
            Walk::Exprs(v)
        }
        ExprKind::IndirectCall { callee, args } => {
            let mut v = vec![*callee];
            v.extend(args.iter().copied());
            Walk::Exprs(v)
        }
        ExprKind::StructLiteral { fields, .. } => {
            Walk::Exprs(fields.iter().map(|f| f.value).collect())
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            Walk::Exprs(elements.clone())
        }
        ExprKind::VariantConstruct { payload, .. } => {
            Walk::Exprs(payload.iter().copied().collect())
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
            Walk::ExprsAndBlocks(vec![*condition], blocks)
        }
        ExprKind::Match { expr, arms } => {
            let mut exprs = vec![*expr];
            for arm in arms {
                exprs.push(arm.body);
                if let Some(g) = arm.guard {
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
            Walk::ExprsAndBlocks(vec![*scrutinee], blocks)
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
fn block_contains_loop(body: &Body, block: BlockId) -> bool {
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
        | StmtKind::Expr(value)
        | StmtKind::Return { value: Some(value) } => expr_contains_loop(body, *value),
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
            expr_has_free_unlabeled_loop_exit(body, *condition, loop_depth)
                || stmts_have_free_unlabeled_loop_exit(body, *then_block, loop_depth)
                || else_block
                    .is_some_and(|b| stmts_have_free_unlabeled_loop_exit(body, b, loop_depth))
        }
        StmtKind::Let { value, .. }
        | StmtKind::LetDestructure { value, .. }
        | StmtKind::Expr(value)
        | StmtKind::Return { value: Some(value) }
        | StmtKind::Break {
            value: Some(value), ..
        } => expr_has_free_unlabeled_loop_exit(body, *value, loop_depth),
        _ => false,
    }
}

fn expr_has_free_unlabeled_loop_exit(body: &Body, e: ExprId, loop_depth: u32) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Block(block) | ExprKind::LabeledBlock { block, .. } => {
            stmts_have_free_unlabeled_loop_exit(body, *block, loop_depth)
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_free_unlabeled_loop_exit(body, *condition, loop_depth)
                || stmts_have_free_unlabeled_loop_exit(body, *then_branch, loop_depth)
                || else_branch
                    .is_some_and(|b| stmts_have_free_unlabeled_loop_exit(body, b, loop_depth))
        }
        ExprKind::Binary { left, right, .. } => {
            expr_has_free_unlabeled_loop_exit(body, *left, loop_depth)
                || expr_has_free_unlabeled_loop_exit(body, *right, loop_depth)
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. } => {
            expr_has_free_unlabeled_loop_exit(body, *inner, loop_depth)
        }
        ExprKind::Assign { target, value } => {
            expr_has_free_unlabeled_loop_exit(body, *target, loop_depth)
                || expr_has_free_unlabeled_loop_exit(body, *value, loop_depth)
        }
        ExprKind::Call { args, .. } => args
            .iter()
            .any(|a| expr_has_free_unlabeled_loop_exit(body, a.expr, loop_depth)),
        ExprKind::MethodCall { receiver, args, .. } => {
            expr_has_free_unlabeled_loop_exit(body, *receiver, loop_depth)
                || args
                    .iter()
                    .any(|a| expr_has_free_unlabeled_loop_exit(body, a.expr, loop_depth))
        }
        ExprKind::IndirectCall { callee, args } => {
            expr_has_free_unlabeled_loop_exit(body, *callee, loop_depth)
                || args
                    .iter()
                    .any(|a| expr_has_free_unlabeled_loop_exit(body, *a, loop_depth))
        }
        ExprKind::CmRawCall { args, .. } => args
            .iter()
            .any(|a| expr_has_free_unlabeled_loop_exit(body, *a, loop_depth)),
        ExprKind::Index { expr: inner, index } => {
            expr_has_free_unlabeled_loop_exit(body, *inner, loop_depth)
                || expr_has_free_unlabeled_loop_exit(body, *index, loop_depth)
        }
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| expr_has_free_unlabeled_loop_exit(body, f.value, loop_depth)),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
            .iter()
            .any(|e| expr_has_free_unlabeled_loop_exit(body, *e, loop_depth)),
        ExprKind::VariantConstruct { payload, .. } => {
            payload.is_some_and(|p| expr_has_free_unlabeled_loop_exit(body, p, loop_depth))
        }
        ExprKind::Match { expr, arms } => {
            expr_has_free_unlabeled_loop_exit(body, *expr, loop_depth)
                || arms
                    .iter()
                    .any(|arm| expr_has_free_unlabeled_loop_exit(body, arm.body, loop_depth))
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_free_unlabeled_loop_exit(body, *scrutinee, loop_depth)
                || arms
                    .iter()
                    .any(|b| stmts_have_free_unlabeled_loop_exit(body, *b, loop_depth))
                || stmts_have_free_unlabeled_loop_exit(body, *default, loop_depth)
        }
        _ => false,
    }
}

