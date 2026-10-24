//! Whether control reaches past a construct, answered once per IR for every
//! walk that asks. Over the AST: the missing-return walk, labeled-block typing
//! and the affine move check. Over TIR: drop elaboration.
//!
//! Evaluating an expression ends its path when the expression is of type `!`
//! (or a newtype over it), or when an operand it always evaluates does, so
//! `show(panic("x"))` and `1 + panic("x")` end it as `panic("x")` does. A loop
//! is left only by an unlabeled `break` it binds: a labeled one lands past the
//! block it names, never right after the loop.

use crate::ast::{self, AstVisitor};
use crate::tir::{TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirTemplatePart};
use crate::tir_visitor::TirRefVisitor;
use crate::type_table::TypeTable;

/// The recorded types a walk reads divergence from.
pub(crate) trait ExprTypes {
    /// Whether `expr` is of type `!`, or a newtype over it.
    fn is_never(&self, expr: &ast::Expr) -> bool;
}

/// Whether every control path through `block` exits before reaching its end.
pub(crate) fn block_always_exits(types: &impl ExprTypes, block: &ast::Block) -> bool {
    block.stmts.iter().any(|s| stmt_always_exits(types, s))
}

fn stmt_always_exits(types: &impl ExprTypes, stmt: &ast::Stmt) -> bool {
    if stmt_head_always_exits(types, stmt) {
        return true;
    }
    match stmt {
        ast::Stmt::Return(_) | ast::Stmt::Break(_) | ast::Stmt::Continue(_) => true,
        ast::Stmt::If(if_stmt) => {
            branches_always_exit(types, &if_stmt.then_block, if_stmt.else_block.as_ref())
        }
        ast::Stmt::Loop(loop_stmt) => !loop_body_can_escape(&loop_stmt.body),
        // `while true` is the same loop written differently: reify desugars it
        // to `loop { if !true { break } B }`, so only a `break` in `B` ends it.
        ast::Stmt::While(w) if w.condition.is_always_true() => !loop_body_can_escape(&w.body),
        // So is a C-style `for` with no condition, or one always true: its
        // `init` and `update` cannot leave it.
        ast::Stmt::For(f)
            if f.condition
                .as_ref()
                .is_none_or(ast::Condition::is_always_true) =>
        {
            !loop_body_can_escape(&f.body)
        }
        ast::Stmt::LabeledBlock(lb) => labeled_block_always_exits(types, &lb.block, &lb.label),
        ast::Stmt::Match(m) => arms_always_exit(types, &m.arms),
        ast::Stmt::Let(_)
        | ast::Stmt::Expr(_)
        | ast::Stmt::TaskReturn(_)
        | ast::Stmt::While(_)
        | ast::Stmt::For(_)
        | ast::Stmt::ForOf(_)
        | ast::Stmt::Assert(_)
        | ast::Stmt::Item(_)
        | ast::Stmt::Error(_) => false,
    }
}

/// Whether what `stmt` evaluates before choosing a path never completes: a
/// `let` value or an expression statement, a condition, a scrutinee, an
/// iterable, a `for` loop's `init`. A walk that follows the paths a statement
/// chooses answers the rest of its divergence itself.
pub(crate) fn stmt_head_always_exits(types: &impl ExprTypes, stmt: &ast::Stmt) -> bool {
    let exits = |e: &ast::Expr| expr_always_exits(types, e);
    match stmt {
        ast::Stmt::Let(l) => l.value.as_ref().is_some_and(exits),
        ast::Stmt::Expr(e) => exits(&e.expr),
        ast::Stmt::TaskReturn(t) => exits(&t.value),
        ast::Stmt::Return(r) => r.value.as_ref().is_some_and(exits),
        ast::Stmt::Break(b) => b.value.as_deref().is_some_and(exits),
        ast::Stmt::If(if_stmt) => condition_always_exits(types, &if_stmt.condition),
        ast::Stmt::While(w) => condition_always_exits(types, &w.condition),
        ast::Stmt::For(f) => {
            f.init
                .as_ref()
                .is_some_and(|init| stmt_always_exits(types, init))
                || f.condition
                    .as_ref()
                    .is_some_and(|c| condition_always_exits(types, c))
        }
        ast::Stmt::ForOf(fo) => exits(&fo.iterable),
        ast::Stmt::Match(m) => exits(&m.expr),
        ast::Stmt::Assert(a) => exits(&a.condition),
        ast::Stmt::Continue(_)
        | ast::Stmt::Loop(_)
        | ast::Stmt::LabeledBlock(_)
        | ast::Stmt::Item(_)
        | ast::Stmt::Error(_) => false,
    }
}

