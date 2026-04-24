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
use crate::name::ModuleSource;
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, ResolvedType, TirExpr, TirExprKind, TirStmt, TirStmtKind,
    TypeId, TypeTable,
};
use crate::tir_visitor::{TirOptVisitor, opt_walk_expr, opt_walk_stmt};

/// Run the value-copy insertion pass on every user-defined function body.
pub fn insert_value_copy_calls(project: &mut FlatPackage) {
    let mut visitor = ValueCopyInserter {
        type_table: project.type_table.clone(),
    };
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if func.is_cm_binding {
            continue;
        }
        if let Some(ref mut body) = func.body {
            visitor.visit_block(body);
        }
    }
}

struct ValueCopyInserter {
    type_table: Rc<RefCell<TypeTable>>,
}

impl ValueCopyInserter {
    /// True when a value of `type_id` must be deep-copied on assignment or
    /// parameter passing. Mirrors the former `wir_build::value_copy`
    /// `needs_value_copy` predicate.
    fn needs_value_copy(&self, type_id: TypeId) -> bool {
        match self.type_table.borrow().get(type_id) {
            ResolvedType::Struct { base_name, .. } => base_name.as_deref() != Some("Box"),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                if name == "Box" {
                    return false;
                }
                if TypeTable::is_tuple_type(&name, &module_source) && type_args.is_empty() {
                    return false;
                }
                true
            }
            ResolvedType::Variant { .. } => true,
            _ => false,
        }
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
}

impl TirOptVisitor for ValueCopyInserter {
    fn visit_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        // Recurse first so nested Calls/Lets get their own wrappers before we
        // consider wrapping the outer Let's RHS.
        opt_walk_stmt(self, stmt);

        match &mut stmt.kind {
            TirStmtKind::Let {
                value,
                skip_value_copy,
                ..
            } if !*skip_value_copy => {
                self.wrap_slot(value);
                true
            }
            TirStmtKind::LetDestructure { value, .. } => {
                self.wrap_slot(value);
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
                        self.wrap_slot(&mut arg.expr);
                    }
                }
            }
            TirExprKind::IndirectCall { args, .. } => {
                // Indirect call args used to receive an unconditional value
                // copy at WIR build; preserve that semantics here since the
                // dispatched function may observe the argument as `mut`.
                for arg in args.iter_mut() {
                    self.wrap_slot(arg);
                }
            }
            TirExprKind::Assign { value, .. } => {
                self.wrap_slot(value);
            }
            _ => {}
        }
        false
    }
}
