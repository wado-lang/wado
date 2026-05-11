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

use crate::nir_package::NirPackage;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::tir::{ResolvedType, TypeId, TypeTable};
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind, NirUnaryOp};

pub fn elide_synthesized_value_copies(project: &mut NirPackage) {
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
    let type_table = project.type_table.clone();

    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if func.is_value_copy() {
            continue;
        }
        let Some(ref mut body) = func.body else {
            continue;
        };
        let usage = analyze_usage(body, &type_table.borrow());
        strip_in_block(body, &value_copy_set, &usage);
    }
}

fn is_mut_ref_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(type_table.get(type_id), ResolvedType::MutRef(_))
}

#[derive(Debug, Default)]
struct LocalUsage {
    /// Count of `local = expr` assignments. Used by the Assign-form
    /// elision branch to recognize "binding-style" assignments (count
    /// == 1) — those behave like a `Let` and are eligible to strip.
    assign_count: u32,
    has_field_mutation: bool,
    is_captured: bool,
}

impl LocalUsage {
    /// True when the local is assigned at least once after its
    /// initialization. The Let-form elision uses this to refuse stripping
    /// a binding whose target is later overwritten (and therefore can't
    /// be safely aliased to the source).
    fn is_assigned(&self) -> bool {
        self.assign_count > 0
    }
}

fn analyze_usage(body: &NirBlock, type_table: &TypeTable) -> IndexMap<u32, LocalUsage> {
    let mut usage: IndexMap<u32, LocalUsage> = IndexMap::default();
    analyze_block(body, &mut usage, type_table);
    usage
}

fn analyze_block(block: &NirBlock, usage: &mut IndexMap<u32, LocalUsage>, type_table: &TypeTable) {
    for stmt in &block.stmts {
        analyze_stmt(stmt, usage, type_table);
    }
}

fn analyze_stmt(stmt: &NirStmt, usage: &mut IndexMap<u32, LocalUsage>, type_table: &TypeTable) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } | NirStmtKind::LetDestructure { value, .. } => {
            analyze_expr(value, usage, type_table);
        }
        NirStmtKind::Expr(expr) => analyze_expr(expr, usage, type_table),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                analyze_expr(v, usage, type_table);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            analyze_expr(condition, usage, type_table);
            analyze_block(then_block, usage, type_table);
            if let Some(eb) = else_block {
                analyze_block(eb, usage, type_table);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            analyze_block(body, usage, type_table);
        }
        NirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            analyze_expr(scrutinee, usage, type_table);
            analyze_block(then_block, usage, type_table);
            if let Some(eb) = else_block {
                analyze_block(eb, usage, type_table);
            }
        }
        NirStmtKind::Continue
        | NirStmtKind::TaskReturn { .. }
        | NirStmtKind::VariadicForOf { .. } => {}
    }
}

/// Mark every local that contributes to `expr`'s observable storage as
/// potentially field-mutated, following pure projections (`FieldAccess`,
/// `VariantPayload`, Cast, Unary). This is the analog of `copy_prop`'s
/// `mark_potentially_mutated_local` and conservatively tracks the
/// receiver / mut-ref-arg paths through which a callee can mutate
/// caller state.
fn mark_root_field_mutated(expr: &NirExpr, usage: &mut IndexMap<u32, LocalUsage>) {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            usage.entry(*index).or_default().has_field_mutation = true;
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => {
            mark_root_field_mutated(inner, usage);
        }
        _ => {}
    }
}

