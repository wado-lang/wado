//! Control-flow analyses for missing-return diagnosis (Stage 5 of WEP
//! 2026-05-26).
//!
//! These walks consume the parsed AST and look up types from
//! [`crate::elaborator::sem::types::TypeAnnotations::expression_types`].
//! They replace the equivalent `block_always_exits` /
//! `find_return_type_in_block` TIR walkers in `expr.rs`, freeing the
//! combined walk to stop producing body TIR.
//!
//! Both phases consult identical analyses: the combined walk's
//! diagnostic point (via [`super::Elaborator`]) and reify's closure
//! return-type derivation (via [`super::reify::Reify`]) construct a
//! [`CtrlFlowCtx`] over their respective `expression_types` map and
//! current module key, then dispatch to the free walker functions.
//!
//! The shape mirrors the TIR walkers exactly: an `ast::Block` /
//! `ast::Stmt` / `ast::Expr` arm corresponds 1:1 to the TIR arm.
//! NEVER detection (TIR walker's `expr.type_id == TypeTable::NEVER`)
//! becomes an `expression_types[(module, expr.id)] == NEVER` lookup.

use std::cell::RefCell;

use crate::ast;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::symbol::SymbolKey;
use crate::tir::{TypeId, TypeTable};

/// Lookup context for AST control-flow walks. Holds the per-AstId
/// type table and the current module key so `expression_types` reads
/// resolve to the right module's facts. `type_table` backs the
/// `contains_unknown` filter the missing-return walk applies (since
/// Stage 7-B `expression_types` records UNKNOWN-containing types).
#[derive(Clone, Copy)]
pub(super) struct CtrlFlowCtx<'a> {
    pub(super) expression_types: &'a IndexMap<SymbolKey, TypeId>,
    pub(super) module: &'a ModuleSource,
    pub(super) type_table: &'a RefCell<TypeTable>,
}

impl CtrlFlowCtx<'_> {
    fn type_of(&self, expr: &ast::Expr) -> Option<TypeId> {
        self.type_of_id(expr.id())
    }

    fn type_of_id(&self, id: crate::ast::AstId) -> Option<TypeId> {
        let key = SymbolKey::new(self.module.clone(), id);
        self.expression_types.get(&key).copied()
    }

    /// `type_of`, but an UNKNOWN-containing recorded type counts as "no
    /// definite type" (`None`). The missing-return walk uses this so an
    /// unresolved-`null` return value does not masquerade as a concrete
    /// return type — preserving the behaviour from when the recording site
    /// skipped UNKNOWN types entirely.
    fn definite_type_of(&self, expr: &ast::Expr) -> Option<TypeId> {
        self.type_of(expr)
            .filter(|t| !self.type_table.borrow().contains_unknown(*t))
    }

    fn is_never(&self, expr: &ast::Expr) -> bool {
        self.type_of(expr) == Some(TypeTable::NEVER)
    }
}

/// Whether every control path through `block` exits before reaching
/// the end. AST mirror of `Elaborator::block_always_exits`.
pub(super) fn block_always_exits(ctx: CtrlFlowCtx<'_>, block: &ast::Block) -> bool {
    block.stmts.iter().any(|s| stmt_always_exits(ctx, s))
}

fn stmt_always_exits(ctx: CtrlFlowCtx<'_>, stmt: &ast::Stmt) -> bool {
    match stmt {
        ast::Stmt::Return(_) => true,
        ast::Stmt::Expr(e) => expr_always_exits(ctx, &e.expr),
        ast::Stmt::If(if_stmt) => {
            if let Some(else_block) = &if_stmt.else_block {
                block_always_exits(ctx, &if_stmt.then_block) && block_always_exits(ctx, else_block)
            } else {
                false
            }
        }
        ast::Stmt::Loop(loop_stmt) => !loop_body_can_escape(ctx, &loop_stmt.body),
        ast::Stmt::LabeledBlock(lb) => {
            block_always_exits(ctx, &lb.block)
                && !block_can_break_to_label(ctx, &lb.block, &lb.label)
        }
        // `match { ... }` as a top-level statement diverges when every
        // arm diverges. AST keeps it as `Stmt::Match`, but the TIR
        // walker saw the same construct as `TirStmtKind::Expr` over an
        // `Expr::Match` — route through the expression-level check.
        ast::Stmt::Match(m) => {
            !m.arms.is_empty() && m.arms.iter().all(|a| expr_always_exits(ctx, &a.body))
        }
        _ => false,
    }
}

