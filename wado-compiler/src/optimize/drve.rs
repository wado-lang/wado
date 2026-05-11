//! Dead Return Value Elimination for Wado TIR.
//!
//! Converts non-void functions whose return value is always immediately
//! dropped at every call site into void-returning functions. Every
//! `Return { value: Some(expr) }` becomes `Return { value: None }` once
//! `expr` is verified pure, and call sites stay structurally identical
//! (the call expression now produces `Unit`).
//!
//! TIR analog of `wir_optimize/drve.rs`. Running at TIR exposes the freshly
//! dead expressions to the rest of the fixed-point loop. Especially useful
//! after `inline` collapses a `Result<(), Error>`-returning helper whose
//! callers all `_ = helper(args);`-style discard the result — DCE can then
//! remove the (now unreferenced) `Ok(())` constructor wholesale.
//!
//! Conservative scope:
//!
//! - Skips the same pinned set as DAE.
//! - Requires the body to end with an explicit `Return { value: Some(_) }`
//!   so we never have to reason about an implicit trailing-value return.
//! - Requires every other `Return` in the body to also carry a pure value.
//! - Requires every call site to appear as a top-level statement
//!   (`NirStmtKind::Expr(Call(f, ...))` or `NirStmtKind::Expr(MethodCall(f,
//!   ...))`); any nested or `Let`-bound use disqualifies the candidate.
//! - Requires at least one observed call site (otherwise DCE will delete
//!   the function anyway and there is nothing to optimise).

use crate::nir_package::NirPackage;
use crate::hashmap::IndexSet;
use crate::tir::{ResolvedType, TypeTable};
use crate::nir::{FunctionKind, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind};
use crate::nir_visitor::{NirMutVisitor, NirRefVisitor};

use super::dae;
use super::elide_local::is_pure_expr;

type FnKey = dae::FnKey;

pub fn eliminate_dead_return_values(project: &mut NirPackage) -> bool {
    let pinned = collect_pinned(project);
    let type_table = project.type_table.borrow();

    let mut candidates: IndexSet<FnKey> = IndexSet::default();
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if !is_eligible(&func, &pinned, &type_table) {
            continue;
        }
        if has_only_pure_returns_with_explicit_tail(&func) {
            candidates.insert((func.module_source.clone(), func.name.clone()));
        }
    }
    drop(type_table);
    if candidates.is_empty() {
        return false;
    }

    let confirmed = validate_call_sites(project, candidates);
    if confirmed.is_empty() {
        return false;
    }

    apply_drve(project, &confirmed);
    true
}

fn is_eligible(func: &NirFunction, pinned: &IndexSet<FnKey>, type_table: &TypeTable) -> bool {
    if func.body.is_none() {
        return false;
    }
    if func.is_export
        || func.is_cm_export
        || func.is_cm_binding
        || func.is_dispatch_wrapper
        || func.is_ambient
        || func.is_async
    {
        return false;
    }
    if !matches!(func.kind, FunctionKind::Regular) {
        return false;
    }
    if func.module_source.is_core_builtin() || func.module_source.is_wasm_asset() {
        return false;
    }
    if func.return_type == TypeTable::UNIT || func.return_type == TypeTable::NEVER {
        return false;
    }
    // Match the WIR-level DRVE scope: only convert returns that allocate
    // a heap-typed value (struct / variant / array). Primitive-returning
    // helpers like `fn f() -> i32 { return c.threshold + c.scale; }` save
    // nothing from being voided and can break test fixtures that assert
    // post-optimizer body shape.
    if !is_heap_alloc_return(func.return_type, type_table) {
        return false;
    }
    // Trait methods share a signature contract with sibling impls and the
    // trait declaration; rewriting their return type breaks dispatch.
    if func
        .method_info
        .as_ref()
        .is_some_and(|mi| mi.trait_name.is_some())
    {
        return false;
    }
    if pinned.contains(&(func.module_source.clone(), func.name.clone())) {
        return false;
    }
    true
}

fn is_heap_alloc_return(type_id: crate::tir::TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Struct { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::BuiltinArray(_)
            | ResolvedType::GenericInstance { .. }
    )
}

