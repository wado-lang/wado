//! LabeledBlock-IfVariant fusion pass.
//!
//! This pass detects the pattern produced by inlining `Option<T>`/`Result<T, E>`-returning
//! functions into if-let call sites, where an intermediate GC allocation is created for the
//! variant result and then immediately unpacked by a `VariantTest`/`VariantPayload` pair.
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
//! This eliminates the GC-allocated `temp: Option<T>` entirely. Subsequent passes
//! (copy propagation, DCE) clean up the remaining `break '__fused_L;` bookkeeping.
//!
//! The pass reads and mutates the arena [`Body`] directly. Fusion moves the
//! labeled block's statements (reusing their ids) and deep-clones the THEN/ELSE
//! blocks into each break site via [`Body::clone_block`].

use crate::nir::{NirFunction, NirLocal};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, PatKind, StmtId, StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::tir::TypeId;
use crate::token::Span;

use super::arena_query::{has_break_to, is_local};

/// `expr_has_break_to` arena adapter.
fn expr_has_break_to(body: &Body, label: &str, e: ExprId) -> bool {
    has_break_to(body, NodeRef::Expr(e), label)
}

pub fn fuse_labeled_blocks(project: &mut NirPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= fuse_in_function(&mut func);
    }
    changed
}

fn fuse_in_function(func: &mut NirFunction) -> bool {
    if func.body.is_none() {
        return false;
    }
    let mut local_count = func.local_count;
    // The local list is read (binding-type checks) and grown (fused payload
    // slots), so thread an owned clone and write it back once the body borrow
    // ends.
    let mut locals = func.locals.clone();
    let r = {
        let body = func.body.as_mut().unwrap();
        let root = body.root;
        fuse_in_block(
            body,
            root,
            /* yields_value */ false,
            &mut local_count,
            &mut locals,
        )
    };
    func.local_count = local_count;
    func.locals = locals;
    r
}

/// `yields_value` is `true` when the value of `block`'s terminal statement is
/// consumed by the enclosing context (e.g. `let x = { …; if-let-expr }`).
fn fuse_in_block(
    body: &mut Body,
    block: BlockId,
    yields_value: bool,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
) -> bool {
    let mut changed = false;
    let stmts = body.blocks[block].stmts.clone();
    let last_idx = stmts.len().saturating_sub(1);
    for (i, s) in stmts.iter().enumerate() {
        let stmt_yields_value = yields_value && i == last_idx;
        changed |= fuse_in_stmt(body, *s, stmt_yields_value, local_count, locals);
    }
    changed |= fuse_adjacent_pairs(body, block, yields_value, local_count, locals);
    changed
}

fn fuse_in_stmt(
    body: &mut Body,
    s: StmtId,
    yields_value: bool,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
) -> bool {
    enum Shape {
        Expr(ExprId),
        If(ExprId, BlockId, Option<BlockId>),
        Block(BlockId),
        None,
    }
    let shape = match &body.stmts[s].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => Shape::Expr(*value),
        StmtKind::Expr(expr) => Shape::Expr(*expr),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => Shape::If(*condition, *then_block, *else_block),
        StmtKind::Loop { body: b } | StmtKind::LabeledBlock { block: b, .. } => Shape::Block(*b),
        StmtKind::Break { value, .. } | StmtKind::Return { value } => match value {
            Some(v) => Shape::Expr(*v),
            None => Shape::None,
        },
        StmtKind::Continue => Shape::None,
    };
    match shape {
        Shape::Expr(e) => fuse_in_expr(body, e, local_count, locals),
        Shape::If(cond, tb, eb) => {
            let mut changed = fuse_in_expr(body, cond, local_count, locals);
            changed |= fuse_in_block(body, tb, yields_value, local_count, locals);
            if let Some(eb) = eb {
                changed |= fuse_in_block(body, eb, yields_value, local_count, locals);
            }
            changed
        }
        // Loop / statement-level labeled blocks discard their value.
        Shape::Block(b) => fuse_in_block(body, b, false, local_count, locals),
        Shape::None => false,
    }
}

/// Check if a labeled block expression is trivially a single `break label: value`
/// statement; if so, inline the break value, eliminating the labeled block.
fn try_inline_trivial_labeled_block(body: &mut Body, e: ExprId) -> bool {
    let (label, block) = match &body.exprs[e].kind {
        ExprKind::LabeledBlock { label, block, .. } => (label.clone(), *block),
        _ => return false,
    };
    if body.blocks[block].stmts.len() != 1 {
        return false;
    }
    let s0 = body.blocks[block].stmts[0];
    let break_value = match &body.stmts[s0].kind {
        StmtKind::Break {
            label: Some(break_label),
            value: Some(break_value),
        } if *break_label == label => *break_value,
        _ => return false,
    };
    // Don't inline if the break value itself contains breaks to the same label.
    if expr_has_break_to(body, &label, break_value) {
        return false;
    }
    // Replace `e` with the break value's kind, keeping `e`'s own type/span.
    let bv_kind = body.exprs[break_value].kind.clone();
    body.exprs[e].kind = bv_kind;
    true
}

