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

use crate::flat_package::FlatPackage;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TypeId};
use crate::tir_visitor::expr_has_break_to;

pub fn fuse_labeled_blocks(project: &mut FlatPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= fuse_in_function(&mut func);
    }
    changed
}

fn fuse_in_function(func: &mut TirFunction) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };
    fuse_in_block(body, &mut func.local_count, &mut func.local_types)
}

fn fuse_in_block(
    block: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
) -> bool {
    // Recurse first into any nested blocks/stmts.
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= fuse_in_stmt(stmt, local_count, local_types);
    }
    // Then look for adjacent (Let+LabeledBlock, If+VariantTest) pairs at this level.
    changed |= fuse_adjacent_pairs(block, local_count, local_types);
    changed
}

fn fuse_in_stmt(stmt: &mut TirStmt, local_count: &mut u32, local_types: &mut Vec<TypeId>) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => fuse_in_expr(value, local_count, local_types),
        TirStmtKind::Expr(expr) => fuse_in_expr(expr, local_count, local_types),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = fuse_in_expr(condition, local_count, local_types);
            changed |= fuse_in_block(then_block, local_count, local_types);
            if let Some(eb) = else_block {
                changed |= fuse_in_block(eb, local_count, local_types);
            }
            changed
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            fuse_in_block(body, local_count, local_types)
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = fuse_in_expr(scrutinee, local_count, local_types);
            changed |= fuse_in_block(then_block, local_count, local_types);
            if let Some(eb) = else_block {
                changed |= fuse_in_block(eb, local_count, local_types);
            }
            changed
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                fuse_in_expr(v, local_count, local_types)
            } else {
                false
            }
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                fuse_in_expr(v, local_count, local_types)
            } else {
                false
            }
        }
        TirStmtKind::LetDestructure { value, .. } => fuse_in_expr(value, local_count, local_types),
        TirStmtKind::Continue
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::VariadicForOf { .. } => false,
    }
}

/// Check if a labeled block expression is trivially a single `break label: value` statement.
/// If so, inline the break value directly, eliminating the labeled block overhead.
///
/// Pattern:
/// ```text
/// label: { break label: expr }  →  expr
/// ```
fn try_inline_trivial_labeled_block(expr: &mut TirExpr) -> bool {
    let TirExprKind::LabeledBlock { label, block, .. } = &mut expr.kind else {
        return false;
    };
    if block.stmts.len() != 1 {
        return false;
    }
    let TirStmtKind::Break {
        label: Some(break_label),
        value: Some(break_value),
    } = &block.stmts[0].kind
    else {
        return false;
    };
    if break_label != label {
        return false;
    }
    // Don't inline if the break value itself contains breaks to the same label.
    // This happens with try-op (?) expansions inside nested expressions: the
    // error path breaks to the inline label, and removing the labeled block
    // would leave those breaks without a target.
    if expr_has_break_to(label, break_value) {
        return false;
    }
    // Extract the break value, replacing expr in place.
    let TirStmtKind::Break {
        value: Some(break_value),
        ..
    } = std::mem::replace(&mut block.stmts[0].kind, TirStmtKind::Continue)
    else {
        unreachable!()
    };
    let span = expr.span;
    let type_id = expr.type_id;
    *expr = TirExpr {
        kind: break_value.kind,
        type_id,
        span,
    };
    true
}

fn fuse_in_expr(expr: &mut TirExpr, local_count: &mut u32, local_types: &mut Vec<TypeId>) -> bool {
    match &mut expr.kind {
        TirExprKind::LabeledBlock { block, .. } => {
            // First recurse into the block's contents
            let changed = fuse_in_block(block, local_count, local_types);
            // Then check if this became a trivial single-break block
            try_inline_trivial_labeled_block(expr) || changed
        }
        TirExprKind::Block(block) => fuse_in_block(block, local_count, local_types),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut changed = fuse_in_expr(condition, local_count, local_types);
            changed |= fuse_in_block(then_branch, local_count, local_types);
            if let Some(eb) = else_branch {
                changed |= fuse_in_block(eb, local_count, local_types);
            }
            changed
        }
        TirExprKind::Binary { left, right, .. } => {
            fuse_in_expr(left, local_count, local_types)
                | fuse_in_expr(right, local_count, local_types)
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            fuse_in_expr(inner, local_count, local_types)
        }
        TirExprKind::Assign { target, value } => {
            fuse_in_expr(target, local_count, local_types)
                | fuse_in_expr(value, local_count, local_types)
        }
        TirExprKind::Call { args, .. } => {
            let mut changed = false;
            for arg in args {
                changed |= fuse_in_expr(&mut arg.expr, local_count, local_types);
            }
            changed
        }
        TirExprKind::CmRawCall { args, .. } => {
            let mut changed = false;
            for arg in args {
                changed |= fuse_in_expr(arg, local_count, local_types);
            }
            changed
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            let mut changed = fuse_in_expr(receiver, local_count, local_types);
            for arg in args {
                changed |= fuse_in_expr(&mut arg.expr, local_count, local_types);
            }
            changed
        }
        TirExprKind::IndirectCall { callee, args } => {
            let mut changed = fuse_in_expr(callee, local_count, local_types);
            for arg in args {
                changed |= fuse_in_expr(arg, local_count, local_types);
            }
            changed
        }
        TirExprKind::Index { expr: inner, index } => {
            fuse_in_expr(inner, local_count, local_types)
                | fuse_in_expr(index, local_count, local_types)
        }
        TirExprKind::StructLiteral { fields, .. } => {
            let mut changed = false;
            for f in fields {
                changed |= fuse_in_expr(&mut f.value, local_count, local_types);
            }
            changed
        }
        TirExprKind::TupleLiteral { elements } => {
            let mut changed = false;
            for e in elements {
                changed |= fuse_in_expr(e, local_count, local_types);
            }
            changed
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                fuse_in_expr(p, local_count, local_types)
            } else {
                false
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            fuse_in_expr(functor, local_count, local_types)
        }
        TirExprKind::GlobalVarSet { value, .. } => fuse_in_expr(value, local_count, local_types),
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => fuse_in_expr(inner, local_count, local_types),
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut changed = fuse_in_expr(scrutinee, local_count, local_types);
            for arm in arms {
                changed |= fuse_in_block(arm, local_count, local_types);
            }
            changed |= fuse_in_block(default, local_count, local_types);
            changed
        }
        TirExprKind::Match { expr, arms } => {
            let mut changed = fuse_in_expr(expr, local_count, local_types);
            for arm in arms {
                changed |= fuse_in_expr(&mut arm.body, local_count, local_types);
                if let Some(guard) = &mut arm.guard {
                    changed |= fuse_in_expr(guard, local_count, local_types);
                }
            }
            changed
        }
        TirExprKind::Closure { body, .. } => fuse_in_expr(body, local_count, local_types),
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => false,
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Look for adjacent (Let+LabeledBlock, If+VariantTest) statement pairs in `block`
/// and fuse them when all preconditions are met.
fn fuse_adjacent_pairs(
    block: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
) -> bool {
    let stmts = std::mem::take(&mut block.stmts);
    let mut new_stmts = Vec::with_capacity(stmts.len());
    let mut iter = stmts.into_iter().peekable();
    let mut changed = false;

    while let Some(let_stmt) = iter.next() {
        // Check preconditions by borrowing both statements.
        let fusion_info = iter
            .peek()
            .and_then(|if_stmt| check_fusion_preconditions(&let_stmt, if_stmt));

        if let Some(info) = fusion_info {
            let if_stmt = iter.next().unwrap();
            // Refuse to fuse when the If is the last statement of this block:
            // its value is then the block's terminal value, but the fused
            // labeled block produces only Unit (its breaks carry no value),
            // which would silently change the block's type. See
            // tests/fixtures/if-let-some-ref-from-fn.wado.
            if iter.peek().is_none() {
                new_stmts.push(let_stmt);
                new_stmts.push(if_stmt);
                continue;
            }
            let fused = perform_fusion(let_stmt, if_stmt, info, local_count, local_types);
            new_stmts.extend(fused);
            changed = true;
        } else {
            new_stmts.push(let_stmt);
        }
    }
    block.stmts = new_stmts;
    changed
}