/// Only a condition's first operand always runs: the rest of a let chain is
/// `&&`-joined to it.
fn condition_always_exits(types: &impl ExprTypes, condition: &ast::Condition) -> bool {
    let first = match condition {
        ast::Condition::Expr(e) => e,
        ast::Condition::LetChain { elements, .. } => elements[0].expr(),
    };
    expr_always_exits(types, first)
}

fn branches_always_exit(
    types: &impl ExprTypes,
    then_block: &ast::Block,
    else_block: Option<&ast::Block>,
) -> bool {
    else_block.is_some_and(|else_block| {
        block_always_exits(types, then_block) && block_always_exits(types, else_block)
    })
}

fn arms_always_exit(types: &impl ExprTypes, arms: &[ast::MatchArm]) -> bool {
    !arms.is_empty() && arms.iter().all(|a| expr_always_exits(types, &a.body))
}

fn labeled_block_always_exits(types: &impl ExprTypes, block: &ast::Block, label: &str) -> bool {
    block_always_exits(types, block) && !block_can_break_to_label(block, label)
}

/// Whether evaluating `expr` never completes.
pub(crate) fn expr_always_exits(types: &impl ExprTypes, expr: &ast::Expr) -> bool {
    if types.is_never(expr) {
        return true;
    }
    let exits = |e: &ast::Expr| expr_always_exits(types, e);
    match expr {
        ast::Expr::Block(block) => block_always_exits(types, block),
        ast::Expr::LabeledBlock(lb) => labeled_block_always_exits(types, &lb.block, &lb.label),
        ast::Expr::If(if_expr) => {
            condition_always_exits(types, &if_expr.condition)
                || branches_always_exit(types, &if_expr.then_block, if_expr.else_block.as_ref())
        }
        ast::Expr::Match(m) => exits(&m.expr) || arms_always_exit(types, &m.arms),
        ast::Expr::Matches(m) => exits(&m.expr),
        ast::Expr::WithHandler(wh) => {
            wh.handlers.iter().any(|b| exits(&b.handler)) || block_always_exits(types, &wh.body)
        }
        ast::Expr::Binary(b) => exits(&b.left) || (!b.op.is_short_circuit() && exits(&b.right)),
        ast::Expr::Unary(u) => exits(&u.expr),
        ast::Expr::Cast(c) => exits(&c.expr),
        ast::Expr::FieldAccess(f) => exits(&f.expr),
        ast::Expr::TryOp(t) => exits(&t.expr),
        ast::Expr::Spread(inner, _) => exits(inner),
        ast::Expr::Assign(a) => exits(&a.target) || exits(&a.value),
        ast::Expr::CompoundAssign(a) => exits(&a.target) || exits(&a.value),
        ast::Expr::ComparisonChain(c) => {
            exits(&c.first) || c.comparisons.iter().any(|cmp| exits(&cmp.right))
        }
        ast::Expr::Index(i) => exits(&i.expr) || exits(&i.index),
        ast::Expr::Call(c) => exits(&c.callee) || c.args.iter().any(exits),
        ast::Expr::MethodCall(mc) => exits(&mc.receiver) || mc.args.iter().any(exits),
        ast::Expr::StaticMethodCall(sc) => sc.args.iter().any(exits),
        ast::Expr::StructLiteral(s) => {
            s.fields.iter().any(|f| exits(&f.value)) || s.spreads.iter().any(|sp| exits(&sp.expr))
        }
        ast::Expr::TupleLiteral(t) => t.elements.iter().any(exits),
        // The body runs once per element, and a tuple may have none.
        ast::Expr::TupleComprehension(c) => exits(&c.iterable),
        ast::Expr::Range(r) => exits(&r.start) || exits(&r.end),
        ast::Expr::TemplateString(ts) => ts.interpolations().any(exits),
        ast::Expr::TaggedTemplate(t) => exits(&t.tag) || t.template.interpolations().any(exits),
        // A closure's body runs when it is called, not where it is written.
        ast::Expr::Closure(_)
        | ast::Expr::Ident(_)
        | ast::Expr::Literal(_)
        | ast::Expr::Error(_) => false,
    }
}

/// Per-node decision for [`any_in_tree`].
#[derive(Clone, Copy)]
pub(crate) enum Step {
    /// Predicate matched — short-circuit and return `true`.
    Match,
    /// Not matched here, and do not descend into children.
    Skip,
    /// Not matched here; continue descending into children.
    Descend,
}

