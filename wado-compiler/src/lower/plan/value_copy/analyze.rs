//! Read-only seed walker for the fold's value-copy decision.
//!
//! The fold (`lower::translate`) emits a `$value_copy$T(...)` wrap
//! directly at each wrap site, using the shared predicates exported
//! here ([`should_wrap`], [`is_fresh_value`], [`is_source_immutable`]).
//! [`collect_seed_types`] walks every function with the same
//! predicates to feed [`super::synthesize::synthesize_helpers`].

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Every `TypeId` the fold will wrap in `$value_copy$T(...)`, plus
/// element types of `array_clone::<T>(...)` calls that codegen
/// routes through the same helper.
pub fn collect_seed_types(project: &FlatPackage) -> IndexSet<TypeId> {
    let type_table = project.type_table.borrow();
    let mut walker = SeedWalker {
        type_table: &type_table,
        out: IndexSet::default(),
        immutable_locals: IndexSet::default(),
    };
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        walker.immutable_locals = func
            .locals
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_mut)
            .map(|(i, _)| u32::try_from(i).unwrap())
            .collect();
        if let Some(ref body) = func.body {
            walker.visit_block(body);
        }
    }
    walker.out
}

struct SeedWalker<'a> {
    type_table: &'a TypeTable,
    out: IndexSet<TypeId>,
    immutable_locals: IndexSet<u32>,
}

impl SeedWalker<'_> {
    fn record_if_wrap(&mut self, expr: &TirExpr) {
        if should_wrap(expr, self.type_table) {
            self.out.insert(expr.type_id);
        }
    }

    fn record_array_clone_element(&mut self, expr: &TirExpr) {
        if let Some(t) = array_clone_element_type_arg(expr)
            && super::needs_value_copy(t, self.type_table)
        {
            self.out.insert(t);
        }
    }
}

