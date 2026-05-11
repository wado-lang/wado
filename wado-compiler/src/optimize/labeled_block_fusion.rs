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

use crate::nir_package::NirPackage;
use crate::tir::{TypeId};
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirFunction, NirLocal, NirStmt, NirStmtKind};
use crate::nir_visitor::expr_has_break_to;

pub fn fuse_labeled_blocks(project: &mut NirPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= fuse_in_function(&mut func);
    }
    changed
}

fn fuse_in_function(func: &mut NirFunction) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };
    // Function body is a statement-level block: any value of the trailing
    // statement is dropped (functions returning a value use explicit `return`).
    fuse_in_block(
        body,
        /* yields_value */ false,
        &mut func.local_count,
        &mut func.locals,
    )
}

/// `yields_value` is `true` when the value of `block`'s terminal statement is
/// consumed by the enclosing context (e.g. `let x = { …; if-let-expr }`).
/// In that case, fusing an `If` at the tail position would silently change the
/// block's type from the if's value type to Unit, since the fused labeled block's
/// breaks carry no value.
fn fuse_in_block(
    block: &mut NirBlock,
    yields_value: bool,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
) -> bool {
    // Recurse first into any nested blocks/stmts.
    let mut changed = false;
    let last_idx = block.stmts.len().saturating_sub(1);
    for (i, stmt) in block.stmts.iter_mut().enumerate() {
        // Only the LAST statement of a value-yielding block contributes the
        // block's terminal value. All earlier statements are in statement
        // position and may always be fused.
        let stmt_yields_value = yields_value && i == last_idx;
        changed |= fuse_in_stmt(stmt, stmt_yields_value, local_count, locals);
    }
    // Then look for adjacent (Let+LabeledBlock, If+VariantTest) pairs at this level.
    changed |= fuse_adjacent_pairs(block, yields_value, local_count, locals);
    changed
}

fn fuse_in_stmt(
    stmt: &mut NirStmt,
    yields_value: bool,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
) -> bool {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } => fuse_in_expr(value, local_count, locals),
        NirStmtKind::Expr(expr) => fuse_in_expr(expr, local_count, locals),
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = fuse_in_expr(condition, local_count, locals);
            // The branches' tail values become this If's value, which becomes the
            // enclosing block's value when `yields_value` is true.
            changed |= fuse_in_block(then_block, yields_value, local_count, locals);
            if let Some(eb) = else_block {
                changed |= fuse_in_block(eb, yields_value, local_count, locals);
            }
            changed
        }
        NirStmtKind::Loop { body } => {
            // Loop bodies don't fall through with a value.
            fuse_in_block(body, false, local_count, locals)
        }
        NirStmtKind::LabeledBlock { block: body, .. } => {
            // A statement-level labeled block discards its value.
            fuse_in_block(body, false, local_count, locals)
        }
        NirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = fuse_in_expr(scrutinee, local_count, locals);
            changed |= fuse_in_block(then_block, yields_value, local_count, locals);
            if let Some(eb) = else_block {
                changed |= fuse_in_block(eb, yields_value, local_count, locals);
            }
            changed
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                fuse_in_expr(v, local_count, locals)
            } else {
                false
            }
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                fuse_in_expr(v, local_count, locals)
            } else {
                false
            }
        }
        NirStmtKind::LetDestructure { value, .. } => fuse_in_expr(value, local_count, locals),
        NirStmtKind::Continue
        | NirStmtKind::TaskReturn { .. }
        | NirStmtKind::VariadicForOf { .. } => false,
    }
}