fn has_only_pure_returns_with_explicit_tail(func: &NirFunction) -> bool {
    let body = func.body.as_ref().unwrap();
    // The last stmt must be `Return { value: Some(_) }` so we never have to
    // think about an implicit trailing-value return path.
    let Some(last) = body.stmts.last() else {
        return false;
    };
    let NirStmtKind::Return { value: Some(_) } = &last.kind else {
        return false;
    };
    // Every return in the function (including the tail) must carry a pure
    // value — `Return { value: None }` would mean the function already exits
    // void via that path, which is structurally inconsistent for a non-void
    // signature.
    let mut checker = ReturnPurityChecker { ok: true };
    checker.visit_block(body);
    checker.ok
}

struct ReturnPurityChecker {
    ok: bool,
}

impl NirRefVisitor for ReturnPurityChecker {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        if !self.ok {
            return;
        }
        match &stmt.kind {
            NirStmtKind::Return { value: None } => self.ok = false,
            NirStmtKind::Return { value: Some(v) } if !is_pure_expr(v) => self.ok = false,
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &NirExpr) {
        // Closure bodies are a separate function scope. Their `Return`
        // statements belong to the closure, not to the candidate we are
        // evaluating; do not descend into them. This mirrors the closure
        // skip in `ReturnVoidRewriter`.
        if matches!(&expr.kind, NirExprKind::Closure { .. }) {
            return;
        }
        self.walk_expr(expr);
    }
}

fn collect_pinned(project: &NirPackage) -> IndexSet<FnKey> {
    dae::collect_pinned(project)
}

fn validate_call_sites(project: &NirPackage, mut candidates: IndexSet<FnKey>) -> IndexSet<FnKey> {
    let mut validator = CallValidator {
        candidates: &candidates,
        rejected: IndexSet::default(),
        observed: IndexSet::default(),
    };
    for func_rc in &project.functions {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            validator.visit_block(body);
        }
    }
    for global in &project.globals {
        // Globals can never be `_ = call(...)` style — any appearance of a
        // candidate function in a global initializer means its result is
        // *consumed*, which disqualifies the candidate.
        UseScanner {
            candidates: &candidates,
            rejected: &mut validator.rejected,
        }
        .visit_expr(&global.initializer);
    }
    let CallValidator {
        rejected, observed, ..
    } = validator;
    for r in &rejected {
        candidates.shift_remove(r);
    }
    candidates.retain(|k| observed.contains(k));
    candidates
}

/// Walks blocks recognising the "top-level Expr(Call)" drop-position pattern.
/// At a stmt boundary, `Expr(Call(f, args))` and `Expr(MethodCall(f, ...))`
/// observe `f` as drop-position; everything inside `args` / receiver is then
/// fed through `UseScanner`, which rejects any candidate appearing as a
/// value. Outside drop-position stmts (Let value, return value, sub-expressions
/// of any expression), the same `UseScanner` rule applies.
struct CallValidator<'a> {
    candidates: &'a IndexSet<FnKey>,
    rejected: IndexSet<FnKey>,
    observed: IndexSet<FnKey>,
}

