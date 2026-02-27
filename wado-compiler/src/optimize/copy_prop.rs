//! Copy propagation optimization for Wado TIR
//!
//! This module eliminates trivial copy bindings like `let x = y` or `let x = 42`
//! by propagating the source value to all uses of the target variable.
//!
//! The optimization is safe when:
//! - The target variable is not assigned after initialization
//! - The target variable does not have its address taken
//! - The target variable is not captured by a closure
//! - For local-to-local copies: the source is not modified after the copy
//! - For value types: the source is dead after the binding (`read_count` is 1)

use crate::project::Project;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use indexmap::IndexMap;
use indexmap::IndexSet;

/// Information about a copy binding that may be eliminable.
/// Pattern: `let x: T = y` where y is a local variable or simple literal
#[derive(Debug, Clone)]
struct CopyBinding {
    /// Local index of the target variable (x)
    target_local: u32,
    /// The source expression (either a Local or a simple literal)
    source: CopySource,
    /// Type of the binding
    type_id: TypeId,
    /// Whether the target is mutable
    #[allow(dead_code)]
    is_mut: bool,
}

/// Source of a copy binding
#[derive(Debug, Clone)]
enum CopySource {
    /// Copy from another local variable
    Local { index: u32, name: String },
    /// Copy from an integer literal
    IntLiteral { value: u64, repr: String },
    /// Copy from a float literal
    FloatLiteral { value: f64, repr: String },
    /// Copy from a bool literal
    BoolLiteral(bool),
    /// Copy from a char literal
    CharLiteral(char),
}

/// Usage information for a local variable
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
struct LocalUsage {
    /// Number of times the local is read
    read_count: u32,
    /// Whether the local is ever assigned to (after initialization)
    is_assigned: bool,
    /// Whether the local is used in a loop condition (risky to propagate)
    #[allow(dead_code)]
    in_loop_condition: bool,
    /// Whether the local has its address taken
    address_taken: bool,
    /// Whether the local is captured by a closure
    is_captured: bool,
}

/// Analyze a Let statement to see if it's a copy binding.
fn analyze_copy_binding(stmt: &TirStmt) -> Option<CopyBinding> {
    let TirStmtKind::Let {
        local_index,
        is_mut,
        value,
        ..
    } = &stmt.kind
    else {
        return None;
    };

    let source = match &value.kind {
        TirExprKind::Local { index, name } => CopySource::Local {
            index: *index,
            name: name.clone(),
        },
        TirExprKind::IntLiteral { value, repr } => CopySource::IntLiteral {
            value: *value,
            repr: repr.clone(),
        },
        TirExprKind::FloatLiteral { value, repr } => CopySource::FloatLiteral {
            value: *value,
            repr: repr.clone(),
        },
        TirExprKind::BoolLiteral(b) => CopySource::BoolLiteral(*b),
        TirExprKind::CharLiteral(c) => CopySource::CharLiteral(*c),
        _ => return None,
    };

    Some(CopyBinding {
        target_local: *local_index,
        source,
        type_id: value.type_id,
        is_mut: *is_mut,
    })
}

/// Collect usage information for all locals in a function body.
fn collect_local_usage(body: &TirBlock) -> IndexMap<u32, LocalUsage> {
    let mut usage: IndexMap<u32, LocalUsage> = IndexMap::new();
    collect_usage_in_block(body, &mut usage, false);
    usage
}

fn collect_usage_in_block(block: &TirBlock, usage: &mut IndexMap<u32, LocalUsage>, in_loop: bool) {
    for stmt in &block.stmts {
        collect_usage_in_stmt(stmt, usage, in_loop);
    }
}

