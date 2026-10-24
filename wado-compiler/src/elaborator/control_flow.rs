//! Control-flow analyses for missing-return diagnosis (WEP 2026-05-26): walks
//! over the parsed AST reading types from `TypeAnnotations::expression_types`,
//! so the body walk answers them without a `TirBlock` to read.
//! Both the diagnostic point and reify's closure return-type derivation build a
//! [`CtrlFlowCtx`] and dispatch to the same free functions, arm for arm. Whether
//! control reaches past a construct is [`crate::divergence`]'s to answer.

use std::cell::RefCell;

use crate::ast;
use crate::hashmap::IndexMap;
use crate::tir::TypeId;
use crate::token::Span;
use crate::type_table::TypeTable;

use super::types::LoopJump;
use crate::ast::AstId;
use crate::divergence::{AstTreeProbe, ExprTypes, Step, any_in_tree};
use crate::tir::agree_branch_types;

/// Lookup context for AST control-flow walks. Holds the per-AstId
/// type table; `expression_types` is keyed by globally-unique `AstId`,
/// so no module qualifier is needed. `type_table` backs the definite-type
/// filter the missing-return walk applies, since `expression_types` records
/// indefinite types too.
#[derive(Clone, Copy)]
pub(super) struct CtrlFlowCtx<'a> {
    pub(super) expression_types: &'a IndexMap<AstId, TypeId>,
    pub(super) type_table: &'a RefCell<TypeTable>,
}

impl CtrlFlowCtx<'_> {
    fn type_of(&self, expr: &ast::Expr) -> Option<TypeId> {
        self.type_of_id(expr.id())
    }

    fn type_of_id(&self, id: AstId) -> Option<TypeId> {
        self.expression_types.get(&id).copied()
    }

    /// `type_of`, but an indefinite recorded type counts as "no definite type"
    /// (`None`), so an unresolved-`null` return value does not masquerade as a
    /// concrete return type in the missing-return walk.
    fn definite_type_of(&self, expr: &ast::Expr) -> Option<TypeId> {
        self.type_of(expr)
            .filter(|t| !self.type_table.borrow().is_indefinite(*t))
    }
}

impl ExprTypes for CtrlFlowCtx<'_> {
    fn is_never(&self, expr: &ast::Expr) -> bool {
        self.type_of(expr)
            .is_some_and(|t| self.type_table.borrow().is_never(t))
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
                agree_branch_types(&ctx.type_table.borrow(), then_type, else_type)
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
    return_types_in_block(ctx, block).into_iter().next()
}

/// Every return statement's value type in `block`, in source order.
pub(super) fn return_types_in_block(ctx: CtrlFlowCtx<'_>, block: &ast::Block) -> Vec<TypeId> {
    let mut out = Vec::new();
    collect_return_types_in_block(ctx, block, &mut out);
    out
}

fn collect_return_types_in_block(ctx: CtrlFlowCtx<'_>, block: &ast::Block, out: &mut Vec<TypeId>) {
    for stmt in &block.stmts {
        collect_return_types_in_stmt(ctx, stmt, out);
    }
}

fn collect_return_types_in_stmt(ctx: CtrlFlowCtx<'_>, stmt: &ast::Stmt, out: &mut Vec<TypeId>) {
    match stmt {
        ast::Stmt::Return(r) => out.extend(match &r.value {
            // The value's type comes from `expression_types`; an ERROR /
            // UNKNOWN-containing type counts as "not recorded" (via
            // `definite_type_of`), so the caller treats this arm as "no
            // return type found" rather than silently fabricating `Unit` and
            // producing a misleading missing-return diagnostic.
            Some(expr) => ctx.definite_type_of(expr),
            None => Some(TypeTable::UNIT),
        }),
        ast::Stmt::If(if_stmt) => {
            collect_return_types_in_block(ctx, &if_stmt.then_block, out);
            if let Some(else_block) = &if_stmt.else_block {
                collect_return_types_in_block(ctx, else_block, out);
            }
        }
        ast::Stmt::Loop(loop_stmt) => collect_return_types_in_block(ctx, &loop_stmt.body, out),
        ast::Stmt::LabeledBlock(lb) => collect_return_types_in_block(ctx, &lb.block, out),
        ast::Stmt::Expr(e) => collect_return_types_in_expr(ctx, &e.expr, out),
        // Top-level `match` statement — recurse through arms like the
        // expression form; `Stmt::Match` is just an AST surface that
        // lowers to the same TIR shape.
        ast::Stmt::Match(m) => {
            for arm in &m.arms {
                collect_return_types_in_expr(ctx, &arm.body, out);
            }
        }
        ast::Stmt::Let(l) => {
            if let Some(b) = &l.else_block {
                collect_return_types_in_block(ctx, b, out);
            }
        }
        _ => {}
    }
}

