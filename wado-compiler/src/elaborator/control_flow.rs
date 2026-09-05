//! Control-flow analyses for missing-return diagnosis (WEP 2026-05-26): walks
//! over the parsed AST reading types from `TypeAnnotations::expression_types`,
//! so the body walk answers them without a `TirBlock` to read.
//! Both the diagnostic point and reify's closure return-type derivation build a
//! [`CtrlFlowCtx`] and dispatch to the same free functions, arm for arm.

use std::cell::RefCell;

use crate::ast;
use crate::hashmap::IndexMap;
use crate::tir::{TypeId, TypeTable};
use crate::token::Span;

use super::types::LoopJump;

/// Lookup context for AST control-flow walks. Holds the per-AstId
/// type table; `expression_types` is keyed by globally-unique `AstId`,
/// so no module qualifier is needed. `type_table` backs the definite-type
/// filter the missing-return walk applies, since `expression_types` records
/// indefinite types too.
#[derive(Clone, Copy)]
pub(super) struct CtrlFlowCtx<'a> {
    pub(super) expression_types: &'a IndexMap<crate::ast::AstId, TypeId>,
    pub(super) type_table: &'a RefCell<TypeTable>,
}

impl CtrlFlowCtx<'_> {
    fn type_of(&self, expr: &ast::Expr) -> Option<TypeId> {
        self.type_of_id(expr.id())
    }

    fn type_of_id(&self, id: crate::ast::AstId) -> Option<TypeId> {
        self.expression_types.get(&id).copied()
    }

    /// `type_of`, but an indefinite recorded type counts as "no definite type"
    /// (`None`), so an unresolved-`null` return value does not masquerade as a
    /// concrete return type in the missing-return walk.
    fn definite_type_of(&self, expr: &ast::Expr) -> Option<TypeId> {
        self.type_of(expr)
            .filter(|t| !self.type_table.borrow().is_indefinite(*t))
    }

    fn is_never(&self, expr: &ast::Expr) -> bool {
        self.type_of(expr) == Some(TypeTable::NEVER)
    }
}

/// Whether every control path through `block` exits before reaching
/// the end. AST mirror of `Elaborator::block_always_exits`.
pub(super) fn block_always_exits(ctx: CtrlFlowCtx<'_>, block: &ast::Block) -> bool {
    block_always_exits_past(ctx, block, &[])
}

/// Whether control can reach the end of a labeled block's body, which decides
/// whether its trailing statement is a branch of the block.
pub(super) fn labeled_block_falls_through(
    ctx: CtrlFlowCtx<'_>,
    block: &ast::Block,
    label: &str,
) -> bool {
    !block_always_exits_past(ctx, block, &[label])
}

/// `exit_labels` name blocks a `break` leaves along with the region analysed,
/// so such a break never reaches the statement after a loop it sits in.
fn block_always_exits_past(ctx: CtrlFlowCtx<'_>, block: &ast::Block, exit_labels: &[&str]) -> bool {
    block
        .stmts
        .iter()
        .any(|s| stmt_always_exits(ctx, s, exit_labels))
}

fn stmt_always_exits(ctx: CtrlFlowCtx<'_>, stmt: &ast::Stmt, exit_labels: &[&str]) -> bool {
    match stmt {
        ast::Stmt::Return(_) | ast::Stmt::Break(_) | ast::Stmt::Continue(_) => true,
        ast::Stmt::Expr(e) => expr_always_exits_past(ctx, &e.expr, exit_labels),
        ast::Stmt::If(if_stmt) => {
            if let Some(else_block) = &if_stmt.else_block {
                block_always_exits_past(ctx, &if_stmt.then_block, exit_labels)
                    && block_always_exits_past(ctx, else_block, exit_labels)
            } else {
                false
            }
        }
        ast::Stmt::Loop(loop_stmt) => !loop_body_can_escape(ctx, &loop_stmt.body, exit_labels),
        // `while true` is the same loop written differently: reify desugars it
        // to `loop { if !true { break } B }`, so only a `break` in `B` ends it.
        ast::Stmt::While(w) if condition_is_always_true(&w.condition) => {
            !loop_body_can_escape(ctx, &w.body, exit_labels)
        }
        ast::Stmt::LabeledBlock(lb) => {
            block_always_exits_past(ctx, &lb.block, exit_labels)
                && !block_can_break_to_label(ctx, &lb.block, &lb.label)
        }
        // `match { ... }` as a top-level statement diverges when every
        // arm diverges. AST keeps it as `Stmt::Match`, but the TIR
        // walker saw the same construct as `TirStmtKind::Expr` over an
        // `Expr::Match` — route through the expression-level check.
        ast::Stmt::Match(m) => {
            !m.arms.is_empty()
                && m.arms
                    .iter()
                    .all(|a| expr_always_exits_past(ctx, &a.body, exit_labels))
        }
        _ => false,
    }
}

