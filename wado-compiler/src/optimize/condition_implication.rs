//! Condition Implication — eliminates conditions implied false by guards.
//!
//! When a loop guard proves `i < bound`, any inner condition `i >= bound` is
//! known false and can be replaced with `false`. The existing `const_branch_prune`
//! pass then removes the dead branch on the next iteration.
//!
//! Also handles dominating if-conditions (`if (i + k) < bound { … }` proves
//! `(i + j) >= bound` false for `0 <= j <= k` inside the then-block),
//! early-exit guards (`if (i + k) >= bound { return; }` proves the same for
//! the statements after it), short-circuit guards (the right side of
//! `(i + k) >= bound || expr` only runs when the left is false), and
//! bitmask-bounded checks (`(x & MASK) >= BOUND` is false when
//! `BOUND > MASK >= 0`).
//!
//! All guard facts are [`GuardFact`]s over the engine's `ValueGraph`: the
//! guarded variable and the bound are captured as `ValueId`s at the guard
//! position, and a check is implied false by plain `ValueId` comparison plus
//! constant / `Add`-decomposition on the value kinds. This is flow-correct by
//! construction — the `ValueGraph` resolves locals to their reaching
//! definitions, so a mutation of the variable (`i += 1`) or of the bound's
//! backing storage (an inlined `pop()` shrinking `.used`) between the guard
//! and a check changes the check operand's `ValueId` and the implication
//! simply fails. See `array_bounds_elim_oob_guard_var_mutated.wado` /
//! `array_bounds_elim_oob_bound_shrunk.wado` for the fixtures pinning this.
//! (The pre-ValueGraph implementation compared operands syntactically —
//! `resolves_to(i, i)` was unconditionally true — and needed a positional
//! kill-tracking layer plus a `DefMap` of copy / field / struct-literal
//! chains to approximate what the `ValueGraph` now answers directly.)
//!
//! Runs on the worklist rewrite engine as a [`Rule`]: a per-function
//! standalone engine session whose `apply_block` fires once at the body root.
//! The single rewrite point (`condition → BoolLiteral(false)`) routes through
//! `engine.replace_expr_kind` so the parent map and use index stay coherent.
//! The session's value graph is built once, lazily; `set_false` replaces
//! already-judged condition nodes only, so the cached graph stays valid for
//! every later query (no node is re-queried after being rewritten and no new
//! nodes are allocated).

use std::cell::Cell;

