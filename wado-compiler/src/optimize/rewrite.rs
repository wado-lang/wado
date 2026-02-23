//! TIR rewrite optimizations for Wado
//!
//! This module provides lightweight TIR rewrites that don't warrant their own module:
//!
//! 1. **Labeled Block Simplification**: Eliminates trivial `label: { break label: expr; }`
//!    patterns (common after inlining) by replacing them with just `expr`.
//!
//! 2. **Select Lowering**: Converts simple `if cond { a } else { b }` expressions to
//!    `builtin::select(cond, a, b)` which emits the branchless Wasm `select` instruction.
//!    Both branches must be pure (no side effects, no traps) since `select` evaluates
//!    both operands eagerly.
//!
//! 3. **Move Insertion**: Wraps fresh values in `Move` nodes to avoid unnecessary copies.
//!    Fresh values (literals, call results, etc.) can be moved directly without copying
//!    since they are newly created and owned by the current expression.

use crate::name::ModuleSource;
use crate::project::Project;
use crate::tir::{
    FunctionRef, MonomorphInfo, ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind,
    TypeId, TypeTable,
};

/// Run all post-optimization TIR rewrites in a single pass over all functions.
///
/// For each function, this performs (in order):
/// 1. Labeled block simplification (`L: { break L: expr; }` -> `expr`)
/// 2. Move insertion (wrap fresh values in `Move` to avoid copies)
pub fn rewrite(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();

            // 1. Simplify trivial labeled blocks
            if let Some(ref mut body) = func.body {
                simplify_labeled_blocks_in_block(body);
            }

            // 2. Insert moves for fresh values
            if let Some(ref mut body) = func.body {
                insert_moves_in_block(body, &type_table);
            }
        }
    }
}

fn simplify_labeled_blocks_in_block(block: &mut TirBlock) -> bool {
    let mut changed = false;
    for stmt in &mut block.stmts {
        changed |= simplify_labeled_blocks_in_stmt(stmt);
    }
    changed
}