impl TirRefVisitor for SeedWalker<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
                value,
                is_mut,
                skip_value_copy,
                ..
            } => {
                if !*skip_value_copy
                    && (*is_mut || !is_source_immutable(value, &self.immutable_locals))
                {
                    self.record_if_wrap(value);
                }
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.record_if_wrap(value);
            }
            _ => {}
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        self.record_array_clone_element(expr);
        match &expr.kind {
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args {
                    if arg.is_mut {
                        self.record_if_wrap(&arg.expr);
                    }
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
                for arg in args {
                    self.record_if_wrap(arg);
                }
            }
            TirExprKind::Assign { target, value } => {
                // Field / index writes mutate an existing slot, no
                // defensive copy needed.
                if matches!(&target.kind, TirExprKind::Local { .. }) {
                    self.record_if_wrap(value);
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// Shape predicate shared with the fold. Site-specific gating
/// (e.g. `skip_value_copy`, `is_source_immutable` for `Let`, the
/// `Local`-target check for `Assign`) is the caller's job.
pub fn should_wrap(expr: &TirExpr, type_table: &TypeTable) -> bool {
    super::needs_value_copy(expr.type_id, type_table)
        && !is_copy_value_call(expr)
        && !is_fresh_value(expr, type_table)
}

/// Avoid re-wrapping the `copy_value::<NestedT>(...)` markers
/// `synthesize_helpers` plants inside helper bodies.
fn is_copy_value_call(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::Call { func, .. }
            if func.module_source.is_core_builtin() && func.name == "copy_value"
    )
}

/// A fresh expression does not alias existing data, so no
/// defensive copy is needed.
pub fn is_fresh_value(expr: &TirExpr, type_table: &TypeTable) -> bool {
    is_fresh_in_context(expr, &IndexSet::default(), type_table)
}

fn is_fresh_in_context(
    expr: &TirExpr,
    fresh_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    match &expr.kind {
        TirExprKind::StringLiteral(_)
        | TirExprKind::StructLiteral { .. }
        | TirExprKind::TupleLiteral { .. }
        | TirExprKind::TupleSpread { .. }
        | TirExprKind::TupleZip { .. }
        | TirExprKind::TupleLen { .. }
        | TirExprKind::TypePackExpansion { .. }
        | TirExprKind::Null => true,
        TirExprKind::Call { .. }
        | TirExprKind::MethodCall { .. }
        | TirExprKind::CmRawCall { .. }
        | TirExprKind::IndirectCall { .. } => true,
        TirExprKind::VariantConstruct { .. } | TirExprKind::EnumConstruct { .. } => true,
        TirExprKind::Local { index, .. } => fresh_locals.contains(index),
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => is_fresh_in_context(inner, fresh_locals, type_table),
        TirExprKind::LabeledBlock { label, block, .. } => {
            block_breaks_are_fresh(label, block, fresh_locals, type_table)
        }
        TirExprKind::Match { expr: scrut, arms } => {
            match_result_is_fresh(scrut, arms, fresh_locals, type_table)
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            is_fresh_in_context(inner, fresh_locals, type_table)
        }
        _ => false,
    }
}

/// A `match` yields a fresh value when every value-producing arm yields a
/// fresh value. Divergent arms (`Never`-typed body: `=> return …`, `=> panic()`)
/// contribute no value and are skipped. When the scrutinee is itself fresh, an
/// arm's pattern bindings destructure unaliased data, so they are fresh too —
/// this is what makes `let x = f()?` (which desugars to
/// `match f() { Ok(v) => v, Err(e) => return Err(e) }`) copy-free when `f()`
/// returns a fresh value.
fn match_result_is_fresh(
    scrut: &TirExpr,
    arms: &[TirMatchArm],
    fresh_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    let scrut_fresh = is_fresh_in_context(scrut, fresh_locals, type_table);
    let mut saw_value_arm = false;
    for arm in arms {
        if type_table.is_never(arm.body.type_id) {
            continue;
        }
        saw_value_arm = true;
        let mut arm_fresh = fresh_locals.clone();
        if scrut_fresh {
            collect_pattern_bindings(&arm.pattern, &mut arm_fresh);
        }
        if !is_fresh_in_context(&arm.body, &arm_fresh, type_table) {
            return false;
        }
    }
    saw_value_arm
}

/// Collect every local a pattern binds, so a fresh scrutinee's destructured
/// parts can be treated as fresh in the arm body.
fn collect_pattern_bindings(pattern: &TirPattern, out: &mut IndexSet<u32>) {
    match pattern {
        TirPattern::Binding { local_index, .. } => {
            out.insert(*local_index);
        }
        TirPattern::Tuple(subs, _) | TirPattern::Variant { bindings: subs, .. } => {
            for sub in subs {
                collect_pattern_bindings(sub, out);
            }
        }
        TirPattern::Struct { fields, .. } => {
            for field in fields {
                collect_pattern_bindings(&field.pattern, out);
            }
        }
        TirPattern::Or(alts) => {
            for alt in alts {
                collect_pattern_bindings(alt, out);
            }
        }
        TirPattern::Wildcard
        | TirPattern::Literal(_)
        | TirPattern::Enum { .. }
        | TirPattern::ConstantValue { .. }
        | TirPattern::Range { .. } => {}
    }
}

fn block_breaks_are_fresh(
    label: &str,
    block: &TirBlock,
    parent_fresh: &IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    let mut found = false;
    let mut fresh_locals = parent_fresh.clone();
    if scan_block_for_breaks(label, block, &mut found, &mut fresh_locals, type_table) {
        found
    } else {
        false
    }
}

fn scan_block_for_breaks(
    label: &str,
    block: &TirBlock,
    found: &mut bool,
    fresh_locals: &mut IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    for stmt in &block.stmts {
        if !scan_stmt_for_breaks(label, stmt, found, fresh_locals, type_table) {
            return false;
        }
    }
    true
}

fn scan_stmt_for_breaks(
    label: &str,
    stmt: &TirStmt,
    found: &mut bool,
    fresh_locals: &mut IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if is_fresh_in_context(value, fresh_locals, type_table) {
                fresh_locals.insert(*local_index);
            }
            true
        }
        TirStmtKind::Break {
            label: Some(l),
            value: Some(v),
        } if l == label => {
            *found = true;
            is_fresh_in_context(v, fresh_locals, type_table)
        }
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            if !scan_block_for_breaks(label, then_block, found, fresh_locals, type_table) {
                return false;
            }
            if let Some(eb) = else_block
                && !scan_block_for_breaks(label, eb, found, fresh_locals, type_table)
            {
                return false;
            }
            true
        }
        TirStmtKind::Loop { body } => {
            scan_block_for_breaks(label, body, found, fresh_locals, type_table)
        }
        TirStmtKind::Expr(expr) => {
            scan_expr_for_breaks(label, expr, found, fresh_locals, type_table)
        }
        _ => true,
    }
}

fn scan_expr_for_breaks(
    label: &str,
    expr: &TirExpr,
    found: &mut bool,
    fresh_locals: &mut IndexSet<u32>,
    type_table: &TypeTable,
) -> bool {
    match &expr.kind {
        TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
            scan_block_for_breaks(label, block, found, fresh_locals, type_table)
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            if !scan_block_for_breaks(label, then_branch, found, fresh_locals, type_table) {
                return false;
            }
            if let Some(eb) = else_branch
                && !scan_block_for_breaks(label, eb, found, fresh_locals, type_table)
            {
                return false;
            }
            true
        }
        _ => true,
    }
}

/// An immutable destination binding can alias an immutable-rooted
/// source without a defensive copy. Mirrors
/// `wir_build::value_copy::is_source_immutable`.
pub fn is_source_immutable(expr: &TirExpr, immutable_locals: &IndexSet<u32>) -> bool {
    match &expr.kind {
        TirExprKind::Local { index, .. } => immutable_locals.contains(index),
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => is_source_immutable(inner, immutable_locals),
        _ => false,
    }
}

fn array_clone_element_type_arg(expr: &TirExpr) -> Option<TypeId> {
    if let TirExprKind::Call { func, .. } = &expr.kind
        && func.module_source.is_core_builtin()
        && func.name == "array_clone"
    {
        func.monomorph_info
            .as_ref()
            .and_then(|mi| mi.impl_type_args.first().copied())
    } else {
        None
    }
}
