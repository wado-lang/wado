//! Dropped-value elimination: a value in discarded position keeps only its
//! effects.
//!
//! `let _ = xs.pop()` inlines to a labeled block that breaks with an
//! `Option::Some { … }` on one path and an `Option::None` on the other. Nothing
//! reads the result, so both allocations — and the bounds-checked element read
//! one of them wraps — are dead; only the `used` decrement must survive.
//! `elide_local` gets as far as demoting the dead binding to `Expr(block)`,
//! which still evaluates the value it then throws away, and stops there: the
//! read may trap, so the aggregate around it is not deletable whole.
//!
//! The rewrite is a change of shape rather than of contents: the value-producing
//! `ExprKind::LabeledBlock` becomes the value-discarding `StmtKind::LabeledBlock`,
//! and each `break L: v` targeting it gives up its operand — decomposed into the
//! statements its own operands' effects need, so the allocation goes and the
//! trapping read inside it stays. Everything left dead is then `elide_local`'s
//! and DCE's to reclaim.
//!
//! The same decomposition applied to a discarded `Expr(aggregate)` _statement_
//! does not converge: `sroa_variant_return` tracks a call by whether its result
//! is dropped, reads that off bare `Expr` statements only, and reboxes what the
//! decomposition moves one level down — every round.

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
        // A block's tail may be its value, and at WIR that is decided by the
        // block expression's own type rather than by whether anything reads the
        // enclosing construct: a `match` arm whose result is dropped still
        // translates as a value region and expects its last statement to leave
        // one. So only a statement something follows is discarded — plus the
        // root block's tail, which `translate_block` lowers with no value at all.
        let is_root = id == engine.body.root;
        let discarded = |s: StmtId| Some(s) != last || is_root;
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
/// An aggregate decomposes into its operands, each strictly smaller, so the
/// recursion terminates and a second visit finds nothing to do.
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

/// The (label, block) of a statement whose whole value is a labeled block
/// nothing reads and whose exits this pass can rewrite.
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
    // The block must not fall off its end: a fall-through would leave the value
    // in the tail statement, which is not an exit this pass rewrites.
    let last = *engine.body.blocks[block].stmts.last()?;
    if !matches!(
        engine.body.stmts[last].kind,
        StmtKind::Break { .. } | StmtKind::Return { .. }
    ) {
        return None;
    }
    strippable(engine, block, &label).then_some((label, block))
}

/// Whether every `break <label>` in `block` sits where [`strip_exits`] rewrites
/// it. The two walks share one shape: each arm here either recurses exactly
/// where the rewriter does, or refuses an operand that hides a break to `label`.
fn strippable(engine: &Engine, block: BlockId, label: &str) -> bool {
    engine.body.blocks[block]
        .stmts
        .iter()
        .all(|s| strippable_stmt(engine, *s, label))
}

fn strippable_stmt(engine: &Engine, s: StmtId, label: &str) -> bool {
    let no_hidden_break =
        |op: &Option<Operand>| op.is_none_or(|v| !operand_breaks_to(engine, v, label));
    match &engine.body.stmts[s].kind {
        StmtKind::Break {
            label: Some(l),
            value,
        } if l == label => no_hidden_break(value),
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
        // walk descends — `strip_exits` skips the same shape.
        StmtKind::LabeledBlock { label: l, block } => {
            l == label || strippable(engine, *block, label)
        }
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            !operand_breaks_to(engine, *value, label)
        }
        StmtKind::Expr(value) => !operand_breaks_to(engine, *value, label),
        StmtKind::Return { value } | StmtKind::Break { value, .. } => no_hidden_break(value),
        StmtKind::Continue => true,
    }
}

fn operand_breaks_to(engine: &Engine, op: Operand, label: &str) -> bool {
    op.as_expr()
        .is_some_and(|e| arena_query::has_break_to(engine.body, NodeRef::Expr(e), label))
}

/// Drop the operand of every `break <label>` in `block`, keeping one that still
/// has an effect to run as the statement ahead of the break. Reports whether it
/// rewrote anything, so a subtree holding no such break is left untouched rather
/// than written back identical — the engine re-runs every rule over a block it
/// is told changed.
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