fn fuse_in_expr(
    body: &mut Body,
    e: ExprId,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
) -> bool {
    enum Shape {
        LabeledBlock(BlockId),
        Block(BlockId),
        If(ExprId, BlockId, Option<BlockId>),
        Exprs(Vec<ExprId>),
        None,
    }
    let shape = match &body.exprs[e].kind {
        ExprKind::LabeledBlock { block, .. } => Shape::LabeledBlock(*block),
        ExprKind::Block(block) => Shape::Block(*block),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => Shape::If(*condition, *then_branch, *else_branch),
        ExprKind::Binary { left, right, .. }
        | ExprKind::Assign {
            target: left,
            value: right,
        }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => Shape::Exprs(vec![*left, *right]),
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. } => Shape::Exprs(vec![*inner]),
        ExprKind::Call { args, .. } | ExprKind::MethodCall { args, .. } => {
            // (MethodCall handled together; receiver added below if present.)
            let mut v: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            if let ExprKind::MethodCall { receiver, .. } = &body.exprs[e].kind {
                v.insert(0, *receiver);
            }
            Shape::Exprs(v)
        }
        ExprKind::CmRawCall { args, .. } => Shape::Exprs(args.clone()),
        ExprKind::IndirectCall { callee, args } => {
            let mut v = vec![*callee];
            v.extend(args.iter().copied());
            Shape::Exprs(v)
        }
        ExprKind::StructLiteral { fields, .. } => {
            Shape::Exprs(fields.iter().map(|f| f.value).collect())
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            Shape::Exprs(elements.clone())
        }
        ExprKind::VariantConstruct { payload, .. } => {
            Shape::Exprs(payload.iter().copied().collect())
        }
        ExprKind::Match { expr, arms } => {
            let mut v = vec![*expr];
            for arm in arms {
                v.push(arm.body);
                if let Some(g) = arm.guard {
                    v.push(g);
                }
            }
            Shape::Exprs(v)
        }
        ExprKind::Switch { .. } => {
            // Switch arms are blocks; recurse via a dedicated path.
            Shape::None
        }
        _ => Shape::None,
    };
    match shape {
        Shape::LabeledBlock(block) => {
            let changed = fuse_in_block(
                body,
                block,
                /* yields_value */ true,
                local_count,
                locals,
            );
            try_inline_trivial_labeled_block(body, e) || changed
        }
        Shape::Block(block) => fuse_in_block(body, block, true, local_count, locals),
        Shape::If(cond, tb, eb) => {
            let mut changed = fuse_in_expr(body, cond, local_count, locals);
            changed |= fuse_in_block(body, tb, true, local_count, locals);
            if let Some(eb) = eb {
                changed |= fuse_in_block(body, eb, true, local_count, locals);
            }
            changed
        }
        Shape::Exprs(v) => {
            let mut changed = false;
            for id in v {
                changed |= fuse_in_expr(body, id, local_count, locals);
            }
            changed
        }
        Shape::None => {
            // Switch: recurse into scrutinee + arm/default blocks.
            if let ExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } = &body.exprs[e].kind
            {
                let scrutinee = *scrutinee;
                let arms = arms.clone();
                let default = *default;
                // Switch is an expression: each arm contributes the value.
                let mut changed = fuse_in_expr(body, scrutinee, local_count, locals);
                for a in arms {
                    changed |= fuse_in_block(body, a, true, local_count, locals);
                }
                changed |= fuse_in_block(body, default, true, local_count, locals);
                changed
            } else {
                false
            }
        }
    }
}

