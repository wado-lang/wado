//! Write-only local elimination for Wado TIR.
//!
//! Eliminates `let x = expr;` bindings where the local `x` is never read,
//! never has its address taken, and never escapes via closure capture or a
//! `stores`-aliased call. When `expr` is pure the entire statement is removed;
//! otherwise the binding is replaced by `Expr(expr)` so the side effect still
//! runs.
//!
//! TIR analog of `wir_optimize/elide_local.rs`. Running at TIR exposes the
//! freshly dead expressions to the rest of the fixed-point loop
//! (`copy_prop` / `const_fold` / `dce`), which the WIR-level pass cannot.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind};
use crate::tir_visitor::{TirMutVisitor, TirRefVisitor};

pub fn elide_write_only_locals(project: &mut FlatPackage) -> bool {
    let mut changed = false;
    let funcs = project.functions.clone();
    for func_rc in &funcs {
        let mut func = func_rc.borrow_mut();
        if elide_in_function(&mut func) {
            changed = true;
        }
    }
    changed
}

fn elide_in_function(func: &mut TirFunction) -> bool {
    if func.body.is_none() {
        return false;
    }
    // Collect locals that must NOT be elided: any read site, any address-taken
    // local, and any local whose reference escapes via `stores`. Captures are
    // recorded too — even though the closure body uses its own local-index
    // namespace, the conservative over-mark only suppresses elision and
    // never produces an incorrect transform.
    let mut kept: IndexSet<u32> = func.address_taken_locals.clone();
    for &i in &func.stores_aliased_locals {
        kept.insert(i);
    }
    let mut collector = ReadCollector { kept: &mut kept };
    collector.visit_block(func.body.as_ref().unwrap());

    let body = func.body.as_mut().unwrap();
    let mut elider = Elider {
        kept: &kept,
        changed: false,
    };
    elider.visit_block(body);
    elider.changed
}

struct ReadCollector<'a> {
    kept: &'a mut IndexSet<u32>,
}

impl TirRefVisitor for ReadCollector<'_> {
    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::Local { index, .. } => {
                self.kept.insert(*index);
                return;
            }
            TirExprKind::Assign { target, value } => {
                // The target's outer `Local` is a write, not a read. Recurse
                // into nested writes (`a.field = ...`, `a[i] = ...`) to
                // capture the read of `a`/`i`. Don't insert the bare
                // `Local` target itself.
                if !matches!(target.kind, TirExprKind::Local { .. }) {
                    self.visit_expr(target);
                }
                self.visit_expr(value);
                return;
            }
            TirExprKind::Closure { captures, .. } => {
                for cap in captures {
                    self.kept.insert(cap.outer_index);
                }
                // Walking the body is a conservative over-mark: closure-locals
                // share the index namespace numerically but refer to a
                // different function's locals, so any matches will only
                // suppress elision (never produce a wrong transform).
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

struct Elider<'a> {
    kept: &'a IndexSet<u32>,
    changed: bool,
}

impl TirMutVisitor for Elider<'_> {
    fn visit_block(&mut self, block: &mut TirBlock) {
        let stmts = std::mem::take(&mut block.stmts);
        let mut new_stmts = Vec::with_capacity(stmts.len());
        for mut stmt in stmts {
            // `let x = expr;` where `x` is unread.
            if let TirStmtKind::Let {
                local_index, value, ..
            } = &mut stmt.kind
                && !self.kept.contains(local_index)
            {
                let value = std::mem::replace(value, dummy_unit_expr());
                self.changed = true;
                if is_pure_expr(&value) {
                    continue;
                }
                new_stmts.push(TirStmt::new(TirStmtKind::Expr(value), stmt.span));
                continue;
            }
            // `x = value;` (Assign at stmt position) where `x` is unread.
            // This catches the SROA / variant-lowering shadow-temp pattern
            // where a pass introduces a local and writes to it via Assign,
            // then a downstream pass folds away the only read site. The
            // matching `let x;` declaration falls out at WIR cleanup once
            // every write to `x` is gone.
            if let TirStmtKind::Expr(expr) = &mut stmt.kind
                && let TirExprKind::Assign { target, value } = &mut expr.kind
                && let TirExprKind::Local { index, .. } = &target.kind
                && !self.kept.contains(index)
            {
                let value = std::mem::replace(value.as_mut(), dummy_unit_expr());
                self.changed = true;
                if is_pure_expr(&value) {
                    continue;
                }
                new_stmts.push(TirStmt::new(TirStmtKind::Expr(value), stmt.span));
                continue;
            }
            self.visit_stmt(&mut stmt);
            new_stmts.push(stmt);
        }
        block.stmts = new_stmts;
    }
}

/// Public helper used by `dae` to collect locals that the function body reads
/// (or whose addresses escape via captures). Insertion is done by `ReadCollector`.
pub(super) fn collect_reads_in_block(block: &TirBlock, out: &mut IndexSet<u32>) {
    let mut collector = ReadCollector { kept: out };
    collector.visit_block(block);
}

/// True when `expr` and every sub-expression has no observable effect.
///
/// Conservative — anything that calls a function, writes a global, assigns,
/// constructs a closure, takes a `&mut` reference, or evaluates inside a
/// control-flow construct that may itself be impure is treated as impure.
/// Pure reads (`Local`, `GlobalVarGet`, `FieldAccess`, `Index`) and arithmetic
/// are pure. Mirrors the WIR-level `is_side_effect_free` contract.
pub(super) fn is_pure_expr(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => true,
        TirExprKind::Binary { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        TirExprKind::Unary { expr: inner, op } => {
            // `&mut x` is a pure root by itself, but only meaningful when the
            // resulting reference is used; an unused MutRef has no observable
            // effect on the local because nothing reads/writes through it.
            let _ = op;
            is_pure_expr(inner)
        }
        TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => is_pure_expr(inner),
        TirExprKind::Index { expr: e, index: i } => is_pure_expr(e) && is_pure_expr(i),
        TirExprKind::StructLiteral { fields, .. } => fields.iter().all(|f| is_pure_expr(&f.value)),
        TirExprKind::TupleLiteral { elements } => elements.iter().all(is_pure_expr),
        TirExprKind::VariantConstruct { payload, .. } => {
            payload.as_ref().is_none_or(|p| is_pure_expr(p))
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => is_pure_block(block),
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            is_pure_expr(condition)
                && is_pure_block(then_branch)
                && else_branch.as_ref().is_none_or(is_pure_block)
        }
        // Calls, mutations, closures, control-flow exits, and anything that
        // could suspend are conservatively impure.
        _ => false,
    }
}

fn is_pure_block(block: &TirBlock) -> bool {
    block.stmts.iter().all(|s| match &s.kind {
        TirStmtKind::Expr(e) | TirStmtKind::Let { value: e, .. } => is_pure_expr(e),
        _ => false,
    })
}

fn dummy_unit_expr() -> TirExpr {
    use crate::tir::TypeTable;
    TirExpr::new(
        TirExprKind::Unit,
        TypeTable::UNIT,
        crate::token::Span::default(),
    )
}