fn condition_is_always_true(condition: &ast::Condition) -> bool {
    matches!(condition, ast::Condition::Expr(ast::Expr::Literal(lit))
        if matches!(lit.value, ast::Literal::Bool(true)))
}

fn expr_always_exits_past(ctx: CtrlFlowCtx<'_>, expr: &ast::Expr, exit_labels: &[&str]) -> bool {
    if ctx.is_never(expr) {
        return true;
    }
    match expr {
        ast::Expr::Block(block) => block_always_exits_past(ctx, block, exit_labels),
        ast::Expr::LabeledBlock(lb) => {
            block_always_exits_past(ctx, &lb.block, exit_labels)
                && !block_can_break_to_label(ctx, &lb.block, &lb.label)
        }
        ast::Expr::If(if_expr) => {
            if let Some(else_block) = &if_expr.else_block {
                block_always_exits_past(ctx, &if_expr.then_block, exit_labels)
                    && block_always_exits_past(ctx, else_block, exit_labels)
            } else {
                false
            }
        }
        ast::Expr::Match(m) => {
            !m.arms.is_empty()
                && m.arms
                    .iter()
                    .all(|a| expr_always_exits_past(ctx, &a.body, exit_labels))
        }
        // `resume value` transfers control out of the enclosing
        // handler method — lowered to `return value`.
        ast::Expr::Resume(_) => true,
        // `with … do { body }`: defer to body's definite-exit.
        ast::Expr::WithHandler(wh) => block_always_exits_past(ctx, &wh.body, exit_labels),
        _ => false,
    }
}

/// Result type of `block` — its trailing expression's type, or `Unit`. The AST
/// mirror of [`crate::tir::block_result_type`], reading
/// `expression_types[(module, id)]` so a block's value type needs no body TIR.
/// Unlike the missing-return walk it does *not* filter an indefinite type: an
/// unresolved-`null` tail stays `Option<!>`, which its caller must see to type
/// the branch.
pub(super) fn block_result_type(ctx: CtrlFlowCtx<'_>, block: &ast::Block) -> TypeId {
    block
        .stmts
        .last()
        .and_then(|s| match s {
            ast::Stmt::Expr(e) => ctx.type_of(&e.expr),
            // A trailing `match` lowers to `TirStmtKind::Expr(match)`, so its
            // recorded type is the block's value (the body walk records
            // `match.id` for both stmt-position and trailing-with-expected
            // matches). `Stmt::Match` is an AST surface for the expression
            // form, so it is read like one.
            ast::Stmt::Match(m) => ctx.type_of_id(m.id),
            ast::Stmt::If(if_stmt) => if_stmt.else_block.as_ref().and_then(|else_block| {
                let (then_type, else_type) = (
                    block_result_type(ctx, &if_stmt.then_block),
                    block_result_type(ctx, else_block),
                );
                crate::tir::agree_branch_types(&ctx.type_table.borrow(), then_type, else_type)
            }),
            ast::Stmt::Return(_) | ast::Stmt::Break(_) | ast::Stmt::Continue(_) => {
                Some(TypeTable::NEVER)
            }
            _ => None,
        })
        .unwrap_or(TypeTable::UNIT)
}

