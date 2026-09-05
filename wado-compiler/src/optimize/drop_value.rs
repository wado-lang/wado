//! Dropped-value elimination: a value in discarded position keeps only its
//! effects. `docs/optimizer.md` states the rewrite and what counts as discarded.
//!
//! Deliberately not extended to a discarded `Expr(aggregate)` statement:
//! `sroa_variant_return` tracks a call by whether its result is dropped, reads
//! that off bare `Expr` statements only, and reboxes what the decomposition
//! moves one level down — every round, past the optimizer's iteration cap.

use crate::nir_arena::{BlockId, ExprKind, NodeRef, Operand, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};
use crate::tir::TypeTable;
use crate::token::Span;

use super::arena_query;

pub(super) struct DropValueRule;

impl Rule for DropValueRule {
    fn apply_block(&self, engine: &mut Engine, id: BlockId) -> bool {
        let stmts = engine.body.blocks[id].stmts.clone();
        let last = stmts.last().copied();
        // A statement something follows is discarded, and so is the tail of a
        // block whose value reaches no consumer — the root, a loop body, the
        // arm of a `match` nothing reads. WIR lowers exactly those with no
        // value region, and a `match` arm whose result is consumed still
        // expects its last statement to leave a value.
        let tail_discarded = !arena_query::block_yields_value(engine, id);
        let discarded = |s: StmtId| Some(s) != last || tail_discarded;
        let mut changed = false;
        let mut new_stmts = Vec::with_capacity(stmts.len());
        for s in stmts {
            let Some((label, block)) = discarded(s)
                .then(|| plan_labeled_block(engine, s))
                .flatten()
            else {
                new_stmts.push(s);
                continue;
            };
            let span = engine.body.stmts[s].span;
            strip_exits(engine, block, &label);
            new_stmts.push(engine.alloc_stmt(StmtKind::LabeledBlock { label, block }, span));
            changed = true;
        }
        if changed {
            engine.set_block_stmts(id, new_stmts);
        }
        changed
    }
}

/// Replace a discarded value with the statements its operands' effects need.
/// Each operand is smaller than the aggregate it came from, so this terminates.
fn emit_discarded(engine: &mut Engine, op: Operand, span: Span, out: &mut Vec<StmtId>) {
    let Some(e) = op.as_expr() else {
        return;
    };
    let operands: Vec<Operand> = match &engine.body.exprs[e].kind {
        ExprKind::StructLiteral { fields, .. } => fields.iter().map(|f| f.value).collect(),
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            elements.clone()
        }
        ExprKind::VariantConstruct { payload, .. } => payload.iter().copied().collect(),
        _ => {
            out.push(engine.alloc_stmt(StmtKind::Expr(op), span));
            return;
        }
    };
    for operand in operands {
        emit_discarded(engine, operand, span, out);
    }
}

/// The (label, block) of a statement whose whole value is a labeled block with
/// exits this pass can rewrite. The caller decides the value is discarded.
fn plan_labeled_block(engine: &Engine, s: StmtId) -> Option<(String, BlockId)> {
    let StmtKind::Expr(Operand::Expr(e)) = &engine.body.stmts[s].kind else {
        return None;
    };
    let ExprKind::LabeledBlock {
        label,
        block,
        result_type,
    } = &engine.body.exprs[*e].kind
    else {
        return None;
    };
    if *result_type == TypeTable::UNIT {
        return None;
    }
    let (label, block) = (label.clone(), *block);
    // A fall-through would leave the value in the tail statement, which is not
    // an exit this pass rewrites.
    let last = *engine.body.blocks[block].stmts.last()?;
    if !matches!(
        engine.body.stmts[last].kind,
        StmtKind::Break { .. } | StmtKind::Return { .. }
    ) {
        return None;
    }
    strippable(engine, block, &label).then_some((label, block))
}

/// Whether every `break <label>` in `block` sits where [`strip_stmt`] rewrites
/// it. The two walks match arm for arm; what the rewriter passes over unchanged
/// is what has to hide no such break.
fn strippable(engine: &Engine, block: BlockId, label: &str) -> bool {
    engine.body.blocks[block]
        .stmts
        .iter()
        .all(|s| strippable_stmt(engine, *s, label))
}

fn strippable_stmt(engine: &Engine, s: StmtId, label: &str) -> bool {
    match &engine.body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value,
        } if l == label => value.is_none_or(|v| !operand_breaks_to(engine, v, label)),
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            !operand_breaks_to(engine, *condition, label)
                && strippable(engine, *then_block, label)
                && else_block.is_none_or(|eb| strippable(engine, eb, label))
        }
        StmtKind::Loop { body } => strippable(engine, *body, label),
        // A block rebinding the label owns every break to it inside, so neither
        // walk descends.
        StmtKind::LabeledBlock { label: l, block } => {
            l == label || strippable(engine, *block, label)
        }
        StmtKind::Let { .. }
        | StmtKind::LetDestructure { .. }
        | StmtKind::Expr(_)
        | StmtKind::Return { .. }
        | StmtKind::Break { .. }
        | StmtKind::Continue => !arena_query::has_break_to(engine.body, NodeRef::Stmt(s), label),
    }
}

fn operand_breaks_to(engine: &Engine, op: Operand, label: &str) -> bool {
    op.as_expr()
        .is_some_and(|e| arena_query::has_break_to(engine.body, NodeRef::Expr(e), label))
}

/// Drop the operand of every `break <label>` in `block`, keeping what it still
/// has to run as the statement ahead of the break. Returns whether it rewrote
/// anything: the engine re-runs every rule over a block it is told changed.
fn strip_exits(engine: &mut Engine, block: BlockId, label: &str) -> bool {
    let stmts = engine.body.blocks[block].stmts.clone();
    let mut out = Vec::with_capacity(stmts.len());
    let mut changed = false;
    for s in stmts {
        changed |= strip_stmt(engine, s, label, &mut out);
    }
    if changed {
        engine.set_block_stmts(block, out);
    }
    changed
}

fn strip_stmt(engine: &mut Engine, s: StmtId, label: &str, out: &mut Vec<StmtId>) -> bool {
    let value = match &engine.body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value: Some(v),
        } if l == label => *v,
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let (then_block, else_block) = (*then_block, *else_block);
            let mut changed = strip_exits(engine, then_block, label);
            if let Some(eb) = else_block {
                changed |= strip_exits(engine, eb, label);
            }
            out.push(s);
            return changed;
        }
        StmtKind::Loop { body } => {
            let body = *body;
            let changed = strip_exits(engine, body, label);
            out.push(s);
            return changed;
        }
        StmtKind::LabeledBlock { label: l, block } => {
            let (shadows, block) = (l == label, *block);
            let changed = !shadows && strip_exits(engine, block, label);
            out.push(s);
            return changed;
        }
        StmtKind::Break { .. }
        | StmtKind::Return { .. }
        | StmtKind::Continue
        | StmtKind::Let { .. }
        | StmtKind::LetDestructure { .. }
        | StmtKind::Expr(_) => {
            out.push(s);
            return false;
        }
    };
    let span = engine.body.stmts[s].span;
    emit_discarded(engine, value, span, out);
    out.push(engine.alloc_stmt(
        StmtKind::Break {
            label: Some(label.to_string()),
            value: None,
        },
        span,
    ));
    true
}