fn simplify_labeled_blocks_in_stmt(stmt: &mut TirStmt) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => simplify_labeled_blocks_in_expr(value),
        TirStmtKind::Expr(expr) => simplify_labeled_blocks_in_expr(expr),
        TirStmtKind::Return { value } => {
            value.as_mut().is_some_and(simplify_labeled_blocks_in_expr)
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = simplify_labeled_blocks_in_expr(condition);
            changed |= simplify_labeled_blocks_in_block(then_block);
            if let Some(eb) = else_block {
                changed |= simplify_labeled_blocks_in_block(eb);
            }
            changed
        }
        TirStmtKind::Loop { body } => simplify_labeled_blocks_in_block(body),
        TirStmtKind::LabeledBlock { block, .. } => simplify_labeled_blocks_in_block(block),
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = simplify_labeled_blocks_in_expr(scrutinee);
            changed |= simplify_labeled_blocks_in_block(then_block);
            if let Some(eb) = else_block {
                changed |= simplify_labeled_blocks_in_block(eb);
            }
            changed
        }
        TirStmtKind::Break { value, .. } => {
            value.as_mut().is_some_and(simplify_labeled_blocks_in_expr)
        }
        TirStmtKind::Continue => false,
        TirStmtKind::LetPattern { value, .. } => simplify_labeled_blocks_in_expr(value),
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn simplify_labeled_blocks_in_expr(expr: &mut TirExpr) -> bool {
    let mut changed = false;

    // First, recurse into sub-expressions
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            changed |= simplify_labeled_blocks_in_expr(left);
            changed |= simplify_labeled_blocks_in_expr(right);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Move { expr: inner }
        | TirExprKind::IsNotNull { expr: inner }
        | TirExprKind::UnwrapOption { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            changed |= simplify_labeled_blocks_in_expr(inner);
        }
        TirExprKind::Assign { target, value } => {
            changed |= simplify_labeled_blocks_in_expr(target);
            changed |= simplify_labeled_blocks_in_expr(value);
        }
        TirExprKind::Index { expr: inner, index } => {
            changed |= simplify_labeled_blocks_in_expr(inner);
            changed |= simplify_labeled_blocks_in_expr(index);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                changed |= simplify_labeled_blocks_in_expr(arg);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            changed |= simplify_labeled_blocks_in_expr(receiver);
            for arg in args {
                changed |= simplify_labeled_blocks_in_expr(arg);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            changed |= simplify_labeled_blocks_in_expr(callee);
            for arg in args {
                changed |= simplify_labeled_blocks_in_expr(arg);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            changed |= simplify_labeled_blocks_in_expr(functor);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            changed |= simplify_labeled_blocks_in_block(block);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= simplify_labeled_blocks_in_expr(condition);
            changed |= simplify_labeled_blocks_in_block(then_branch);
            if let Some(eb) = else_branch {
                changed |= simplify_labeled_blocks_in_block(eb);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            changed |= simplify_labeled_blocks_in_expr(inner);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= simplify_labeled_blocks_in_expr(guard);
                }
                changed |= simplify_labeled_blocks_in_expr(&mut arm.body);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                changed |= simplify_labeled_blocks_in_expr(&mut field.value);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                changed |= simplify_labeled_blocks_in_expr(elem);
            }
        }
        TirExprKind::MapLiteral { entries } => {
            for (_, value) in entries {
                changed |= simplify_labeled_blocks_in_expr(value);
            }
        }
        TirExprKind::OptionSome { value } => {
            changed |= simplify_labeled_blocks_in_expr(value);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                changed |= simplify_labeled_blocks_in_expr(p);
            }
        }
        TirExprKind::Closure { body, .. } => {
            changed |= simplify_labeled_blocks_in_expr(body);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            changed |= simplify_labeled_blocks_in_expr(value);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= simplify_labeled_blocks_in_expr(scrutinee);
            for arm in arms {
                changed |= simplify_labeled_blocks_in_block(arm);
            }
            changed |= simplify_labeled_blocks_in_block(default);
        }
        // Leaf nodes
        TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::EnumConstruct { .. } => {}
    }

    // Simplify: `label: { break label: expr; }` → `expr`
    if let TirExprKind::LabeledBlock { label, block, .. } = &expr.kind
        && block.stmts.len() == 1
        && let TirStmtKind::Break {
            label: Some(break_label),
            value: Some(_),
        } = &block.stmts[0].kind
        && break_label == label
    {
        let TirExprKind::LabeledBlock { block, .. } =
            std::mem::replace(&mut expr.kind, TirExprKind::Unit)
        else {
            unreachable!();
        };
        let mut stmts = block.stmts;
        let TirStmtKind::Break {
            value: Some(inner), ..
        } = stmts.remove(0).kind
        else {
            unreachable!();
        };
        *expr = inner;
        changed = true;
    }

    changed
}

/// Check if a TIR expression is side-effect-free and suitable for `select` operands.
///
/// The Wasm `select` instruction evaluates both operands eagerly, so both must be
/// pure (no side effects, no traps). We conservatively accept only:
/// - Local variable reads
/// - Literals (int, float, bool, char)
fn is_select_eligible_expr(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::Local { .. }
            | TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
    )
}

/// Try to extract a single pure expression from a block for select optimization.
///
/// Returns `Some(expr)` if the block contains exactly one `Expr` statement
/// whose expression is side-effect-free.
fn try_select_value(block: &TirBlock) -> Option<&TirExpr> {
    if block.stmts.len() != 1 {
        return None;
    }
    if let TirStmtKind::Expr(expr) = &block.stmts[0].kind
        && is_select_eligible_expr(expr)
    {
        return Some(expr);
    }
    None
}

/// Try to transform an `If` expression into a `builtin::select` call.
///
/// Returns `Some(call_expr)` if the if-expression is eligible:
/// - Has both then and else branches
/// - Both branches are single pure expressions
/// - Result type is not unit
fn try_lower_to_select(
    condition: &TirExpr,
    then_branch: &TirBlock,
    else_branch: &Option<TirBlock>,
    result_type: TypeId,
    span: crate::token::Span,
) -> Option<TirExpr> {
    let else_block = else_branch.as_ref()?;

    // Unit-typed if-expressions are statements, not value-producing selects
    if result_type == TypeTable::UNIT {
        return None;
    }

    let true_val = try_select_value(then_branch)?;
    let false_val = try_select_value(else_block)?;

    // Construct: builtin::select(condition, true_val, false_val)
    let func_ref = FunctionRef::External {
        module_source: ModuleSource::builtin(),
        name: "select".to_string(),
        monomorph_info: Some(MonomorphInfo {
            generic_name: "select".to_string(),
            type_args: vec![result_type],
        }),
        method_info: None,
    };

    Some(TirExpr::new(
        TirExprKind::Call {
            func: func_ref,
            type_args: vec![result_type],
            args: vec![condition.clone(), true_val.clone(), false_val.clone()],
        },
        result_type,
        span,
    ))
}