fn collect_return_types_in_expr(ctx: CtrlFlowCtx<'_>, expr: &ast::Expr, out: &mut Vec<TypeId>) {
    match expr {
        ast::Expr::Match(m) => {
            for arm in &m.arms {
                collect_return_types_in_expr(ctx, &arm.body, out);
            }
        }
        ast::Expr::Block(block) => collect_return_types_in_block(ctx, block, out),
        ast::Expr::If(if_expr) => {
            collect_return_types_in_block(ctx, &if_expr.then_block, out);
            if let Some(else_block) = &if_expr.else_block {
                collect_return_types_in_block(ctx, else_block, out);
            }
        }
        ast::Expr::LabeledBlock(lb) => collect_return_types_in_block(ctx, &lb.block, out),
        ast::Expr::WithHandler(wh) => collect_return_types_in_block(ctx, &wh.body, out),
        _ => {}
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

impl AstTreeProbe<'_> for NullBreakCollector<'_> {
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
    any_in_tree(block, &mut probe);
    probe.spans
}

/// The values every `break <label>: <value>` inside `block` yields, an inner
/// labeled block reusing the name shadowing it.
pub(super) fn break_values<'t>(block: &'t ast::Block, label: &str) -> Vec<&'t ast::Expr> {
    let mut probe = BreakValues {
        label,
        values: Vec::new(),
    };
    any_in_tree(block, &mut probe);
    probe.values
}

struct BreakValues<'l, 't> {
    label: &'l str,
    values: Vec<&'t ast::Expr>,
}

impl<'t> AstTreeProbe<'t> for BreakValues<'_, 't> {
    fn check_stmt(&mut self, stmt: &'t ast::Stmt) -> Step {
        match stmt {
            ast::Stmt::Break(b) if b.label.as_deref() == Some(self.label) => {
                self.values.extend(b.value.as_deref());
                Step::Skip
            }
            ast::Stmt::LabeledBlock(lb) if lb.label == self.label => Step::Skip,
            _ => Step::Descend,
        }
    }

    fn check_expr(&mut self, expr: &'t ast::Expr) -> Step {
        match expr {
            ast::Expr::LabeledBlock(lb) if lb.label == self.label => Step::Skip,
            _ => Step::Descend,
        }
    }
}

/// Whether the body names its result anywhere. The walk stops at a closure
/// body, which is a function of its own and where `task return` is rejected.
pub(super) fn block_delivers(block: &ast::Block) -> bool {
    any_in_tree(block, &mut TaskReturnPresent)
}

struct TaskReturnPresent;

impl AstTreeProbe<'_> for TaskReturnPresent {
    fn check_stmt(&mut self, stmt: &ast::Stmt) -> Step {
        match stmt {
            ast::Stmt::TaskReturn(_) => Step::Match,
            _ => Step::Descend,
        }
    }
}

/// Searches for an unlabeled `break` / `continue` that no loop binds. A loop
/// binds every such jump inside it, so the walk stops at one.
struct UnboundLoopJump {
    found: Option<(LoopJump, Span)>,
}

impl AstTreeProbe<'_> for UnboundLoopJump {
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
pub(super) fn find_unbound_loop_jump(block: &ast::Block) -> Option<(LoopJump, Span)> {
    let mut probe = UnboundLoopJump { found: None };
    any_in_tree(block, &mut probe);
    probe.found
}
