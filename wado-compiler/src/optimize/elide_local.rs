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

use crate::hashmap::IndexSet;
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirMutVisitor, NirRefVisitor};

pub fn elide_write_only_locals(project: &mut NirPackage) -> bool {
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

fn elide_in_function(func: &mut NirFunction) -> bool {
    if func.body.is_none() {
        return false;
    }
    // Collect locals that must NOT be elided. Three sources:
    //
    // 1. Every body read of `Local { index }`, including the inner `Local`
    //    of `Unary { op: Ref / MutRef, expr: Local }` — `&local` and
    //    `&mut local` count as reads, since their values can later be
    //    dereferenced.
    // 2. Closure captures' `outer_index` — over-mark relative to the
    //    closure body's own local namespace, but always safe.
    // 3. `stores_aliased_locals` — params whose reference escaped via a
    //    callee's `stores` declaration. The callee may retain that
    //    reference past its return, so writes through the local stay
    //    observable via the alias.
    //
    // `address_taken_locals` is *not* used as a kept-set source. That
    // field is set during `lower::plan::boxing` and reflects a static "address
    // ever taken" property of the source TIR. After `inline` /
    // `ref_elim` strip away `&local` references, the field is stale —
    // including it would re-pin locals whose address-taking sites are
    // no longer in the body. Source 1 already catches every live
    // `&local`, so the static record is redundant.
    let mut kept: IndexSet<u32> = IndexSet::default();
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

impl NirRefVisitor for ReadCollector<'_> {
    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Local { index, .. } => {
                self.kept.insert(*index);
                return;
            }
            NirExprKind::Assign { target, value } => {
                // The target's outer `Local` is a write, not a read. Recurse
                // into nested writes (`a.field = ...`, `a[i] = ...`) to
                // capture the read of `a`/`i`. Don't insert the bare
                // `Local` target itself.
                if !matches!(target.kind, NirExprKind::Local { .. }) {
                    self.visit_expr(target);
                }
                self.visit_expr(value);
                return;
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

impl NirMutVisitor for Elider<'_> {
    fn visit_block(&mut self, block: &mut NirBlock) {
        let stmts = std::mem::take(&mut block.stmts);
        let mut new_stmts = Vec::with_capacity(stmts.len());
        for mut stmt in stmts {
            // `let x = expr;` where `x` is unread.
            if let NirStmtKind::Let {
                local_index, value, ..
            } = &mut stmt.kind
                && !self.kept.contains(local_index)
            {
                let value = std::mem::replace(value, dummy_unit_expr());
                self.changed = true;
                if is_pure_expr(&value) {
                    continue;
                }
                new_stmts.push(NirStmt::new(NirStmtKind::Expr(value), stmt.span));
                continue;
            }
            // `x = value;` (Assign at stmt position) where `x` is unread.
            // This catches the SROA / variant-lowering shadow-temp pattern
            // where a pass introduces a local and writes to it via Assign,
            // then a downstream pass folds away the only read site. The
            // matching `let x;` declaration falls out at WIR cleanup once
            // every write to `x` is gone.
            if let NirStmtKind::Expr(expr) = &mut stmt.kind
                && let NirExprKind::Assign { target, value } = &mut expr.kind
                && let NirExprKind::Local { index, .. } = &target.kind
                && !self.kept.contains(index)
            {
                let value = std::mem::replace(value.as_mut(), dummy_unit_expr());
                self.changed = true;
                if is_pure_expr(&value) {
                    continue;
                }
                new_stmts.push(NirStmt::new(NirStmtKind::Expr(value), stmt.span));
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
pub(super) fn collect_reads_in_block(block: &NirBlock, out: &mut IndexSet<u32>) {
    let mut collector = ReadCollector { kept: out };
    collector.visit_block(block);
}

/// True when `expr` and every sub-expression has no observable effect.
///
/// Conservative — calls, global writes, assignments, closure construction,
/// and control-flow constructs whose branches are themselves impure are
/// treated as impure. Pure reads (`Local`, `GlobalVarGet`, `FieldAccess`,
/// `Index`), arithmetic, and reference-taking (`&x` / `&mut x`) are pure
/// since the *act* of taking a reference does not mutate; only writing
/// through the resulting reference would, and that shows up as a separate
/// `Assign` / call. Mirrors the WIR-level `is_side_effect_free` contract.
pub(super) fn is_pure_expr(expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => true,
        NirExprKind::Binary { left, right, .. } => is_pure_expr(left) && is_pure_expr(right),
        NirExprKind::Unary { expr: inner, op } => {
            // `&mut x` is a pure root by itself, but only meaningful when the
            // resulting reference is used; an unused MutRef has no observable
            // effect on the local because nothing reads/writes through it.
            let _ = op;
            is_pure_expr(inner)
        }
        NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => is_pure_expr(inner),
        NirExprKind::Index { expr: e, index: i } => is_pure_expr(e) && is_pure_expr(i),
        NirExprKind::StructLiteral { fields, .. } => fields.iter().all(|f| is_pure_expr(&f.value)),
        NirExprKind::TupleLiteral { elements } => elements.iter().all(is_pure_expr),
        NirExprKind::VariantConstruct { payload, .. } => {
            payload.as_ref().is_none_or(|p| is_pure_expr(p))
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => is_pure_block(block),
        NirExprKind::If {
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

fn is_pure_block(block: &NirBlock) -> bool {
    block.stmts.iter().all(|s| match &s.kind {
        NirStmtKind::Expr(e) | NirStmtKind::Let { value: e, .. } => is_pure_expr(e),
        _ => false,
    })
}

fn dummy_unit_expr() -> NirExpr {
    use crate::tir::TypeTable;
    NirExpr::new(
        NirExprKind::Unit,
        TypeTable::UNIT,
        crate::token::Span::default(),
    )
}