/// Spans of unresolved-`null` tail values in `expr` whose recorded type
/// still contains UNKNOWN. AST mirror of `patch_unresolved_null` (which
/// mutated the built TIR's `null.type_id`): only the *tail* positions are
/// walked (block tails, `if`/`match` arms), and a tail `null` that cannot
/// fit a non-`Option` result type is collected for the caller to report.
/// The TIR-mutation half was dead (reify rebuilds the `null` from its
/// `expected_type`), so only the diagnostic survives.
pub(super) fn collect_unresolved_null_tails(
    ctx: CtrlFlowCtx<'_>,
    expr: &ast::Expr,
    out: &mut Vec<Span>,
) {
    match expr {
        ast::Expr::Literal(lit) if matches!(lit.value, ast::Literal::Null) => {
            if ctx
                .type_of_id(lit.id)
                .is_some_and(|t| ctx.type_table.borrow().is_indefinite(t))
            {
                out.push(lit.span);
            }
        }
        ast::Expr::Block(block) => collect_unresolved_null_tails_in_block(ctx, block, out),
        ast::Expr::If(if_expr) => {
            collect_unresolved_null_tails_in_block(ctx, &if_expr.then_block, out);
            if let Some(eb) = &if_expr.else_block {
                collect_unresolved_null_tails_in_block(ctx, eb, out);
            }
        }
        ast::Expr::Match(m) => {
            for arm in &m.arms {
                collect_unresolved_null_tails(ctx, &arm.body, out);
            }
        }
        _ => {}
    }
}

/// Tail-position helper for [`collect_unresolved_null_tails`], mirroring
/// `patch_unresolved_null_in_block`. A trailing `match` lowers to
/// `TirStmtKind::Expr(match)`, so it is descended here like the expression
/// form.
pub(super) fn collect_unresolved_null_tails_in_block(
    ctx: CtrlFlowCtx<'_>,
    block: &ast::Block,
    out: &mut Vec<Span>,
) {
    match block.stmts.last() {
        Some(ast::Stmt::Expr(e)) => collect_unresolved_null_tails(ctx, &e.expr, out),
        Some(ast::Stmt::Match(m)) => {
            for arm in &m.arms {
                collect_unresolved_null_tails(ctx, &arm.body, out);
            }
        }
        Some(ast::Stmt::If(if_stmt)) => {
            if let Some(eb) = &if_stmt.else_block {
                collect_unresolved_null_tails_in_block(ctx, &if_stmt.then_block, out);
                collect_unresolved_null_tails_in_block(ctx, eb, out);
            }
        }
        _ => {}
    }
}

/// First return statement's value type discovered while walking
/// `block`. AST mirror of `Elaborator::find_return_type_in_block`.
pub(super) fn find_return_type_in_block(
    ctx: CtrlFlowCtx<'_>,
    block: &ast::Block,
) -> Option<TypeId> {
    for stmt in &block.stmts {
        if let Some(t) = find_return_type_in_stmt(ctx, stmt) {
            return Some(t);
        }
    }
    None
}

fn find_return_type_in_stmt(ctx: CtrlFlowCtx<'_>, stmt: &ast::Stmt) -> Option<TypeId> {
    match stmt {
        ast::Stmt::Return(r) => match &r.value {
            // The value's type comes from `expression_types`; an ERROR /
            // UNKNOWN-containing type counts as "not recorded" (via
            // `definite_type_of`), so the caller treats this arm as "no
            // return type found" rather than silently fabricating `Unit` and
            // producing a misleading missing-return diagnostic.
            Some(expr) => ctx.definite_type_of(expr),
            None => Some(TypeTable::UNIT),
        },
        ast::Stmt::If(if_stmt) => {
            if let Some(t) = find_return_type_in_block(ctx, &if_stmt.then_block) {
                return Some(t);
            }
            if let Some(else_block) = &if_stmt.else_block
                && let Some(t) = find_return_type_in_block(ctx, else_block)
            {
                return Some(t);
            }
            None
        }
        ast::Stmt::Loop(loop_stmt) => find_return_type_in_block(ctx, &loop_stmt.body),
        ast::Stmt::LabeledBlock(lb) => find_return_type_in_block(ctx, &lb.block),
        ast::Stmt::Expr(e) => find_return_type_in_expr(ctx, &e.expr),
        // Top-level `match` statement — recurse through arms like the
        // expression form; `Stmt::Match` is just an AST surface that
        // lowers to the same TIR shape.
        ast::Stmt::Match(m) => {
            for arm in &m.arms {
                if let Some(t) = find_return_type_in_expr(ctx, &arm.body) {
                    return Some(t);
                }
            }
            None
        }
        ast::Stmt::Let(l) => l
            .else_block
            .as_ref()
            .and_then(|b| find_return_type_in_block(ctx, b)),
        _ => None,
    }
}