fn collect_usage_in_stmt(stmt: &TirStmt, usage: &mut IndexMap<u32, LocalUsage>, in_loop: bool) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_usage_in_expr(value, usage, in_loop, false);
        }
        TirStmtKind::Expr(expr) => {
            collect_usage_in_expr(expr, usage, in_loop, false);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_usage_in_expr(v, usage, in_loop, false);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_usage_in_expr(condition, usage, in_loop, false);
            collect_usage_in_block(then_block, usage, in_loop);
            if let Some(eb) = else_block {
                collect_usage_in_block(eb, usage, in_loop);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_usage_in_block(body, usage, true);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_usage_in_block(block, usage, in_loop);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_usage_in_expr(scrutinee, usage, in_loop, false);
            collect_usage_in_block(then_block, usage, in_loop);
            if let Some(eb) = else_block {
                collect_usage_in_block(eb, usage, in_loop);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_usage_in_expr(v, usage, in_loop, false);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            collect_usage_in_expr(value, usage, in_loop, false);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn collect_usage_in_expr(
    expr: &TirExpr,
    usage: &mut IndexMap<u32, LocalUsage>,
    in_loop: bool,
    in_condition: bool,
) {
    match &expr.kind {
        TirExprKind::Local { index, .. } => {
            let entry = usage.entry(*index).or_default();
            entry.read_count += 1;
            if in_loop && in_condition {
                entry.in_loop_condition = true;
            }
        }
        TirExprKind::Assign { target, value } => {
            // Check if target is a local being assigned
            if let TirExprKind::Local { index, .. } = &target.kind {
                usage.entry(*index).or_default().is_assigned = true;
            }
            collect_usage_in_expr(target, usage, in_loop, in_condition);
            collect_usage_in_expr(value, usage, in_loop, in_condition);
        }
        TirExprKind::Unary { op, expr: inner } => {
            // Check for address-taken
            if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                && let TirExprKind::Local { index, .. } = &inner.kind
            {
                usage.entry(*index).or_default().address_taken = true;
            }
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_usage_in_expr(left, usage, in_loop, in_condition);
            collect_usage_in_expr(right, usage, in_loop, in_condition);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_usage_in_expr(arg, usage, in_loop, in_condition);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_usage_in_expr(receiver, usage, in_loop, in_condition);
            for arg in args {
                collect_usage_in_expr(arg, usage, in_loop, in_condition);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_usage_in_expr(callee, usage, in_loop, in_condition);
            for arg in args {
                collect_usage_in_expr(arg, usage, in_loop, in_condition);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_usage_in_expr(functor, usage, in_loop, in_condition);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
            collect_usage_in_expr(index, usage, in_loop, in_condition);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
        }
        TirExprKind::Block(block) => {
            collect_usage_in_block(block, usage, in_loop);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_usage_in_block(block, usage, in_loop);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_usage_in_expr(condition, usage, in_loop, in_condition);
            collect_usage_in_block(then_branch, usage, in_loop);
            if let Some(eb) = else_branch {
                collect_usage_in_block(eb, usage, in_loop);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_usage_in_expr(&field.value, usage, in_loop, in_condition);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_usage_in_expr(elem, usage, in_loop, in_condition);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_usage_in_expr(payload_expr, usage, in_loop, in_condition);
            }
        }
        TirExprKind::Closure { body, captures, .. } => {
            // Mark all captured variables as captured
            for capture in captures {
                usage.entry(capture.outer_index).or_default().is_captured = true;
            }
            collect_usage_in_expr(body, usage, in_loop, in_condition);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_usage_in_expr(inner, usage, in_loop, in_condition);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_usage_in_expr(guard, usage, in_loop, in_condition);
                }
                collect_usage_in_expr(&arm.body, usage, in_loop, in_condition);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_usage_in_expr(value, usage, in_loop, in_condition);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. } => {
            collect_usage_in_expr(expr, usage, in_loop, in_condition);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            collect_usage_in_expr(expr, usage, in_loop, in_condition);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_usage_in_expr(scrutinee, usage, in_loop, in_condition);
            for arm in arms {
                collect_usage_in_block(arm, usage, in_loop);
            }
            collect_usage_in_block(default, usage, in_loop);
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Check if a type requires value copying (composite types with value semantics).
fn needs_value_copy(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => true,
        ResolvedType::Tuple(elements) => !elements.is_empty(),
        // References, primitives, etc. don't need copying
        _ => false,
    }
}

/// Check if a binding can be safely eliminated via copy propagation.
fn can_propagate_copy(
    binding: &CopyBinding,
    usage: &IndexMap<u32, LocalUsage>,
    type_table: &TypeTable,
) -> bool {
    let target_usage = usage.get(&binding.target_local);

    // If the target is never used, it can be eliminated (dead code)
    let Some(target_usage) = target_usage else {
        return true;
    };

    // Don't propagate if target is assigned to after initialization
    if target_usage.is_assigned {
        return false;
    }

    // Don't propagate if address is taken (could be modified through pointer)
    if target_usage.address_taken {
        return false;
    }

    // Don't propagate if target is captured by a closure
    // (closure captures need to preserve the value at capture time)
    if target_usage.is_captured {
        return false;
    }

    match &binding.source {
        CopySource::Local { index, .. } => {
            // For local-to-local copy:
            // Safe if source is not modified after the copy
            let source_usage = usage.get(index);
            if let Some(su) = source_usage {
                // Source is assigned after initialization - not safe to propagate
                // because uses of the target should see the pre-assignment value
                if su.is_assigned {
                    return false;
                }
                // Source could be modified through a reference
                if su.address_taken {
                    return false;
                }
                // Source could be modified by a mutable closure capture
                if su.is_captured {
                    return false;
                }
            }

            // For value types (structs, arrays, tuples, strings), only propagate
            // if the source is dead after this binding (read_count == 1)
            if needs_value_copy(binding.type_id, type_table)
                && let Some(su) = source_usage
            {
                // Source must only be read once (in this binding) and not captured
                if su.read_count > 1 || su.address_taken || su.is_captured {
                    return false;
                }
            }
            // If no usage info, source is unused - safe to eliminate

            true
        }
        // Literals are always safe to propagate
        CopySource::IntLiteral { .. }
        | CopySource::FloatLiteral { .. }
        | CopySource::BoolLiteral(_)
        | CopySource::CharLiteral(_) => true,
    }
}

/// Substitute local references in a block.
/// Replaces uses of locals in `substitutions` with the corresponding source expression.
fn substitute_in_block(block: &mut TirBlock, substitutions: &IndexMap<u32, CopySource>) {
    for stmt in &mut block.stmts {
        substitute_in_stmt(stmt, substitutions);
    }
}

fn substitute_in_stmt(stmt: &mut TirStmt, substitutions: &IndexMap<u32, CopySource>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            substitute_in_expr(value, substitutions);
        }
        TirStmtKind::Expr(expr) => {
            substitute_in_expr(expr, substitutions);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                substitute_in_expr(v, substitutions);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            substitute_in_expr(condition, substitutions);
            substitute_in_block(then_block, substitutions);
            if let Some(eb) = else_block {
                substitute_in_block(eb, substitutions);
            }
        }
        TirStmtKind::Loop { body } => {
            substitute_in_block(body, substitutions);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            substitute_in_block(block, substitutions);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            substitute_in_expr(scrutinee, substitutions);
            substitute_in_block(then_block, substitutions);
            if let Some(eb) = else_block {
                substitute_in_block(eb, substitutions);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                substitute_in_expr(v, substitutions);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            substitute_in_expr(value, substitutions);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn substitute_in_expr(expr: &mut TirExpr, substitutions: &IndexMap<u32, CopySource>) {
    // Check if this is a local that needs substitution
    if let TirExprKind::Local { index, .. } = &expr.kind
        && let Some(source) = substitutions.get(index)
    {
        // Replace with the source expression
        expr.kind = match source {
            CopySource::Local {
                index: src_idx,
                name: src_name,
            } => TirExprKind::Local {
                index: *src_idx,
                name: src_name.clone(),
            },
            CopySource::IntLiteral { value, repr } => TirExprKind::IntLiteral {
                value: *value,
                repr: repr.clone(),
            },
            CopySource::FloatLiteral { value, repr } => TirExprKind::FloatLiteral {
                value: *value,
                repr: repr.clone(),
            },
            CopySource::BoolLiteral(b) => TirExprKind::BoolLiteral(*b),
            CopySource::CharLiteral(c) => TirExprKind::CharLiteral(*c),
        };
        return;
    }

    // Recurse into child expressions
    match &mut expr.kind {
        TirExprKind::Local { .. } => {
            // Already handled above
        }
        TirExprKind::Binary { left, right, .. } => {
            substitute_in_expr(left, substitutions);
            substitute_in_expr(right, substitutions);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            substitute_in_expr(inner, substitutions);
        }
        TirExprKind::Assign { target, value } => {
            substitute_in_expr(target, substitutions);
            substitute_in_expr(value, substitutions);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                substitute_in_expr(arg, substitutions);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            substitute_in_expr(receiver, substitutions);
            for arg in args {
                substitute_in_expr(arg, substitutions);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            substitute_in_expr(callee, substitutions);
            for arg in args {
                substitute_in_expr(arg, substitutions);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            substitute_in_expr(functor, substitutions);
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            substitute_in_expr(inner, substitutions);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            substitute_in_expr(inner, substitutions);
            substitute_in_expr(index, substitutions);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            substitute_in_expr(inner, substitutions);
        }
        TirExprKind::Block(block) => {
            substitute_in_block(block, substitutions);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            substitute_in_block(block, substitutions);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            substitute_in_expr(condition, substitutions);
            substitute_in_block(then_branch, substitutions);
            if let Some(eb) = else_branch {
                substitute_in_block(eb, substitutions);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                substitute_in_expr(&mut field.value, substitutions);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                substitute_in_expr(elem, substitutions);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                substitute_in_expr(payload_expr, substitutions);
            }
        }
        TirExprKind::Closure { body, .. } => {
            substitute_in_expr(body, substitutions);
        }
        TirExprKind::Match { expr: inner, arms } => {
            substitute_in_expr(inner, substitutions);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    substitute_in_expr(guard, substitutions);
                }
                substitute_in_expr(&mut arm.body, substitutions);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            substitute_in_expr(value, substitutions);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. } => {
            substitute_in_expr(expr, substitutions);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            substitute_in_expr(expr, substitutions);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            substitute_in_expr(scrutinee, substitutions);
            for arm in arms {
                substitute_in_block(arm, substitutions);
            }
            substitute_in_block(default, substitutions);
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Global { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
    }
}

/// Collect copy bindings from statements.
fn collect_copy_bindings(stmts: &[TirStmt], bindings: &mut Vec<CopyBinding>) {
    for stmt in stmts {
        if let Some(binding) = analyze_copy_binding(stmt) {
            bindings.push(binding);
        }
        collect_copy_bindings_in_stmt(stmt, bindings);
    }
}

fn collect_copy_bindings_in_stmt(stmt: &TirStmt, bindings: &mut Vec<CopyBinding>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_copy_bindings_in_expr(value, bindings);
        }
        TirStmtKind::Expr(expr) => {
            collect_copy_bindings_in_expr(expr, bindings);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_copy_bindings_in_expr(v, bindings);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_copy_bindings_in_expr(condition, bindings);
            collect_copy_bindings(&then_block.stmts, bindings);
            if let Some(eb) = else_block {
                collect_copy_bindings(&eb.stmts, bindings);
            }
        }
        TirStmtKind::Loop { body } => {
            collect_copy_bindings(&body.stmts, bindings);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_copy_bindings(&block.stmts, bindings);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_copy_bindings_in_expr(scrutinee, bindings);
            collect_copy_bindings(&then_block.stmts, bindings);
            if let Some(eb) = else_block {
                collect_copy_bindings(&eb.stmts, bindings);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_copy_bindings_in_expr(v, bindings);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            collect_copy_bindings_in_expr(value, bindings);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn collect_copy_bindings_in_expr(expr: &TirExpr, bindings: &mut Vec<CopyBinding>) {
    match &expr.kind {
        TirExprKind::Block(block) => {
            collect_copy_bindings(&block.stmts, bindings);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            collect_copy_bindings(&block.stmts, bindings);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_copy_bindings_in_expr(condition, bindings);
            collect_copy_bindings(&then_branch.stmts, bindings);
            if let Some(eb) = else_branch {
                collect_copy_bindings(&eb.stmts, bindings);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_copy_bindings_in_expr(left, bindings);
            collect_copy_bindings_in_expr(right, bindings);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_copy_bindings_in_expr(inner, bindings);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_copy_bindings_in_expr(receiver, bindings);
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_copy_bindings_in_expr(callee, bindings);
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_copy_bindings_in_expr(functor, bindings);
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. } => {
            collect_copy_bindings_in_expr(inner, bindings);
        }
        TirExprKind::Assign { target, value } => {
            collect_copy_bindings_in_expr(target, bindings);
            collect_copy_bindings_in_expr(value, bindings);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_copy_bindings_in_expr(&field.value, bindings);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_copy_bindings_in_expr(elem, bindings);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                collect_copy_bindings_in_expr(payload_expr, bindings);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_copy_bindings_in_expr(body, bindings);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_copy_bindings_in_expr(inner, bindings);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_copy_bindings_in_expr(guard, bindings);
                }
                collect_copy_bindings_in_expr(&arm.body, bindings);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_copy_bindings_in_expr(value, bindings);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. } => {
            collect_copy_bindings_in_expr(expr, bindings);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            collect_copy_bindings_in_expr(expr, bindings);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_copy_bindings_in_expr(scrutinee, bindings);
            for arm in arms {
                collect_copy_bindings(&arm.stmts, bindings);
            }
            collect_copy_bindings(&default.stmts, bindings);
        }
        // Leaf nodes
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

/// Remove dead copy bindings from statements.
fn remove_copy_bindings(stmts: &mut Vec<TirStmt>, dead_locals: &IndexSet<u32>) {
    stmts.retain(|stmt| {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            !dead_locals.contains(local_index)
        } else {
            true
        }
    });

    for stmt in stmts {
        remove_copy_bindings_in_stmt(stmt, dead_locals);
    }
}

fn remove_copy_bindings_in_stmt(stmt: &mut TirStmt, dead_locals: &IndexSet<u32>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirStmtKind::Expr(expr) => {
            remove_copy_bindings_in_expr(expr, dead_locals);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                remove_copy_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            remove_copy_bindings_in_expr(condition, dead_locals);
            remove_copy_bindings(&mut then_block.stmts, dead_locals);
            if let Some(eb) = else_block {
                remove_copy_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirStmtKind::Loop { body } => {
            remove_copy_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            remove_copy_bindings(&mut block.stmts, dead_locals);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            remove_copy_bindings_in_expr(scrutinee, dead_locals);
            remove_copy_bindings(&mut then_block.stmts, dead_locals);
            if let Some(eb) = else_block {
                remove_copy_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                remove_copy_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetPattern { value, .. } => {
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirStmtKind::TaskReturn { .. } => {
            unreachable!("TaskReturn should be eliminated by synthesis before this phase")
        }
    }
}

fn remove_copy_bindings_in_expr(expr: &mut TirExpr, dead_locals: &IndexSet<u32>) {
    match &mut expr.kind {
        TirExprKind::Block(block) => {
            remove_copy_bindings(&mut block.stmts, dead_locals);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            remove_copy_bindings(&mut block.stmts, dead_locals);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            remove_copy_bindings_in_expr(condition, dead_locals);
            remove_copy_bindings(&mut then_branch.stmts, dead_locals);
            if let Some(eb) = else_branch {
                remove_copy_bindings(&mut eb.stmts, dead_locals);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            remove_copy_bindings_in_expr(left, dead_locals);
            remove_copy_bindings_in_expr(right, dead_locals);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            remove_copy_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                remove_copy_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            remove_copy_bindings_in_expr(receiver, dead_locals);
            for arg in args {
                remove_copy_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            remove_copy_bindings_in_expr(callee, dead_locals);
            for arg in args {
                remove_copy_bindings_in_expr(arg, dead_locals);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            remove_copy_bindings_in_expr(functor, dead_locals);
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. } => {
            remove_copy_bindings_in_expr(inner, dead_locals);
        }
        TirExprKind::Assign { target, value } => {
            remove_copy_bindings_in_expr(target, dead_locals);
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                remove_copy_bindings_in_expr(&mut field.value, dead_locals);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                remove_copy_bindings_in_expr(elem, dead_locals);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                remove_copy_bindings_in_expr(payload_expr, dead_locals);
            }
        }
        TirExprKind::Closure { body, .. } => {
            remove_copy_bindings_in_expr(body, dead_locals);
        }
        TirExprKind::Match { expr: inner, arms } => {
            remove_copy_bindings_in_expr(inner, dead_locals);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    remove_copy_bindings_in_expr(guard, dead_locals);
                }
                remove_copy_bindings_in_expr(&mut arm.body, dead_locals);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. } => {
            remove_copy_bindings_in_expr(expr, dead_locals);
        }
        TirExprKind::VariantPayload { expr, .. } => {
            remove_copy_bindings_in_expr(expr, dead_locals);
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            remove_copy_bindings_in_expr(scrutinee, dead_locals);
            for arm in arms {
                remove_copy_bindings(&mut arm.stmts, dead_locals);
            }
            remove_copy_bindings(&mut default.stmts, dead_locals);
        }
        // Leaf nodes
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

/// Eliminate trivial copy bindings in a function.
/// Batches non-conflicting substitutions to reduce TIR walk count from O(4K) to O(4) per iteration.
fn propagate_copies_in_function(func: &mut TirFunction, type_table: &TypeTable) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    let mut ever_changed = false;

    loop {
        // Step 1: Collect all copy bindings (single walk)
        let mut copy_bindings: Vec<CopyBinding> = Vec::new();
        collect_copy_bindings(&body.stmts, &mut copy_bindings);

        if copy_bindings.is_empty() {
            break;
        }

        // Step 2: Collect usage information (single walk)
        let usage = collect_local_usage(body);

        // Step 3: Find all eliminable bindings
        let eliminable: Vec<CopyBinding> = copy_bindings
            .into_iter()
            .filter(|b| can_propagate_copy(b, &usage, type_table))
            .collect();

        if eliminable.is_empty() {
            break;
        }

        // Step 4: Build non-conflicting batch.
        // A binding can be batched if its source local is not a target of another eliminable
        // binding. This prevents interference when two bindings form a chain
        // (e.g., `let a = 5; let x = a`).
        let target_set: IndexSet<u32> = eliminable.iter().map(|b| b.target_local).collect();

        let mut substitutions: IndexMap<u32, CopySource> = IndexMap::new();
        let mut has_deferred = false;

        for binding in eliminable {
            let source_conflicts = match &binding.source {
                CopySource::Local { index, .. } => target_set.contains(index),
                _ => false,
            };
            if source_conflicts {
                has_deferred = true;
            } else {
                substitutions.insert(binding.target_local, binding.source);
            }
        }

        if substitutions.is_empty() {
            // All bindings conflict with each other -- cannot make progress
            break;
        }

        // Step 5: Apply all substitutions in a single walk
        let dead_locals: IndexSet<u32> = substitutions.keys().copied().collect();
        substitute_in_block(body, &substitutions);

        // Step 6: Remove all dead bindings in a single walk
        remove_copy_bindings(&mut body.stmts, &dead_locals);
        ever_changed = true;

        // If no bindings were deferred, we're done
        if !has_deferred {
            break;
        }
    }

    ever_changed
}

/// Apply copy propagation to all functions in the project.
pub fn propagate_copies(project: &mut Project) -> bool {
    let mut changed = false;
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            changed |= propagate_copies_in_function(&mut func, &type_table);
        }
    }
    changed
}