pub(super) fn expr_always_exits(ctx: CtrlFlowCtx<'_>, expr: &ast::Expr) -> bool {
    if ctx.is_never(expr) {
        return true;
    }
    match expr {
        ast::Expr::Block(block) => block_always_exits(ctx, block),
        ast::Expr::LabeledBlock(lb) => {
            block_always_exits(ctx, &lb.block)
                && !block_can_break_to_label(ctx, &lb.block, &lb.label)
        }
        ast::Expr::If(if_expr) => {
            if let Some(else_block) = &if_expr.else_block {
                block_always_exits(ctx, &if_expr.then_block) && block_always_exits(ctx, else_block)
            } else {
                false
            }
        }
        ast::Expr::Match(m) => {
            !m.arms.is_empty() && m.arms.iter().all(|a| expr_always_exits(ctx, &a.body))
        }
        // `resume value` transfers control out of the enclosing
        // handler method — lowered to `return value`.
        ast::Expr::Resume(_) => true,
        // `with … do { body }`: defer to body's definite-exit.
        ast::Expr::WithHandler(wh) => block_always_exits(ctx, &wh.body),
        _ => false,
    }
}

/// Result type of `block` — the type of its trailing expression, or
/// `Unit`. AST mirror of [`crate::tir::block_result_type`] (which reads
/// the built `TirBlock`); this reads `expression_types[(module, id)]`
/// instead so the combined walk can compute a block's value type without
/// inspecting the body TIR it builds.
///
/// Unlike the missing-return walk this does NOT filter `contains_unknown`:
/// an unresolved-`null` tail is `Option<UNKNOWN>` here exactly as the TIR
/// walker saw `null_tir.type_id`, so the if/match result-type inference
/// (which special-cases `contains_unknown` branches) behaves identically.
///
/// The arm-to-arm correspondence with the TIR walker:
/// - trailing `Stmt::Expr(e)` ↔ `TirStmtKind::Expr` — the recorded type of
///   `e` (an `if`/`match`/block tail keeps its already-recorded result
///   type here).
/// - trailing `Stmt::If` with an `else` ↔ `TirStmtKind::If` — the branches
///   agree via [`crate::tir::agree_branch_types`].
/// - trailing `Return`/`Break`/`Continue` ↔ the diverging arms — `Never`.
/// - anything else ↔ the TIR walker's `_ => None` — `Unit`.
pub(super) fn block_result_type(ctx: CtrlFlowCtx<'_>, block: &ast::Block) -> TypeId {
    block
        .stmts
        .last()
        .and_then(|s| match s {
            ast::Stmt::Expr(e) => ctx.type_of(&e.expr),
            // A trailing `match` lowers to `TirStmtKind::Expr(match)`, so its
            // recorded type is the block's value (the combined walk records
            // `match.id` for both stmt-position and trailing-with-expected
            // matches). The TIR walker reached it through the `Expr` arm.
            ast::Stmt::Match(m) => ctx.type_of_id(m.id),
            ast::Stmt::If(if_stmt) => if_stmt.else_block.as_ref().and_then(|else_block| {
                crate::tir::agree_branch_types(
                    block_result_type(ctx, &if_stmt.then_block),
                    block_result_type(ctx, else_block),
                )
            }),
            ast::Stmt::Return(_) | ast::Stmt::Break(_) | ast::Stmt::Continue(_) => {
                Some(TypeTable::NEVER)
            }
            _ => None,
        })
        .unwrap_or(TypeTable::UNIT)
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
        // expression form. Matches the TIR walker's expression-level
        // arm; `Stmt::Match` is just an AST surface that lowers to the
        // same TIR shape.
        ast::Stmt::Match(m) => {
            for arm in &m.arms {
                if let Some(t) = find_return_type_in_expr(ctx, &arm.body) {
                    return Some(t);
                }
            }
            None
        }
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

/// Searches for `break <label>` whose `label` is NOT defined inside
/// the walked loop body. Labels declared by `LabeledBlock` (stmt or
/// expr) nodes we enter are pushed onto `inner_labels`; a `break label`
/// matches only when `label` is not on that stack.
#[derive(Default)]
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

fn loop_body_can_escape(ctx: CtrlFlowCtx<'_>, body: &ast::Block) -> bool {
    let mut probe = LoopEscape::default();
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
        ast::Stmt::Let(l) => l.value.as_ref().is_some_and(|v| any_in_expr(ctx, v, probe)),
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
    // Mirror the TIR walker's `any_in_expr`: descend into every
    // sub-expression / sub-block carrier except `Closure`, whose body
    // has its own control-flow scope.
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
        ast::Expr::StructLiteral(s) => s.fields.iter().any(|f| any_in_expr(ctx, &f.value, probe)),
        ast::Expr::TupleLiteral(t) => t.elements.iter().any(|e| any_in_expr(ctx, e, probe)),
        ast::Expr::Spread(inner, _) => any_in_expr(ctx, inner, probe),
        ast::Expr::Range(r) => any_in_expr(ctx, &r.start, probe) || any_in_expr(ctx, &r.end, probe),
        ast::Expr::TryOp(t) => any_in_expr(ctx, &t.expr, probe),
        ast::Expr::TemplateString(ts) => ts.parts.iter().any(|p| match p {
            ast::TemplatePart::Interpolation { expr, .. } => any_in_expr(ctx, expr, probe),
            ast::TemplatePart::String(_) => false,
        }),
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