fn analyze_expr(expr: &NirExpr, usage: &mut IndexMap<u32, LocalUsage>, type_table: &TypeTable) {
    match &expr.kind {
        NirExprKind::Local { .. } => {}
        NirExprKind::Assign { target, value } => {
            if let NirExprKind::Local { index, .. } = &target.kind {
                usage.entry(*index).or_default().assign_count += 1;
            }
            if let NirExprKind::FieldAccess { expr: inner, .. } = &target.kind {
                mark_root_field_mutated(inner, usage);
            }
            analyze_expr(target, usage, type_table);
            analyze_expr(value, usage, type_table);
        }
        NirExprKind::Unary { op, expr: inner } => {
            if matches!(op, NirUnaryOp::MutRef) {
                mark_root_field_mutated(inner, usage);
            }
            analyze_expr(inner, usage, type_table);
        }
        NirExprKind::Binary { left, right, .. } => {
            analyze_expr(left, usage, type_table);
            analyze_expr(right, usage, type_table);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                if arg.is_mut || is_mut_ref_type(arg.expr.type_id, type_table) {
                    mark_root_field_mutated(&arg.expr, usage);
                }
                analyze_expr(&arg.expr, usage, type_table);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                analyze_expr(arg, usage, type_table);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            // Auto-ref: the receiver expression carries `T` even for
            // `&mut self` methods, so a precise check needs the callee's
            // first-param type. Be conservative and treat any local
            // receiver as potentially field-mutated by the call.
            mark_root_field_mutated(receiver, usage);
            analyze_expr(receiver, usage, type_table);
            for arg in args {
                if arg.is_mut || is_mut_ref_type(arg.expr.type_id, type_table) {
                    mark_root_field_mutated(&arg.expr, usage);
                }
                analyze_expr(&arg.expr, usage, type_table);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            analyze_expr(callee, usage, type_table);
            for arg in args {
                if is_mut_ref_type(arg.type_id, type_table) {
                    mark_root_field_mutated(arg, usage);
                }
                analyze_expr(arg, usage, type_table);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            analyze_expr(functor, usage, type_table);
        }
        NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => analyze_expr(inner, usage, type_table),
        NirExprKind::Index {
            expr: inner, index, ..
        } => {
            analyze_expr(inner, usage, type_table);
            analyze_expr(index, usage, type_table);
        }
        NirExprKind::Cast { expr: inner, .. } => analyze_expr(inner, usage, type_table),
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            analyze_block(block, usage, type_table);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_expr(condition, usage, type_table);
            analyze_block(then_branch, usage, type_table);
            if let Some(eb) = else_branch {
                analyze_block(eb, usage, type_table);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_expr(&field.value, usage, type_table);
            }
        }
        NirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                analyze_expr(elem, usage, type_table);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                analyze_expr(p, usage, type_table);
            }
        }
        NirExprKind::Closure { body, captures, .. } => {
            for capture in captures {
                usage.entry(capture.outer_index).or_default().is_captured = true;
            }
            analyze_expr(body, usage, type_table);
        }
        NirExprKind::Match { expr: inner, arms } => {
            analyze_expr(inner, usage, type_table);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    analyze_expr(guard, usage, type_table);
                }
                analyze_expr(&arm.body, usage, type_table);
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => analyze_expr(value, usage, type_table),
        NirExprKind::VariantTag { expr }
        | NirExprKind::VariantTest { expr, .. }
        | NirExprKind::VariantPayload { expr, .. } => analyze_expr(expr, usage, type_table),
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            analyze_expr(scrutinee, usage, type_table);
            for arm in arms {
                analyze_block(arm, usage, type_table);
            }
            analyze_block(default, usage, type_table);
        }
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
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

fn is_value_copy_call(
    expr: &NirExpr,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
) -> bool {
    if let NirExprKind::Call { func, args, .. } = &expr.kind
        && args.len() == 1
    {
        value_copy_set.contains_key(&(func.module_source.clone(), func.name.clone()))
    } else {
        false
    }
}

