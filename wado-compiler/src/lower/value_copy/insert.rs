//! Insert `builtin::copy_value::<T>(x)` wrappers at every TIR position where
//! Wado value semantics require a defensive deep-copy.
//!
//! Runs post-monomorphize so concrete types are available when the wrapper is
//! constructed. The insertion is unconditional; the TIR optimizer is
//! responsible for eliding wrappers whose argument is provably fresh or
//! otherwise safe to share. This keeps the insertion rule purely semantic and
//! separates it from optimization concerns — `wir_build` no longer needs
//! freshness analysis because the decision is already materialized in TIR.

use std::cell::RefCell;
use std::rc::Rc;

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::name::ModuleSource;
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr, opt_walk_stmt};

/// Run the value-copy insertion pass on every function body, including CM
/// binding adapters — they still carry Wado value semantics on the guest
/// side of the ABI boundary and need the same defensive copies as ordinary
/// functions.
pub fn insert_value_copy_calls(project: &mut FlatPackage) {
    let type_table = project.type_table.clone();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        // The inserter consults `is_mut` per local index to decide whether a
        // binding can safely alias an immutable source; reading from
        // `func.locals` directly avoids re-walking the body to rediscover
        // mutability information that the resolver / synthesis already
        // recorded.
        let immutable_locals: IndexSet<u32> = func
            .locals
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_mut)
            .map(|(i, _)| u32::try_from(i).unwrap())
            .collect();
        let mut visitor = ValueCopyInserter {
            type_table: type_table.clone(),
            immutable_locals,
        };
        if let Some(ref mut body) = func.body {
            visitor.visit_block(body);
        }
    }
}

struct ValueCopyInserter {
    type_table: Rc<RefCell<TypeTable>>,
    immutable_locals: IndexSet<u32>,
}

impl ValueCopyInserter {
    fn needs_value_copy(&self, type_id: TypeId) -> bool {
        super::needs_value_copy(type_id, &self.type_table.borrow())
    }

    /// Return true when `expr` is already a `builtin::copy_value` call, so the
    /// visitor does not nest wrappers on its own output.
    fn is_copy_value_call(expr: &TirExpr) -> bool {
        matches!(
            &expr.kind,
            TirExprKind::Call { func, .. }
                if func.module_source.is_core_builtin() && func.name == "copy_value"
        )
    }

    /// Wrap `expr` in `builtin::copy_value::<T>(expr)` when `T` is
    /// value-semantic and the expression is not already wrapped.
    fn wrap_if_needed(&self, expr: TirExpr) -> TirExpr {
        let type_id = expr.type_id;
        if !self.needs_value_copy(type_id) || Self::is_copy_value_call(&expr) {
            return expr;
        }
        let span = expr.span;
        let func = FunctionRef {
            module_source: ModuleSource::builtin(),
            name: "copy_value".to_string(),
            monomorph_info: Some(MonomorphInfo {
                generic_name: "copy_value".to_string(),
                impl_type_args: vec![type_id],
                method_type_args: vec![],
                is_blanket: false,
            }),
            method_info: None,
        };
        TirExpr::new(
            TirExprKind::Call {
                func,
                type_args: vec![type_id],
                args: vec![CallArg::new(expr, false)],
            },
            type_id,
            span,
        )
    }