/// Check if an expression produces a fresh value that can be moved.
/// Fresh values are those that don't need copying because they're newly created.
fn is_fresh_value(expr: &TirExpr) -> bool {
    match &expr.kind {
        // Literals always produce fresh values
        TirExprKind::StringLiteral(_)
        | TirExprKind::StructLiteral { .. }
        | TirExprKind::ArrayLiteral { .. }
        | TirExprKind::TupleLiteral { .. }
        | TirExprKind::MapLiteral { .. }
        | TirExprKind::Null => true,

        // All call variants return fresh values (callee constructs/copies the return value)
        TirExprKind::Call { .. }
        | TirExprKind::StaticCall { .. }
        | TirExprKind::MethodCall { .. }
        | TirExprKind::CmRawCall { .. }
        | TirExprKind::IndirectCall { .. } => true,

        // ClosureToCanonical creates a fresh closure struct
        TirExprKind::ClosureToCanonical { .. } => true,

        // OptionSome is fresh if its inner value is fresh
        TirExprKind::OptionSome { value } => is_fresh_value(value),

        // VariantConstruct is fresh (it's a literal-like construction)
        TirExprKind::VariantConstruct { .. } => true,

        // EnumConstruct is fresh (it's a literal-like construction)
        TirExprKind::EnumConstruct { .. } => true,

        // Move is already marked as fresh
        TirExprKind::Move { .. } => true,

        // Everything else is not fresh
        _ => false,
    }
}

/// Check if a type requires value copying (composite types with value semantics).
fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. }
        | ResolvedType::GenericInstance { .. }
        | ResolvedType::Variant { .. } => true,
        ResolvedType::Tuple(elements) => !elements.is_empty(),
        // References, primitives, etc. don't need copying
        _ => false,
    }
}

/// Wrap an expression in Move if it's a fresh value that would otherwise be copied.
fn wrap_in_move_if_eligible(expr: TirExpr, type_table: &TypeTable) -> TirExpr {
    if needs_value_copy(expr.type_id, type_table) && is_fresh_value(&expr) {
        let type_id = expr.type_id;
        let span = expr.span;
        TirExpr::new(
            TirExprKind::Move {
                expr: Box::new(expr),
            },
            type_id,
            span,
        )
    } else {
        expr
    }
}

/// Insert move semantics for fresh values in a block.
fn insert_moves_in_block(block: &mut TirBlock, type_table: &TypeTable) {
    for stmt in &mut block.stmts {
        insert_moves_in_stmt(stmt, type_table);
    }
}

/// Insert move semantics for fresh values in a statement.
fn insert_moves_in_stmt(stmt: &mut TirStmt, type_table: &TypeTable) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            // First recursively process nested expressions (e.g., LabeledBlock containing Let)
            insert_moves_in_expr(value, type_table);
            // Then wrap the value in Move if eligible
            let old_value = std::mem::replace(
                value,
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, stmt.span),
            );
            *value = wrap_in_move_if_eligible(old_value, type_table);
        }
        TirStmtKind::Expr(expr) => {
            insert_moves_in_expr(expr, type_table);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                insert_moves_in_expr(v, type_table);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            insert_moves_in_expr(condition, type_table);
            insert_moves_in_block(then_block, type_table);
            if let Some(eb) = else_block {
                insert_moves_in_block(eb, type_table);
            }
        }
        TirStmtKind::Loop { body } => {
            insert_moves_in_block(body, type_table);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            insert_moves_in_block(block, type_table);
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                insert_moves_in_expr(v, type_table);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            insert_moves_in_expr(value, type_table);
            // Wrap the tuple value in Move if eligible
            let TirStmtKind::LetPattern { value, .. } = &mut stmt.kind else {
                unreachable!()
            };
            let old_value = std::mem::replace(
                value,
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, stmt.span),
            );
            *value = wrap_in_move_if_eligible(old_value, type_table);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            insert_moves_in_expr(scrutinee, type_table);
            insert_moves_in_block(then_block, type_table);
            if let Some(eb) = else_block {
                insert_moves_in_block(eb, type_table);
            }
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