fn fuse_adjacent_pairs(
    body: &mut Body,
    block: BlockId,
    yields_value: bool,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
) -> bool {
    let stmts = std::mem::take(&mut body.blocks[block].stmts);
    let mut new_stmts = Vec::with_capacity(stmts.len());
    let mut changed = false;
    let mut i = 0;
    while i < stmts.len() {
        let let_s = stmts[i];
        let info = if i + 1 < stmts.len() {
            check_fusion_preconditions(body, let_s, stmts[i + 1], locals)
        } else {
            None
        };
        if let Some(info) = info {
            let if_s = stmts[i + 1];
            // Refuse to fuse when the If is the last statement of a
            // value-yielding block (the fused block's breaks carry no value).
            if yields_value && i + 2 == stmts.len() {
                new_stmts.push(let_s);
                new_stmts.push(if_s);
                i += 2;
                continue;
            }
            let fused = perform_fusion(body, let_s, if_s, info, local_count, locals);
            new_stmts.extend(fused);
            changed = true;
            i += 2;
        } else {
            new_stmts.push(let_s);
            i += 1;
        }
    }
    body.blocks[block].stmts = new_stmts;
    changed
}

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
/// when the body is not already a `Block`.
fn arm_body_into_block(body: &mut Body, arm_body: ExprId, fallback_span: Span) -> BlockId {
    if let ExprKind::Block(block) = &body.exprs[arm_body].kind {
        *block
    } else {
        let stmt = body.stmts.push(StmtNode {
            kind: StmtKind::Expr(arm_body),
            span: fallback_span,
        });
        body.blocks.push(crate::nir_arena::BlockNode {
            stmts: vec![stmt],
            span: fallback_span,
        })
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

/// Perform the actual fusion transformation, returning the fused statement id(s).
fn perform_fusion(
    body: &mut Body,
    let_s: StmtId,
    if_s: StmtId,
    info: FusionInfo,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
) -> Vec<StmtId> {
    let span = body.stmts[let_s].span;

    // Extract the LabeledBlock body from the Let statement.
    let StmtKind::Let {
        value: let_value, ..
    } = &body.stmts[let_s].kind
    else {
        unreachable!()
    };
    let ExprKind::LabeledBlock {
        block: lb_block, ..
    } = &body.exprs[*let_value].kind
    else {
        unreachable!()
    };
    let lb_block = *lb_block;

    // Extract the then/else blocks from the consumer statement.
    let (then_block, else_block) = match &body.stmts[if_s].kind {
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => (*then_block, *else_block),
        StmtKind::Expr(match_expr) => {
            let ExprKind::Match { arms, .. } = &body.exprs[*match_expr].kind else {
                unreachable!()
            };
            let variant_body = arms[0].body;
            let else_body = arms[1].body;
            let then_block = arm_body_into_block(body, variant_body, span);
            let else_block = match &body.exprs[else_body].kind {
                ExprKind::Unit => None,
                _ => Some(arm_body_into_block(body, else_body, span)),
            };
            (then_block, else_block)
        }
        _ => unreachable!(),
    };

    // Pick the payload local — reuse the Match arm's pattern binding slot if any,
    // else allocate a fresh `__fused_payload_N`.
    let payload_local = if let Some(b_idx) = info.pattern_payload_binding {
        b_idx
    } else {
        let payload_local = *local_count;
        *local_count += 1;
        locals.push(NirLocal {
            name: format!("__fused_payload_{payload_local}"),
            type_id: info.payload_type,
            is_mut: false,
        });
        payload_local
    };

    let fused_label = format!("__fused_{}", info.label);

    let lb_stmts = std::mem::take(&mut body.blocks[lb_block].stmts);
    let fused_stmts = transform_lb_stmts(
        body,
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

    let fused_body = body.blocks.push(crate::nir_arena::BlockNode {
        stmts: fused_stmts,
        span,
    });
    let fused_stmt = body.stmts.push(StmtNode {
        kind: StmtKind::LabeledBlock {
            label: fused_label,
            block: fused_body,
        },
        span,
    });
    vec![fused_stmt]
}

#[allow(clippy::too_many_arguments)]
fn transform_lb_stmts(
    body: &mut Body,
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
            body,
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
    body: &mut Body,
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
    let break_value = match &body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value,
        } if l == orig_label => Some(*value),
        _ => None,
    };

    if let Some(value) = break_value {
        let is_some_case = match value {
            Some(v) => matches!(&body.exprs[v].kind,
                ExprKind::VariantConstruct { case_index: ci, .. } if *ci == case_index),
            None => false,
        };

        if is_some_case {
            // Extract payload expression from the VariantConstruct.
            let v = value.unwrap();
            let ExprKind::VariantConstruct { payload, .. } = &body.exprs[v].kind else {
                unreachable!()
            };
            let payload_expr = payload.unwrap_or_else(|| {
                body.exprs.push(ExprNode {
                    kind: ExprKind::Unit,
                    type_id: payload_type,
                    span,
                })
            });

            // Emit: let __payload = payload_expr;
            let let_stmt = body.stmts.push(StmtNode {
                kind: StmtKind::Let {
                    name: format!("__fused_payload_{payload_local}"),
                    local_index: payload_local,
                    is_mut: false,
                    is_reactive: false,
                    type_id: payload_type,
                    value: payload_expr,
                    skip_value_copy: false,
                },
                span,
            });
            out.push(let_stmt);

            // Emit then_block stmts (a fresh clone) with the variant payload subst.
            let subst_then = body.clone_block(then_block);
            subst_variant_payload_in_block(body, subst_then, temp_local, case_index, payload_local);
            out.extend(body.blocks[subst_then].stmts.clone());
        } else if let Some(eb) = else_block {
            // None / non-matching case → emit a clone of the else block.
            let cloned = body.clone_block(eb);
            out.extend(body.blocks[cloned].stmts.clone());
        }

        // Emit `break fused_label;` unless the last emitted statement already
        // terminates control flow.
        let last_terminates = out.last().is_some_and(|s| {
            matches!(
                body.stmts[*s].kind,
                StmtKind::Break { .. } | StmtKind::Return { .. } | StmtKind::Continue
            )
        });
        if !last_terminates {
            let brk = body.stmts.push(StmtNode {
                kind: StmtKind::Break {
                    label: Some(fused_label.to_owned()),
                    value: None,
                },
                span,
            });
            out.push(brk);
        }
        return;
    }

    // For any other statement, recursively transform nested blocks in place.
    enum Shape {
        Blocks(Vec<BlockId>),
        Other,
    }
    let shape = match &body.stmts[s].kind {
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
                let inner = std::mem::take(&mut body.blocks[b].stmts);
                let transformed = transform_lb_stmts(
                    body,
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
                body.blocks[b].stmts = transformed;
            }
            out.push(s);
        }
        Shape::Other => {
            transform_lb_in_stmt_kind(
                body,
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
    body: &mut Body,
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
    let target = match &body.stmts[s].kind {
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
            body,
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
    body: &mut Body,
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
    let shape = match &body.exprs[e].kind {
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
    let recurse_block = |body: &mut Body, b: BlockId| {
        let inner = std::mem::take(&mut body.blocks[b].stmts);
        let transformed = transform_lb_stmts(
            body,
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
        body.blocks[b].stmts = transformed;
    };
    match shape {
        Shape::Block(b) => recurse_block(body, b),
        Shape::Exprs(exprs) => {
            for ex in exprs {
                transform_lb_in_expr(
                    body,
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
                    body,
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
                recurse_block(body, b);
            }
        }
        Shape::None => {}
    }
}

/// Replace `VariantPayload { expr: Local(temp_local), case_index }` with
/// `Local(payload_local)` throughout a block.
fn subst_variant_payload_in_block(
    body: &mut Body,
    block: BlockId,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    for s in body.blocks[block].stmts.clone() {
        subst_variant_payload_in_stmt(body, s, temp_local, case_index, payload_local);
    }
}

fn subst_variant_payload_in_stmt(
    body: &mut Body,
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
    let shape = match &body.stmts[s].kind {
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
            subst_variant_payload_in_expr(body, e, temp_local, case_index, payload_local);
        }
        Shape::ExprAndBlocks(cond, blocks) => {
            if let Some(c) = cond {
                subst_variant_payload_in_expr(body, c, temp_local, case_index, payload_local);
            }
            for b in blocks {
                subst_variant_payload_in_block(body, b, temp_local, case_index, payload_local);
            }
        }
        Shape::None => {}
    }
}

fn subst_variant_payload_in_expr(
    body: &mut Body,
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
    } = &body.exprs[e].kind
    {
        *ci == case_index
            && matches!(&body.exprs[*inner].kind, ExprKind::Local { index, .. } if *index == temp_local)
    } else {
        false
    };
    if is_target {
        body.exprs[e].kind = ExprKind::Local {
            index: payload_local,
            name: format!("__fused_payload_{payload_local}"),
        };
        return;
    }

    // Recurse into sub-expressions / sub-blocks (patterns excluded).
    enum Walk {
        Exprs(Vec<ExprId>),
        ExprsAndBlocks(Vec<ExprId>, Vec<BlockId>),
        Block(BlockId),
        None,
    }
    let walk = match &body.exprs[e].kind {
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
                subst_variant_payload_in_expr(body, id, temp_local, case_index, payload_local);
            }
        }
        Walk::ExprsAndBlocks(exprs, blocks) => {
            for id in exprs {
                subst_variant_payload_in_expr(body, id, temp_local, case_index, payload_local);
            }
            for b in blocks {
                subst_variant_payload_in_block(body, b, temp_local, case_index, payload_local);
            }
        }
        Walk::Block(b) => {
            subst_variant_payload_in_block(body, b, temp_local, case_index, payload_local);
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