    /// Replace `slot` with `wrap_if_needed(slot)` in-place.
    fn wrap_slot(&self, slot: &mut TirExpr) {
        let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, slot.span);
        let owned = std::mem::replace(slot, placeholder);
        *slot = self.wrap_if_needed(owned);
    }

    /// Mirror `wir_build::value_copy::is_fresh_value` at the TIR level: a
    /// fresh expression does not alias existing data and therefore does not
    /// need a defensive copy. The TIR optimizer may later relax this in
    /// favour of a more thorough freshness pass.
    fn is_fresh_value(expr: &TirExpr) -> bool {
        Self::is_fresh_in_context(expr, &IndexSet::default())
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
            TirExprKind::ClosureToCanonical { .. } => true,
            TirExprKind::VariantConstruct { .. } | TirExprKind::EnumConstruct { .. } => true,
            TirExprKind::Local { index, .. } => fresh_locals.contains(index),
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => Self::is_fresh_in_context(inner, fresh_locals),
            TirExprKind::LabeledBlock { label, block, .. } => {
                Self::block_breaks_are_fresh(label, block, fresh_locals)
            }
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::VariantPayload { expr: inner, .. } => {
                Self::is_fresh_in_context(inner, fresh_locals)
            }
            _ => false,
        }
    }

    fn block_breaks_are_fresh(label: &str, block: &TirBlock, parent_fresh: &IndexSet<u32>) -> bool {
        let mut found = false;
        let mut fresh_locals = parent_fresh.clone();
        if Self::scan_block_for_breaks(label, block, &mut found, &mut fresh_locals) {
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
            if !Self::scan_stmt_for_breaks(label, stmt, found, fresh_locals) {
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
                if Self::is_fresh_in_context(value, fresh_locals) {
                    fresh_locals.insert(*local_index);
                }
                true
            }
            TirStmtKind::Break {
                label: Some(l),
                value: Some(v),
            } if l == label => {
                *found = true;
                Self::is_fresh_in_context(v, fresh_locals)
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                if !Self::scan_block_for_breaks(label, then_block, found, fresh_locals) {
                    return false;
                }
                if let Some(eb) = else_block
                    && !Self::scan_block_for_breaks(label, eb, found, fresh_locals)
                {
                    return false;
                }
                true
            }
            TirStmtKind::Loop { body } => {
                Self::scan_block_for_breaks(label, body, found, fresh_locals)
            }
            TirStmtKind::Expr(expr) => Self::scan_expr_for_breaks(label, expr, found, fresh_locals),
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
                Self::scan_block_for_breaks(label, block, found, fresh_locals)
            }
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                if !Self::scan_block_for_breaks(label, then_branch, found, fresh_locals) {
                    return false;
                }
                if let Some(eb) = else_branch
                    && !Self::scan_block_for_breaks(label, eb, found, fresh_locals)
                {
                    return false;
                }
                true
            }
            _ => true,
        }
    }

    /// Mirror `wir_build::value_copy::is_source_immutable`: when the source
    /// expression is rooted at a local known to be immutable, the destination
    /// binding can safely alias it without a copy.
    fn is_source_immutable(&self, expr: &TirExpr) -> bool {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.immutable_locals.contains(index),
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => self.is_source_immutable(inner),
            _ => false,
        }
    }

    /// Wrap `slot` only if the expression semantically requires a copy — i.e.
    /// it is a value-semantic type and is not provably safe to share.
    fn wrap_slot_if_needed(&self, slot: &mut TirExpr) {
        if !self.needs_value_copy(slot.type_id)
            || Self::is_copy_value_call(slot)
            || Self::is_fresh_value(slot)
        {
            return;
        }
        self.wrap_slot(slot);
    }
}

impl TirOptVisitor for ValueCopyInserter {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        // Recurse first so nested Calls/Lets get their own wrappers before we
        // consider wrapping the outer Let's RHS.
        opt_walk_stmt(self, stmt);

        match &mut stmt.kind {
            TirStmtKind::Let {
                value,
                is_mut,
                skip_value_copy,
                ..
            } => {
                // `skip_value_copy` is set by LICM/SROA/template hoisting to
                // indicate the binding is safe to alias. An immutable binding
                // from an immutable source is also safe.
                if *skip_value_copy || (!*is_mut && self.is_source_immutable(value)) {
                    return false;
                }
                self.wrap_slot_if_needed(value);
                true
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.wrap_slot_if_needed(value);
                true
            }
            _ => false,
        }
    }

    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        opt_walk_expr(self, expr);

        match &mut expr.kind {
            TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
                for arg in args.iter_mut() {
                    if arg.is_mut {
                        self.wrap_slot_if_needed(&mut arg.expr);
                    }
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
                // Indirect call args receive an unconditional copy because
                // the callee signature is not known at this point.
                for arg in args.iter_mut() {
                    self.wrap_slot_if_needed(arg);
                }
            }
            TirExprKind::Assign { target, value } => {
                // Only Local targets receive a defensive copy. StructSet /
                // ArraySet / IndexAssign on an existing container stash the
                // reference as-is — matching the old WIR-side rule that no
                // wrapper was emitted for field/index writes. SROA-renamed
                // locals (`__sroa_*`) are alias-preserving and also skip.
                if let TirExprKind::Local { name, .. } = &target.kind
                    && !name.starts_with("__sroa_")
                {
                    self.wrap_slot_if_needed(value);
                }
            }
            _ => {}
        }
        false
    }
}