/// Information extracted from the two statements during the precondition check.
struct FusionInfo {
    /// Local index of the temp variable (X).
    temp_local: u32,
    /// The label of the inner `LabeledBlock` (L).
    label: String,
    /// Case index being tested in the `VariantTest` (C).
    case_index: u32,
    /// Type of the payload for case C.
    payload_type: TypeId,
}

/// Returns `Some(FusionInfo)` when the pair can be fused, `None` otherwise.
fn check_fusion_preconditions(let_stmt: &TirStmt, if_stmt: &TirStmt) -> Option<FusionInfo> {
    // --- Stmt 1: Let { value: LabeledBlock { label, block } } ---
    let TirStmtKind::Let {
        local_index: temp_local,
        value: let_value,
        ..
    } = &let_stmt.kind
    else {
        return None;
    };
    let TirExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &let_value.kind
    else {
        return None;
    };

    // --- Stmt 2: If { condition: VariantTest(Local(X), case=C), then_block, else_block } ---
    let TirStmtKind::If {
        condition,
        then_block,
        else_block,
    } = &if_stmt.kind
    else {
        return None;
    };
    let TirExprKind::VariantTest {
        expr: vt_expr,
        case_index,
        ..
    } = &condition.kind
    else {
        return None;
    };
    let TirExprKind::Local {
        index: tested_idx, ..
    } = &vt_expr.kind
    else {
        return None;
    };
    if *tested_idx != *temp_local {
        return None;
    }

    // --- LabeledBlock only breaks to L with null or VariantConstruct ---
    let payload_type = check_lb_breaks_and_get_payload(lb_block, label, *case_index)?;

    // --- temp is only used as VariantPayload(Local(X), C) in then_block,
    //     and not at all in else_block ---
    let then_uses = count_local_uses_in_block(then_block, *temp_local);
    let payload_uses = count_variant_payload_uses_in_block(then_block, *temp_local, *case_index);
    if then_uses != payload_uses {
        return None;
    }
    if let Some(eb) = else_block
        && count_local_uses_in_block(eb, *temp_local) > 0
    {
        return None;
    }

    // --- THEN/ELSE blocks must not contain free unlabeled break/continue
    //     when the labeled block being fused contains a loop ---
    //
    // Fusion clones THEN and ELSE into the labeled block's nesting context.
    // An unlabeled `break;` or `continue` targets the *innermost* enclosing
    // loop. If `lb_block` contains a loop, fusion places THEN/ELSE inside a
    // deeper loop nesting, so an unlabeled break/continue would target the
    // wrong loop (e.g., IterFilter's inner `loop {}` instead of the outer
    // collect loop), producing incorrect control flow.
    //
    // If `lb_block` has no loops (e.g., StrUtf8ByteIter::next), fusion does
    // not add any loop nesting, so unlabeled breaks remain safe.
    if block_contains_loop(lb_block) {
        if block_has_free_unlabeled_loop_exit(then_block) {
            return None;
        }
        if let Some(eb) = else_block
            && block_has_free_unlabeled_loop_exit(eb)
        {
            return None;
        }
    }

    Some(FusionInfo {
        temp_local: *temp_local,
        label: label.clone(),
        case_index: *case_index,
        payload_type,
    })
}

/// Verify that all `break L: v` in `block` have `v` as either `null` or
/// `VariantConstruct`. Returns the payload type of the matching case, or
/// `None` if any break is not in the expected form.
///
/// The payload type is obtained from the first `VariantConstruct(case=C)` break
/// found, or from a `VariantPayload(_, C)` expression if the matching break has
/// no payload (which shouldn't happen for a typed payload variant).
fn check_lb_breaks_and_get_payload(
    block: &TirBlock,
    label: &str,
    case_index: u32,
) -> Option<TypeId> {
    let mut payload_type: Option<TypeId> = None;
    if !check_lb_breaks_in_block(block, label, case_index, &mut payload_type) {
        return None;
    }
    // If no matching VariantConstruct break was found, the payload_type is unknown.
    // This might mean the matching case has no payload or none was found yet; skip fusion.
    payload_type
}

