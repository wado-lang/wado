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
use std::collections::{HashMap, HashSet};

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
    /// Locals assigned within the containing labeled block (empty if not in a labeled block).
    /// Used to check if source is modified within the block scope.
    block_local_assigned: HashSet<u32>,
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
        block_local_assigned: HashSet::new(),
    })
}

/// Collect usage information for all locals in a function body.
fn collect_local_usage(body: &TirBlock) -> HashMap<u32, LocalUsage> {
    let mut usage: HashMap<u32, LocalUsage> = HashMap::new();
    collect_usage_in_block(body, &mut usage, false);
    usage
}

/// Collect which locals are assigned within a block (non-recursively for labeled blocks).
/// This is used to check if a source variable is modified within a labeled block scope.
fn collect_assigned_in_block(block: &TirBlock) -> HashSet<u32> {
    let mut assigned: HashSet<u32> = HashSet::new();
    collect_assigned_in_stmts(&block.stmts, &mut assigned);
    assigned
}

fn collect_assigned_in_stmts(stmts: &[TirStmt], assigned: &mut HashSet<u32>) {
    for stmt in stmts {
        collect_assigned_in_stmt(stmt, assigned);
    }
}

fn collect_assigned_in_stmt(stmt: &TirStmt, assigned: &mut HashSet<u32>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_assigned_in_expr(value, assigned);
        }
        TirStmtKind::Expr(expr) => {
            collect_assigned_in_expr(expr, assigned);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_assigned_in_expr(v, assigned);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_assigned_in_expr(condition, assigned);
            collect_assigned_in_stmts(&then_block.stmts, assigned);
            if let Some(eb) = else_block {
                collect_assigned_in_stmts(&eb.stmts, assigned);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_assigned_in_expr(condition, assigned);
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                collect_assigned_in_stmt(s, assigned);
            }
            if let Some(c) = condition {
                collect_assigned_in_expr(c, assigned);
            }
            if let Some(u) = update {
                collect_assigned_in_expr(u, assigned);
            }
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            collect_assigned_in_expr(iterable, assigned);
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::Loop { body } => {
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            collect_assigned_in_stmts(&block.stmts, assigned);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_assigned_in_expr(scrutinee, assigned);
            collect_assigned_in_stmts(&then_block.stmts, assigned);
            if let Some(eb) = else_block {
                collect_assigned_in_stmts(&eb.stmts, assigned);
            }
        }
        TirStmtKind::WhilePattern {
            scrutinee, body, ..
        } => {
            collect_assigned_in_expr(scrutinee, assigned);
            collect_assigned_in_stmts(&body.stmts, assigned);
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            body,
            update,
            ..
        } => {
            for s in init {
                collect_assigned_in_stmt(s, assigned);
            }
            collect_assigned_in_expr(scrutinee, assigned);
            collect_assigned_in_stmts(&body.stmts, assigned);
            if let Some(u) = update {
                collect_assigned_in_expr(u, assigned);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_assigned_in_expr(v, assigned);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn collect_assigned_in_expr(expr: &TirExpr, assigned: &mut HashSet<u32>) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            if let TirExprKind::Local { index, .. } = &target.kind {
                assigned.insert(*index);
            }
            collect_assigned_in_expr(target, assigned);
            collect_assigned_in_expr(value, assigned);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_assigned_in_expr(left, assigned);
            collect_assigned_in_expr(right, assigned);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_assigned_in_expr(inner, assigned);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_assigned_in_expr(arg, assigned);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_assigned_in_expr(receiver, assigned);
            for arg in args {
                collect_assigned_in_expr(arg, assigned);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_assigned_in_expr(callee, assigned);
            for arg in args {
                collect_assigned_in_expr(arg, assigned);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
            collect_assigned_in_expr(inner, assigned);
        }
        TirExprKind::Move { value } => {
            collect_assigned_in_expr(value, assigned);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_assigned_in_expr(inner, assigned);
            collect_assigned_in_expr(index, assigned);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_assigned_in_stmts(&block.stmts, assigned);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_assigned_in_expr(condition, assigned);
            collect_assigned_in_stmts(&then_branch.stmts, assigned);
            if let Some(eb) = else_branch {
                collect_assigned_in_stmts(&eb.stmts, assigned);
            }
        }
        TirExprKind::Match { expr, arms } => {
            collect_assigned_in_expr(expr, assigned);
            for arm in arms {
                collect_assigned_in_expr(&arm.body, assigned);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_assigned_in_expr(&field.value, assigned);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_assigned_in_expr(elem, assigned);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_assigned_in_expr(value, assigned);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_assigned_in_expr(field, assigned);
            }
        }
        TirExprKind::Closure { body, .. } => {
            collect_assigned_in_expr(body, assigned);
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_assigned_in_expr(value, assigned);
        }
        // Terminals - no nested expressions
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
}

fn collect_usage_in_block(block: &TirBlock, usage: &mut HashMap<u32, LocalUsage>, in_loop: bool) {
    for stmt in &block.stmts {
        collect_usage_in_stmt(stmt, usage, in_loop);
    }
}

fn collect_usage_in_stmt(stmt: &TirStmt, usage: &mut HashMap<u32, LocalUsage>, in_loop: bool) {
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
        TirStmtKind::While { condition, body } => {
            // Mark uses in condition as in_loop_condition
            collect_usage_in_expr(condition, usage, true, true);
            collect_usage_in_block(body, usage, true);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                collect_usage_in_stmt(s, usage, true);
            }
            if let Some(c) = condition {
                collect_usage_in_expr(c, usage, true, true);
            }
            if let Some(u) = update {
                collect_usage_in_expr(u, usage, true, false);
            }
            collect_usage_in_block(body, usage, true);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            collect_usage_in_expr(iterable, usage, in_loop, false);
            collect_usage_in_block(body, usage, true);
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
        TirStmtKind::WhilePattern {
            scrutinee, body, ..
        } => {
            collect_usage_in_expr(scrutinee, usage, true, true);
            collect_usage_in_block(body, usage, true);
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            body,
            update,
            ..
        } => {
            for s in init {
                collect_usage_in_stmt(s, usage, true);
            }
            collect_usage_in_expr(scrutinee, usage, true, true);
            collect_usage_in_block(body, usage, true);
            if let Some(u) = update {
                collect_usage_in_expr(u, usage, true, false);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_usage_in_expr(v, usage, in_loop, false);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn collect_usage_in_expr(
    expr: &TirExpr,
    usage: &mut HashMap<u32, LocalUsage>,
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
        | TirExprKind::EffectCall { args, .. } => {
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
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                collect_usage_in_expr(elem, usage, in_loop, in_condition);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_usage_in_expr(elem, usage, in_loop, in_condition);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_usage_in_expr(value, usage, in_loop, in_condition);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_usage_in_expr(field, usage, in_loop, in_condition);
            }
        }
        TirExprKind::Move { value } => {
            collect_usage_in_expr(value, usage, in_loop, in_condition);
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
                collect_usage_in_expr(&arm.body, usage, in_loop, in_condition);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_usage_in_expr(value, usage, in_loop, in_condition);
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
        ResolvedType::Option(inner) => needs_value_copy(*inner, type_table),
        // References, primitives, etc. don't need copying
        _ => false,
    }
}

/// Check if a binding can be safely eliminated via copy propagation.
fn can_propagate_copy(
    binding: &CopyBinding,
    usage: &HashMap<u32, LocalUsage>,
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
                // Check if source is assigned
                if su.is_assigned {
                    // Source is assigned somewhere in the function.
                    // But if this binding is inside a labeled block and source is NOT
                    // assigned within that block, it's safe to propagate because
                    // source can't be modified between the binding and use within the block.
                    if binding.block_local_assigned.contains(index) {
                        // Source is assigned within the same labeled block - not safe
                        return false;
                    }
                    // Source is assigned elsewhere but not in this block scope - safe
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
/// Replaces uses of `from_local` with the expression from `source`.
fn substitute_in_block(block: &mut TirBlock, from_local: u32, source: &CopySource) {
    for stmt in &mut block.stmts {
        substitute_in_stmt(stmt, from_local, source);
    }
}

fn substitute_in_stmt(stmt: &mut TirStmt, from_local: u32, source: &CopySource) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => {
            substitute_in_expr(value, from_local, source);
        }
        TirStmtKind::Expr(expr) => {
            substitute_in_expr(expr, from_local, source);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                substitute_in_expr(v, from_local, source);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            substitute_in_expr(condition, from_local, source);
            substitute_in_block(then_block, from_local, source);
            if let Some(eb) = else_block {
                substitute_in_block(eb, from_local, source);
            }
        }
        TirStmtKind::While { condition, body } => {
            substitute_in_expr(condition, from_local, source);
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                substitute_in_stmt(s, from_local, source);
            }
            if let Some(c) = condition {
                substitute_in_expr(c, from_local, source);
            }
            if let Some(u) = update {
                substitute_in_expr(u, from_local, source);
            }
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            substitute_in_expr(iterable, from_local, source);
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::Loop { body } => {
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            substitute_in_block(block, from_local, source);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            substitute_in_expr(scrutinee, from_local, source);
            substitute_in_block(then_block, from_local, source);
            if let Some(eb) = else_block {
                substitute_in_block(eb, from_local, source);
            }
        }
        TirStmtKind::WhilePattern {
            scrutinee, body, ..
        } => {
            substitute_in_expr(scrutinee, from_local, source);
            substitute_in_block(body, from_local, source);
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            body,
            update,
            ..
        } => {
            for s in init {
                substitute_in_stmt(s, from_local, source);
            }
            substitute_in_expr(scrutinee, from_local, source);
            substitute_in_block(body, from_local, source);
            if let Some(u) = update {
                substitute_in_expr(u, from_local, source);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                substitute_in_expr(v, from_local, source);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn substitute_in_expr(expr: &mut TirExpr, from_local: u32, source: &CopySource) {
    // Check if this is a local that needs substitution
    if let TirExprKind::Local { index, .. } = &expr.kind
        && *index == from_local
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
            substitute_in_expr(left, from_local, source);
            substitute_in_expr(right, from_local, source);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            substitute_in_expr(inner, from_local, source);
        }
        TirExprKind::Assign { target, value } => {
            substitute_in_expr(target, from_local, source);
            substitute_in_expr(value, from_local, source);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                substitute_in_expr(arg, from_local, source);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            substitute_in_expr(receiver, from_local, source);
            for arg in args {
                substitute_in_expr(arg, from_local, source);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            substitute_in_expr(callee, from_local, source);
            for arg in args {
                substitute_in_expr(arg, from_local, source);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. } => {
            substitute_in_expr(inner, from_local, source);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            substitute_in_expr(inner, from_local, source);
            substitute_in_expr(index, from_local, source);
        }
        TirExprKind::Cast { expr: inner, .. } => {
            substitute_in_expr(inner, from_local, source);
        }
        TirExprKind::Block(block) => {
            substitute_in_block(block, from_local, source);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            substitute_in_block(block, from_local, source);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            substitute_in_expr(condition, from_local, source);
            substitute_in_block(then_branch, from_local, source);
            if let Some(eb) = else_branch {
                substitute_in_block(eb, from_local, source);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                substitute_in_expr(&mut field.value, from_local, source);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } => {
            for elem in elements {
                substitute_in_expr(elem, from_local, source);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                substitute_in_expr(elem, from_local, source);
            }
        }
        TirExprKind::OptionSome { value } => {
            substitute_in_expr(value, from_local, source);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                substitute_in_expr(field, from_local, source);
            }
        }
        TirExprKind::Move { value } => {
            substitute_in_expr(value, from_local, source);
        }
        TirExprKind::Closure { body, .. } => {
            substitute_in_expr(body, from_local, source);
        }
        TirExprKind::Match { expr: inner, arms } => {
            substitute_in_expr(inner, from_local, source);
            for arm in arms {
                substitute_in_expr(&mut arm.body, from_local, source);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            substitute_in_expr(value, from_local, source);
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
/// `block_local_assigned` contains locals assigned within the current labeled block scope.
fn collect_copy_bindings(
    stmts: &[TirStmt],
    bindings: &mut Vec<CopyBinding>,
    block_local_assigned: &HashSet<u32>,
) {
    for stmt in stmts {
        if let Some(mut binding) = analyze_copy_binding(stmt) {
            binding.block_local_assigned = block_local_assigned.clone();
            bindings.push(binding);
        }
        collect_copy_bindings_in_stmt(stmt, bindings, block_local_assigned);
    }
}

fn collect_copy_bindings_in_stmt(
    stmt: &TirStmt,
    bindings: &mut Vec<CopyBinding>,
    block_local_assigned: &HashSet<u32>,
) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => {
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
        }
        TirStmtKind::Expr(expr) => {
            collect_copy_bindings_in_expr(expr, bindings, block_local_assigned);
        }
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                collect_copy_bindings_in_expr(v, bindings, block_local_assigned);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_copy_bindings_in_expr(condition, bindings, block_local_assigned);
            collect_copy_bindings(&then_block.stmts, bindings, block_local_assigned);
            if let Some(eb) = else_block {
                collect_copy_bindings(&eb.stmts, bindings, block_local_assigned);
            }
        }
        TirStmtKind::While { condition, body } => {
            collect_copy_bindings_in_expr(condition, bindings, block_local_assigned);
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                collect_copy_bindings_in_stmt(s, bindings, block_local_assigned);
            }
            if let Some(c) = condition {
                collect_copy_bindings_in_expr(c, bindings, block_local_assigned);
            }
            if let Some(u) = update {
                collect_copy_bindings_in_expr(u, bindings, block_local_assigned);
            }
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            collect_copy_bindings_in_expr(iterable, bindings, block_local_assigned);
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::Loop { body } => {
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::LabeledBlock { block, .. } => {
            // For labeled blocks, compute which locals are assigned within
            let local_assigned = collect_assigned_in_block(block);
            collect_copy_bindings(&block.stmts, bindings, &local_assigned);
        }
        TirStmtKind::IfPattern {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_copy_bindings_in_expr(scrutinee, bindings, block_local_assigned);
            collect_copy_bindings(&then_block.stmts, bindings, block_local_assigned);
            if let Some(eb) = else_block {
                collect_copy_bindings(&eb.stmts, bindings, block_local_assigned);
            }
        }
        TirStmtKind::WhilePattern {
            scrutinee, body, ..
        } => {
            collect_copy_bindings_in_expr(scrutinee, bindings, block_local_assigned);
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            body,
            update,
            ..
        } => {
            for s in init {
                collect_copy_bindings_in_stmt(s, bindings, block_local_assigned);
            }
            collect_copy_bindings_in_expr(scrutinee, bindings, block_local_assigned);
            collect_copy_bindings(&body.stmts, bindings, block_local_assigned);
            if let Some(u) = update {
                collect_copy_bindings_in_expr(u, bindings, block_local_assigned);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_copy_bindings_in_expr(v, bindings, block_local_assigned);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn collect_copy_bindings_in_expr(
    expr: &TirExpr,
    bindings: &mut Vec<CopyBinding>,
    block_local_assigned: &HashSet<u32>,
) {
    match &expr.kind {
        TirExprKind::Block(block) => {
            collect_copy_bindings(&block.stmts, bindings, block_local_assigned);
        }
        TirExprKind::LabeledBlock { block, .. } => {
            // For labeled block expressions, compute which locals are assigned within
            let local_assigned = collect_assigned_in_block(block);
            collect_copy_bindings(&block.stmts, bindings, &local_assigned);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_copy_bindings_in_expr(condition, bindings, block_local_assigned);
            collect_copy_bindings(&then_branch.stmts, bindings, block_local_assigned);
            if let Some(eb) = else_branch {
                collect_copy_bindings(&eb.stmts, bindings, block_local_assigned);
            }
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_copy_bindings_in_expr(left, bindings, block_local_assigned);
            collect_copy_bindings_in_expr(right, bindings, block_local_assigned);
        }
        TirExprKind::Unary { expr: inner, .. } => {
            collect_copy_bindings_in_expr(inner, bindings, block_local_assigned);
        }
        TirExprKind::Call { args, .. }
        | TirExprKind::StaticCall { args, .. }
        | TirExprKind::EffectCall { args, .. } => {
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings, block_local_assigned);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            collect_copy_bindings_in_expr(receiver, bindings, block_local_assigned);
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings, block_local_assigned);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_copy_bindings_in_expr(callee, bindings, block_local_assigned);
            for arg in args {
                collect_copy_bindings_in_expr(arg, bindings, block_local_assigned);
            }
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Index { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. } => {
            collect_copy_bindings_in_expr(inner, bindings, block_local_assigned);
        }
        TirExprKind::Assign { target, value } => {
            collect_copy_bindings_in_expr(target, bindings, block_local_assigned);
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                collect_copy_bindings_in_expr(&field.value, bindings, block_local_assigned);
            }
        }
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                collect_copy_bindings_in_expr(elem, bindings, block_local_assigned);
            }
        }
        TirExprKind::OptionSome { value } => {
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                collect_copy_bindings_in_expr(field, bindings, block_local_assigned);
            }
        }
        TirExprKind::Move { value } => {
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
        }
        TirExprKind::Closure { body, .. } => {
            collect_copy_bindings_in_expr(body, bindings, block_local_assigned);
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_copy_bindings_in_expr(inner, bindings, block_local_assigned);
            for arm in arms {
                collect_copy_bindings_in_expr(&arm.body, bindings, block_local_assigned);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            collect_copy_bindings_in_expr(value, bindings, block_local_assigned);
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
fn remove_copy_bindings(stmts: &mut Vec<TirStmt>, dead_locals: &HashSet<u32>) {
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

fn remove_copy_bindings_in_stmt(stmt: &mut TirStmt, dead_locals: &HashSet<u32>) {
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
        TirStmtKind::While { condition, body } => {
            remove_copy_bindings_in_expr(condition, dead_locals);
            remove_copy_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::For {
            init,
            condition,
            update,
            body,
        } => {
            for s in init {
                remove_copy_bindings_in_stmt(s, dead_locals);
            }
            if let Some(c) = condition {
                remove_copy_bindings_in_expr(c, dead_locals);
            }
            if let Some(u) = update {
                remove_copy_bindings_in_expr(u, dead_locals);
            }
            remove_copy_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::ForOf { iterable, body, .. } => {
            remove_copy_bindings_in_expr(iterable, dead_locals);
            remove_copy_bindings(&mut body.stmts, dead_locals);
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
        TirStmtKind::WhilePattern {
            scrutinee, body, ..
        } => {
            remove_copy_bindings_in_expr(scrutinee, dead_locals);
            remove_copy_bindings(&mut body.stmts, dead_locals);
        }
        TirStmtKind::ForPattern {
            init,
            scrutinee,
            body,
            update,
            ..
        } => {
            for s in init {
                remove_copy_bindings_in_stmt(s, dead_locals);
            }
            remove_copy_bindings_in_expr(scrutinee, dead_locals);
            remove_copy_bindings(&mut body.stmts, dead_locals);
            if let Some(u) = update {
                remove_copy_bindings_in_expr(u, dead_locals);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                remove_copy_bindings_in_expr(v, dead_locals);
            }
        }
        TirStmtKind::Continue => {}
    }
}

fn remove_copy_bindings_in_expr(expr: &mut TirExpr, dead_locals: &HashSet<u32>) {
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
        | TirExprKind::EffectCall { args, .. } => {
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
        TirExprKind::ArrayLiteral { elements, .. } | TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                remove_copy_bindings_in_expr(elem, dead_locals);
            }
        }
        TirExprKind::OptionSome { value } => {
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::VariantConstruct { fields, .. } => {
            for field in fields {
                remove_copy_bindings_in_expr(field, dead_locals);
            }
        }
        TirExprKind::Move { value } => {
            remove_copy_bindings_in_expr(value, dead_locals);
        }
        TirExprKind::Closure { body, .. } => {
            remove_copy_bindings_in_expr(body, dead_locals);
        }
        TirExprKind::Match { expr: inner, arms } => {
            remove_copy_bindings_in_expr(inner, dead_locals);
            for arm in arms {
                remove_copy_bindings_in_expr(&mut arm.body, dead_locals);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => {
            remove_copy_bindings_in_expr(value, dead_locals);
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
fn propagate_copies_in_function(func: &mut TirFunction, type_table: &TypeTable) {
    let Some(body) = &mut func.body else {
        return;
    };

    // Iterate until no more changes
    // We process ONE binding per iteration to avoid interference between substitutions
    // (e.g., if `let a = 5; let x = a;`, substituting both at once would break references)
    loop {
        // Collect all copy bindings
        let mut copy_bindings: Vec<CopyBinding> = Vec::new();
        // Start with empty set - bindings inside labeled blocks will get their own set
        collect_copy_bindings(&body.stmts, &mut copy_bindings, &HashSet::new());

        if copy_bindings.is_empty() {
            break;
        }

        // Collect usage information
        let usage = collect_local_usage(body);

        // Find FIRST binding that can be eliminated (one at a time for safety)
        let mut to_eliminate: Option<CopyBinding> = None;
        for binding in copy_bindings {
            if can_propagate_copy(&binding, &usage, type_table) {
                to_eliminate = Some(binding);
                break;
            }
        }

        let Some(binding) = to_eliminate else {
            break;
        };

        // Apply substitution for this one binding
        substitute_in_block(body, binding.target_local, &binding.source);

        // Remove the dead binding
        let dead_locals: HashSet<u32> = [binding.target_local].into_iter().collect();
        remove_copy_bindings(&mut body.stmts, &dead_locals);
    }
}

/// Apply copy propagation to all functions in the project.
pub fn propagate_copies(project: &mut Project) {
    for module in project.tir_modules.values_mut() {
        let type_table = module.type_table.borrow();
        for func_rc in &module.functions {
            let mut func = func_rc.borrow_mut();
            propagate_copies_in_function(&mut func, &type_table);
        }
    }
}
