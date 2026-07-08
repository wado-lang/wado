//! Read-only seed walker for the fold's value-copy decision.
//!
//! The fold (`lower::translate`) emits a `$value_copy$T(...)` wrap
//! directly at each wrap site, using the shared predicates exported
//! here ([`should_wrap`], [`is_fresh_value`], [`is_source_immutable`]).
//! [`collect_seed_types`] walks every function with the same
//! predicates to feed [`super::synthesize::synthesize_helpers`].

use super::ownership::OwnedCalls;
use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::name::FunctionId;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirMatchArm, TirPattern, TirStmt, TirStmtKind, TirUnaryOp,
    TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Every `TypeId` the fold will wrap in `$value_copy$T(...)`, plus
/// element types of `array_clone::<T>(...)` calls that codegen
/// routes through the same helper.
///
/// Runs before the return-convention fixpoint, so it uses a conservative
/// oracle (only builtins are owned). That over-collects seed types — a helper
/// the precise fold never calls is dead-code-eliminated — but never misses one.
pub fn collect_seed_types(project: &FlatPackage) -> IndexSet<TypeId> {
    let type_table = project.type_table.borrow();
    let no_owned: IndexSet<FunctionId> = IndexSet::default();
    let no_self_proj: IndexSet<FunctionId> = IndexSet::default();
    let oracle = OwnedCalls::new(&no_owned, &no_self_proj);
    let mut walker = SeedWalker {
        type_table: &type_table,
        oracle: &oracle,
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
    oracle: &'a OwnedCalls<'a>,
    out: IndexSet<TypeId>,
    immutable_locals: IndexSet<u32>,
}

impl SeedWalker<'_> {
    fn record_if_wrap(&mut self, expr: &TirExpr) {
        if should_wrap(expr, self.type_table, self.oracle) {
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
                // Every by-value argument is copied — value semantics: passing
                // a value to a function deep-copies it. `should_wrap` already
                // excludes references (`&T` / `&mut T`), fresh values, and
                // non-copy types, so a `&mut` arg is not copied.
                for arg in args {
                    self.record_if_wrap(&arg.expr);
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
                for arg in args {
                    self.record_if_wrap(arg);
                }
            }
            TirExprKind::Assign { target, value } => {
                // A whole-local rebind (`x = v`) and a whole-value deref-assign
                // (`*ref = v`, lowered by `try_expand_deref_aggregate_assign`)
                // both replace the value, so the RHS needs a defensive copy.
                // Field / index writes mutate an existing slot in place and
                // don't.
                let replaces_whole_value = matches!(
                    &target.kind,
                    TirExprKind::Local { .. }
                        | TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            ..
                        }
                );
                if replaces_whole_value {
                    self.record_if_wrap(value);
                }
            }
            // An aggregate literal stores each element / field by value, so a
            // non-fresh aggregate element is deep-copied into the fresh literal.
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.record_if_wrap(&field.value);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for element in elements {
                    self.record_if_wrap(element);
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
pub fn should_wrap(expr: &TirExpr, type_table: &TypeTable, oracle: &OwnedCalls) -> bool {
    super::needs_value_copy(expr.type_id, type_table)
        && !is_copy_value_call(expr)
        && !is_fresh_value(expr, oracle, type_table)
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

/// A fresh (owned) expression does not alias existing data, so no defensive
/// copy is needed. `oracle` decides a call's return convention interprocedurally.
pub fn is_fresh_value(expr: &TirExpr, oracle: &OwnedCalls, type_table: &TypeTable) -> bool {
    is_owned_value(expr, &IndexSet::default(), oracle, type_table)
}

/// Whether `expr` produces an *owned* value in the context of the owned locals
/// in `fresh_locals` — a value that aliases nothing the caller can still reach,
/// so consuming it into an owner is a move. A call is owned iff its callee
/// returns owned (`oracle`), the caller-side, single-phase replacement for the
/// old interprocedural escape recovery: the accessor `index_value(self: &List,
/// i) -> T { return self.repr[i] }` returns a borrowed projection of `&self`
/// (wado-lang/wado#1527), so it is *not* owned and its result is copied at a
/// materialization — but never at a mutable-place use, which is not a
/// materialization, so `arr[i].field.push(x)` keeps its element aliased.
pub(crate) fn is_owned_value(
    expr: &TirExpr,
    fresh_locals: &IndexSet<u32>,
    oracle: &OwnedCalls,
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
        // A call is owned iff its callee returns an owned value. A core builtin
        // allocates or computes a fresh result — except `array_get`, the element
        // read that aliases its container — handled inside `oracle.is_owned`. A
        // raw CM call lifts a fresh value across the ABI boundary. A callee that
        // instead returns a projection of its receiver / first argument
        // (`build(&self) -> List { return *self }`) yields a fresh value exactly
        // when that receiver is itself fresh, so `[1, 2, 3]`'s builder — a fresh
        // block-local finalized by `.build()` — is not defensively copied.
        TirExprKind::Call { func, args, .. } => {
            oracle.is_owned(func)
                || (oracle.returns_self_projection(func)
                    && args
                        .first()
                        .is_some_and(|a| is_owned_value(&a.expr, fresh_locals, oracle, type_table)))
        }
        TirExprKind::MethodCall { func, receiver, .. } => {
            oracle.is_owned(func)
                || (oracle.returns_self_projection(func)
                    && is_owned_value(receiver, fresh_locals, oracle, type_table))
        }
        TirExprKind::CmRawCall { .. } => true,
        TirExprKind::VariantConstruct { .. } | TirExprKind::EnumConstruct { .. } => true,
        TirExprKind::Local { index, .. } => fresh_locals.contains(index),
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => is_owned_value(inner, fresh_locals, oracle, type_table),
        TirExprKind::LabeledBlock { label, block, .. } => {
            block_breaks_are_fresh(label, block, fresh_locals, oracle, type_table)
        }
        TirExprKind::Match { expr: scrut, arms } => {
            match_result_is_fresh(scrut, arms, fresh_locals, oracle, type_table)
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            is_owned_value(inner, fresh_locals, oracle, type_table)
        }
        _ => false,
    }
}

/// A `match` yields an owned value when every value-producing arm yields one.
/// Divergent arms (`Never`-typed body: `=> return …`, `=> panic()`) contribute
/// no value and are skipped. When the scrutinee is owned, an arm's pattern
/// bindings destructure unaliased data, so they are owned too — this is what
/// makes `let x = f()?` (which desugars to `match f() { Ok(v) => v, Err(e) =>
/// return Err(e) }`) copy-free when `f()` returns an owned value.
fn match_result_is_fresh(
    scrut: &TirExpr,
    arms: &[TirMatchArm],
    fresh_locals: &IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    let scrut_fresh = is_owned_value(scrut, fresh_locals, oracle, type_table);
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
        if !is_owned_value(&arm.body, &arm_fresh, oracle, type_table) {
            return false;
        }
    }
    saw_value_arm
}

/// Collect every local a pattern binds, so a fresh scrutinee's destructured
/// parts can be treated as fresh in the arm body.
pub(crate) fn collect_pattern_bindings(pattern: &TirPattern, out: &mut IndexSet<u32>) {
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
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    let mut found = false;
    let mut fresh_locals = parent_fresh.clone();
    if scan_block_for_breaks(
        label,
        block,
        &mut found,
        &mut fresh_locals,
        oracle,
        type_table,
    ) {
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
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    for stmt in &block.stmts {
        if !scan_stmt_for_breaks(label, stmt, found, fresh_locals, oracle, type_table) {
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
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if is_owned_value(value, fresh_locals, oracle, type_table) {
                fresh_locals.insert(*local_index);
            }
            true
        }
        TirStmtKind::Break {
            label: Some(l),
            value: Some(v),
        } if l == label => {
            *found = true;
            is_owned_value(v, fresh_locals, oracle, type_table)
        }
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            if !scan_block_for_breaks(label, then_block, found, fresh_locals, oracle, type_table) {
                return false;
            }
            if let Some(eb) = else_block
                && !scan_block_for_breaks(label, eb, found, fresh_locals, oracle, type_table)
            {
                return false;
            }
            true
        }
        TirStmtKind::Loop { body } => {
            scan_block_for_breaks(label, body, found, fresh_locals, oracle, type_table)
        }
        TirStmtKind::Expr(expr) => {
            scan_expr_for_breaks(label, expr, found, fresh_locals, oracle, type_table)
        }
        _ => true,
    }
}

fn scan_expr_for_breaks(
    label: &str,
    expr: &TirExpr,
    found: &mut bool,
    fresh_locals: &mut IndexSet<u32>,
    oracle: &OwnedCalls,
    type_table: &TypeTable,
) -> bool {
    match &expr.kind {
        TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
            scan_block_for_breaks(label, block, found, fresh_locals, oracle, type_table)
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            if !scan_block_for_breaks(label, then_branch, found, fresh_locals, oracle, type_table) {
                return false;
            }
            if let Some(eb) = else_branch
                && !scan_block_for_breaks(label, eb, found, fresh_locals, oracle, type_table)
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