impl NirRefVisitor for CallValidator<'_> {
    fn visit_stmt(&mut self, stmt: &NirStmt) {
        if let NirStmtKind::Expr(expr) = &stmt.kind {
            match &expr.kind {
                NirExprKind::Call { func, args, .. } => {
                    let key = (func.module_source.clone(), func.name.clone());
                    if self.candidates.contains(&key) {
                        self.observed.insert(key);
                    }
                    let mut scanner = UseScanner {
                        candidates: self.candidates,
                        rejected: &mut self.rejected,
                    };
                    for a in args {
                        scanner.visit_expr(&a.expr);
                    }
                    return;
                }
                NirExprKind::MethodCall {
                    func,
                    receiver,
                    args,
                    ..
                } => {
                    let key = (func.module_source.clone(), func.name.clone());
                    if self.candidates.contains(&key) {
                        self.observed.insert(key);
                    }
                    let mut scanner = UseScanner {
                        candidates: self.candidates,
                        rejected: &mut self.rejected,
                    };
                    scanner.visit_expr(receiver);
                    for a in args {
                        scanner.visit_expr(&a.expr);
                    }
                    return;
                }
                _ => {
                    let mut scanner = UseScanner {
                        candidates: self.candidates,
                        rejected: &mut self.rejected,
                    };
                    scanner.visit_expr(expr);
                    return;
                }
            }
        }
        // For non-Expr stmts, recurse normally — nested blocks (`If`,
        // `Loop`, `LabeledBlock`, `IfLet`, `VariadicForOf`) re-enter
        // `visit_stmt` here so their inner drop-position stmts are still
        // recognised. Sub-expressions of the stmt that aren't blocks get
        // routed through `UseScanner` via `visit_expr`.
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &NirExpr) {
        // Outside drop-position stmts, the value of every NirExpr is
        // observable. Hand off to `UseScanner` so any nested candidate call
        // is treated as a use.
        let mut scanner = UseScanner {
            candidates: self.candidates,
            rejected: &mut self.rejected,
        };
        scanner.visit_expr(expr);
    }
}

/// Walks an expression tree and rejects every candidate that appears as a
/// value (i.e. as a `Call` / `MethodCall` anywhere). Default-walks over
/// blocks / control flow / nested expressions.
struct UseScanner<'a> {
    candidates: &'a IndexSet<FnKey>,
    rejected: &'a mut IndexSet<FnKey>,
}

impl NirRefVisitor for UseScanner<'_> {
    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Call { func, .. } | NirExprKind::MethodCall { func, .. } => {
                let key = (func.module_source.clone(), func.name.clone());
                if self.candidates.contains(&key) {
                    self.rejected.insert(key);
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

fn apply_drve(project: &mut NirPackage, confirmed: &IndexSet<FnKey>) {
    // Step A: convert each candidate to void return. The candidate filter
    // guarantees every reachable `Return { value: Some(_) }` carries a pure
    // expression, so dropping its value is observably equivalent.
    let mut return_rewriter = ReturnVoidRewriter;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        let key = (func.module_source.clone(), func.name.clone());
        if !confirmed.contains(&key) {
            continue;
        }
        func.return_type = TypeTable::UNIT;
        if let Some(body) = func.body.as_mut() {
            return_rewriter.visit_block(body);
        }
    }

    // Step B: refresh `expr.type_id` at every call site that targets a
    // converted function. Without this, `Expr(Call(f))` in stmt position
    // still claims the old return type and `wir_build::translate.rs`
    // (around line 1172) wraps the call in `Drop`, underflowing the
    // Wasm stack.
    let mut retyper = CallRetyper { confirmed };
    let funcs = project.functions.clone();
    for func_rc in &funcs {
        let mut func = func_rc.borrow_mut();
        if let Some(body) = func.body.as_mut() {
            retyper.visit_block(body);
        }
    }
    for global in &mut project.globals {
        retyper.visit_expr(&mut global.initializer);
    }
}

struct CallRetyper<'a> {
    confirmed: &'a IndexSet<FnKey>,
}

impl NirMutVisitor for CallRetyper<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        match &expr.kind {
            NirExprKind::Call { func, .. } | NirExprKind::MethodCall { func, .. } => {
                let key = (func.module_source.clone(), func.name.clone());
                if self.confirmed.contains(&key) {
                    expr.type_id = TypeTable::UNIT;
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

/// Rewrites every reachable `Return { value: Some(_) }` to `Return { value: None }`
/// while leaving closure bodies alone — their `Return`s belong to the closure's
/// own function scope, which DRVE evaluates separately if at all.
struct ReturnVoidRewriter;

impl NirMutVisitor for ReturnVoidRewriter {
    fn visit_stmt(&mut self, stmt: &mut NirStmt) {
        if let NirStmtKind::Return { value } = &mut stmt.kind {
            *value = None;
            return;
        }
        self.walk_stmt(stmt);
    }

    fn visit_expr(&mut self, expr: &mut NirExpr) {
        if matches!(&expr.kind, NirExprKind::Closure { .. }) {
            return;
        }
        self.walk_expr(expr);
    }
}