/// Check if a labeled block expression is trivially a single `break label: value` statement.
/// If so, inline the break value directly, eliminating the labeled block overhead.
///
/// Pattern:
/// ```text
/// label: { break label: expr }  →  expr
/// ```
fn try_inline_trivial_labeled_block(expr: &mut NirExpr) -> bool {
    let NirExprKind::LabeledBlock { label, block, .. } = &mut expr.kind else {
        return false;
    };
    if block.stmts.len() != 1 {
        return false;
    }
    let NirStmtKind::Break {
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
    let NirStmtKind::Break {
        value: Some(break_value),
        ..
    } = std::mem::replace(&mut block.stmts[0].kind, NirStmtKind::Continue)
    else {
        unreachable!()
    };
    let span = expr.span;
    let type_id = expr.type_id;
    *expr = NirExpr {
        kind: break_value.kind,
        type_id,
        span,
    };
    true
}

fn fuse_in_expr(expr: &mut NirExpr, local_count: &mut u32, locals: &mut Vec<NirLocal>) -> bool {
    match &mut expr.kind {
        NirExprKind::LabeledBlock { block, .. } => {
            // Expression-level labeled block: its terminal value is consumed.
            let changed = fuse_in_block(block, /* yields_value */ true, local_count, locals);
            // Then check if this became a trivial single-break block
            try_inline_trivial_labeled_block(expr) || changed
        }
        NirExprKind::Block(block) => {
            // Expression-level block: terminal value is consumed.
            fuse_in_block(block, /* yields_value */ true, local_count, locals)
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut changed = fuse_in_expr(condition, local_count, locals);
            // If-expression branches contribute the if's value.
            changed |= fuse_in_block(then_branch, true, local_count, locals);
            if let Some(eb) = else_branch {
                changed |= fuse_in_block(eb, true, local_count, locals);
            }
            changed
        }
        NirExprKind::Binary { left, right, .. } => {
            fuse_in_expr(left, local_count, locals) | fuse_in_expr(right, local_count, locals)
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => {
            fuse_in_expr(inner, local_count, locals)
        }
        NirExprKind::Assign { target, value } => {
            fuse_in_expr(target, local_count, locals) | fuse_in_expr(value, local_count, locals)
        }
        NirExprKind::Call { args, .. } => {
            let mut changed = false;
            for arg in args {
                changed |= fuse_in_expr(&mut arg.expr, local_count, locals);
            }
            changed
        }
        NirExprKind::CmRawCall { args, .. } => {
            let mut changed = false;
            for arg in args {
                changed |= fuse_in_expr(arg, local_count, locals);
            }
            changed
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            let mut changed = fuse_in_expr(receiver, local_count, locals);
            for arg in args {
                changed |= fuse_in_expr(&mut arg.expr, local_count, locals);
            }
            changed
        }
        NirExprKind::IndirectCall { callee, args } => {
            let mut changed = fuse_in_expr(callee, local_count, locals);
            for arg in args {
                changed |= fuse_in_expr(arg, local_count, locals);
            }
            changed
        }
        NirExprKind::Index { expr: inner, index } => {
            fuse_in_expr(inner, local_count, locals) | fuse_in_expr(index, local_count, locals)
        }
        NirExprKind::StructLiteral { fields, .. } => {
            let mut changed = false;
            for f in fields {
                changed |= fuse_in_expr(&mut f.value, local_count, locals);
            }
            changed
        }
        NirExprKind::TupleLiteral { elements } => {
            let mut changed = false;
            for e in elements {
                changed |= fuse_in_expr(e, local_count, locals);
            }
            changed
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                fuse_in_expr(p, local_count, locals)
            } else {
                false
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            fuse_in_expr(functor, local_count, locals)
        }
        NirExprKind::GlobalVarSet { value, .. } => fuse_in_expr(value, local_count, locals),
        NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => fuse_in_expr(inner, local_count, locals),
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut changed = fuse_in_expr(scrutinee, local_count, locals);
            // Switch is an expression: each arm contributes the switch's value.
            for arm in arms {
                changed |= fuse_in_block(arm, true, local_count, locals);
            }
            changed |= fuse_in_block(default, true, local_count, locals);
            changed
        }
        NirExprKind::Match { expr, arms } => {
            let mut changed = fuse_in_expr(expr, local_count, locals);
            for arm in arms {
                changed |= fuse_in_expr(&mut arm.body, local_count, locals);
                if let Some(guard) = &mut arm.guard {
                    changed |= fuse_in_expr(guard, local_count, locals);
                }
            }
            changed
        }
        NirExprKind::Closure { body, .. } => fuse_in_expr(body, local_count, locals),
        // Leaf nodes
        NirExprKind::Local { .. }
        | NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => false,
        NirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        NirExprKind::WithHandler { .. } | NirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Look for adjacent (Let+LabeledBlock, If+VariantTest) statement pairs in `block`
/// and fuse them when all preconditions are met.
///
/// `yields_value` is `true` when the block's terminal-statement value is consumed
/// by the enclosing context: in that case, fusing an If at the tail position would
/// silently turn the block's type into Unit, since the fused labeled block's breaks
/// carry no value.
fn fuse_adjacent_pairs(
    block: &mut NirBlock,
    yields_value: bool,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
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
            // Refuse to fuse when (a) the If is the last statement of this block,
            // and (b) the block's terminal value is consumed by the enclosing
            // context. The fused labeled block's breaks carry no value, so its
            // type is Unit; replacing a value-yielding if-expression with it
            // would corrupt the block's type. See
            // tests/fixtures/if_let_some_ref_from_fn.wado.
            if yields_value && iter.peek().is_none() {
                new_stmts.push(let_stmt);
                new_stmts.push(if_stmt);
                continue;
            }
            let fused = perform_fusion(let_stmt, if_stmt, info, local_count, locals);
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
fn check_fusion_preconditions(let_stmt: &NirStmt, if_stmt: &NirStmt) -> Option<FusionInfo> {
    // --- Stmt 1: Let { value: LabeledBlock { label, block } } ---
    let NirStmtKind::Let {
        local_index: temp_local,
        value: let_value,
        ..
    } = &let_stmt.kind
    else {
        return None;
    };
    let NirExprKind::LabeledBlock {
        label,
        block: lb_block,
        ..
    } = &let_value.kind
    else {
        return None;
    };

    // --- Stmt 2: If { condition: VariantTest(Local(X), case=C), then_block, else_block } ---
    let NirStmtKind::If {
        condition,
        then_block,
        else_block,
    } = &if_stmt.kind
    else {
        return None;
    };
    let NirExprKind::VariantTest {
        expr: vt_expr,
        case_index,
        ..
    } = &condition.kind
    else {
        return None;
    };
    let NirExprKind::Local {
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
    block: &NirBlock,
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
    block: &NirBlock,
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
    stmt: &NirStmt,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    match &stmt.kind {
        NirStmtKind::Break {
            label: Some(l),
            value,
        } if l == label => {
            match value.as_ref().map(|v| &v.kind) {
                // null → None case, always valid
                None | Some(NirExprKind::Null) => true,
                // VariantConstruct → valid if payload doesn't contain breaks to this label
                Some(NirExprKind::VariantConstruct {
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
        NirStmtKind::LabeledBlock { label: l, .. } if l == label => true,
        // Recurse into other statement kinds.
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            check_lb_breaks_in_block(body, label, case_index, payload_type)
        }
        NirStmtKind::IfLet {
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
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            check_lb_breaks_in_expr(value, label, case_index, payload_type)
        }
        NirStmtKind::Break { value, .. } => value
            .as_ref()
            .is_none_or(|v| check_lb_breaks_in_expr(v, label, case_index, payload_type)),
        NirStmtKind::Return { value } => value
            .as_ref()
            .is_none_or(|v| check_lb_breaks_in_expr(v, label, case_index, payload_type)),
        _ => true,
    }
}

fn check_lb_breaks_in_expr(
    expr: &NirExpr,
    label: &str,
    case_index: u32,
    payload_type: &mut Option<TypeId>,
) -> bool {
    match &expr.kind {
        NirExprKind::LabeledBlock {
            label: l, block, ..
        } => {
            if l == label {
                // Same label shadowing — don't recurse (label is rebound here)
                true
            } else {
                check_lb_breaks_in_block(block, label, case_index, payload_type)
            }
        }
        NirExprKind::Block(block) => {
            check_lb_breaks_in_block(block, label, case_index, payload_type)
        }
        NirExprKind::If {
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
fn count_local_uses_in_block(block: &NirBlock, local_idx: u32) -> usize {
    block
        .stmts
        .iter()
        .map(|s| count_local_uses_in_stmt(s, local_idx))
        .sum()
}

fn count_local_uses_in_stmt(stmt: &NirStmt, local_idx: u32) -> usize {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            count_local_uses_in_expr(value, local_idx)
        }
        NirStmtKind::Expr(expr) => count_local_uses_in_expr(expr, local_idx),
        NirStmtKind::Return { value } => value
            .as_ref()
            .map_or(0, |v| count_local_uses_in_expr(v, local_idx)),
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            count_local_uses_in_block(body, local_idx)
        }
        NirStmtKind::IfLet {
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
        NirStmtKind::Break { value, .. } => value
            .as_ref()
            .map_or(0, |v| count_local_uses_in_expr(v, local_idx)),
        NirStmtKind::Continue
        | NirStmtKind::TaskReturn { .. }
        | NirStmtKind::VariadicForOf { .. } => 0,
    }
}

fn count_local_uses_in_expr(expr: &NirExpr, local_idx: u32) -> usize {
    match &expr.kind {
        NirExprKind::Local { index, .. } => usize::from(*index == local_idx),
        NirExprKind::Binary { left, right, .. } => {
            count_local_uses_in_expr(left, local_idx) + count_local_uses_in_expr(right, local_idx)
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::ClosureToCanonical { functor: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            count_local_uses_in_expr(inner, local_idx)
        }
        NirExprKind::Assign { target, value } => {
            count_local_uses_in_expr(target, local_idx) + count_local_uses_in_expr(value, local_idx)
        }
        NirExprKind::Index { expr: inner, index } => {
            count_local_uses_in_expr(inner, local_idx) + count_local_uses_in_expr(index, local_idx)
        }
        NirExprKind::Call { args, .. } => args
            .iter()
            .map(|a| count_local_uses_in_expr(&a.expr, local_idx))
            .sum(),
        NirExprKind::CmRawCall { args, .. } => args
            .iter()
            .map(|a| count_local_uses_in_expr(a, local_idx))
            .sum(),
        NirExprKind::MethodCall { receiver, args, .. } => {
            count_local_uses_in_expr(receiver, local_idx)
                + args
                    .iter()
                    .map(|a| count_local_uses_in_expr(&a.expr, local_idx))
                    .sum::<usize>()
        }
        NirExprKind::IndirectCall { callee, args } => {
            count_local_uses_in_expr(callee, local_idx)
                + args
                    .iter()
                    .map(|a| count_local_uses_in_expr(a, local_idx))
                    .sum::<usize>()
        }
        NirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .map(|f| count_local_uses_in_expr(&f.value, local_idx))
            .sum(),
        NirExprKind::TupleLiteral { elements } => elements
            .iter()
            .map(|e| count_local_uses_in_expr(e, local_idx))
            .sum(),
        NirExprKind::VariantConstruct { payload, .. } => payload
            .as_ref()
            .map_or(0, |p| count_local_uses_in_expr(p, local_idx)),
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            count_local_uses_in_block(block, local_idx)
        }
        NirExprKind::If {
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
        NirExprKind::Match { expr, arms } => {
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
        NirExprKind::Switch {
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
        NirExprKind::Closure { body, .. } => count_local_uses_in_expr(body, local_idx),
        NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => count_local_uses_in_expr(inner, local_idx),
        // Leaf nodes
        NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => 0,
        NirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        NirExprKind::WithHandler { .. } | NirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Count `VariantPayload { expr: Local(local_idx), case_index }` in a block.
fn count_variant_payload_uses_in_block(block: &NirBlock, local_idx: u32, case_index: u32) -> usize {
    block
        .stmts
        .iter()
        .map(|s| count_variant_payload_uses_in_stmt(s, local_idx, case_index))
        .sum()
}

fn count_variant_payload_uses_in_stmt(stmt: &NirStmt, local_idx: u32, case_index: u32) -> usize {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            count_variant_payload_uses_in_expr(value, local_idx, case_index)
        }
        NirStmtKind::Expr(expr) => count_variant_payload_uses_in_expr(expr, local_idx, case_index),
        NirStmtKind::Return { value } => value.as_ref().map_or(0, |v| {
            count_variant_payload_uses_in_expr(v, local_idx, case_index)
        }),
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            count_variant_payload_uses_in_block(body, local_idx, case_index)
        }
        NirStmtKind::IfLet {
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
        NirStmtKind::Break { value, .. } => value.as_ref().map_or(0, |v| {
            count_variant_payload_uses_in_expr(v, local_idx, case_index)
        }),
        NirStmtKind::Continue
        | NirStmtKind::TaskReturn { .. }
        | NirStmtKind::VariadicForOf { .. } => 0,
    }
}

fn count_variant_payload_uses_in_expr(expr: &NirExpr, local_idx: u32, case_index: u32) -> usize {
    match &expr.kind {
        NirExprKind::VariantPayload {
            expr: inner,
            case_index: ci,
            ..
        } if *ci == case_index => {
            if matches!(inner.kind, NirExprKind::Local { index, .. } if index == local_idx) {
                return 1;
            }
            count_variant_payload_uses_in_expr(inner, local_idx, case_index)
        }
        NirExprKind::Local { .. } => 0,
        NirExprKind::Binary { left, right, .. } => {
            count_variant_payload_uses_in_expr(left, local_idx, case_index)
                + count_variant_payload_uses_in_expr(right, local_idx, case_index)
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::ClosureToCanonical { functor: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            count_variant_payload_uses_in_expr(inner, local_idx, case_index)
        }
        NirExprKind::Assign { target, value } => {
            count_variant_payload_uses_in_expr(target, local_idx, case_index)
                + count_variant_payload_uses_in_expr(value, local_idx, case_index)
        }
        NirExprKind::Index { expr: inner, index } => {
            count_variant_payload_uses_in_expr(inner, local_idx, case_index)
                + count_variant_payload_uses_in_expr(index, local_idx, case_index)
        }
        NirExprKind::Call { args, .. } => args
            .iter()
            .map(|a| count_variant_payload_uses_in_expr(&a.expr, local_idx, case_index))
            .sum(),
        NirExprKind::CmRawCall { args, .. } => args
            .iter()
            .map(|a| count_variant_payload_uses_in_expr(a, local_idx, case_index))
            .sum(),
        NirExprKind::MethodCall { receiver, args, .. } => {
            count_variant_payload_uses_in_expr(receiver, local_idx, case_index)
                + args
                    .iter()
                    .map(|a| count_variant_payload_uses_in_expr(&a.expr, local_idx, case_index))
                    .sum::<usize>()
        }
        NirExprKind::IndirectCall { callee, args } => {
            count_variant_payload_uses_in_expr(callee, local_idx, case_index)
                + args
                    .iter()
                    .map(|a| count_variant_payload_uses_in_expr(a, local_idx, case_index))
                    .sum::<usize>()
        }
        NirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .map(|f| count_variant_payload_uses_in_expr(&f.value, local_idx, case_index))
            .sum(),
        NirExprKind::TupleLiteral { elements } => elements
            .iter()
            .map(|e| count_variant_payload_uses_in_expr(e, local_idx, case_index))
            .sum(),
        NirExprKind::VariantConstruct { payload, .. } => payload.as_ref().map_or(0, |p| {
            count_variant_payload_uses_in_expr(p, local_idx, case_index)
        }),
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            count_variant_payload_uses_in_block(block, local_idx, case_index)
        }
        NirExprKind::If {
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
        NirExprKind::Match { expr, arms } => {
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
        NirExprKind::Switch {
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
        NirExprKind::Closure { body, .. } => {
            count_variant_payload_uses_in_expr(body, local_idx, case_index)
        }
        NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => count_variant_payload_uses_in_expr(inner, local_idx, case_index),
        // Leaf nodes
        NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => 0,
        NirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        NirExprKind::WithHandler { .. } | NirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Perform the actual fusion transformation.
///
/// Consumes the two matched statements and produces the fused labeled block statement(s).
fn perform_fusion(
    let_stmt: NirStmt,
    if_stmt: NirStmt,
    info: FusionInfo,
    local_count: &mut u32,
    locals: &mut Vec<NirLocal>,
) -> Vec<NirStmt> {
    let span = let_stmt.span;

    // Extract the LabeledBlock body from the Let statement.
    let NirStmtKind::Let {
        value: let_value, ..
    } = let_stmt.kind
    else {
        unreachable!()
    };
    let NirExprKind::LabeledBlock {
        block: lb_block, ..
    } = let_value.kind
    else {
        unreachable!()
    };

    // Extract the then/else blocks from the If statement.
    let NirStmtKind::If {
        then_block,
        else_block,
        ..
    } = if_stmt.kind
    else {
        unreachable!()
    };

    // Allocate a fresh local for the payload value. The pasted-in `Let`
    // statements created below name the slot `__fused_payload_N`; mirror
    // that on the `NirLocal` so wir_build's local-name lookup matches.
    let payload_local = *local_count;
    *local_count += 1;
    locals.push(NirLocal {
        name: format!("__fused_payload_{payload_local}"),
        type_id: info.payload_type,
        is_mut: false,
    });

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

    vec![NirStmt::new(
        NirStmtKind::LabeledBlock {
            label: fused_label,
            block: NirBlock {
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
    stmts: Vec<NirStmt>,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &NirBlock,
    else_block: Option<&NirBlock>,
    span: crate::token::Span,
) -> Vec<NirStmt> {
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
    stmt: NirStmt,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &NirBlock,
    else_block: Option<&NirBlock>,
    span: crate::token::Span,
    out: &mut Vec<NirStmt>,
) {
    // Check for `break orig_label: v` first.
    let is_orig_label_break = matches!(&stmt.kind,
        NirStmtKind::Break { label: Some(l), .. } if l == orig_label);

    if is_orig_label_break {
        let NirStmtKind::Break { value, .. } = stmt.kind else {
            unreachable!()
        };
        let is_some_case = match &value {
            Some(v) => matches!(&v.kind,
                NirExprKind::VariantConstruct { case_index: ci, .. } if *ci == case_index),
            _ => false,
        };

        if is_some_case {
            // Extract payload expression from the VariantConstruct.
            let Some(v) = value else { unreachable!() };
            let NirExprKind::VariantConstruct { payload, .. } = v.kind else {
                unreachable!()
            };
            let payload_expr = payload
                .map(|p| *p)
                .unwrap_or_else(|| NirExpr::new(NirExprKind::Unit, payload_type, span));

            // Emit: let __payload = payload_expr;
            out.push(NirStmt::new(
                NirStmtKind::Let {
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
                NirStmtKind::Break { .. } | NirStmtKind::Return { .. } | NirStmtKind::Continue
            )
        });
        if !last_terminates {
            out.push(NirStmt::new(
                NirStmtKind::Break {
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
        NirStmtKind::If {
            condition,
            then_block: tb,
            else_block: eb,
        } => {
            let new_then = NirBlock {
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
            let new_else = eb.map(|e| NirBlock {
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
            out.push(NirStmt::new(
                NirStmtKind::If {
                    condition,
                    then_block: new_then,
                    else_block: new_else,
                },
                stmt_span,
            ));
        }
        NirStmtKind::Loop { body } => {
            let new_body = NirBlock {
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
            out.push(NirStmt::new(
                NirStmtKind::Loop { body: new_body },
                stmt_span,
            ));
        }
        NirStmtKind::LabeledBlock {
            label: ref l,
            block: inner,
        } if l != orig_label => {
            let l = l.clone();
            let new_inner = NirBlock {
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
            out.push(NirStmt::new(
                NirStmtKind::LabeledBlock {
                    label: l,
                    block: new_inner,
                },
                stmt_span,
            ));
        }
        NirStmtKind::IfLet {
            scrutinee,
            pattern,
            then_block: tb,
            else_block: eb,
        } => {
            let new_then = NirBlock {
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
            let new_else = eb.map(|e| NirBlock {
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
            out.push(NirStmt::new(
                NirStmtKind::IfLet {
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
            out.push(NirStmt::new(other, stmt_span));
        }
    }
}

/// Walk a `NirStmtKind` and apply label transformation to any nested block expressions
/// that may contain `break orig_label`.
fn transform_lb_in_stmt_kind(
    kind: &mut NirStmtKind,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &NirBlock,
    else_block: Option<&NirBlock>,
    span: crate::token::Span,
) {
    match kind {
        NirStmtKind::Let { value, .. }
        | NirStmtKind::LetDestructure { value, .. }
        | NirStmtKind::Expr(value)
        | NirStmtKind::TaskReturn { value } => {
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
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
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
        NirStmtKind::If { .. }
        | NirStmtKind::Loop { .. }
        | NirStmtKind::LabeledBlock { .. }
        | NirStmtKind::IfLet { .. }
        | NirStmtKind::Continue
        | NirStmtKind::VariadicForOf { .. } => {}
    }
}

/// Recursively walk a `NirExpr` to find and transform blocks that contain
/// `break orig_label` statements.
fn transform_lb_in_expr(
    expr: &mut NirExpr,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &NirBlock,
    else_block: Option<&NirBlock>,
    span: crate::token::Span,
) {
    match &mut expr.kind {
        NirExprKind::Block(block) => {
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
        NirExprKind::LabeledBlock {
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
        NirExprKind::Match {
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
        NirExprKind::If {
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
        NirExprKind::Switch {
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
        NirExprKind::Binary { .. }
        | NirExprKind::Unary { .. }
        | NirExprKind::Cast { .. }
        | NirExprKind::FieldAccess { .. }
        | NirExprKind::TupleSpread { .. }
        | NirExprKind::TupleZip { .. }
        | NirExprKind::TypePackExpansion { .. }
        | NirExprKind::Assign { .. }
        | NirExprKind::Index { .. }
        | NirExprKind::Call { .. }
        | NirExprKind::CmRawCall { .. }
        | NirExprKind::MethodCall { .. }
        | NirExprKind::IndirectCall { .. }
        | NirExprKind::StructLiteral { .. }
        | NirExprKind::TupleLiteral { .. }
        | NirExprKind::VariantConstruct { .. }
        | NirExprKind::VariantTag { .. }
        | NirExprKind::VariantTest { .. }
        | NirExprKind::VariantPayload { .. }
        | NirExprKind::Closure { .. }
        | NirExprKind::ClosureToCanonical { .. }
        | NirExprKind::GlobalVarSet { .. }
        | NirExprKind::Local { .. }
        | NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. }
        | NirExprKind::TemplateString { .. } => {}
        NirExprKind::WithHandler { .. } | NirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Transform break statements within a block's stmts in-place.
fn transform_lb_in_block(
    block: &mut NirBlock,
    orig_label: &str,
    fused_label: &str,
    case_index: u32,
    temp_local: u32,
    payload_local: u32,
    payload_type: TypeId,
    then_block: &NirBlock,
    else_block: Option<&NirBlock>,
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
    block: &mut NirBlock,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    for stmt in &mut block.stmts {
        subst_variant_payload_in_stmt(stmt, temp_local, case_index, payload_local);
    }
}

fn subst_variant_payload_in_stmt(
    stmt: &mut NirStmt,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            subst_variant_payload_in_expr(value, temp_local, case_index, payload_local);
        }
        NirStmtKind::Expr(expr) => {
            subst_variant_payload_in_expr(expr, temp_local, case_index, payload_local);
        }
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                subst_variant_payload_in_expr(v, temp_local, case_index, payload_local);
            }
        }
        NirStmtKind::If {
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
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            subst_variant_payload_in_block(body, temp_local, case_index, payload_local);
        }
        NirStmtKind::IfLet {
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
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                subst_variant_payload_in_expr(v, temp_local, case_index, payload_local);
            }
        }
        NirStmtKind::Continue
        | NirStmtKind::TaskReturn { .. }
        | NirStmtKind::VariadicForOf { .. } => {}
    }
}

fn subst_variant_payload_in_expr(
    expr: &mut NirExpr,
    temp_local: u32,
    case_index: u32,
    payload_local: u32,
) {
    // Match the target pattern first (top-down, before recursing).
    if let NirExprKind::VariantPayload {
        expr: inner,
        case_index: ci,
        ..
    } = &expr.kind
        && *ci == case_index
        && let NirExprKind::Local { index, .. } = inner.kind
        && index == temp_local
    {
        expr.kind = NirExprKind::Local {
            index: payload_local,
            name: format!("__fused_payload_{payload_local}"),
        };
        return;
    }

    // Recurse into sub-expressions.
    match &mut expr.kind {
        NirExprKind::Local { .. } => {}
        NirExprKind::Binary { left, right, .. } => {
            subst_variant_payload_in_expr(left, temp_local, case_index, payload_local);
            subst_variant_payload_in_expr(right, temp_local, case_index, payload_local);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::ClosureToCanonical { functor: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            subst_variant_payload_in_expr(inner, temp_local, case_index, payload_local);
        }
        NirExprKind::Assign { target, value } => {
            subst_variant_payload_in_expr(target, temp_local, case_index, payload_local);
            subst_variant_payload_in_expr(value, temp_local, case_index, payload_local);
        }
        NirExprKind::Index { expr: inner, index } => {
            subst_variant_payload_in_expr(inner, temp_local, case_index, payload_local);
            subst_variant_payload_in_expr(index, temp_local, case_index, payload_local);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                subst_variant_payload_in_expr(&mut arg.expr, temp_local, case_index, payload_local);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                subst_variant_payload_in_expr(arg, temp_local, case_index, payload_local);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            subst_variant_payload_in_expr(receiver, temp_local, case_index, payload_local);
            for arg in args {
                subst_variant_payload_in_expr(&mut arg.expr, temp_local, case_index, payload_local);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            subst_variant_payload_in_expr(callee, temp_local, case_index, payload_local);
            for arg in args {
                subst_variant_payload_in_expr(arg, temp_local, case_index, payload_local);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                subst_variant_payload_in_expr(&mut f.value, temp_local, case_index, payload_local);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for e in elements {
                subst_variant_payload_in_expr(e, temp_local, case_index, payload_local);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                subst_variant_payload_in_expr(p, temp_local, case_index, payload_local);
            }
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            subst_variant_payload_in_block(block, temp_local, case_index, payload_local);
        }
        NirExprKind::If {
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
        NirExprKind::Match { expr, arms } => {
            subst_variant_payload_in_expr(expr, temp_local, case_index, payload_local);
            for arm in arms {
                subst_variant_payload_in_expr(&mut arm.body, temp_local, case_index, payload_local);
                if let Some(g) = &mut arm.guard {
                    subst_variant_payload_in_expr(g, temp_local, case_index, payload_local);
                }
            }
        }
        NirExprKind::Switch {
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
        NirExprKind::Closure { body, .. } => {
            subst_variant_payload_in_expr(body, temp_local, case_index, payload_local);
        }
        NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            subst_variant_payload_in_expr(inner, temp_local, case_index, payload_local);
        }
        // Leaf nodes carry no sub-expressions to substitute into.
        NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => {}
        NirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        NirExprKind::WithHandler { .. } | NirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Returns `true` if `block` contains a `Loop` statement at any nesting depth.
///
/// This is used to determine whether `labeled_block_fusion` would introduce a
/// new loop nesting that could confuse free unlabeled `break`/`continue` in
/// the THEN/ELSE blocks being merged.
fn block_contains_loop(block: &NirBlock) -> bool {
    block.stmts.iter().any(stmt_contains_loop)
}

fn stmt_contains_loop(stmt: &NirStmt) -> bool {
    match &stmt.kind {
        NirStmtKind::Loop { .. } => true,
        NirStmtKind::LabeledBlock { block, .. } => block.stmts.iter().any(stmt_contains_loop),
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        }
        | NirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            then_block.stmts.iter().any(stmt_contains_loop)
                || else_block
                    .as_ref()
                    .is_some_and(|b| b.stmts.iter().any(stmt_contains_loop))
        }
        NirStmtKind::Let { value, .. }
        | NirStmtKind::LetDestructure { value, .. }
        | NirStmtKind::Expr(value)
        | NirStmtKind::Return { value: Some(value) } => expr_contains_loop(value),
        _ => false,
    }
}

fn expr_contains_loop(expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
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
fn block_has_free_unlabeled_loop_exit(block: &NirBlock) -> bool {
    stmts_have_free_unlabeled_loop_exit(&block.stmts, 0)
}

fn stmts_have_free_unlabeled_loop_exit(stmts: &[NirStmt], loop_depth: u32) -> bool {
    stmts
        .iter()
        .any(|s| stmt_has_free_unlabeled_loop_exit(s, loop_depth))
}

fn stmt_has_free_unlabeled_loop_exit(stmt: &NirStmt, loop_depth: u32) -> bool {
    match &stmt.kind {
        NirStmtKind::Break { label: None, .. } | NirStmtKind::Continue => loop_depth == 0,
        NirStmtKind::Loop { body } => {
            stmts_have_free_unlabeled_loop_exit(&body.stmts, loop_depth + 1)
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            stmts_have_free_unlabeled_loop_exit(&block.stmts, loop_depth)
        }
        NirStmtKind::If {
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
        NirStmtKind::IfLet {
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
        NirStmtKind::Let { value, .. }
        | NirStmtKind::LetDestructure { value, .. }
        | NirStmtKind::Expr(value)
        | NirStmtKind::Return { value: Some(value) }
        | NirStmtKind::Break {
            value: Some(value), ..
        }
        | NirStmtKind::TaskReturn { value } => expr_has_free_unlabeled_loop_exit(value, loop_depth),
        _ => false,
    }
}

fn expr_has_free_unlabeled_loop_exit(expr: &NirExpr, loop_depth: u32) -> bool {
    match &expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            stmts_have_free_unlabeled_loop_exit(&block.stmts, loop_depth)
        }
        NirExprKind::If {
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
        NirExprKind::Binary { left, right, .. } => {
            expr_has_free_unlabeled_loop_exit(left, loop_depth)
                || expr_has_free_unlabeled_loop_exit(right, loop_depth)
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::ClosureToCanonical { functor: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            expr_has_free_unlabeled_loop_exit(inner, loop_depth)
        }
        NirExprKind::Assign { target, value } => {
            expr_has_free_unlabeled_loop_exit(target, loop_depth)
                || expr_has_free_unlabeled_loop_exit(value, loop_depth)
        }
        NirExprKind::Call { args, .. } => args
            .iter()
            .any(|a| expr_has_free_unlabeled_loop_exit(&a.expr, loop_depth)),
        NirExprKind::MethodCall { receiver, args, .. } => {
            expr_has_free_unlabeled_loop_exit(receiver, loop_depth)
                || args
                    .iter()
                    .any(|a| expr_has_free_unlabeled_loop_exit(&a.expr, loop_depth))
        }
        NirExprKind::IndirectCall { callee, args } => {
            expr_has_free_unlabeled_loop_exit(callee, loop_depth)
                || args
                    .iter()
                    .any(|a| expr_has_free_unlabeled_loop_exit(a, loop_depth))
        }
        NirExprKind::CmRawCall { args, .. } => args
            .iter()
            .any(|a| expr_has_free_unlabeled_loop_exit(a, loop_depth)),
        NirExprKind::Index { expr: inner, index } => {
            expr_has_free_unlabeled_loop_exit(inner, loop_depth)
                || expr_has_free_unlabeled_loop_exit(index, loop_depth)
        }
        NirExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|f| expr_has_free_unlabeled_loop_exit(&f.value, loop_depth)),
        NirExprKind::TupleLiteral { elements } => elements
            .iter()
            .any(|e| expr_has_free_unlabeled_loop_exit(e, loop_depth)),
        NirExprKind::VariantConstruct { payload, .. } => payload
            .as_deref()
            .is_some_and(|p| expr_has_free_unlabeled_loop_exit(p, loop_depth)),
        NirExprKind::Closure { body, .. } => expr_has_free_unlabeled_loop_exit(body, loop_depth),
        NirExprKind::Match { expr, arms } => {
            expr_has_free_unlabeled_loop_exit(expr, loop_depth)
                || arms
                    .iter()
                    .any(|arm| expr_has_free_unlabeled_loop_exit(&arm.body, loop_depth))
        }
        NirExprKind::Switch {
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