/// Find the root local that `arg` reads from, descending through pure
/// projections that share storage with their inner expression — field
/// access, variant payload extraction, casts, and unary ops that simply
/// alias (`Deref`, `Ref`, `MutRef`). Eliding the wrapper makes the
/// binding alias whatever this root local references, so the safety
/// check must verify that the root is not mutated between binding and
/// use.
///
/// Returns `Some(EXTERNAL_SOURCE)` when the root is something we cannot
/// inspect locally (e.g., a global or a function parameter passed by
/// reference at the language level). Use `external_source_unsafe` to
/// gate elision in that case.
///
/// Returns `None` only for genuinely fresh expressions (calls,
/// literals, struct/variant constructors) — those produce new GC
/// values that cannot be aliased from outside, so elision is always
/// safe regardless of the surrounding state.
fn arg_source_root(expr: &NirExpr) -> Option<u32> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some(*index),
        NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::Unary { expr: inner, .. } => arg_source_root(inner),
        _ => None,
    }
}

fn strip_in_block(
    block: &mut NirBlock,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) {
    for stmt in &mut block.stmts {
        strip_in_stmt(stmt, value_copy_set, usage);
    }
}

/// Check whether `value` is a `$value_copy$T(arg)` call whose wrapper
/// can be safely stripped given the binding target's local index and
/// the function-wide usage map.
fn elision_safe(
    target_index: u32,
    target_assign_limit: u32,
    value: &NirExpr,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) -> bool {
    if !is_value_copy_call(value, value_copy_set) {
        return false;
    }
    let target_ok = match usage.get(&target_index) {
        Some(u) => u.assign_count <= target_assign_limit && !u.has_field_mutation && !u.is_captured,
        None => true,
    };
    if !target_ok {
        return false;
    }
    let NirExprKind::Call { args, .. } = &value.kind else {
        return false;
    };
    let Some(arg) = args.first() else {
        return false;
    };
    match arg_source_root(&arg.expr) {
        Some(root) => match usage.get(&root) {
            Some(u) => !u.is_assigned() && !u.has_field_mutation && !u.is_captured,
            None => true,
        },
        None => true,
    }
}

/// Replace `value` (a `$value_copy$T(arg)` call) with `arg` in place,
/// extracting the call's single argument and dropping the wrapper.
fn strip_wrapper(value: &mut NirExpr) {
    if let NirExprKind::Call { args, .. } = &mut value.kind
        && let Some(arg) = args.first_mut()
    {
        let span = value.span;
        let mut taken = NirExpr::new(NirExprKind::Unit, value.type_id, span);
        std::mem::swap(&mut arg.expr, &mut taken);
        taken.span = span;
        *value = taken;
    }
}

fn strip_in_stmt(
    stmt: &mut NirStmt,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) {
    match &mut stmt.kind {
        // `let x = $value_copy$T(arg)` — Let establishes a fresh binding,
        // so any subsequent assignment to `x` invalidates the snapshot;
        // require `assign_count == 0`.
        NirStmtKind::Let {
            local_index,
            value,
            is_mut,
            skip_value_copy,
            ..
        } => {
            if !*is_mut
                && !*skip_value_copy
                && elision_safe(*local_index, 0, value, value_copy_set, usage)
            {
                strip_wrapper(value);
            } else {
                // Even when this Let itself isn't a value-copy wrapper,
                // its value expression may contain nested blocks (an
                // `if`/`match`/`Block` rvalue) that hold Let stmts whose
                // wrappers are still candidates. Walk into the value
                // expression so those nested Lets are reached.
                strip_in_expr(value, value_copy_set, usage);
            }
        }
        NirStmtKind::LetDestructure { value, .. } => {
            strip_in_expr(value, value_copy_set, usage);
        }
        // `x = $value_copy$T(arg)` as a top-level statement — the Assign
        // *is* the binding. Allow elision when this is the only
        // assignment to `x` (`assign_count == 1`); a second assignment
        // would invalidate the snapshot just like a re-bound Let.
        NirStmtKind::Expr(expr) => {
            if let NirExprKind::Assign { target, value } = &mut expr.kind
                && let NirExprKind::Local { index, .. } = &target.kind
                && elision_safe(*index, 1, value, value_copy_set, usage)
            {
                strip_wrapper(value);
            } else {
                strip_in_expr(expr, value_copy_set, usage);
            }
        }
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                strip_in_expr(v, value_copy_set, usage);
            }
        }
        _ => {}
    }
    match &mut stmt.kind {
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            strip_in_expr(condition, value_copy_set, usage);
            strip_in_block(then_block, value_copy_set, usage);
            if let Some(eb) = else_block {
                strip_in_block(eb, value_copy_set, usage);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            strip_in_block(body, value_copy_set, usage);
        }
        NirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            strip_in_expr(scrutinee, value_copy_set, usage);
            strip_in_block(then_block, value_copy_set, usage);
            if let Some(eb) = else_block {
                strip_in_block(eb, value_copy_set, usage);
            }
        }
        _ => {}
    }
}