/// Returns `false` if any `break L: v` in the block has an unexpected `v`.
/// Fills `payload_type` from the first matching `VariantConstruct(case=C, payload)` seen.
fn check_lb_breaks_in_block(
    block: &TirBlock,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    block
        .stmts
        .iter()
        .all(|s| check_lb_breaks_in_stmt(s, label, case_index, payload_type))
}

fn check_lb_breaks_in_stmt(
    stmt: &TirStmt,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    match &stmt.kind {
        TirStmtKind::Break {
            label: Some(l),
            value,
        } if l == label => {
            match value.as_ref().map(|v| &v.kind) {
                // null → None case, always valid
                None | Some(TirExprKind::Null) => true,
                // VariantConstruct → valid if payload doesn't contain breaks to this label
                Some(TirExprKind::VariantConstruct {
                    case_index: ci,
                    payload,
                    ..
                }) => {
                    // Reject if the payload contains nested breaks to the same label
                    // (e.g., try-op error paths inside tuple literals in the payload)
                    if let Some(p) = payload
                        && expr_has_break_to(label, p)
                    {
                        return false;
                    }
                    if *ci == case_index
                        && let Some(p) = payload
                    {
                        *payload_type = Some(p.type_id);
                    }
                    true
                }
                // Anything else → not eligible for fusion
                _ => false,
            }
        }
        // Don't cross into nested labeled blocks with the same label (shouldn't occur in practice
        // since labels are unique, but be safe).
        TirStmtKind::LabeledBlock { label: l, .. } if l == label => true,
        // Recurse into other statement kinds.
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            check_lb_breaks_in_expr(condition, label, case_index, payload_type)
                && check_lb_breaks_in_block(then_block, label, case_index, payload_type)
                && else_block
                    .as_ref()
                    .is_none_or(|eb| check_lb_breaks_in_block(eb, label, case_index, payload_type))
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            check_lb_breaks_in_block(body, label, case_index, payload_type)
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            check_lb_breaks_in_expr(scrutinee, label, case_index, payload_type)
                && check_lb_breaks_in_block(then_block, label, case_index, payload_type)
                && else_block
                    .as_ref()
                    .is_none_or(|eb| check_lb_breaks_in_block(eb, label, case_index, payload_type))
        }
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            check_lb_breaks_in_expr(value, label, case_index, payload_type)
        }
        TirStmtKind::Break { value, .. } => value
            .as_ref()
            .is_none_or(|v| check_lb_breaks_in_expr(v, label, case_index, payload_type)),
        TirStmtKind::Return { value } => value
            .as_ref()
            .is_none_or(|v| check_lb_breaks_in_expr(v, label, case_index, payload_type)),
        _ => true,
    }
}

fn check_lb_breaks_in_expr(
    expr: &TirExpr,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    match &expr.kind {
        TirExprKind::LabeledBlock {
            label: l, block, ..
        } => {
            if l == label {
                // Same label shadowing — don't recurse (label is rebound here)
                true
            } else {
                check_lb_breaks_in_block(block, label, case_index, payload_type)
            }
        }
        TirExprKind::Block(block) => {
            check_lb_breaks_in_block(block, label, case_index, payload_type)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_lb_breaks_in_expr(condition, label, case_index, payload_type)
                && check_lb_breaks_in_block(then_branch, label, case_index, payload_type)
                && else_branch
                    .as_ref()
                    .is_none_or(|eb| check_lb_breaks_in_block(eb, label, case_index, payload_type))
        }
        // For any other expression type, reject fusion if it contains breaks
        // to the target label. The transform_lb_stmt function only handles
        // breaks at the statement level, not those nested inside expression
        // types like VariantConstruct, TupleLiteral, StructLiteral, Match, etc.
        _ => !expr_has_break_to(label, expr),
    }
}

/// Count all occurrences of `Local { index: local_idx }` in a block.
fn count_local_uses_in_block(block: &TirBlock, local_idx: u32) -> usize {
    block
        .stmts
        .iter()
        .map(|s| count_local_uses_in_stmt(s, local_idx))
        .sum()
}

fn count_local_uses_in_stmt(stmt: &TirStmt, local_idx: u32) -> usize {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            count_local_uses_in_expr(value, local_idx)
        }
        TirStmtKind::Expr(expr) => count_local_uses_in_expr(expr, local_idx),
        TirStmtKind::Return { value } => value
            .as_ref()
            .map_or(0, |v| count_local_uses_in_expr(v, local_idx)),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            count_local_uses_in_expr(condition, local_idx)
                + count_local_uses_in_block(then_block, local_idx)
                + else_block
                    .as_ref()
                    .map_or(0, |eb| count_local_uses_in_block(eb, local_idx))
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            count_local_uses_in_block(body, local_idx)
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            count_local_uses_in_expr(scrutinee, local_idx)
                + count_local_uses_in_block(then_block, local_idx)
                + else_block
                    .as_ref()
                    .map_or(0, |eb| count_local_uses_in_block(eb, local_idx))
        }
        TirStmtKind::Break { value, .. } => value
            .as_ref()
            .map_or(0, |v| count_local_uses_in_expr(v, local_idx)),
        TirStmtKind::Continue
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::VariadicForOf { .. } => 0,
    }
}