use crate::nir::{NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, ExprId, ExprKind, NodeRef, PatId, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{ValueId, ValueKind};

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

pub fn eliminate_implied_conditions(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::ConditionImplication, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = ConditionImplicationRule {
            applied: Cell::new(false),
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function condition-implication walk at the body root.
pub(super) struct ConditionImplicationRule {
    applied: Cell<bool>,
}

impl Rule for ConditionImplicationRule {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        let root = engine.body.root;
        process_block(engine, root)
    }
}

/// A guard fact: `var + max_offset < bound`, with the variable and bound
/// captured as `ValueGraph` identities at the guard's program point.
///
/// - A loop guard `if !(i < b) { break }` yields `{ var: vn(i), max_offset:
///   0, bound: vn(b), is_strict: true }` (`<=` sets `is_strict: false`).
/// - A dominating condition `(i + k) < b` and an early-exit / short-circuit
///   `(i + k) >= b` (known false on the surviving path) yield
///   `{ var: vn(i), max_offset: k, bound: vn(b), is_strict: true }`.
#[derive(Clone, Copy)]
struct GuardFact {
    /// `ValueId` of the guarded variable (the `Add` base after decomposition).
    var_vn: ValueId,
    /// The guard proves `var + j < bound` for every `0 <= j <= max_offset`.
    max_offset: i64,
    /// `ValueId` of the bound expression.
    bound_vn: ValueId,
    /// `true` for `<`; `false` for `<=` (loop guards only, `max_offset` 0).
    is_strict: bool,
}

impl GuardFact {
    /// Build a fact from a comparison `lhs OP rhs` whose truth the dominating
    /// context establishes as `lhs < rhs` (`is_strict`) or `lhs <= rhs`.
    fn from_comparison(
        engine: &mut Engine,
        lhs: ExprId,
        rhs: ExprId,
        is_strict: bool,
    ) -> Option<GuardFact> {
        let lhs_vn = engine.value(lhs)?;
        let rhs_vn = engine.value(rhs)?;
        let (var_vn, max_offset) = decompose_add_const(engine, lhs_vn);
        if max_offset < 0 {
            return None;
        }
        Some(GuardFact {
            var_vn,
            max_offset,
            bound_vn: rhs_vn,
            is_strict,
        })
    }

    /// Whether this fact proves the condition `check_lhs >= check_rhs` false.
    ///
    /// The check's left side must decompose to `var + j` with
    /// `0 <= j <= max_offset`; the guard then gives
    /// `var + j < bound - (max_offset - j)`. The check is false when:
    ///
    /// - identity: `check_rhs`'s `ValueId` equals the bound's (the slack
    ///   `max_offset - j >= 0` only strengthens the claim), or
    /// - plus-one (non-strict guards): `check_rhs` is structurally
    ///   `bound + 1`, or
    /// - numeric: both bounds are integer constants and
    ///   `check >= bound - slack` (strict) / `check > bound` (non-strict).
    fn implies_false(&self, engine: &mut Engine, condition: ExprId) -> bool {
        let ExprKind::Binary {
            left,
            op: NirBinaryOp::GtEq,
            right,
        } = &engine.body.exprs[condition].kind
        else {
            return false;
        };
        let (left, right) = (*left, *right);
        let (Some(lhs_vn), Some(rhs_vn)) = (engine.value(left), engine.value(right)) else {
            return false;
        };
        let (base, offset) = decompose_add_const(engine, lhs_vn);
        if base != self.var_vn || offset < 0 || offset > self.max_offset {
            return false;
        }
        let slack = self.max_offset - offset;
        if self.is_strict {
            if rhs_vn == self.bound_vn {
                return true;
            }
            match (int_const(engine, rhs_vn), int_const(engine, self.bound_vn)) {
                (Some(check), Some(bound)) => check >= bound - slack,
                _ => false,
            }
        } else {
            // `var <= bound` (max_offset is 0 for non-strict guards, so
            // `offset == 0 == slack`): the check is false iff its bound
            // exceeds the guard bound — structurally `bound + 1`, or
            // numerically greater.
            if is_plus_one_of(engine, rhs_vn, self.bound_vn) {
                return true;
            }
            match (int_const(engine, rhs_vn), int_const(engine, self.bound_vn)) {
                (Some(check), Some(bound)) => check > bound,
                _ => false,
            }
        }
    }
}

/// Decompose a value as `base + k` (`k >= 0` enforcement is the caller's):
/// `Binary(Add, base, Int(k))` yields `(base, k)`, anything else `(v, 0)`.
/// The `ValueGraph` already resolved copy chains and constant locals, so a
/// source-level `i + offset_local` arrives here as `Add(vn(i), Int(k))` when
/// the offset is a known constant.
fn decompose_add_const(engine: &mut Engine, v: ValueId) -> (ValueId, i64) {
    if let ValueKind::Binary {
        op: NirBinaryOp::Add,
        lhs,
        rhs,
    } = engine.value_kind(v)
    {
        let (lhs, rhs) = (*lhs, *rhs);
        if let ValueKind::Int(k) = engine.value_kind(rhs) {
            #[allow(clippy::cast_possible_wrap)]
            return (lhs, *k as i64);
        }
    }
    (v, 0)
}

/// The integer constant a value denotes, if its kind is `Int`. Bit patterns
/// that do not fit `i64` (bit 63 set) yield `None` rather than feeding a
/// negative to the numeric comparisons.
fn int_const(engine: &mut Engine, v: ValueId) -> Option<i64> {
    match engine.value_kind(v) {
        ValueKind::Int(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}

/// Whether `v` is structurally `base + 1`.
fn is_plus_one_of(engine: &mut Engine, v: ValueId, base: ValueId) -> bool {
    if let ValueKind::Binary {
        op: NirBinaryOp::Add,
        lhs,
        rhs,
    } = engine.value_kind(v)
    {
        let (lhs, rhs) = (*lhs, *rhs);
        return lhs == base && matches!(engine.value_kind(rhs), ValueKind::Int(1));
    }
    false
}

/// Whether `(lhs >= rhs)` is provably false because `lhs` is bitmask-bounded:
/// `(x & MASK) >= BOUND` is false when `MASK >= 0` and `BOUND > MASK`. The
/// mask may appear on either side of the `&`; copy chains are already
/// resolved by the `ValueGraph`.
fn is_bitmask_bounded(engine: &mut Engine, condition: ExprId) -> bool {
    let ExprKind::Binary {
        left,
        op: NirBinaryOp::GtEq,
        right,
    } = &engine.body.exprs[condition].kind
    else {
        return false;
    };
    let (left, right) = (*left, *right);
    let (Some(lhs_vn), Some(rhs_vn)) = (engine.value(left), engine.value(right)) else {
        return false;
    };
    let ValueKind::Binary {
        op: NirBinaryOp::BitAnd,
        lhs,
        rhs,
    } = engine.value_kind(lhs_vn)
    else {
        return false;
    };
    let (and_l, and_r) = (*lhs, *rhs);
    let mask = match (int_const(engine, and_l), int_const(engine, and_r)) {
        (_, Some(m)) | (Some(m), None) => m,
        _ => return false,
    };
    let Some(bound) = int_const(engine, rhs_vn) else {
        return false;
    };
    mask >= 0 && bound > mask
}

fn process_block(engine: &mut Engine, block: BlockId) -> bool {
    let mut changed = false;
    // Guard facts from earlier early-exit statements in this block. No
    // staleness tracking is needed: a mutation of the variable or bound
    // between the guard and a later check changes the check operand's
    // `ValueId`, so the fact stops matching by itself.
    let mut guards: Vec<GuardFact> = Vec::new();
    let stmts = engine.body.blocks[block].stmts.clone();
    for s in stmts {
        for guard in &guards {
            changed |= GuardEliminator { fact: *guard }.visit_stmt(engine, s);
        }
        changed |= BitmaskEliminator.visit_stmt(engine, s);
        changed |= ShortCircuitEliminator.visit_stmt(engine, s);
        changed |= process_stmt(engine, s);
        if let Some(guard) = extract_early_exit_guard(engine, s) {
            guards.push(guard);
        }
    }
    changed
}

fn process_stmt(engine: &mut Engine, s: StmtId) -> bool {
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::Loop { body: lb } => StmtShape::Loop(*lb),
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => StmtShape::If(*then_block, *else_block),
        StmtKind::LabeledBlock { block, .. } => StmtShape::Labeled(*block),
        _ => StmtShape::None,
    };
    match shape {
        StmtShape::Loop(lb) => process_loop(engine, lb),
        StmtShape::If(then_b, else_b) => {
            let mut changed = process_block(engine, then_b);
            if let Some(eb) = else_b {
                changed |= process_block(engine, eb);
            }
            changed
        }
        StmtShape::Labeled(b) => process_block(engine, b),
        StmtShape::None => false,
    }
}

enum StmtShape {
    Loop(BlockId),
    If(BlockId, Option<BlockId>),
    Labeled(BlockId),
    None,
}

fn process_loop(engine: &mut Engine, loop_body: BlockId) -> bool {
    let mut changed = false;

    if let Some(guard) = extract_loop_guard(engine, loop_body) {
        // Eliminate implied conditions in the loop body (skip the guard itself).
        let mut condition_elim = ConditionEliminator {
            guard,
            dom_guards: vec![],
        };
        for s in engine.body.blocks[loop_body].stmts.clone().iter().skip(1) {
            changed |= condition_elim.visit_stmt(engine, *s);
        }
    }

    // Eliminate bitmask-bounded checks in the loop body.
    for s in engine.body.blocks[loop_body].stmts.clone() {
        changed |= BitmaskEliminator.visit_stmt(engine, s);
    }

    // Recurse into nested loops.
    for s in engine.body.blocks[loop_body].stmts.clone() {
        changed |= process_stmt_nested_loops(engine, s);
    }

    changed
}

/// Recurse into nested structures to find inner loops, but don't re-process
/// the current loop level.
fn process_stmt_nested_loops(engine: &mut Engine, s: StmtId) -> bool {
    let shape = match &engine.body.stmts[s].kind {
        StmtKind::Loop { body: lb } => StmtShape::Loop(*lb),
        StmtKind::If {
            then_block,
            else_block,
            ..
        } => StmtShape::If(*then_block, *else_block),
        StmtKind::LabeledBlock { block, .. } => StmtShape::Labeled(*block),
        _ => StmtShape::None,
    };
    match shape {
        StmtShape::Loop(lb) => process_loop(engine, lb),
        StmtShape::If(then_b, else_b) => {
            let mut changed = false;
            for s in engine.body.blocks[then_b].stmts.clone() {
                changed |= process_stmt_nested_loops(engine, s);
            }
            if let Some(eb) = else_b {
                for s in engine.body.blocks[eb].stmts.clone() {
                    changed |= process_stmt_nested_loops(engine, s);
                }
            }
            changed
        }
        StmtShape::Labeled(b) => {
            let mut changed = false;
            for s in engine.body.blocks[b].stmts.clone() {
                changed |= process_stmt_nested_loops(engine, s);
            }
            changed
        }
        StmtShape::None => false,
    }
}

/// Extract a loop guard from the first statement of a loop body.
///
/// Matches: `if !(var < bound) { break LABEL; }` → guard `var < bound`
///      or: `if !(var <= bound) { break LABEL; }` → guard `var <= bound`
fn extract_loop_guard(engine: &mut Engine, loop_body: BlockId) -> Option<GuardFact> {
    let first = *engine.body.blocks[loop_body].stmts.first()?;
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &engine.body.stmts[first].kind
    else {
        return None;
    };
    let condition = *condition;
    let then_block = *then_block;

    // then_block must be a single Break statement.
    if engine.body.blocks[then_block].stmts.len() != 1 {
        return None;
    }
    matches!(
        &engine.body.stmts[engine.body.blocks[then_block].stmts[0]].kind,
        StmtKind::Break { .. }
    )
    .then_some(())?;

    // condition must be `Not(Binary(var, Lt|LtEq, bound))`.
    let ExprKind::Unary {
        op: NirUnaryOp::Not,
        expr: inner,
    } = &engine.body.exprs[condition].kind
    else {
        return None;
    };
    let inner = *inner;

    let ExprKind::Binary { left, op, right } = &engine.body.exprs[inner].kind else {
        return None;
    };
    let (is_strict, lhs, rhs) = match op {
        NirBinaryOp::Lt => (true, *left, *right),
        NirBinaryOp::LtEq => (false, *left, *right),
        _ => return None,
    };
    // Loop guards keep the plain-variable shape: the induction variable is
    // compared directly (`i < bound`), so any `Add` decomposition would
    // describe a different program object. Restrict to offset 0.
    let fact = GuardFact::from_comparison(engine, lhs, rhs, is_strict)?;
    (fact.max_offset == 0).then_some(fact)
}

/// Extract a guard from an early-exit if-statement: after
/// `if (var + k) >= bound { return/break }`, the surviving path has
/// `(var + k) < bound`.
fn extract_early_exit_guard(engine: &mut Engine, s: StmtId) -> Option<GuardFact> {
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &engine.body.stmts[s].kind
    else {
        return None;
    };
    let condition = *condition;
    let then_block = *then_block;
    if !block_always_exits(engine, then_block) {
        return None;
    }
    let ExprKind::Binary {
        left,
        op: NirBinaryOp::GtEq,
        right,
    } = &engine.body.exprs[condition].kind
    else {
        return None;
    };
    let (left, right) = (*left, *right);
    GuardFact::from_comparison(engine, left, right, true)
}

fn block_always_exits(engine: &Engine, block: BlockId) -> bool {
    engine.body.blocks[block].stmts.iter().any(|s| {
        matches!(
            engine.body.stmts[*s].kind,
            StmtKind::Return { .. } | StmtKind::Break { .. }
        )
    })
}

/// Extract a dominating guard from an if-condition `lhs < rhs`: inside the
/// then-block, `lhs < rhs` holds, i.e. `var + k < bound` after decomposition.
fn extract_dominating_guard(engine: &mut Engine, condition: ExprId) -> Option<GuardFact> {
    let ExprKind::Binary {
        left,
        op: NirBinaryOp::Lt,
        right,
    } = &engine.body.exprs[condition].kind
    else {
        return None;
    };
    let (left, right) = (*left, *right);
    GuardFact::from_comparison(engine, left, right, true)
}

/// Replace the expression at `cond` with `false`, preserving its type and span.
fn set_false(engine: &mut Engine, cond: ExprId) {
    engine.replace_expr_kind(cond, ExprKind::BoolLiteral(false));
}

/// Check if a block consists of a panic call (bounds check failure path).
fn is_panic_block(engine: &Engine, block: BlockId) -> bool {
    engine.body.blocks[block]
        .stmts
        .iter()
        .any(|s| match &engine.body.stmts[*s].kind {
            StmtKind::Expr(expr) => is_panic_call(engine, *expr),
            _ => false,
        })
}

fn is_panic_call(engine: &Engine, e: ExprId) -> bool {
    match &engine.body.exprs[e].kind {
        ExprKind::Call { func, .. } => func.name.contains("panic"),
        _ => false,
    }
}

/// Whether `s` is a `if (cond) { panic-block }` bounds check whose condition
/// `fact` proves false; rewrites and reports if so.
fn eliminate_panic_check(engine: &mut Engine, s: StmtId, fact: &GuardFact) -> bool {
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &engine.body.stmts[s].kind
    else {
        return false;
    };
    let (condition, then_block) = (*condition, *then_block);
    if is_panic_block(engine, then_block) && fact.implies_false(engine, condition) {
        set_false(engine, condition);
        return true;
    }
    false
}

/// NIR visitor that eliminates loop-guard-implied false bounds checks.
///
/// When a loop guard proves `i < bound`, inner conditions `i >= bound` are
/// replaced with `false`. Dominating if-conditions are tracked (scoped by
/// `Vec` truncation) to extend the elimination into their then-blocks.
struct ConditionEliminator {
    guard: GuardFact,
    dom_guards: Vec<GuardFact>,
}

impl ConditionEliminator {
    fn implied_false(&self, engine: &mut Engine, condition: ExprId) -> bool {
        if self.guard.implies_false(engine, condition) {
            return true;
        }
        self.dom_guards
            .iter()
            .any(|d| d.implies_false(engine, condition))
    }
}

impl ArenaOptVisitor for ConditionEliminator {
    fn visit_stmt(&mut self, engine: &mut Engine, s: StmtId) -> bool {
        let if_ids = match &engine.body.stmts[s].kind {
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => Some((*condition, *then_block, *else_block)),
            _ => None,
        };
        if let Some((condition, then_block, else_block)) = if_ids {
            // Check if this statement is a bounds check that can be eliminated.
            if else_block.is_none()
                && is_panic_block(engine, then_block)
                && self.implied_false(engine, condition)
            {
                set_false(engine, condition);
                return true;
            }

            // Extract a dominating guard from the condition to extend
            // elimination into the then-block.
            let mut changed = self.visit_expr(engine, condition);
            let dom = extract_dominating_guard(engine, condition);
            let scope_len = self.dom_guards.len();
            if let Some(dg) = dom {
                self.dom_guards.push(dg);
            }
            changed |= self.visit_block(engine, then_block);
            self.dom_guards.truncate(scope_len);
            if let Some(eb) = else_block {
                changed |= self.visit_block(engine, eb);
            }
            return changed;
        }

        arena_opt_walk(self, engine, NodeRef::Stmt(s))
    }

    fn visit_expr(&mut self, engine: &mut Engine, e: ExprId) -> bool {
        // For If exprs: extract a dominating guard and propagate into then-branch.
        let if_ids = match &engine.body.exprs[e].kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => Some((*condition, *then_branch, *else_branch)),
            _ => None,
        };
        if let Some((condition, then_branch, else_branch)) = if_ids {
            let mut changed = self.visit_expr(engine, condition);
            let dom = extract_dominating_guard(engine, condition);
            let scope_len = self.dom_guards.len();
            if let Some(dg) = dom {
                self.dom_guards.push(dg);
            }
            changed |= self.visit_block(engine, then_branch);
            self.dom_guards.truncate(scope_len);
            if let Some(eb) = else_branch {
                changed |= self.visit_block(engine, eb);
            }
            return changed;
        }
        arena_opt_walk(self, engine, NodeRef::Expr(e))
    }
}

/// Applies one [`GuardFact`] (from an early-exit statement or the false side
/// of a `||`) across a subtree, eliminating implied-false panic checks.
struct GuardEliminator {
    fact: GuardFact,
}

impl ArenaOptVisitor for GuardEliminator {
    fn visit_stmt(&mut self, engine: &mut Engine, s: StmtId) -> bool {
        if eliminate_panic_check(engine, s, &self.fact) {
            return true;
        }
        arena_opt_walk(self, engine, NodeRef::Stmt(s))
    }
}

/// Eliminates bitmask-bounded false bounds checks:
/// `if (x & MASK) >= BOUND { panic(...) }` with `BOUND > MASK >= 0`.
struct BitmaskEliminator;

impl ArenaOptVisitor for BitmaskEliminator {
    fn visit_stmt(&mut self, engine: &mut Engine, s: StmtId) -> bool {
        let if_ids = match &engine.body.stmts[s].kind {
            StmtKind::If {
                condition,
                then_block,
                else_block: None,
            } => Some((*condition, *then_block)),
            _ => None,
        };
        if let Some((condition, then_block)) = if_ids
            && is_panic_block(engine, then_block)
            && is_bitmask_bounded(engine, condition)
        {
            set_false(engine, condition);
            return true;
        }
        arena_opt_walk(self, engine, NodeRef::Stmt(s))
    }
}

/// Eliminates redundant bounds checks inside short-circuit `||` expressions:
/// in `(start + k) >= bound || expr`, the right operand only executes when
/// `(start + k) < bound`.
struct ShortCircuitEliminator;

impl ArenaOptVisitor for ShortCircuitEliminator {
    fn visit_expr(&mut self, engine: &mut Engine, e: ExprId) -> bool {
        let or_ids = match &engine.body.exprs[e].kind {
            ExprKind::Binary {
                left,
                op: NirBinaryOp::Or,
                right,
            } => Some((*left, *right)),
            _ => None,
        };
        if let Some((left, right)) = or_ids {
            let mut changed = self.visit_expr(engine, left);
            let fact = if let ExprKind::Binary {
                left: cmp_l,
                op: NirBinaryOp::GtEq,
                right: cmp_r,
            } = &engine.body.exprs[left].kind
            {
                let (cmp_l, cmp_r) = (*cmp_l, *cmp_r);
                GuardFact::from_comparison(engine, cmp_l, cmp_r, true)
            } else {
                None
            };
            if let Some(fact) = fact {
                changed |= GuardEliminator { fact }.visit_expr(engine, right);
            }
            changed |= self.visit_expr(engine, right);
            return changed;
        }
        arena_opt_walk(self, engine, NodeRef::Expr(e))
    }
}

// ---------------------------------------------------------------------------
// Arena opt-visitor
// ---------------------------------------------------------------------------

/// A mutating arena walk that returns `true` when any node changed. The default
/// `visit_*` delegate to [`arena_opt_walk`], which recurses into every
/// id-bearing child; the eliminators override the nodes they rewrite.
trait ArenaOptVisitor {
    fn visit_stmt(&mut self, engine: &mut Engine, s: StmtId) -> bool
    where
        Self: Sized,
    {
        arena_opt_walk(self, engine, NodeRef::Stmt(s))
    }
    fn visit_expr(&mut self, engine: &mut Engine, e: ExprId) -> bool
    where
        Self: Sized,
    {
        arena_opt_walk(self, engine, NodeRef::Expr(e))
    }
    fn visit_block(&mut self, engine: &mut Engine, b: BlockId) -> bool
    where
        Self: Sized,
    {
        arena_opt_walk(self, engine, NodeRef::Block(b))
    }
    fn visit_pattern(&mut self, engine: &mut Engine, p: PatId) -> bool
    where
        Self: Sized,
    {
        arena_opt_walk(self, engine, NodeRef::Pat(p))
    }
}

/// Recurse into every id-bearing child of `node`, dispatching by category, and
/// OR the per-child change flags. The eliminators here only rewrite condition
/// kinds in place (never add/remove nodes), so the upfront child snapshot stays
/// valid through the walk.
fn arena_opt_walk<V: ArenaOptVisitor>(v: &mut V, engine: &mut Engine, node: NodeRef) -> bool {
    let mut kids = Vec::new();
    engine.body.for_each_child(node, |c| kids.push(c));
    let mut changed = false;
    for c in kids {
        changed |= match c {
            NodeRef::Stmt(s) => v.visit_stmt(engine, s),
            NodeRef::Expr(e) => v.visit_expr(engine, e),
            NodeRef::Block(b) => v.visit_block(engine, b),
            NodeRef::Pat(p) => v.visit_pattern(engine, p),
        };
    }
    changed
}