pub(super) fn find_return_type_in_expr(ctx: CtrlFlowCtx<'_>, expr: &ast::Expr) -> Option<TypeId> {
    match expr {
        ast::Expr::Match(m) => {
            for arm in &m.arms {
                if let Some(t) = find_return_type_in_expr(ctx, &arm.body) {
                    return Some(t);
                }
            }
            None
        }
        ast::Expr::Block(block) => find_return_type_in_block(ctx, block),
        ast::Expr::If(if_expr) => {
            if let Some(t) = find_return_type_in_block(ctx, &if_expr.then_block) {
                return Some(t);
            }
            if let Some(else_block) = &if_expr.else_block
                && let Some(t) = find_return_type_in_block(ctx, else_block)
            {
                return Some(t);
            }
            None
        }
        ast::Expr::LabeledBlock(lb) => find_return_type_in_block(ctx, &lb.block),
        ast::Expr::WithHandler(wh) => find_return_type_in_block(ctx, &wh.body),
        // `resume value` lowers to `return value` in the MVP, so a
        // body whose tail is `resume X` satisfies missing-return as
        // if it were `return X`. Same `expression_types`-missing
        // rule as `Stmt::Return` above: yield `None` rather than
        // synthesising `Unit`, to keep ERROR-recovery diagnostics
        // free of bogus missing-return reports.
        ast::Expr::Resume(r) => ctx.definite_type_of(&r.value),
        _ => None,
    }
}

/// Per-node decision for the generic AST predicate walk used by
/// [`loop_body_can_escape`] and [`block_can_break_to_label`].
#[derive(Clone, Copy)]
enum Step {
    /// Predicate matched — short-circuit and return `true`.
    Match,
    /// Not matched here, and do not descend into children.
    Skip,
    /// Not matched here; continue descending into children.
    Descend,
}

trait AstTreeProbe {
    fn check_stmt(&mut self, stmt: &ast::Stmt) -> Step;
    fn check_expr(&mut self, _expr: &ast::Expr) -> Step {
        Step::Descend
    }
}

/// Searches a loop body for a `break` that reaches the statement after the
/// loop. `inner_labels` holds the labels whose `break` does not: those declared
/// inside the body, and the seeded `exit_labels`.
struct LoopEscape {
    inner_labels: Vec<String>,
}

impl AstTreeProbe for LoopEscape {
    fn check_stmt(&mut self, stmt: &ast::Stmt) -> Step {
        match stmt {
            ast::Stmt::Break(b) => match &b.label {
                None => Step::Match,
                Some(l) => {
                    if self.inner_labels.iter().any(|owned| owned == l) {
                        Step::Skip
                    } else {
                        Step::Match
                    }
                }
            },
            // Nested loops / for-of / while / C-style for catch their own
            // unlabeled breaks.
            ast::Stmt::Loop(_) | ast::Stmt::While(_) | ast::Stmt::For(_) | ast::Stmt::ForOf(_) => {
                Step::Skip
            }
            ast::Stmt::LabeledBlock(lb) => {
                self.inner_labels.push(lb.label.clone());
                Step::Descend
            }
            _ => Step::Descend,
        }
    }

    fn check_expr(&mut self, expr: &ast::Expr) -> Step {
        if let ast::Expr::LabeledBlock(lb) = expr {
            self.inner_labels.push(lb.label.clone());
        }
        Step::Descend
    }
}

/// Searches for `break <label>` targeting `label`, treating an inner
/// labeled block that reuses the same name as a shadowing scope.
struct BreakToLabel<'a> {
    label: &'a str,
}

impl AstTreeProbe for BreakToLabel<'_> {
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

