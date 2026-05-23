//! Read-only analysis pass that mirrors the fold's value-copy wrap decision.
//!
//! Before Phase A of WEP 2026-05-11 Step 5, `insert.rs` mutated user-program
//! TIR by wrapping every wrap-site expression in `builtin::copy_value::<T>(x)`.
//! The translator then rewrote those markers into `$value_copy$T(x)` calls.
//!
//! After Phase A, the translator (TIR → NIR fold) emits the `$value_copy$T`
//! call directly at wrap sites; no TIR markers are inserted. This module
//! provides two things:
//!
//! 1. The shape predicates ([`should_wrap`], [`is_fresh_value`],
//!    [`is_source_immutable`]) that the fold consults at every wrap site.
//! 2. [`collect_seed_types`] — a read-only walker that mirrors the fold's
//!    wrap-site decisions across all function bodies, returning every
//!    `TypeId` the fold would wrap. The set seeds
//!    [`super::synthesize::synthesize_helpers`] so the helpers are registered
//!    in `FlatPackage::functions` before the translator runs.
//!
//! Synthesized helper bodies still contain `builtin::copy_value::<NestedT>(x)`
//! markers (the synthesizer emits them for nested value-typed fields); the
//! translator's existing `convert_call` arm rewrites those markers uniformly.
//! The marker path is only used by synthesized helpers; user-program TIR
//! never carries markers after Phase A.

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::TirRefVisitor;

/// Collect every `TypeId` for which the fold will emit a `$value_copy$T(...)`
/// wrap call, plus every value-semantic element type referenced by an
/// `array_clone::<T>(...)` call. The returned set seeds
/// [`super::synthesize::synthesize_helpers`].
pub fn collect_seed_types(project: &FlatPackage) -> IndexSet<TypeId> {
    let type_table = project.type_table.clone();
    let mut walker = SeedWalker {
        type_table,
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

struct SeedWalker {
    type_table: Rc<RefCell<TypeTable>>,
    out: IndexSet<TypeId>,
    immutable_locals: IndexSet<u32>,
}

impl SeedWalker {
    fn record_if_wrap(&mut self, expr: &TirExpr) {
        if should_wrap(expr, &self.type_table.borrow()) {
            self.out.insert(expr.type_id);
        }
    }

    fn record_array_clone_element(&mut self, expr: &TirExpr) {
        if let Some(t) = array_clone_element_type_arg(expr) {
            let tt = self.type_table.borrow();
            if super::needs_value_copy(t, &tt) {
                self.out.insert(t);
            }
        }
    }
}

impl TirRefVisitor for SeedWalker {
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
                if let TirExprKind::Local { name, .. } = &target.kind
                    && !name.starts_with("__sroa_")
                {
                    self.record_if_wrap(value);
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// Predicate shared with the fold. Returns true iff a wrap call should be
/// emitted around `expr` when it appears at a wrap site (`Let` value,
/// `LetDestructure` value, mut `Call` / `MethodCall` arg, any
/// `IndirectCall` arg, or `Assign` value whose target is a non-SROA
/// `Local`).
///
/// Wrap-site gating that depends on context (`skip_value_copy`,
/// `is_source_immutable` for `Let`, the Local/non-SROA target check for
/// `Assign`) is the caller's responsibility — see [`is_source_immutable`].
pub fn should_wrap(expr: &TirExpr, type_table: &TypeTable) -> bool {
    super::needs_value_copy(expr.type_id, type_table)
        && !is_copy_value_call(expr)
        && !is_fresh_value(expr)
}

/// True when `expr` is already a `builtin::copy_value` call. The fold never
/// produces this shape in user code (it emits `$value_copy$T` directly), but
/// synthesized helper bodies contain `copy_value::<NestedT>(...)` markers, so
/// the predicate exists to skip re-wrapping when the helper body itself is
/// processed by the fold.
pub fn is_copy_value_call(expr: &TirExpr) -> bool {
    matches!(
        &expr.kind,
        TirExprKind::Call { func, .. }
            if func.module_source.is_core_builtin() && func.name == "copy_value"
    )
}

/// Mirror of `wir_build::value_copy::is_fresh_value`: a fresh expression
/// does not alias existing data and therefore does not need a defensive
/// copy. Same shape predicate `insert.rs` used pre-Phase A.
pub fn is_fresh_value(expr: &TirExpr) -> bool {
    is_fresh_in_context(expr, &IndexSet::default())
}

fn is_fresh_in_context(expr: &TirExpr, fresh_locals: &IndexSet<u32>) -> bool {
    match &expr.kind {
        TirExprKind::StringLiteral(_)
        | TirExprKind::StructLiteral { .. }
        | TirExprKind::TupleLiteral { .. }
        | TirExprKind::TupleSpread { .. }
        | TirExprKind::TupleZip { .. }
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
        } => is_fresh_in_context(inner, fresh_locals),
        TirExprKind::LabeledBlock { label, block, .. } => {
            block_breaks_are_fresh(label, block, fresh_locals)
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            is_fresh_in_context(inner, fresh_locals)
        }
        _ => false,
    }
}

fn block_breaks_are_fresh(label: &str, block: &TirBlock, parent_fresh: &IndexSet<u32>) -> bool {
    let mut found = false;
    let mut fresh_locals = parent_fresh.clone();
    if scan_block_for_breaks(label, block, &mut found, &mut fresh_locals) {
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
) -> bool {
    for stmt in &block.stmts {
        if !scan_stmt_for_breaks(label, stmt, found, fresh_locals) {
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
) -> bool {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if is_fresh_in_context(value, fresh_locals) {
                fresh_locals.insert(*local_index);
            }
            true
        }
        TirStmtKind::Break {
            label: Some(l),
            value: Some(v),
        } if l == label => {
            *found = true;
            is_fresh_in_context(v, fresh_locals)
        }
        TirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            if !scan_block_for_breaks(label, then_block, found, fresh_locals) {
                return false;
            }
            if let Some(eb) = else_block
                && !scan_block_for_breaks(label, eb, found, fresh_locals)
            {
                return false;
            }
            true
        }
        TirStmtKind::Loop { body } => scan_block_for_breaks(label, body, found, fresh_locals),
        TirStmtKind::Expr(expr) => scan_expr_for_breaks(label, expr, found, fresh_locals),
        _ => true,
    }
}

fn scan_expr_for_breaks(
    label: &str,
    expr: &TirExpr,
    found: &mut bool,
    fresh_locals: &mut IndexSet<u32>,
) -> bool {
    match &expr.kind {
        TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
            scan_block_for_breaks(label, block, found, fresh_locals)
        }
        TirExprKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            if !scan_block_for_breaks(label, then_branch, found, fresh_locals) {
                return false;
            }
            if let Some(eb) = else_branch
                && !scan_block_for_breaks(label, eb, found, fresh_locals)
            {
                return false;
            }
            true
        }
        _ => true,
    }
}

/// Mirror of `wir_build::value_copy::is_source_immutable`: when the source
/// expression is rooted at a local known to be immutable, an immutable
/// destination binding can safely alias it without a copy.
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