/// Walk a `NirExpr` tree looking for nested blocks whose statements may
/// be Let / Assign / Return / Break / If-stmt forms holding strippable
/// `$value_copy$T(...)` wrappers. The traversal mirrors `analyze_expr`
/// — visit every child that can syntactically embed a `NirBlock`. The
/// rewrite stays purely structural; safety is still gated by the
/// per-Let `elision_safe` checks inside `strip_in_stmt` /
/// `strip_in_block`.
fn strip_in_expr(
    expr: &mut NirExpr,
    value_copy_set: &IndexMap<(ModuleSource, String), TypeId>,
    usage: &IndexMap<u32, LocalUsage>,
) {
    match &mut expr.kind {
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            strip_in_block(block, value_copy_set, usage);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            strip_in_expr(condition, value_copy_set, usage);
            strip_in_block(then_branch, value_copy_set, usage);
            if let Some(eb) = else_branch {
                strip_in_block(eb, value_copy_set, usage);
            }
        }
        NirExprKind::Match { expr: scrut, arms } => {
            strip_in_expr(scrut, value_copy_set, usage);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    strip_in_expr(guard, value_copy_set, usage);
                }
                strip_in_expr(&mut arm.body, value_copy_set, usage);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            strip_in_expr(scrutinee, value_copy_set, usage);
            for arm in arms {
                strip_in_block(arm, value_copy_set, usage);
            }
            strip_in_block(default, value_copy_set, usage);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                strip_in_expr(&mut arg.expr, value_copy_set, usage);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            strip_in_expr(receiver, value_copy_set, usage);
            for arg in args {
                strip_in_expr(&mut arg.expr, value_copy_set, usage);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                strip_in_expr(arg, value_copy_set, usage);
            }
        }
        NirExprKind::IndirectCall { callee, args, .. } => {
            strip_in_expr(callee, value_copy_set, usage);
            for arg in args {
                strip_in_expr(arg, value_copy_set, usage);
            }
        }
        NirExprKind::Binary { left, right, .. } => {
            strip_in_expr(left, value_copy_set, usage);
            strip_in_expr(right, value_copy_set, usage);
        }
        NirExprKind::Assign { target, value } => {
            strip_in_expr(target, value_copy_set, usage);
            strip_in_expr(value, value_copy_set, usage);
        }
        NirExprKind::Index { expr: inner, index } => {
            strip_in_expr(inner, value_copy_set, usage);
            strip_in_expr(index, value_copy_set, usage);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::TupleSpread { expr: inner }
        | NirExprKind::TupleZip { expr: inner }
        | NirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. }
        | NirExprKind::ClosureToCanonical { functor: inner, .. } => {
            strip_in_expr(inner, value_copy_set, usage);
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                strip_in_expr(&mut field.value, value_copy_set, usage);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                strip_in_expr(elem, value_copy_set, usage);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                strip_in_expr(p, value_copy_set, usage);
            }
        }
        NirExprKind::Closure { body, .. } => {
            strip_in_expr(body, value_copy_set, usage);
        }
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::FuncRef { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::Capture { .. }
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