/// Collects the spans of `break <label>: <null>` values whose recorded
/// type is still unresolved. AST mirror of `NullBreakPatcher` (whose TIR
/// mutation was dead — reify rebuilds the `null` from its `expected_type`):
/// a matching break's value is walked for unresolved `null` tails via
/// [`collect_unresolved_null_tails`]; an inner labeled block reusing the
/// same name shadows it (its breaks target the inner block), and closure
/// bodies have their own control-flow scope (`any_in_expr` does not descend
/// into them).
struct NullBreakCollector<'a> {
    ctx: CtrlFlowCtx<'a>,
    label: &'a str,
    spans: Vec<Span>,
}

impl AstTreeProbe for NullBreakCollector<'_> {
    fn check_stmt(&mut self, stmt: &ast::Stmt) -> Step {
        match stmt {
            ast::Stmt::Break(b) if b.label.as_deref() == Some(self.label) && b.value.is_some() => {
                let value = b.value.as_ref().unwrap();
                collect_unresolved_null_tails(self.ctx, value, &mut self.spans);
                // The value was walked here; do not descend into it again
                // (mirrors `NullBreakPatcher::visit_stmt` returning after the
                // patch).
                Step::Skip
            }
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

/// Spans of `break <label>: null` values inside `block` that cannot fit the
/// labeled block's resolved non-`Option` result type. AST replacement for
/// the `NullBreakPatcher` pass.
pub(super) fn collect_unresolved_null_breaks(
    ctx: CtrlFlowCtx<'_>,
    block: &ast::Block,
    label: &str,
) -> Vec<Span> {
    let mut probe = NullBreakCollector {
        ctx,
        label,
        spans: Vec::new(),
    };
    any_in_tree(ctx, block, &mut probe);
    probe.spans
}

/// Searches for an unlabeled `break` / `continue` that no loop binds. A loop
/// binds every such jump inside it, so the walk stops at one.
struct UnboundLoopJump {
    found: Option<(LoopJump, Span)>,
}

impl AstTreeProbe for UnboundLoopJump {
    fn check_stmt(&mut self, stmt: &ast::Stmt) -> Step {
        match stmt {
            ast::Stmt::Break(b) if b.label.is_none() => {
                self.found = Some((LoopJump::Break, b.span));
                Step::Match
            }
            ast::Stmt::Continue(c) => {
                self.found = Some((LoopJump::Continue, c.span));
                Step::Match
            }
            ast::Stmt::Loop(_) | ast::Stmt::While(_) | ast::Stmt::For(_) | ast::Stmt::ForOf(_) => {
                Step::Skip
            }
            _ => Step::Descend,
        }
    }
}

/// Kind and span of the first `break` / `continue` in `block` that no enclosing
/// loop binds. WIR panics on one, so it has to be rejected here.
pub(super) fn find_unbound_loop_jump(
    ctx: CtrlFlowCtx<'_>,
    block: &ast::Block,
) -> Option<(LoopJump, Span)> {
    let mut probe = UnboundLoopJump { found: None };
    any_in_tree(ctx, block, &mut probe);
    probe.found
}

fn loop_body_can_escape(ctx: CtrlFlowCtx<'_>, body: &ast::Block, exit_labels: &[&str]) -> bool {
    let mut probe = LoopEscape {
        inner_labels: exit_labels.iter().map(|l| (*l).to_string()).collect(),
    };
    any_in_tree(ctx, body, &mut probe)
}

fn block_can_break_to_label(ctx: CtrlFlowCtx<'_>, block: &ast::Block, label: &str) -> bool {
    let mut probe = BreakToLabel { label };
    any_in_tree(ctx, block, &mut probe)
}

fn any_in_tree<P: AstTreeProbe>(ctx: CtrlFlowCtx<'_>, block: &ast::Block, probe: &mut P) -> bool {
    block.stmts.iter().any(|s| any_in_stmt(ctx, s, probe))
}

fn any_in_stmt<P: AstTreeProbe>(ctx: CtrlFlowCtx<'_>, stmt: &ast::Stmt, probe: &mut P) -> bool {
    match probe.check_stmt(stmt) {
        Step::Match => return true,
        Step::Skip => return false,
        Step::Descend => {}
    }
    match stmt {
        ast::Stmt::If(if_stmt) => {
            condition_any(ctx, &if_stmt.condition, probe)
                || any_in_tree(ctx, &if_stmt.then_block, probe)
                || if_stmt
                    .else_block
                    .as_ref()
                    .is_some_and(|b| any_in_tree(ctx, b, probe))
        }
        ast::Stmt::While(w) => {
            condition_any(ctx, &w.condition, probe) || any_in_tree(ctx, &w.body, probe)
        }
        ast::Stmt::For(f) => {
            f.init.as_ref().is_some_and(|s| any_in_stmt(ctx, s, probe))
                || f.condition
                    .as_ref()
                    .is_some_and(|c| condition_any(ctx, c, probe))
                || f.update
                    .as_ref()
                    .is_some_and(|e| any_in_expr(ctx, e, probe))
                || any_in_tree(ctx, &f.body, probe)
        }
        ast::Stmt::ForOf(fo) => {
            any_in_expr(ctx, &fo.iterable, probe) || any_in_tree(ctx, &fo.body, probe)
        }
        ast::Stmt::Loop(l) => any_in_tree(ctx, &l.body, probe),
        ast::Stmt::LabeledBlock(lb) => any_in_tree(ctx, &lb.block, probe),
        ast::Stmt::Match(m) => {
            any_in_expr(ctx, &m.expr, probe)
                || m.arms.iter().any(|arm| {
                    arm.guard
                        .as_ref()
                        .is_some_and(|g| any_in_expr(ctx, g, probe))
                        || any_in_expr(ctx, &arm.body, probe)
                })
        }
        ast::Stmt::Expr(e) => any_in_expr(ctx, &e.expr, probe),
        ast::Stmt::Let(l) => {
            l.value.as_ref().is_some_and(|v| any_in_expr(ctx, v, probe))
                || l.else_block
                    .as_ref()
                    .is_some_and(|b| any_in_tree(ctx, b, probe))
        }
        ast::Stmt::TaskReturn(tr) => any_in_expr(ctx, &tr.value, probe),
        ast::Stmt::Return(r) => r.value.as_ref().is_some_and(|v| any_in_expr(ctx, v, probe)),
        ast::Stmt::Break(b) => b.value.as_ref().is_some_and(|v| any_in_expr(ctx, v, probe)),
        ast::Stmt::Assert(a) => {
            any_in_expr(ctx, &a.condition, probe)
                || a.message
                    .as_ref()
                    .is_some_and(|m| any_in_expr(ctx, m, probe))
        }
        ast::Stmt::Continue(_) => false,
        // A local type/impl declaration's methods have their own
        // control-flow scope, like a closure body — a `break`/`continue`/
        // loop-escape inside one is not part of the enclosing function.
        ast::Stmt::Item(_) => false,
        // Parser error-recovery placeholder: nothing to probe.
        ast::Stmt::Error(_) => false,
    }
}

fn condition_any<P: AstTreeProbe>(
    ctx: CtrlFlowCtx<'_>,
    cond: &ast::Condition,
    probe: &mut P,
) -> bool {
    match cond {
        ast::Condition::Expr(e) => any_in_expr(ctx, e, probe),
        ast::Condition::LetChain { elements, .. } => elements.iter().any(|el| match el {
            ast::ConditionElement::Let { expr, .. } => any_in_expr(ctx, expr, probe),
            ast::ConditionElement::Expr(e) => any_in_expr(ctx, e, probe),
        }),
    }
}

fn any_in_expr<P: AstTreeProbe>(ctx: CtrlFlowCtx<'_>, expr: &ast::Expr, probe: &mut P) -> bool {
    match probe.check_expr(expr) {
        Step::Match => return true,
        Step::Skip => return false,
        Step::Descend => {}
    }
    // Descend into every sub-expression / sub-block carrier except
    // `Closure`, whose body has its own control-flow scope.
    match expr {
        // Pure leaves.
        ast::Expr::Ident(_) | ast::Expr::Literal(_) => false,

        ast::Expr::Block(block) => any_in_tree(ctx, block, probe),
        ast::Expr::LabeledBlock(lb) => any_in_tree(ctx, &lb.block, probe),

        ast::Expr::If(if_expr) => {
            condition_any(ctx, &if_expr.condition, probe)
                || any_in_tree(ctx, &if_expr.then_block, probe)
                || if_expr
                    .else_block
                    .as_ref()
                    .is_some_and(|b| any_in_tree(ctx, b, probe))
        }
        ast::Expr::Match(m) => {
            any_in_expr(ctx, &m.expr, probe)
                || m.arms.iter().any(|arm| {
                    arm.guard
                        .as_ref()
                        .is_some_and(|g| any_in_expr(ctx, g, probe))
                        || any_in_expr(ctx, &arm.body, probe)
                })
        }
        ast::Expr::Matches(m) => {
            any_in_expr(ctx, &m.expr, probe)
                || m.guard.as_ref().is_some_and(|g| any_in_expr(ctx, g, probe))
        }

        ast::Expr::Resume(r) => any_in_expr(ctx, &r.value, probe),
        ast::Expr::Binary(b) => {
            any_in_expr(ctx, &b.left, probe) || any_in_expr(ctx, &b.right, probe)
        }
        ast::Expr::Unary(u) => any_in_expr(ctx, &u.expr, probe),
        ast::Expr::Cast(c) => any_in_expr(ctx, &c.expr, probe),
        ast::Expr::FieldAccess(f) => any_in_expr(ctx, &f.expr, probe),
        ast::Expr::Assign(a) => {
            any_in_expr(ctx, &a.target, probe) || any_in_expr(ctx, &a.value, probe)
        }
        ast::Expr::CompoundAssign(a) => {
            any_in_expr(ctx, &a.target, probe) || any_in_expr(ctx, &a.value, probe)
        }
        ast::Expr::ComparisonChain(c) => {
            any_in_expr(ctx, &c.first, probe)
                || c.comparisons
                    .iter()
                    .any(|cmp| any_in_expr(ctx, &cmp.right, probe))
        }
        ast::Expr::Index(i) => {
            any_in_expr(ctx, &i.expr, probe) || any_in_expr(ctx, &i.index, probe)
        }
        ast::Expr::Call(c) => {
            any_in_expr(ctx, &c.callee, probe) || c.args.iter().any(|a| any_in_expr(ctx, a, probe))
        }
        ast::Expr::MethodCall(mc) => {
            any_in_expr(ctx, &mc.receiver, probe)
                || mc.args.iter().any(|a| any_in_expr(ctx, a, probe))
        }
        ast::Expr::StaticMethodCall(sc) => sc.args.iter().any(|a| any_in_expr(ctx, a, probe)),
        ast::Expr::StructLiteral(s) => {
            s.fields.iter().any(|f| any_in_expr(ctx, &f.value, probe))
                || s.spreads.iter().any(|sp| any_in_expr(ctx, &sp.expr, probe))
        }
        ast::Expr::TupleLiteral(t) => t.elements.iter().any(|e| any_in_expr(ctx, e, probe)),
        ast::Expr::TupleComprehension(c) => {
            any_in_expr(ctx, &c.iterable, probe) || any_in_expr(ctx, &c.body, probe)
        }
        ast::Expr::Spread(inner, _) => any_in_expr(ctx, inner, probe),
        ast::Expr::Range(r) => any_in_expr(ctx, &r.start, probe) || any_in_expr(ctx, &r.end, probe),
        ast::Expr::TryOp(t) => any_in_expr(ctx, &t.expr, probe),
        ast::Expr::TemplateString(ts) => ts.parts.iter().any(|p| match p {
            ast::TemplatePart::Interpolation { expr, .. } => any_in_expr(ctx, expr, probe),
            ast::TemplatePart::String(_) => false,
        }),
        ast::Expr::TaggedTemplate(t) => {
            any_in_expr(ctx, &t.tag, probe)
                || t.template.parts.iter().any(|p| match p {
                    ast::TemplatePart::Interpolation { expr, .. } => any_in_expr(ctx, expr, probe),
                    ast::TemplatePart::String(_) => false,
                })
        }
        ast::Expr::WithHandler(wh) => {
            wh.handlers
                .iter()
                .any(|b| any_in_expr(ctx, &b.handler, probe))
                || any_in_tree(ctx, &wh.body, probe)
        }

        // Closures stay in their own scope.
        ast::Expr::Closure(_) => false,

        // Parser error-recovery placeholder: nothing to probe.
        ast::Expr::Error(_) => false,
    }
}