/// `'t` is the tree's own lifetime, so a probe may keep what it visits.
pub(crate) trait AstTreeProbe<'t> {
    fn check_stmt(&mut self, stmt: &'t ast::Stmt) -> Step;
    fn check_expr(&mut self, _expr: &'t ast::Expr) -> Step {
        Step::Descend
    }
}

/// Searches a loop body for an unlabeled `break` the loop binds. A nested loop
/// binds its own.
struct LoopEscape;

impl AstTreeProbe<'_> for LoopEscape {
    fn check_stmt(&mut self, stmt: &ast::Stmt) -> Step {
        match stmt {
            ast::Stmt::Break(b) if b.label.is_none() => Step::Match,
            ast::Stmt::Loop(_) | ast::Stmt::While(_) | ast::Stmt::For(_) | ast::Stmt::ForOf(_) => {
                Step::Skip
            }
            _ => Step::Descend,
        }
    }
}

/// Searches for `break <label>` targeting `label`, treating an inner
/// labeled block that reuses the same name as a shadowing scope.
struct BreakToLabel<'a> {
    label: &'a str,
}

impl AstTreeProbe<'_> for BreakToLabel<'_> {
    fn check_stmt(&mut self, stmt: &ast::Stmt) -> Step {
        match stmt {
            ast::Stmt::Break(b) if b.label.as_deref() == Some(self.label) => Step::Match,
            ast::Stmt::LabeledBlock(lb) if lb.label == self.label => Step::Skip,
            _ => Step::Descend,
        }
    }

    fn check_expr(&mut self, expr: &ast::Expr) -> Step {
        match expr {
            ast::Expr::LabeledBlock(lb) if lb.label == self.label => Step::Skip,
            _ => Step::Descend,
        }
    }
}

fn loop_body_can_escape(body: &ast::Block) -> bool {
    any_in_tree(body, &mut LoopEscape)
}

fn block_can_break_to_label(block: &ast::Block, label: &str) -> bool {
    any_in_tree(block, &mut BreakToLabel { label })
}

/// Whether `probe` matches anywhere in `block`. The walk stops at a closure
/// body and at a local item, which have control flow of their own.
pub(crate) fn any_in_tree<'t, P: AstTreeProbe<'t>>(block: &'t ast::Block, probe: &mut P) -> bool {
    let mut search = Search {
        probe,
        found: false,
    };
    search.visit_block(block);
    search.found
}

struct Search<'p, P> {
    probe: &'p mut P,
    found: bool,
}

