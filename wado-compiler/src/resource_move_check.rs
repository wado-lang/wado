//! Resource move checking (WEP 2026-05-21, R2 — the move-check engine).
//!
//! Resources are move-only: a resource handle is a one-shot, `dtor`-bearing
//! value, so copying it would alias it. This pass flags a *use after move* —
//! reading a resource binding after it has been consumed (moved by value) — as
//! a compile error, giving resources their move-only semantics.
//!
//! It runs over [`Semantics`] (the AST plus the facts recorded during
//! `annotate`), a sibling of `effect_check::check_semantics`: the LSP + batch
//! shared path, with source spans and a diagnostic channel. Violations are
//! returned for the caller to route.
//!
//! This is the first slice of the move check. It tracks bare resource-typed
//! locals through the unambiguous consuming forms (a by-value `let` initializer,
//! a by-value call / constructor argument, a returned value) and reports a later
//! use of a moved binding. Classification is conservative — an unrecognised
//! position is treated as a borrow — so the pass never rejects a valid program;
//! it only under-reports moves, which later slices tighten.

use crate::ast::{
    self, AssignExpr, AstId, Block, Condition, ConditionElement, Expr, Function, IdentExpr, Item,
    Stmt,
};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::semantics::Semantics;
use crate::tir::ResolvedType;
use crate::token::Span;

/// A resource binding used after it was moved.
#[derive(Debug, Clone)]
pub struct ResourceMoveError {
    /// The moved binding's name.
    pub name: String,
    /// Span of the offending later use.
    pub use_span: Span,
    /// Span of the earlier move that consumed the binding.
    pub move_span: Span,
    pub module: String,
}

impl std::fmt::Display for ResourceMoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: resource `{}` used after it was moved (moved at {}:{})",
            self.use_span.line,
            self.use_span.column,
            self.name,
            self.move_span.line,
            self.move_span.column,
        )
    }
}

impl std::error::Error for ResourceMoveError {}

impl From<ResourceMoveError> for crate::compiler_host::Diagnostic {
    fn from(e: ResourceMoveError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code: Code::TypeMismatch,
            message: format!(
                "resource `{}` used after it was moved (moved at {}:{})",
                e.name, e.move_span.line, e.move_span.column,
            ),
            span: Some(DiagnosticSpan::from_span(&e.use_span, Some(&e.module))),
        }
    }
}