fn count_local_uses_in_expr(expr: &TirExpr, local_idx: u32) -> usize {
    match &expr.kind {
        TirExprKind::Local { index, .. } => usize::from(*index == local_idx),
        TirExprKind::Binary { left, right, .. } => {
            count_local_uses_in_expr(left, local_idx) + count_local_uses_in_expr(right, local_idx)
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            count_local_uses_in_expr(inner, local_idx)
        }
        TirExprKind::Assign { target, value } => {
            count_local_uses_in_expr(target, local_idx) + count_local_uses_in_expr(value, local_idx)
        }
        TirExprKind::Index { expr: inner, index } => {
            count_local_uses_in_expr(inner, local_idx) + count_local_uses_in_expr(index, local_idx)
        }
        TirExprKind::Call { args, .. } => args
            .iter()
            .map(|a| count_local_uses_in_expr(&a.expr, local_idx))
            .sum(),
        TirExprKind::CmRawCall { args, .. } => args
            .iter()
            .map(|a| count_local_uses_in_expr(a, local_idx))
            .sum(),
        TirExprKind::MethodCall { receiver, args, .. } => {
            count_local_uses_in_expr(receiver, local_idx)
                + args
                    .iter()
                    .map(|a| count_local_uses_in_expr(&a.expr, local_idx))
                    .sum::<usize>()
        }
        TirExprKind::IndirectCall { callee, args } => {
            count_local_uses_in_expr(callee, local_idx)
                + args
                    .iter()
                    .map(|a| count_local_uses_in_expr(a, local_idx))
                    .sum::<usize>()
        }
        TirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .map(|f| count_local_uses_in_expr(&f.value, local_idx))
            .sum(),
        TirExprKind::TupleLiteral { elements } => elements
            .iter()
            .map(|e| count_local_uses_in_expr(e, local_idx))
            .sum(),
        TirExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .map_or(0, |p| count_local_uses_in_expr(p, local_idx)),
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            count_local_uses_in_block(block, local_idx)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            count_local_uses_in_expr(condition, local_idx)
                + count_local_uses_in_block(then_branch, local_idx)
                + else_branch
                    .as_ref()
                    .map_or(0, |eb| count_local_uses_in_block(eb, local_idx))
        }
        TirExprKind::Match { expr, arms } => {
            count_local_uses_in_expr(expr, local_idx)
                + arms
                    .iter()
                    .map(|arm| {
                        count_local_uses_in_expr(&arm.body, local_idx)
                            + arm
                                .guard
                                .as_ref()
                                .map_or(0, |g| count_local_uses_in_expr(g, local_idx))
                    })
                    .sum::<usize>()
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_local_uses_in_expr(scrutinee, local_idx)
                + arms
                    .iter()
                    .map(|arm| count_local_uses_in_block(arm, local_idx))
                    .sum::<usize>()
                + count_local_uses_in_block(default, local_idx)
        }
        TirExprKind::Closure { body, .. } => count_local_uses_in_expr(body, local_idx),
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => count_local_uses_in_expr(inner, local_idx),
        // Leaf nodes
        TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => 0,
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Count `VariantPayload { expr: Local(local_idx), case_index }` in a block.
fn count_variant_payload_uses_in_block(block: &TirBlock, local_idx: u32, case_index: u32) -> usize {
    block
        .stmts
        .iter()
        .map(|s| count_variant_payload_uses_in_stmt(s, local_idx, case_index))
        .sum()
}

fn count_variant_payload_uses_in_stmt(stmt: &TirStmt, local_idx: u32, case_index: u32) -> usize {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            count_variant_payload_uses_in_expr(value, local_idx, case_index)
        }
        TirStmtKind::Expr(expr) => count_variant_payload_uses_in_expr(expr, local_idx, case_index),
        TirStmtKind::Return { value } => value.as_ref().map_or(0, |v| {
            count_variant_payload_uses_in_expr(v, local_idx, case_index)
        }),
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            count_variant_payload_uses_in_expr(condition, local_idx, case_index)
                + count_variant_payload_uses_in_block(then_block, local_idx, case_index)
                + else_block.as_ref().map_or(0, |eb| {
                    count_variant_payload_uses_in_block(eb, local_idx, case_index)
                })
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            count_variant_payload_uses_in_block(body, local_idx, case_index)
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            count_variant_payload_uses_in_expr(scrutinee, local_idx, case_index)
                + count_variant_payload_uses_in_block(then_block, local_idx, case_index)
                + else_block.as_ref().map_or(0, |eb| {
                    count_variant_payload_uses_in_block(eb, local_idx, case_index)
                })
        }
        TirStmtKind::Break { value, .. } => value.as_ref().map_or(0, |v| {
            count_variant_payload_uses_in_expr(v, local_idx, case_index)
        }),
        TirStmtKind::Continue
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::VariadicForOf { .. } => 0,
    }
}

