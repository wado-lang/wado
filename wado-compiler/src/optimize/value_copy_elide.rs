//! Strip the `$value_copy$T<id>` wrapper from `let x = $value_copy$T(arg)`
//! bindings whose target is observably read-only — the resulting `let x = arg`
//! aliases `arg`'s data exactly when the synthesized helper would have
//! returned a fresh struct, but the alias is unobservable because nothing
//! mutates `x` or the source root that `arg` reads from.
//!
//! Runs once after `synthesize_value_copy_funcs`, recovering the freshness
//! elision that the former WIR-level `value_copy` instruction performed
//! inline. Helpers whose remaining call sites are all elided are removed by
//! the post-elision DCE pass.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::name::ModuleSource;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirUnaryOp, TypeId};

pub fn elide_synthesized_value_copies(project: &mut FlatPackage) {
    let value_copy_set: IndexMap<(ModuleSource, String), TypeId> = project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            f.value_copy_type()
                .map(|t| ((f.module_source.clone(), f.name.clone()), t))
        })
        .collect();
    if value_copy_set.is_empty() {
        return;
    }

    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if func.is_value_copy() {
            continue;
        }
        let Some(ref mut body) = func.body else {
            continue;
        };
        let usage = analyze_usage(body);
        strip_in_block(body, &value_copy_set, &usage);
    }
}

#[derive(Debug, Default)]
struct LocalUsage {
    is_assigned: bool,
    has_field_mutation: bool,
    is_captured: bool,
}

fn analyze_usage(body: &TirBlock) -> IndexMap<u32, LocalUsage> {
    let mut usage: IndexMap<u32, LocalUsage> = IndexMap::default();
    analyze_block(body, &mut usage);
    usage
}

fn analyze_block(block: &TirBlock, usage: &mut IndexMap<u32, LocalUsage>) {
    for stmt in &block.stmts {
        analyze_stmt(stmt, usage);
    }
}

fn analyze_stmt(stmt: &TirStmt, usage: &mut IndexMap<u32, LocalUsage>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
            analyze_expr(value, usage);
        }
        TirStmtKind::Expr(expr) => analyze_expr(expr, usage),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                analyze_expr(v, usage);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            analyze_expr(condition, usage);
            analyze_block(then_block, usage);
            if let Some(eb) = else_block {
                analyze_block(eb, usage);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            analyze_block(body, usage);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            analyze_expr(scrutinee, usage);
            analyze_block(then_block, usage);
            if let Some(eb) = else_block {
                analyze_block(eb, usage);
            }
        }
        TirStmtKind::Continue
        | TirStmtKind::TaskReturn { .. }
        | TirStmtKind::VariadicForOf { .. } => {}
    }
}

fn analyze_expr(expr: &TirExpr, usage: &mut IndexMap<u32, LocalUsage>) {
    match &expr.kind {
        TirExprKind::Local { .. } => {}
        TirExprKind::Assign { target, value } => {
            if let TirExprKind::Local { index, .. } = &target.kind {
                usage.entry(*index).or_default().is_assigned = true;
            }
            if let TirExprKind::FieldAccess { expr: inner, .. } = &target.kind
                && let TirExprKind::Local { index, .. } = &inner.kind
            {
                usage.entry(*index).or_default().has_field_mutation = true;
            }
            analyze_expr(target, usage);
            analyze_expr(value, usage);
        }
        TirExprKind::Unary { op, expr: inner } => {
            if matches!(op, TirUnaryOp::MutRef)
                && let TirExprKind::Local { index, .. } = &inner.kind
            {
                usage.entry(*index).or_default().has_field_mutation = true;
            }
            analyze_expr(inner, usage);
        }
        TirExprKind::Binary { left, right, .. } => {
            analyze_expr(left, usage);
            analyze_expr(right, usage);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                if arg.is_mut
                    && let TirExprKind::Local { index, .. } = &arg.expr.kind
                {
                    usage.entry(*index).or_default().has_field_mutation = true;
                }
                analyze_expr(&arg.expr, usage);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                analyze_expr(arg, usage);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            analyze_expr(receiver, usage);
            for arg in args {
                if arg.is_mut
                    && let TirExprKind::Local { index, .. } = &arg.expr.kind
                {
                    usage.entry(*index).or_default().has_field_mutation = true;
                }
                analyze_expr(&arg.expr, usage);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            analyze_expr(callee, usage);
            for arg in args {
                analyze_expr(arg, usage);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => analyze_expr(functor, usage),
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => analyze_expr(inner, usage),
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            analyze_expr(inner, usage);
            analyze_expr(index, usage);
        }
        TirExprKind::Cast { expr: inner, .. } => analyze_expr(inner, usage),
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            analyze_block(block, usage);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_expr(condition, usage);
            analyze_block(then_branch, usage);
            if let Some(eb) = else_branch {
                analyze_block(eb, usage);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_expr(&field.value, usage);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                analyze_expr(elem, usage);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                analyze_expr(p, usage);
            }
        }
        TirExprKind::Closure { body, captures, .. } => {
            for capture in captures {
                usage.entry(capture.outer_index).or_default().is_captured = true;
            }
            analyze_expr(body, usage);
        }
        TirExprKind::Match { expr: inner, arms } => {
            analyze_expr(inner, usage);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    analyze_expr(guard, usage);
                }
                analyze_expr(&arm.body, usage);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => analyze_expr(value, usage),
        TirExprKind::VariantTag { expr }
        | TirExprKind::VariantTest { expr, .. }
        | TirExprKind::VariantPayload { expr, .. } => analyze_expr(expr, usage),
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            analyze_expr(scrutinee, usage);
            for arm in arms {
                analyze_block(arm, usage);
            }
            analyze_block(default, usage);
        }
        _ => {}
    }
}

