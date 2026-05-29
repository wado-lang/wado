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
//!
//! Capture aliasing: NIR materialises closure captures as `NirCapture`
//! entries on a `ClosureFunctor`, snapshotted at functor construction
//! time. Outer mutations after the snapshot don't reach the captured
//! value, so eliding the wrapper at the binding site does not change
//! what the closure observes — no closure-capture safety gate is
//! needed here.

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirStmt, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirOptVisitor, NirRefVisitor, opt_walk_expr, opt_walk_stmt};
use crate::tir::{ResolvedType, TypeId, TypeTable};

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
        let Some(body) = func.body.as_mut() else {
            continue;
        };
        let usage = analyze_usage(body, &type_table.borrow());
        let mut stripper = WrapperStripper {
            value_copy_set: &value_copy_set,
            usage: &usage,
        };
        stripper.visit_block(body);
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

// ──────────────────────────────────────────────────────────────────────────────
// Usage analysis
// ──────────────────────────────────────────────────────────────────────────────

fn analyze_usage(body: &NirBlock, type_table: &TypeTable) -> IndexMap<u32, LocalUsage> {
    let mut analyzer = UsageAnalyzer {
        usage: IndexMap::default(),
        type_table,
    };
    analyzer.visit_block(body);
    analyzer.usage
}

struct UsageAnalyzer<'a> {
    usage: IndexMap<u32, LocalUsage>,
    type_table: &'a TypeTable,
}

impl UsageAnalyzer<'_> {
    /// Mark every local that contributes to `expr`'s observable storage
    /// as potentially field-mutated, following pure projections
    /// (`FieldAccess`, `VariantPayload`, `Cast`, `Unary`). This is the
    /// analog of `copy_prop`'s `mark_potentially_mutated_local` and
    /// conservatively tracks the receiver / mut-ref-arg paths through
    /// which a callee can mutate caller state.
    fn mark_root_field_mutated(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Local { index, .. } => {
                self.usage.entry(*index).or_default().has_field_mutation = true;
            }
            NirExprKind::Unary { expr: inner, .. }
            | NirExprKind::Cast { expr: inner, .. }
            | NirExprKind::FieldAccess { expr: inner, .. }
            | NirExprKind::VariantPayload { expr: inner, .. } => {
                self.mark_root_field_mutated(inner);
            }
            NirExprKind::IntLiteral { .. }
            | NirExprKind::FloatLiteral { .. }
            | NirExprKind::BoolLiteral(_)
            | NirExprKind::CharLiteral(_)
            | NirExprKind::StringLiteral(_)
            | NirExprKind::BytesLiteral(_)
            | NirExprKind::Null
            | NirExprKind::Unit
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::GlobalVarSet { .. }
            | NirExprKind::EnumConstruct { .. }
            | NirExprKind::VariantConstruct { .. }
            | NirExprKind::VariantTag { .. }
            | NirExprKind::VariantTest { .. }
            | NirExprKind::Binary { .. }
            | NirExprKind::Assign { .. }
            | NirExprKind::Call { .. }
            | NirExprKind::CmRawCall { .. }
            | NirExprKind::MethodCall { .. }
            | NirExprKind::IndirectCall { .. }
            | NirExprKind::ClosureToCanonical { .. }
            | NirExprKind::Index { .. }
            | NirExprKind::StructLiteral { .. }
            | NirExprKind::TupleLiteral { .. }
            | NirExprKind::Block(_)
            | NirExprKind::LabeledBlock { .. }
            | NirExprKind::If { .. }
            | NirExprKind::Match { .. }
            | NirExprKind::Switch { .. } => {}
        }
    }
}

