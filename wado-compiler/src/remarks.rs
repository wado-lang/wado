//! Optimizer remarks: surface residual value-semantic copies that survive
//! optimization. See WEP `wep-2026-06-03-optimizer-remarks.md`.
//!
//! Wado deep-copies aggregates on assignment, parameter passing, and return.
//! A large part of the optimizer exists to remove those hidden copies, and most
//! of them are removed; the ones that remain are invisible while coding. After
//! the NIR optimization pipeline, a surviving copy appears as one of:
//!
//! - a call to a synthesized `$value_copy$T` deep-copy helper, or
//! - a `builtin::array_clone` / `array_clone_shallow` / `copy_value` call — the
//!   lowered (and possibly `value_copy_demote`-shallowed) spine copy of a
//!   `List<T>` or `String`.
//!
//! NIR is the last IR that still carries per-expression source spans — WIR
//! instructions do not — and `wir_build` lowers these copies one-to-one, so
//! walking the optimized NIR yields the residual-copy set with exact source
//! locations. Detection is restricted to the entry module so the pervasive
//! buffer copies inside stdlib helpers (`String::push` growth, …) are not
//! reported; those are `array_copy`, which is deliberately excluded anyway.

use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::nir::{NirExpr, NirExprKind, NirStmt, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::nir_visitor::NirRefVisitor;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

/// A single optimizer remark: a residual cost with its source span.
pub struct Remark {
    /// The remark text, without the `remark:` prefix the logger adds.
    pub message: String,
    /// Where the copy survives, in the original source.
    pub span: Span,
}

/// Collect remarks for value-semantic copies that survive optimization,
/// restricted to functions defined in the entry module.
pub fn collect_value_copy_remarks(package: &NirPackage) -> Vec<Remark> {
    let value_copy_set: IndexMap<(ModuleSource, String), TypeId> = package
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            f.value_copy_type()
                .map(|t| ((f.module_source.clone(), f.name.clone()), t))
        })
        .collect();

    let type_table_ref = package.type_table.borrow();
    let type_table: &TypeTable = &type_table_ref;
    let mut remarks = Vec::new();
    for func_rc in &package.functions {
        let func = func_rc.borrow();
        if func.module_source != package.entry_module_source || func.is_value_copy() {
            continue;
        }
        let Some(body) = func.body.as_ref() else {
            continue;
        };
        let mut collector = Collector {
            value_copy_set: &value_copy_set,
            type_table,
            remarks: &mut remarks,
            current_span: body.span,
        };
        collector.visit_block(body);
    }
    remarks
}

struct Collector<'a> {
    value_copy_set: &'a IndexMap<(ModuleSource, String), TypeId>,
    type_table: &'a TypeTable,
    remarks: &'a mut Vec<Remark>,
    /// Span of the enclosing statement. The synthesized copy nodes
    /// (`array_clone`, demoted spine copies) carry no user span, so the remark
    /// points at the statement that performs the copy (`let mut b = a;`).
    current_span: Span,
}

impl Collector<'_> {
    /// If `expr` is a surviving value-copy operation, return the type whose
    /// value is copied.
    fn copied_type(&self, expr: &NirExpr) -> Option<TypeId> {
        let NirExprKind::Call { func, args, .. } = &expr.kind else {
            return None;
        };
        // Deep `$value_copy$T` helper call.
        if args.len() == 1
            && let Some(&type_id) = self
                .value_copy_set
                .get(&(func.module_source.clone(), func.name.clone()))
        {
            return Some(type_id);
        }
        // Lowered / demoted spine copy of a `List<T>` or `String`. `array_copy`
        // is excluded on purpose: it is bulk buffer movement inside stdlib
        // helpers, not a value-semantic copy.
        if matches!(
            func.monomorphized_builtin_name().as_deref(),
            Some("builtin::array_clone" | "builtin::array_clone_shallow" | "builtin::copy_value")
        ) {
            return args.first().map(|a| clone_source_type(&a.expr));
        }
        None
    }
}

impl NirRefVisitor for Collector<'_> {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        let prev = self.current_span;
        self.current_span = stmt.span;
        self.walk_stmt(stmt);
        self.current_span = prev;
    }

    fn visit_expr(&mut self, expr: &NirExpr) {
        if let Some(type_id) = self.copied_type(expr) {
            let type_name = self.type_table.type_name(type_id);
            self.remarks.push(Remark {
                message: format!("a copy of `{type_name}` survives optimization"),
                span: self.current_span,
            });
        }
        self.walk_expr(expr);
    }
}

/// For an `array_clone(&agg.repr)` call, recover the aggregate type (`List<T>`
/// or `String`) that owns the cloned `repr`, peeling the `&`/`&mut` reference
/// and the `.repr` field access. Falls back to the argument's own type.
fn clone_source_type(arg: &NirExpr) -> TypeId {
    let inner = match &arg.kind {
        NirExprKind::Unary {
            op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
            expr,
        } => expr,
        _ => arg,
    };
    match &inner.kind {
        NirExprKind::FieldAccess { expr, .. } => expr.type_id,
        _ => inner.type_id,
    }
}