fn count_variant_payload_uses_in_expr(expr: &TirExpr, local_idx: u32, case_index: u32) -> usize {
    match &expr.kind {
        TirExprKind::VariantPayload {
            expr: inner,
            case_index: ci,
            ..
        } if *ci == case_index => {
            if matches!(inner.kind, TirExprKind::Local { index, .. } if index == local_idx) {
                return 1;
            }
            count_variant_payload_uses_in_expr(inner, local_idx, case_index)
        }
        TirExprKind::Local { .. } => 0,
        TirExprKind::Binary { left, right, .. } => {
            count_variant_payload_uses_in_expr(left, local_idx, case_index)
                + count_variant_payload_uses_in_expr(right, local_idx, case_index)
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            count_variant_payload_uses_in_expr(inner, local_idx, case_index)
        }
        TirExprKind::Assign { target, value } => {
            count_variant_payload_uses_in_expr(target, local_idx, case_index)
                + count_variant_payload_uses_in_expr(value, local_idx, case_index)
        }
        TirExprKind::Index { expr: inner, index } => {
            count_variant_payload_uses_in_expr(inner, local_idx, case_index)
                + count_variant_payload_uses_in_expr(index, local_idx, case_index)
        }
        TirExprKind::Call { args, .. } => args
            .iter()
            .map(|a| count_variant_payload_uses_in_expr(&a.expr, local_idx, case_index))
            .sum(),
        TirExprKind::CmRawCall { args, .. } => args
            .iter()
            .map(|a| count_variant_payload_uses_in_expr(a, local_idx, case_index))
            .sum(),
        TirExprKind::MethodCall { receiver, args, .. } => {
            count_variant_payload_uses_in_expr(receiver, local_idx, case_index)
                + args
                    .iter()
                    .map(|a| count_variant_payload_uses_in_expr(&a.expr, local_idx, case_index))
                    .sum::<usize>()
        }
        TirExprKind::IndirectCall { callee, args } => {
            count_variant_payload_uses_in_expr(callee, local_idx, case_index)
                + args
                    .iter()
                    .map(|a| count_variant_payload_uses_in_expr(a, local_idx, case_index))
                    .sum::<usize>()
        }
        TirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .map(|f| count_variant_payload_uses_in_expr(&f.value, local_idx, case_index))
            .sum(),
        TirExprKind::TupleLiteral { elements } => elements
            .iter()
            .map(|e| count_variant_payload_uses_in_expr(e, local_idx, case_index))
            .sum(),
        TirExprKind::VariantConstruct { payload, .. } => payload.as_ref().map_or(0, |p| {
            count_variant_payload_uses_in_expr(p, local_idx, case_index)
        }),
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            count_variant_payload_uses_in_block(block, local_idx, case_index)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            count_variant_payload_uses_in_expr(condition, local_idx, case_index)
                + count_variant_payload_uses_in_block(then_branch, local_idx, case_index)
                + else_branch.as_ref().map_or(0, |eb| {
                    count_variant_payload_uses_in_block(eb, local_idx, case_index)
                })
        }
        TirExprKind::Match { expr, arms } => {
            count_variant_payload_uses_in_expr(expr, local_idx, case_index)
                + arms
                    .iter()
                    .map(|arm| {
                        count_variant_payload_uses_in_expr(&arm.body, local_idx, case_index)
                            + arm.guard.as_ref().map_or(0, |g| {
                                count_variant_payload_uses_in_expr(g, local_idx, case_index)
                            })
                    })
                    .sum::<usize>()
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            count_variant_payload_uses_in_expr(scrutinee, local_idx, case_index)
                + arms
                    .iter()
                    .map(|arm| count_variant_payload_uses_in_block(arm, local_idx, case_index))
                    .sum::<usize>()
                + count_variant_payload_uses_in_block(default, local_idx, case_index)
        }
        TirExprKind::Closure { body, .. } => {
            count_variant_payload_uses_in_expr(body, local_idx, case_index)
        }
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => count_variant_payload_uses_in_expr(inner, local_idx, case_index),
        // Leaf nodes
        TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => 0,
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Perform the actual fusion transformation.
///
/// Consumes the two matched statements and produces the fused labeled block statement(s).
fn perform_fusion(
    let_stmt: TirStmt,
    if_stmt: TirStmt,
    info: FusionInfo,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
) -> Vec<TirStmt> {
    let span = let_stmt.span;

    // Extract the LabeledBlock body from the Let statement.
    let TirStmtKind::Let {
        value: let_value, ..
    } = let_stmt.kind
    else {
        unreachable!()
    };
    let TirExprKind::LabeledBlock {
        block: lb_block, ..
    } = let_value.kind
    else {
        unreachable!()
    };

    // Extract the then/else blocks from the If statement.
    let TirStmtKind::If {
        then_block,
        else_block,
        ..
    } = if_stmt.kind
    else {
        unreachable!()
    };

    // Allocate a fresh local for the payload value.
    let payload_local = *local_count;
    *local_count += 1;
    local_types.push(info.payload_type);

    let fused_label = format!("__fused_{}", info.label);

    // Transform the labeled block body: replace all `break L: v` with inline expansions.
    let fused_stmts = transform_lb_stmts(
        lb_block.stmts,
        &info.label,
        &fused_label,
        info.case_index,
        info.temp_local,
        payload_local,
        info.payload_type,
        &then_block,
        else_block.as_ref(),
        span,
    );

    vec![TirStmt::new(
        TirStmtKind::LabeledBlock {
            label: fused_label,
            block: TirBlock {
                stmts: fused_stmts,
                span,
            },
        },
        span,
    )]
}

/// Walk `stmts` and replace:
/// - `break orig_label: null` → `{ ELSE; break fused_label; }`
/// - `break orig_label: VariantConstruct(case=C, v)` → `{ let __payload = v; THEN_SUBST; break fused_label; }`
/// - `break orig_label: VariantConstruct(case≠C)` → `{ ELSE; break fused_label; }`
fn transform_lb_stmts(
    stmts: Vec<TirStmt>,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &TirBlock,
    else_block: Option<&TirBlock>,
    span: crate::token::Span,
) -> Vec<TirStmt> {
    let mut out = Vec::new();
    for stmt in stmts {
        transform_lb_stmt(
            stmt,
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

fn transform_lb_stmt(
    stmt: TirStmt,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &TirBlock,
    else_block: Option<&TirBlock>,
    span: crate::token::Span,
    out: &mut Vec<TirStmt>,
) {
    // Check for `break orig_label: v` first.
    let is_orig_label_break = matches!(&stmt.kind,
        TirStmtKind::Break { label: Some(l), .. } if l == orig_label);

    if is_orig_label_break {
        let TirStmtKind::Break { value, .. } = stmt.kind else {
            unreachable!()
        };
        let is_some_case = match &value {
            Some(v) => matches!(&v.kind,
                TirExprKind::VariantConstruct { case_index: ci, .. } if *ci == case_index),
            _ => false,
        };

        if is_some_case {
            // Extract payload expression from the VariantConstruct.
            let Some(v) = value else { unreachable!() };
            let TirExprKind::VariantConstruct { payload, .. } = v.kind else {
                unreachable!()
            };
            let payload_expr = payload
                .map(|p| *p)
                .unwrap_or_else(|| TirExpr::new(TirExprKind::Unit, payload_type, span));

            // Emit: let __payload = payload_expr;
            out.push(TirStmt::new(
                TirStmtKind::Let {
                    name: format!("__fused_payload_{payload_local}"),
                    local_index: payload_local,
                    is_mut: false,
                    is_reactive: false,
                    type_id: payload_type,
                    value: payload_expr,
                    skip_value_copy: false,
                },
                span,
            ));

            // Emit then_block stmts with VariantPayload(temp_local, case_index) substituted.
            let mut subst_then = then_block.clone();
            subst_variant_payload_in_block(&mut subst_then, temp_local, case_index, payload_local);
            out.extend(subst_then.stmts);
        } else {
            // None / non-matching case → emit else block.
            if let Some(eb) = else_block {
                out.extend(eb.stmts.iter().cloned());
            }
        }

        // Emit `break fused_label;` so control exits the wrapper — unless the last
        // emitted statement already terminates control flow (break/return/continue),
        // in which case the fused break would be dead code.
        let last_terminates = out.last().is_some_and(|s| {
            matches!(
                s.kind,
                TirStmtKind::Break { .. } | TirStmtKind::Return { .. } | TirStmtKind::Continue
            )
        });
        if !last_terminates {
            out.push(TirStmt::new(
                TirStmtKind::Break {
                    label: Some(fused_label.to_owned()),
                    value: None,
                },
                span,
            ));
        }
        return;
    }

    // For any other statement, recursively transform nested blocks.
    let stmt_span = stmt.span;
    match stmt.kind {
        TirStmtKind::If {
            condition,
            then_block: tb,
            else_block: eb,
        } => {
            let new_then = TirBlock {
                stmts: transform_lb_stmts(
                    tb.stmts,
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
                span: tb.span,
            };
            let new_else = eb.map(|e| TirBlock {
                stmts: transform_lb_stmts(
                    e.stmts,
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
                span: e.span,
            });
            out.push(TirStmt::new(
                TirStmtKind::If {
                    condition,
                    then_block: new_then,
                    else_block: new_else,
                },
                stmt_span,
            ));
        }
        TirStmtKind::Loop { body } => {
            let new_body = TirBlock {
                stmts: transform_lb_stmts(
                    body.stmts,
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
                span: body.span,
            };
            out.push(TirStmt::new(
                TirStmtKind::Loop { body: new_body },
                stmt_span,
            ));
        }
        TirStmtKind::LabeledBlock {
            label: ref l,
            block: inner,
        } if l != orig_label => {
            let l = l.clone();
            let new_inner = TirBlock {
                stmts: transform_lb_stmts(
                    inner.stmts,
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
                span: inner.span,
            };
            out.push(TirStmt::new(
                TirStmtKind::LabeledBlock {
                    label: l,
                    block: new_inner,
                },
                stmt_span,
            ));
        }
        TirStmtKind::IfLet {
            scrutinee,
            pattern,
            then_block: tb,
            else_block: eb,
        } => {
            let new_then = TirBlock {
                stmts: transform_lb_stmts(
                    tb.stmts,
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
                span: tb.span,
            };
            let new_else = eb.map(|e| TirBlock {
                stmts: transform_lb_stmts(
                    e.stmts,
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
                span: e.span,
            });
            out.push(TirStmt::new(
                TirStmtKind::IfLet {
                    scrutinee,
                    pattern,
                    then_block: new_then,
                    else_block: new_else,
                },
                stmt_span,
            ));
        }
        // Statements that contain expressions: recurse into expressions to find nested breaks.
        mut other => {
            transform_lb_in_stmt_kind(
                &mut other,
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
            out.push(TirStmt::new(other, stmt_span));
        }
    }
}

/// Walk a `TirStmtKind` and apply label transformation to any nested block expressions
/// that may contain `break orig_label`.
fn transform_lb_in_stmt_kind(
    kind: &mut TirStmtKind,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &TirBlock,
    else_block: Option<&TirBlock>,
    span: crate::token::Span,
) {
    match kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::LetDestructure { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value } => {
            transform_lb_in_expr(
                value,
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
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                transform_lb_in_expr(
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
        // If/Loop/LabeledBlock/IfLet are handled before this function is called (in
        // transform_lb_stmt). The remaining kinds carry no expressions to transform.
        TirStmtKind::If { .. }
        | TirStmtKind::Loop { .. }
        | TirStmtKind::LabeledBlock { .. }
        | TirStmtKind::IfLet { .. }
        | TirStmtKind::Continue
        | TirStmtKind::VariadicForOf { .. } => {}
    }
}

/// Recursively walk a `TirExpr` to find and transform blocks that contain
/// `break orig_label` statements.
fn transform_lb_in_expr(
    expr: &mut TirExpr,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &TirBlock,
    else_block: Option<&TirBlock>,
    span: crate::token::Span,
) {
    match &mut expr.kind {
        TirExprKind::Block(block) => {
            transform_lb_in_block(
                block,
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
        TirExprKind::LabeledBlock {
            label: l, block, ..
        } => {
            if l.as_str() != orig_label {
                transform_lb_in_block(
                    block,
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
        TirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            transform_lb_in_expr(
                scrutinee,
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
            for arm in arms {
                transform_lb_in_expr(
                    &mut arm.body,
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
                if let Some(g) = &mut arm.guard {
                    transform_lb_in_expr(
                        g,
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
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            transform_lb_in_expr(
                condition,
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
            transform_lb_in_block(
                then_branch,
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
            if let Some(eb) = else_branch {
                transform_lb_in_block(
                    eb,
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
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            transform_lb_in_expr(
                scrutinee,
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
            for arm in arms {
                transform_lb_in_block(
                    arm,
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
            transform_lb_in_block(
                default,
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
        // These expression kinds cannot contain `break orig_label` that reaches the outer
        // scope, because check_lb_breaks_in_expr conservatively rejects fusion when a
        // break to the target label is found inside any of these expressions.
        TirExprKind::Binary { .. }
        | TirExprKind::Unary { .. }
        | TirExprKind::Cast { .. }
        | TirExprKind::FieldAccess { .. }
        | TirExprKind::TupleSpread { .. }
        | TirExprKind::TupleZip { .. }
        | TirExprKind::TypePackExpansion { .. }
        | TirExprKind::Assign { .. }
        | TirExprKind::Index { .. }
        | TirExprKind::Call { .. }
        | TirExprKind::CmRawCall { .. }
        | TirExprKind::MethodCall { .. }
        | TirExprKind::IndirectCall { .. }
        | TirExprKind::StructLiteral { .. }
        | TirExprKind::TupleLiteral { .. }
        | TirExprKind::VariantConstruct { .. }
        | TirExprKind::VariantTag { .. }
        | TirExprKind::VariantTest { .. }
        | TirExprKind::VariantPayload { .. }
        | TirExprKind::Closure { .. }
        | TirExprKind::ClosureToCanonical { .. }
        | TirExprKind::GlobalVarSet { .. }
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. }
        | TirExprKind::TemplateString { .. } => {}
    }
}

/// Transform break statements within a block's stmts in-place.
fn transform_lb_in_block(
    block: &mut TirBlock,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &TirBlock,
    else_block: Option<&TirBlock>,
    span: crate::token::Span,
) {
    let old_stmts = std::mem::take(&mut block.stmts);
    block.stmts = transform_lb_stmts(
        old_stmts,
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

/// Replace `VariantPayload { expr: Local(temp_local), case_index }` with `Local(payload_local)`
/// throughout a block.
fn subst_variant_payload_in_block(
    block: &mut TirBlock,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    for stmt in &mut block.stmts {
        subst_variant_payload_in_stmt(stmt, temp_local, case_index, payload_local);
    }
}

fn subst_variant_payload_in_stmt(
    stmt: &mut TirStmt,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            subst_variant_payload_in_expr(value, temp_local, case_index, payload_local);
        }
        TirStmtKind::Expr(expr) => {
            subst_variant_payload_in_expr(expr, temp_local, case_index, payload_local);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                subst_variant_payload_in_expr(v, temp_local, case_index, payload_local);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            subst_variant_payload_in_expr(condition, temp_local, case_index, payload_local);
            subst_variant_payload_in_block(then_block, temp_local, case_index, payload_local);
            if let Some(eb) = else_block {
                subst_variant_payload_in_block(eb, temp_local, case_index, payload_local);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            subst_variant_payload_in_block(body, temp_local, case_index, payload_local);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            subst_variant_payload_in_expr(scrutinee, temp_local, case_index, payload_local);
            subst_variant_payload_in_block(then_block, temp_local, case_index, payload_local);
            if let Some(eb) = else_block {
                subst_variant_payload_in_block(eb, temp_local, case_index, payload_local);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                subst_variant_payload_in_expr(v, temp_local, case_index, payload_local);
            }
        }
        TirStmtKind::Continue
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::VariadicForOf { .. } => {}
    }
}

fn subst_variant_payload_in_expr(
    expr: &mut TirExpr,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    // Match the target pattern first (top-down, before recursing).
    if let TirExprKind::VariantPayload {
        expr: inner,
        case_index: ci,
        ..
    } = &expr.kind
        && *ci == case_index
        && let TirExprKind::Local { index, .. } = inner.kind
        && index == temp_local
    {
        expr.kind = TirExprKind::Local {
            index: payload_local,
            name: format!("__fused_payload_{payload_local}"),
        };
        return;
    }

    // Recurse into sub-expressions.
    match &mut expr.kind {
        TirExprKind::Local { .. } => {}
        TirExprKind::Binary { left, right, .. } => {
            subst_variant_payload_in_expr(left, temp_local, case_index, payload_local);
            subst_variant_payload_in_expr(right, temp_local, case_index, payload_local);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            subst_variant_payload_in_expr(inner, temp_local, case_index, payload_local);
        }
        TirExprKind::Assign { target, value } => {
            subst_variant_payload_in_expr(target, temp_local, case_index, payload_local);
            subst_variant_payload_in_expr(value, temp_local, case_index, payload_local);
        }
        TirExprKind::Index { expr: inner, index } => {
            subst_variant_payload_in_expr(inner, temp_local, case_index, payload_local);
            subst_variant_payload_in_expr(index, temp_local, case_index, payload_local);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                subst_variant_payload_in_expr(&mut arg.expr, temp_local, case_index, payload_local);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                subst_variant_payload_in_expr(arg, temp_local, case_index, payload_local);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            subst_variant_payload_in_expr(receiver, temp_local, case_index, payload_local);
            for arg in args {
                subst_variant_payload_in_expr(&mut arg.expr, temp_local, case_index, payload_local);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            subst_variant_payload_in_expr(callee, temp_local, case_index, payload_local);
            for arg in args {
                subst_variant_payload_in_expr(arg, temp_local, case_index, payload_local);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                subst_variant_payload_in_expr(&mut f.value, temp_local, case_index, payload_local);
            }
        }
        TirExprKind::TupleLiteral { elements } => {
            for e in elements {
                subst_variant_payload_in_expr(e, temp_local, case_index, payload_local);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                subst_variant_payload_in_expr(p, temp_local, case_index, payload_local);
            }
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            subst_variant_payload_in_block(block, temp_local, case_index, payload_local);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            subst_variant_payload_in_expr(condition, temp_local, case_index, payload_local);
            subst_variant_payload_in_block(then_branch, temp_local, case_index, payload_local);
            if let Some(eb) = else_branch {
                subst_variant_payload_in_block(eb, temp_local, case_index, payload_local);
            }
        }
        TirExprKind::Match { expr, arms } => {
            subst_variant_payload_in_expr(expr, temp_local, case_index, payload_local);
            for arm in arms {
                subst_variant_payload_in_expr(&mut arm.body, temp_local, case_index, payload_local);
                if let Some(g) = &mut arm.guard {
                    subst_variant_payload_in_expr(g, temp_local, case_index, payload_local);
                }
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            subst_variant_payload_in_expr(scrutinee, temp_local, case_index, payload_local);
            for arm in arms {
                subst_variant_payload_in_block(arm, temp_local, case_index, payload_local);
            }
            subst_variant_payload_in_block(default, temp_local, case_index, payload_local);
        }
        TirExprKind::Closure { body, .. } => {
            subst_variant_payload_in_expr(body, temp_local, case_index, payload_local);
        }
        TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            subst_variant_payload_in_expr(inner, temp_local, case_index, payload_local);
        }
        // Leaf nodes carry no sub-expressions to substitute into.
        TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
    }
}

/// Returns `true` if `block` contains a `Loop` statement at any nesting depth.
///
/// This is used to determine whether `labeled_block_fusion` would introduce a
/// new loop nesting that could confuse free unlabeled `break`/`continue` in
/// the THEN/ELSE blocks being merged.
fn block_contains_loop(block: &TirBlock) -> bool {
    block.stmts.iter().any(stmt_contains_loop)
}

fn stmt_contains_loop(stmt: &TirStmt) -> bool {
    match &stmt.kind {
        TirStmtKind::Loop { .. } => true,
        TirStmtKind::LabeledBlock { block, .. } => block.stmts.iter().any(stmt_contains_loop),
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        }
        | TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            then_block.stmts.iter().any(stmt_contains_loop)
                || else_block
                    .as_ref()
                    .is_some_and(|b| b.stmts.iter().any(stmt_contains_loop))
        }
        TirStmtKind::Let { value, .. }
        | TirStmtKind::LetDestructure { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::Return { value: Some(value) } => expr_contains_loop(value),
        _ => false,
    }
}

fn expr_contains_loop(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            block.stmts.iter().any(stmt_contains_loop)
        }
        _ => false,
    }
}

/// Returns `true` if `block` contains a "free" unlabeled `break;` or `continue`
/// — one that is *not* nested inside a `loop {}` within the block itself.
///
/// Such statements are context-sensitive: they target the *innermost* enclosing
/// loop at their use site. If the block is cloned into a deeper nesting level
/// (e.g., inside the inner loop of an inlined iterator adapter), the unlabeled
/// break/continue would target the wrong loop, producing incorrect control flow.
///
/// Breaks/continues nested inside a `loop {}` within the block are safe: they
/// target that inner loop, not any outer loop.
fn block_has_free_unlabeled_loop_exit(block: &TirBlock) -> bool {
    stmts_have_free_unlabeled_loop_exit(&block.stmts, 0)
}

fn stmts_have_free_unlabeled_loop_exit(stmts: &[TirStmt], loop_depth: u32) -> bool {
    stmts
        .iter()
        .any(|s| stmt_has_free_unlabeled_loop_exit(s, loop_depth))
}

fn stmt_has_free_unlabeled_loop_exit(stmt: &TirStmt, loop_depth: u32) -> bool {
    match &stmt.kind {
        TirStmtKind::Break { label: None, .. } | TirStmtKind::Continue => loop_depth == 0,
        TirStmtKind::Loop { body } => {
            stmts_have_free_unlabeled_loop_exit(&body.stmts, loop_depth + 1)
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            stmts_have_free_unlabeled_loop_exit(&block.stmts, loop_depth)
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            expr_has_free_unlabeled_loop_exit(condition, loop_depth)
                || stmts_have_free_unlabeled_loop_exit(&then_block.stmts, loop_depth)
                || else_block
                    .as_ref()
                    .is_some_and(|b| stmts_have_free_unlabeled_loop_exit(&b.stmts, loop_depth))
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            expr_has_free_unlabeled_loop_exit(scrutinee, loop_depth)
                || stmts_have_free_unlabeled_loop_exit(&then_block.stmts, loop_depth)
                || else_block
                    .as_ref()
                    .is_some_and(|b| stmts_have_free_unlabeled_loop_exit(&b.stmts, loop_depth))
        }
        TirStmtKind::Let { value, .. }
        | TirStmtKind::LetDestructure { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::Return { value: Some(value) }
        | TirStmtKind::Break {
            value: Some(value), ..
        }
        | TirStmtKind::TaskReturn { value } => expr_has_free_unlabeled_loop_exit(value, loop_depth),
        _ => false,
    }
}

fn expr_has_free_unlabeled_loop_exit(expr: &TirExpr, loop_depth: u32) -> bool {
    match &expr.kind {
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            stmts_have_free_unlabeled_loop_exit(&block.stmts, loop_depth)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_has_free_unlabeled_loop_exit(condition, loop_depth)
                || stmts_have_free_unlabeled_loop_exit(&then_branch.stmts, loop_depth)
                || else_branch
                    .as_ref()
                    .is_some_and(|b| stmts_have_free_unlabeled_loop_exit(&b.stmts, loop_depth))
        }
        TirExprKind::Binary { left, right, .. } => {
            expr_has_free_unlabeled_loop_exit(left, loop_depth)
                || expr_has_free_unlabeled_loop_exit(right, loop_depth)
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::ClosureToCanonical { functor: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            expr_has_free_unlabeled_loop_exit(inner, loop_depth)
        }
        TirExprKind::Assign { target, value } => {
            expr_has_free_unlabeled_loop_exit(target, loop_depth)
                || expr_has_free_unlabeled_loop_exit(value, loop_depth)
        }
        TirExprKind::Call { args, .. } => args
            .iter()
            .any(|a| expr_has_free_unlabeled_loop_exit(&a.expr, loop_depth)),
        TirExprKind::MethodCall { receiver, args, .. } => {
            expr_has_free_unlabeled_loop_exit(receiver, loop_depth)
                || args
                    .iter()
                    .any(|a| expr_has_free_unlabeled_loop_exit(&a.expr, loop_depth))
        }
        TirExprKind::IndirectCall { callee, args } => {
            expr_has_free_unlabeled_loop_exit(callee, loop_depth)
                || args
                    .iter()
                    .any(|a| expr_has_free_unlabeled_loop_exit(a, loop_depth))
        }
        TirExprKind::CmRawCall { args, .. } => args
            .iter()
            .any(|a| expr_has_free_unlabeled_loop_exit(a, loop_depth)),
        TirExprKind::Index { expr: inner, index } => {
            expr_has_free_unlabeled_loop_exit(inner, loop_depth)
                || expr_has_free_unlabeled_loop_exit(index, loop_depth)
        }
        TirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| expr_has_free_unlabeled_loop_exit(&f.value, loop_depth)),
        TirExprKind::TupleLiteral { elements } => elements
            .iter()
            .any(|e| expr_has_free_unlabeled_loop_exit(e, loop_depth)),
        TirExprKind::VariantConstruct { payload, .. } => payload
            .as_deref()
            .is_some_and(|p| expr_has_free_unlabeled_loop_exit(p, loop_depth)),
        TirExprKind::Closure { body, .. } => expr_has_free_unlabeled_loop_exit(body, loop_depth),
        TirExprKind::Match { expr, arms } => {
            expr_has_free_unlabeled_loop_exit(expr, loop_depth)
                || arms
                    .iter()
                    .any(|arm| expr_has_free_unlabeled_loop_exit(&arm.body, loop_depth))
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            expr_has_free_unlabeled_loop_exit(scrutinee, loop_depth)
                || arms
                    .iter()
                    .any(|b| stmts_have_free_unlabeled_loop_exit(&b.stmts, loop_depth))
                || stmts_have_free_unlabeled_loop_exit(&default.stmts, loop_depth)
        }
        _ => false,
    }
}