/// Insert move semantics in nested expressions (for consistency).
fn insert_moves_in_expr(expr: &mut TirExpr, type_table: &TypeTable) {
    match &mut expr.kind {
        TirExprKind::Binary { left, right, .. } => {
            insert_moves_in_expr(left, type_table);
            insert_moves_in_expr(right, type_table);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            insert_moves_in_expr(inner, type_table);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            // Wrap arguments in Move if they are fresh values (argument passing is assignment)
            for arg in args.iter_mut() {
                insert_moves_in_expr(arg, type_table);
            }
            for i in 0..args.len() {
                let arg = std::mem::replace(
                    &mut args[i],
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                );
                args[i] = wrap_in_move_if_eligible(arg, type_table);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            insert_moves_in_expr(receiver, type_table);
            // Wrap arguments in Move if they are fresh values
            for arg in args.iter_mut() {
                insert_moves_in_expr(arg, type_table);
            }
            for i in 0..args.len() {
                let arg = std::mem::replace(
                    &mut args[i],
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                );
                args[i] = wrap_in_move_if_eligible(arg, type_table);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            insert_moves_in_expr(callee, type_table);
            // Wrap arguments in Move if they are fresh values
            for arg in args.iter_mut() {
                insert_moves_in_expr(arg, type_table);
            }
            for i in 0..args.len() {
                let arg = std::mem::replace(
                    &mut args[i],
                    TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
                );
                args[i] = wrap_in_move_if_eligible(arg, type_table);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            insert_moves_in_expr(functor, type_table);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            insert_moves_in_expr(inner, type_table);
        }
        TirExprKind::Index { expr: inner, index } => {
            insert_moves_in_expr(inner, type_table);
            insert_moves_in_expr(index, type_table);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            insert_moves_in_expr(inner, type_table);
        }
        TirExprKind::Assign { target, value } => {
            insert_moves_in_expr(target, type_table);
            // Wrap the assigned value in Move if eligible (same as Let)
            let old_value = std::mem::replace(
                value.as_mut(),
                TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, expr.span),
            );
            **value = wrap_in_move_if_eligible(old_value, type_table);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                insert_moves_in_expr(&mut field.value, type_table);
            }
        }
        TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                insert_moves_in_expr(elem, type_table);
            }
        }
        TirExprKind::MapLiteral { entries } => {
            for (_, value) in entries {
                insert_moves_in_expr(value, type_table);
            }
        }
        TirExprKind::OptionSome { value } => {
            insert_moves_in_expr(value, type_table);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                insert_moves_in_expr(payload_expr, type_table);
            }
        }
        TirExprKind::Move { expr } => {
            insert_moves_in_expr(expr, type_table);
        }
        TirExprKind::Block(block) => {
            insert_moves_in_block(block, type_table);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            // Try select lowering before recursing into branches
            if let Some(select_call) =
                try_lower_to_select(condition, then_branch, else_branch, expr.type_id, expr.span)
            {
                *expr = select_call;
                // Re-process the new Call expression for move insertion
                insert_moves_in_expr(expr, type_table);
                return;
            }
            insert_moves_in_expr(condition, type_table);
            insert_moves_in_block(then_branch, type_table);
            if let Some(eb) = else_branch {
                insert_moves_in_block(eb, type_table);
            }
        }
        TirExprKind::Closure { body, .. } => {
            insert_moves_in_expr(body, type_table);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            insert_moves_in_block(block, type_table);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            insert_moves_in_expr(value, type_table);
        }
        TirExprKind::Match { expr, arms } => {
            insert_moves_in_expr(expr, type_table);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    insert_moves_in_expr(guard, type_table);
                }
                insert_moves_in_expr(&mut arm.body, type_table);
            }
        }
        TirExprKind::IsNotNull { expr: inner }
        | TirExprKind::UnwrapOption { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. } => {
            insert_moves_in_expr(inner, type_table);
        }
        TirExprKind::VariantPayload { expr: inner, .. } => {
            insert_moves_in_expr(inner, type_table);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            insert_moves_in_expr(scrutinee, type_table);
            for arm in arms {
                insert_moves_in_block(arm, type_table);
            }
            insert_moves_in_block(default, type_table);
        }
        // Leaf nodes - no nested expressions
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}