/// Check every user-authored function for use-after-move of a resource binding.
#[must_use]
pub fn check_resource_moves_semantic(sem: &Semantics) -> Vec<ResourceMoveError> {
    let mut out = Vec::new();
    for (src, module) in &sem.modules {
        if !crate::elaborator::liveness::is_user_authored(src) {
            continue;
        }
        for item in &module.items {
            match item {
                Item::Function(func) => check_function(sem, src, func, &mut out),
                Item::Impl(impl_block) => {
                    for method in &impl_block.methods {
                        check_function(sem, src, method, &mut out);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn check_function(
    sem: &Semantics,
    module: &ModuleSource,
    func: &Function,
    out: &mut Vec<ResourceMoveError>,
) {
    let Some(body) = &func.body else {
        return;
    };
    let mut walker = MoveWalker {
        sem,
        module: module.source_path(),
        moved: IndexMap::default(),
        out,
    };
    walker.visit_block(body);
}

/// Forward move-state walker. `moved` maps a resource binding's definition
/// `AstId` to the span of the move that consumed it.
struct MoveWalker<'a> {
    sem: &'a Semantics,
    module: String,
    moved: IndexMap<AstId, Span>,
    out: &'a mut Vec<ResourceMoveError>,
}

impl MoveWalker<'_> {
    /// Resolve an identifier use to the definition of a bare resource-typed
    /// local, if that is what it refers to.
    fn resource_def(&self, use_id: AstId) -> Option<AstId> {
        let def = self.sem.referenced_symbol(use_id)?;
        let type_id = self.sem.expression_type(use_id)?;
        matches!(
            self.sem.types.get(type_id),
            ResolvedType::Resource { .. } | ResolvedType::GenericResource { .. }
        )
        .then_some(def)
    }

    /// A borrowing read of `ident`: an error only if the binding is already moved.
    fn read(&mut self, ident: &IdentExpr) {
        if let Some(def) = self.resource_def(ident.id)
            && let Some(&move_span) = self.moved.get(&def)
        {
            self.emit(ident, move_span);
        }
    }

    /// A consuming use of `ident`: an error if already moved, otherwise records
    /// the move.
    fn consume(&mut self, ident: &IdentExpr) {
        if let Some(def) = self.resource_def(ident.id) {
            if let Some(&move_span) = self.moved.get(&def) {
                self.emit(ident, move_span);
            } else {
                self.moved.insert(def, ident.span);
            }
        }
    }

    fn emit(&mut self, ident: &IdentExpr, move_span: Span) {
        self.out.push(ResourceMoveError {
            name: ident.name.clone(),
            use_span: ident.span,
            move_span,
            module: self.module.clone(),
        });
    }

    /// Visit an expression whose value is consumed into an owner (a binding,
    /// argument, return, or aggregate element): a bare identifier is moved;
    /// anything else is visited for the reads / moves it performs internally.
    fn visit_value(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident(ident) => self.consume(ident),
            _ => self.visit_expr(expr),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident(ident) => self.read(ident),

            Expr::Call(call) => {
                self.visit_expr(&call.callee);
                for arg in &call.args {
                    self.visit_value(arg);
                }
            }
            Expr::MethodCall(mc) => {
                // Slice 1 treats every method receiver as a borrow; by-value
                // `self` consumption is a later refinement.
                self.visit_expr(&mc.receiver);
                for arg in &mc.args {
                    self.visit_value(arg);
                }
            }
            Expr::StaticMethodCall(smc) => {
                for arg in &smc.args {
                    self.visit_value(arg);
                }
            }

            // Unary operands are never consumed: `&x` / `&mut x` borrow, and
            // arithmetic / logical operators do not apply to resources. Visiting
            // as a read still flags a use of an already-moved binding.
            Expr::Unary(u) => self.visit_expr(&u.expr),
            Expr::Binary(b) => {
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
            }
            Expr::Assign(a) => self.visit_assign(a),
            Expr::CompoundAssign(a) => {
                self.visit_expr(&a.target);
                self.visit_expr(&a.value);
            }
            Expr::FieldAccess(fa) => self.visit_expr(&fa.expr),
            Expr::Index(ix) => {
                self.visit_expr(&ix.expr);
                self.visit_expr(&ix.index);
            }
            Expr::Cast(c) => self.visit_expr(&c.expr),
            Expr::TryOp(t) => self.visit_value(&t.expr),

            Expr::StructLiteral(s) => {
                for field in &s.fields {
                    self.visit_value(&field.value);
                }
            }
            Expr::TupleLiteral(t) => {
                for el in &t.elements {
                    self.visit_value(el);
                }
            }
            Expr::TemplateString(ts) => {
                for part in &ts.parts {
                    if let ast::TemplatePart::Interpolation { expr, .. } = part {
                        self.visit_expr(expr);
                    }
                }
            }

            Expr::Block(block) => {
                self.visit_block(block);
            }
            Expr::LabeledBlock(lb) => {
                self.visit_block(&lb.block);
            }
            Expr::If(if_expr) => {
                self.visit_condition(&if_expr.condition);
                self.merge_if(&if_expr.then_block, if_expr.else_block.as_ref());
            }
            Expr::Match(m) => {
                self.visit_match(&m.expr, &m.arms);
            }

            Expr::WithHandler(w) => {
                for handler in &w.handlers {
                    self.visit_expr(&handler.handler);
                }
                self.visit_block(&w.body);
            }
            Expr::Resume(r) => self.visit_value(&r.value),
            Expr::Range(r) => {
                self.visit_expr(&r.start);
                self.visit_expr(&r.end);
            }
            Expr::Matches(m) => self.visit_expr(&m.expr),
            Expr::Spread(inner, _) => self.visit_expr(inner),
            Expr::ComparisonChain(c) => {
                self.visit_expr(&c.first);
                for cmp in &c.comparisons {
                    self.visit_expr(&cmp.right);
                }
            }

            // Closures capture into a separate frame; their body's locals are a
            // different scope, out of this slice's scope.
            Expr::Closure(_) | Expr::Literal(_) | Expr::Error(_) => {}
        }
    }

    fn visit_assign(&mut self, a: &AssignExpr) {
        self.visit_value(&a.value);
        // The target is a write, not a read. Re-assigning a resource binding
        // re-initialises it, so it is live again — clear any moved state instead
        // of flagging the write as a use.
        match &a.target {
            Expr::Ident(ident) => {
                if let Some(def) = self.resource_def(ident.id) {
                    self.moved.swap_remove(&def);
                }
            }
            other => self.visit_expr(other),
        }
    }

    /// Visit a block, returning whether it diverges (its reachable end does not
    /// fall through — a `return` / `break` / `continue`, or an `if` whose
    /// branches all diverge). Statements after a diverging one are unreachable
    /// and skipped, so a move on a non-returning path is never carried past it.
    fn visit_block(&mut self, block: &Block) -> bool {
        for stmt in &block.stmts {
            if self.visit_stmt(stmt) {
                return true;
            }
        }
        false
    }

    fn visit_stmt(&mut self, stmt: &Stmt) -> bool {
        match stmt {
            Stmt::Let(l) => {
                if let Some(value) = &l.value {
                    self.visit_value(value);
                }
                false
            }
            Stmt::Expr(e) => {
                self.visit_expr(&e.expr);
                false
            }
            Stmt::Return(r) => {
                if let Some(value) = &r.value {
                    self.visit_value(value);
                }
                true
            }
            Stmt::TaskReturn(t) => {
                self.visit_value(&t.value);
                false
            }
            Stmt::If(s) => {
                self.visit_condition(&s.condition);
                self.merge_if(&s.then_block, s.else_block.as_ref())
            }
            Stmt::While(s) => {
                self.visit_condition(&s.condition);
                self.visit_block(&s.body);
                false
            }
            Stmt::Loop(s) => {
                self.visit_block(&s.body);
                false
            }
            Stmt::For(s) => {
                if let Some(init) = &s.init {
                    self.visit_stmt(init);
                }
                if let Some(cond) = &s.condition {
                    self.visit_condition(cond);
                }
                if let Some(update) = &s.update {
                    self.visit_expr(update);
                }
                self.visit_block(&s.body);
                false
            }
            Stmt::ForOf(s) => {
                self.visit_expr(&s.iterable);
                self.visit_block(&s.body);
                false
            }
            Stmt::Match(m) => self.visit_match(&m.expr, &m.arms),
            Stmt::Break(b) => {
                if let Some(value) = &b.value {
                    self.visit_value(value);
                }
                true
            }
            Stmt::Assert(a) => {
                self.visit_expr(&a.condition);
                if let Some(msg) = &a.message {
                    self.visit_expr(msg);
                }
                false
            }
            Stmt::LabeledBlock(lb) => self.visit_block(&lb.block),
            Stmt::Continue(_) => true,
            Stmt::Item(_) | Stmt::Error(_) => false,
        }
    }

    /// Merge the two arms of an `if`, keeping the moves only from branches that
    /// fall through. Returns whether the `if` diverges (both branches diverge).
    fn merge_if(&mut self, then_block: &Block, else_block: Option<&Block>) -> bool {
        let base = self.moved.clone();
        let then_div = self.visit_block(then_block);
        let then_moved = std::mem::replace(&mut self.moved, base);
        let else_div = match else_block {
            Some(eb) => self.visit_block(eb),
            None => false,
        };
        // `self.moved` now holds the else path (or the untouched base for no
        // else). Fold in the then path only when it falls through.
        if !then_div {
            self.union_into(then_moved);
        }
        else_block.is_some() && then_div && else_div
    }

    fn visit_match(&mut self, scrutinee: &Expr, arms: &[ast::MatchArm]) -> bool {
        // Slice 1 treats the scrutinee as a borrow (a pattern match that moves a
        // resource out is a later refinement).
        self.visit_expr(scrutinee);
        let base = std::mem::take(&mut self.moved);
        let mut merged = base.clone();
        let mut all_diverge = !arms.is_empty();
        for arm in arms {
            self.moved.clone_from(&base);
            if let Some(guard) = &arm.guard {
                self.visit_expr(guard);
            }
            let diverged = self.visit_expr_diverges(&arm.body);
            if !diverged {
                all_diverge = false;
                let arm_moved = std::mem::take(&mut self.moved);
                for (def, span) in arm_moved {
                    merged.entry(def).or_insert(span);
                }
            }
        }
        self.moved = merged;
        all_diverge
    }

    /// Visit an expression used as a branch/arm body, returning whether it
    /// diverges. Only block-like bodies can; anything else falls through.
    fn visit_expr_diverges(&mut self, expr: &Expr) -> bool {
        match expr {
            Expr::Block(block) => self.visit_block(block),
            Expr::LabeledBlock(lb) => self.visit_block(&lb.block),
            _ => {
                self.visit_expr(expr);
                false
            }
        }
    }

    fn visit_condition(&mut self, condition: &Condition) {
        match condition {
            Condition::Expr(e) => self.visit_expr(e),
            Condition::LetChain { elements, .. } => {
                for element in elements {
                    match element {
                        // A `let PAT = EXPR` scrutinee is a borrow in this slice.
                        ConditionElement::Let { expr, .. } => self.visit_expr(expr),
                        ConditionElement::Expr(e) => self.visit_expr(e),
                    }
                }
            }
        }
    }

    /// Union `other` into `self.moved`, keeping the earliest recorded move span.
    fn union_into(&mut self, other: IndexMap<AstId, Span>) {
        for (def, span) in other {
            self.moved.entry(def).or_insert(span);
        }
    }
}