fn is_value_copy_call(
    expr: &TirExpr,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
) -> bool {
    if let TirExprKind::Call { func, args, .. } = &expr.kind
        && args.len() == 1
    {
        value_copy_set.contains_key(&(func.module_source.clone(), func.name.clone()))
    } else {
        false
    }
}

/// Find the root local that `arg` reads from, descending through pure
/// projections (`FieldAccess`, `VariantPayload`, `Index` on a local).
/// Returns `None` for fresh expressions (calls, literals, struct/variant
/// constructors) — those produce new GC values that cannot be aliased
/// from outside, so elision is safe regardless of the surrounding state.
fn arg_source_root(expr: &TirExpr) -> Option<u32> {
    match &expr.kind {
        TirExprKind::Local { index, .. } => Some(*index),
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. } => arg_source_root(inner),
        _ => None,
    }
}

fn strip_in_block(
    block: &mut TirBlock,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) {
    for stmt in &mut block.stmts {
        strip_in_stmt(stmt, value_copy_set, usage);
    }
}

fn strip_in_stmt(
    stmt: &mut TirStmt,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) {
    if let TirStmtKind::Let {
        local_index,
        value,
        is_mut,
        skip_value_copy,
        ..
    } = &mut stmt.kind
        && !*is_mut
        && !*skip_value_copy
        && is_value_copy_call(value, value_copy_set)
    {
        let target_ok = match usage.get(local_index) {
            Some(u) => !u.is_assigned && !u.has_field_mutation && !u.is_captured,
            None => true,
        };
        let arg_ok = if let TirExprKind::Call { args, .. } = &value.kind
            && let Some(arg) = args.first()
        {
            match arg_source_root(&arg.expr) {
                Some(root) => match usage.get(&root) {
                    Some(u) => !u.is_assigned && !u.has_field_mutation && !u.is_captured,
                    None => true,
                },
                None => true,
            }
        } else {
            false
        };
        if target_ok
            && arg_ok
            && let TirExprKind::Call { args, .. } = &mut value.kind
            && let Some(arg) = args.first_mut()
        {
            let span = value.span;
            let mut taken = TirExpr::new(TirExprKind::Unit, value.type_id, span);
            std::mem::swap(&mut arg.expr, &mut taken);
            taken.span = span;
            *value = taken;
        }
    }
    match &mut stmt.kind {
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            strip_in_block(then_block, value_copy_set, usage);
            if let Some(eb) = else_block {
                strip_in_block(eb, value_copy_set, usage);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            strip_in_block(body, value_copy_set, usage);
        }
        TirStmtKind::IfLet {
            then_block,
            else_block,
            ..
        } => {
            strip_in_block(then_block, value_copy_set, usage);
            if let Some(eb) = else_block {
                strip_in_block(eb, value_copy_set, usage);
            }
        }
        _ => {}
    }
}