impl<'t, P: AstTreeProbe<'t>> AstVisitor<'t> for Search<'_, P> {
    fn visit_stmt(&mut self, stmt: &'t ast::Stmt) {
        if self.found {
            return;
        }
        match self.probe.check_stmt(stmt) {
            Step::Match => self.found = true,
            Step::Skip => {}
            Step::Descend => ast::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'t ast::Expr) {
        if self.found {
            return;
        }
        match self.probe.check_expr(expr) {
            Step::Match => self.found = true,
            Step::Skip => {}
            Step::Descend if matches!(expr, ast::Expr::Closure(_)) => {}
            Step::Descend => ast::walk_expr(self, expr),
        }
    }

    fn visit_item(&mut self, _item: &'t ast::Item) {}
}

/// [`block_always_exits`] over TIR.
pub(crate) fn tir_block_always_exits(tt: &TypeTable, block: &TirBlock) -> bool {
    block.stmts.iter().any(|s| tir_stmt_always_exits(tt, s))
}

/// Whether control never reaches past `stmt`.
pub(crate) fn tir_stmt_always_exits(tt: &TypeTable, stmt: &TirStmt) -> bool {
    match &stmt.kind {
        TirStmtKind::Let { value, .. }
        | TirStmtKind::LetDestructure { value, .. }
        | TirStmtKind::Expr(value)
        | TirStmtKind::TaskReturn { value }
        | TirStmtKind::VariadicForOf {
            iterable: value, ..
        } => tir_expr_always_exits(tt, value),
        TirStmtKind::Return { .. } | TirStmtKind::Break { .. } | TirStmtKind::Continue => true,
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => tir_if_always_exits(tt, condition, then_block, else_block.as_ref()),
        TirStmtKind::Loop { body } => !tir_loop_can_escape(body),
        TirStmtKind::LabeledBlock { label, block } => {
            tir_labeled_block_always_exits(tt, block, label)
        }
    }
}

fn tir_if_always_exits(
    tt: &TypeTable,
    condition: &TirExpr,
    then_block: &TirBlock,
    else_block: Option<&TirBlock>,
) -> bool {
    tir_expr_always_exits(tt, condition)
        || else_block.is_some_and(|else_block| {
            tir_block_always_exits(tt, then_block) && tir_block_always_exits(tt, else_block)
        })
}

fn tir_labeled_block_always_exits(tt: &TypeTable, block: &TirBlock, label: &str) -> bool {
    tir_block_always_exits(tt, block) && !tir_breaks_to(block, label)
}

/// Whether evaluating `expr` never completes.
fn tir_expr_always_exits(tt: &TypeTable, expr: &TirExpr) -> bool {
    if tt.is_never(expr.type_id) {
        return true;
    }
    let exits = |e: &TirExpr| tir_expr_always_exits(tt, e);
    match &expr.kind {
        TirExprKind::GlobalVarSet { value: inner, .. }
        | TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::Move { expr: inner }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        // The body runs once per element, and a tuple may have none.
        | TirExprKind::VariadicTupleComprehension {
            iterable: inner, ..
        } => exits(inner),
        TirExprKind::Binary { left, op, right } => {
            exits(left) || (!op.is_short_circuit() && exits(right))
        }
        TirExprKind::Assign {
            target: left,
            value: right,
        }
        | TirExprKind::Index {
            expr: left,
            index: right,
        } => exits(left) || exits(right),
        TirExprKind::Call { args, .. } => args.iter().any(|arg| exits(&arg.expr)),
        TirExprKind::CmRawCall { args, .. } => args.iter().any(exits),
        TirExprKind::IndirectCall { callee, args } => exits(callee) || args.iter().any(exits),
        TirExprKind::Block(block) => tir_block_always_exits(tt, block),
        TirExprKind::LabeledBlock { label, block, .. } => {
            tir_labeled_block_always_exits(tt, block, label)
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => tir_if_always_exits(tt, condition, then_branch, else_branch.as_ref()),
        TirExprKind::Match { expr, arms } => {
            exits(expr) || (!arms.is_empty() && arms.iter().all(|arm| exits(&arm.body)))
        }
        TirExprKind::StructLiteral { fields, .. } => fields.iter().any(|f| exits(&f.value)),
        TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements } => {
            elements.iter().any(exits)
        }
        TirExprKind::VariantConstruct { payload, .. } => payload.as_deref().is_some_and(exits),
        TirExprKind::TemplateString { parts } => parts.iter().any(|part| match part {
            TirTemplatePart::Interpolation { expr, .. } => exits(expr),
            TirTemplatePart::Literal(_) => false,
        }),
        TirExprKind::WithHandler { bindings, body, .. } => {
            bindings.iter().any(|b| exits(&b.handler)) || tir_block_always_exits(tt, body)
        }
        // `len` reads only the type, and a pack may expand to no call.
        TirExprKind::TupleLen { .. }
        | TirExprKind::TypePackExpansion { .. }
        // A closure's body runs when it is called, not where it is written.
        | TirExprKind::Closure { .. }
        | TirExprKind::IntLiteral { .. }
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
        | TirExprKind::EnumConstruct { .. } => false,
    }
}

/// Whether `body` holds an unlabeled `break` its loop binds.
fn tir_loop_can_escape(body: &TirBlock) -> bool {
    let mut probe = TirBreakSearch {
        label: None,
        found: false,
    };
    probe.visit_block(body);
    probe.found
}

/// Whether `block` holds a `break <label>` that an inner block of the same name
/// does not take.
fn tir_breaks_to(block: &TirBlock, label: &str) -> bool {
    let mut probe = TirBreakSearch {
        label: Some(label),
        found: false,
    };
    probe.visit_block(block);
    probe.found
}

/// Searches for a `break` reaching one target: the loop being searched when
/// `label` is `None`, else the labeled block of that name.
struct TirBreakSearch<'a> {
    label: Option<&'a str>,
    found: bool,
}

impl TirRefVisitor for TirBreakSearch<'_> {
    fn visit_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Break { label, .. } if label.as_deref() == self.label => {
                self.found = true;
            }
            TirStmtKind::Loop { .. } if self.label.is_none() => {}
            TirStmtKind::LabeledBlock { label, .. } if Some(label.as_str()) == self.label => {}
            _ => self.walk_stmt(stmt),
        }
    }

    fn visit_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::LabeledBlock { label, .. } if Some(label.as_str()) == self.label => {}
            _ => self.walk_expr_in_frame(expr),
        }
    }
}