impl NirRefVisitor for UsageAnalyzer<'_> {
    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Assign { target, value } => {
                if let NirExprKind::Local { index, .. } = &target.kind {
                    self.usage.entry(*index).or_default().assign_count += 1;
                }
                if let NirExprKind::FieldAccess { expr: inner, .. } = &target.kind {
                    self.mark_root_field_mutated(inner);
                }
                self.visit_expr(target);
                self.visit_expr(value);
            }
            NirExprKind::Unary { op, expr: inner } => {
                if matches!(op, NirUnaryOp::MutRef) {
                    self.mark_root_field_mutated(inner);
                }
                self.visit_expr(inner);
            }
            NirExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut || is_mut_ref_type(arg.expr.type_id, self.type_table) {
                        self.mark_root_field_mutated(&arg.expr);
                    }
                    self.visit_expr(&arg.expr);
                }
            }
            NirExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref: the receiver expression carries `T` even for
                // `&mut self` methods, so a precise check needs the
                // callee's first-param type. Be conservative and treat
                // any local receiver as potentially field-mutated by
                // the call.
                self.mark_root_field_mutated(receiver);
                self.visit_expr(receiver);
                for arg in args {
                    if arg.is_mut || is_mut_ref_type(arg.expr.type_id, self.type_table) {
                        self.mark_root_field_mutated(&arg.expr);
                    }
                    self.visit_expr(&arg.expr);
                }
            }
            NirExprKind::IndirectCall { callee, args } => {
                self.visit_expr(callee);
                for arg in args {
                    if is_mut_ref_type(arg.type_id, self.type_table) {
                        self.mark_root_field_mutated(arg);
                    }
                    self.visit_expr(arg);
                }
            }
            NirExprKind::Local { .. }
            | NirExprKind::IntLiteral { .. }
            | NirExprKind::FloatLiteral { .. }
            | NirExprKind::BoolLiteral(_)
            | NirExprKind::CharLiteral(_)
            | NirExprKind::StringLiteral(_)
            | NirExprKind::BytesLiteral(_)
            | NirExprKind::Null
            | NirExprKind::Unit
            | NirExprKind::GlobalVarGet { .. }
            | NirExprKind::EnumConstruct { .. }
            | NirExprKind::Binary { .. }
            | NirExprKind::Cast { .. }
            | NirExprKind::FieldAccess { .. }
            | NirExprKind::Index { .. }
            | NirExprKind::Block(_)
            | NirExprKind::LabeledBlock { .. }
            | NirExprKind::If { .. }
            | NirExprKind::Match { .. }
            | NirExprKind::Switch { .. }
            | NirExprKind::StructLiteral { .. }
            | NirExprKind::TupleLiteral { .. }
            | NirExprKind::VariantConstruct { .. }
            | NirExprKind::VariantTag { .. }
            | NirExprKind::VariantTest { .. }
            | NirExprKind::VariantPayload { .. }
            | NirExprKind::GlobalVarSet { .. }
            | NirExprKind::ClosureToCanonical { .. }
            | NirExprKind::CmRawCall { .. } => {
                self.walk_expr(expr);
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Wrapper stripping
// ──────────────────────────────────────────────────────────────────────────────

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
/// Returns `None` for non-projection expressions (calls, literals,
/// struct/variant constructors): those produce fresh GC values that
/// cannot be aliased from outside, so elision is always safe regardless
/// of the surrounding state.
fn arg_source_root(expr: &NirExpr) -> Option<u32> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some(*index),
        NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::Unary { expr: inner, .. } => arg_source_root(inner),
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. }
        | NirExprKind::VariantConstruct { .. }
        | NirExprKind::VariantTag { .. }
        | NirExprKind::VariantTest { .. }
        | NirExprKind::Binary { .. }
        | NirExprKind::Assign { .. }
        | NirExprKind::Index { .. }
        | NirExprKind::Call { .. }
        | NirExprKind::CmRawCall { .. }
        | NirExprKind::MethodCall { .. }
        | NirExprKind::IndirectCall { .. }
        | NirExprKind::ClosureToCanonical { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::GlobalVarSet { .. }
        | NirExprKind::StructLiteral { .. }
        | NirExprKind::TupleLiteral { .. }
        | NirExprKind::Block(_)
        | NirExprKind::LabeledBlock { .. }
        | NirExprKind::If { .. }
        | NirExprKind::Match { .. }
        | NirExprKind::Switch { .. } => None,
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
        Some(u) => u.assign_count <= target_assign_limit && !u.has_field_mutation,
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
            Some(u) => !u.is_assigned() && !u.has_field_mutation,
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

struct WrapperStripper<'a> {
    value_copy_set: &'a IndexMap<(ModuleSource, String), TypeId>,
    usage: &'a IndexMap<u32, LocalUsage>,
}

impl NirOptVisitor for WrapperStripper<'_> {
    fn visit_stmt(&mut self, stmt: &mut NirStmt) -> bool {
        match &mut stmt.kind {
            // `let x = $value_copy$T(arg)` — Let establishes a fresh
            // binding, so any subsequent assignment to `x` invalidates
            // the snapshot; require `assign_count == 0`.
            NirStmtKind::Let {
                local_index,
                value,
                is_mut,
                skip_value_copy,
                ..
            } => {
                if !*is_mut
                    && !*skip_value_copy
                    && elision_safe(*local_index, 0, value, self.value_copy_set, self.usage)
                {
                    strip_wrapper(value);
                    return true;
                }
                // Recurse into the value so nested Let stmts (in an
                // `if`/`match`/`Block` rvalue) are still reached.
                self.visit_expr(value)
            }
            // `x = $value_copy$T(arg)` as a top-level statement — the
            // Assign *is* the binding. Allow elision when this is the
            // only assignment to `x` (`assign_count == 1`); a second
            // assignment would invalidate the snapshot just like a
            // re-bound Let.
            NirStmtKind::Expr(expr) => {
                if let NirExprKind::Assign { target, value } = &mut expr.kind
                    && let NirExprKind::Local { index, .. } = &target.kind
                    && elision_safe(*index, 1, value, self.value_copy_set, self.usage)
                {
                    strip_wrapper(value);
                    return true;
                }
                self.visit_expr(expr)
            }
            NirStmtKind::Return { .. }
            | NirStmtKind::Break { .. }
            | NirStmtKind::If { .. }
            | NirStmtKind::Loop { .. }
            | NirStmtKind::LabeledBlock { .. }
            | NirStmtKind::LetDestructure { .. }
            | NirStmtKind::Continue => opt_walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        opt_walk_expr(self, expr)
    }
}
