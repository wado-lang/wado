//! Condition Implication — eliminates conditions implied false by guards.
//!
//! When a loop guard proves `i < bound`, any inner condition `i >= bound` is
//! known false and can be replaced with `false`. The existing `const_branch_prune`
//! pass then removes the dead branch on the next iteration.
//!
//! Also handles dominating if-conditions: when `if (var + offset) < bound { ... }`,
//! bounds checks `(var + k) >= bound` for `k <= offset` inside the then-block
//! are known false.
//!
//! This subsumes the WIR-level `bounds_check` pass, handling both strict `<`
//! and inclusive `<=` guard patterns.
//!
//! Runs on the worklist rewrite engine (combine migration; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`) as a [`Rule`]: a
//! per-function standalone engine session whose `apply_block` fires once at
//! the body root and runs the whole-function dominator / loop-guard walk.
//! The single rewrite point (`condition → BoolLiteral(false)`) routes through
//! `engine.replace_expr_kind` so the parent map and use index stay coherent.
//!
//! The eliminators share a small in-file [`ArenaOptVisitor`] whose default
//! `visit_*` recurse into every child; only `set_false` mutates.

use std::cell::Cell;

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, PatId, StmtId, StmtKind};
use crate::nir_engine::{Engine, EngineBuffers, Rule};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::ValueId;
use crate::tir::TypeTable;

use cranelift_entity::EntityRef;

use super::gate::{FunctionGate, GatedPass};

struct LoopGuard {
    /// Local index of the induction variable (e.g., `i`)
    var: u32,
    /// Bound: a Local, an `IntLiteral`, or a `local.f1.f2…` chain.
    bound: Bound,
    /// `true` for `<` (strict), `false` for `<=` (inclusive)
    is_strict: bool,
    /// `ValueGraph` identity of the guard variable read at the guard
    /// position, if pure. Lets a check `j >= b` be proven false by plain
    /// `ValueId` equality (`engine.value(j) == var_vn`), which already
    /// encodes reaching-defs: a reassignment of the variable between the
    /// guard and the check changes its `ValueId`, so the fast path simply
    /// fails — no separate kill tracking needed for it. `None` for an impure
    /// guard expression (the field-chain / DefMap path then applies).
    var_vn: Option<ValueId>,
    /// `ValueGraph` identity of the guard bound read at the guard position.
    /// Field-chain bounds (`arr.used`) now share a `ValueId` with the check's
    /// read of the same field (per the reachability-aware heap join), so the
    /// fast path catches them without the `Bound::FieldChain` machinery.
    bound_vn: Option<ValueId>,
}

/// A dominating if-condition that proves `var + k < bound` for all `k <= max_offset`.
/// Extracted from `if (var + max_offset) < bound { then_block }`.
#[derive(Clone)]
struct DominatingGuard {
    /// Local index of the base variable
    var: u32,
    /// Maximum offset proven: `var + max_offset < bound`
    max_offset: i64,
    /// Bound (Local, literal, or `local.f1.f2…` chain)
    bound: Bound,
}

/// Right-hand side of a guard or bounds-check comparison.
///
/// - `Local` — e.g. an LICM-hoisted `let _licm_used_25 = arr.used;`,
///   identity-compared via [`resolves_to`].
/// - `Literal` — a folded constant; lets `for n in 0..=143 { … }`
///   work without a `.used` field access surviving to NIR.
/// - `FieldChain` — `local.f1.f2…`; lets `for n in 0..arr.used { … }`
///   work directly. An empty `field_indices` is never constructed
///   (degenerates to `Local`).
#[derive(Clone, PartialEq, Eq)]
enum Bound {
    Local(u32),
    Literal(i64),
    FieldChain {
        root_local: u32,
        field_indices: Vec<u32>,
    },
}

#[derive(Clone)]
enum Def {
    /// `let x = local(y)` — simple copy
    Copy(u32),
    /// `let x = y + const_val`
    AddConst(u32, i64),
    /// `let x = y & const_val` — bitmask, result is in `[0, mask]`
    BitAndConst(i64),
    /// `let x = int_literal` — known constant
    IntConst(i64),
    /// `let x = obj.field` — field access on a local
    FieldAccess { local: u32, field_index: u32 },
    /// Struct literal: maps `field_index` → source for fields
    StructLit(IndexMap<u32, FieldSource>),
}

#[derive(Clone)]
enum FieldSource {
    Local(u32),
    Const(i64),
}

type DefMap = IndexMap<u32, Def>;

pub fn eliminate_implied_conditions(project: &mut NirPackage, gate: &mut FunctionGate) -> bool {
    let type_table = project.type_table.borrow();
    let len = project.functions.len();
    let mut buffers = EngineBuffers::default();
    gate.run_gated(GatedPass::ConditionImplication, len, |fid| {
        let mut func = project.functions[fid.index()].borrow_mut();
        if func.body.is_none() {
            return false;
        }
        let rule = ConditionImplicationRule {
            applied: Cell::new(false),
            type_table: &type_table,
        };
        let NirFunction { body, locals, .. } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.run(&[&rule])
    })
}

/// Standalone-session rule whose single `apply_block` performs the whole-
/// function condition-implication walk at the body root.
pub(super) struct ConditionImplicationRule<'a> {
    applied: Cell<bool>,
    type_table: &'a TypeTable,
}

impl Rule for ConditionImplicationRule<'_> {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        if engine.parent_of(NodeRef::Block(block)).is_some() {
            return false;
        }
        if self.applied.replace(true) {
            return false;
        }
        let tainted = collect_tainted_locals(engine.body);
        let mut defs = DefMap::default();
        let root = engine.body.root;
        process_block(engine, root, &mut defs, &tainted, self.type_table)
    }
}

/// Mutation events observed in a statement subtree that can invalidate a
/// previously established guard fact.
///
/// A guard proves a relation between the values its operands had *when the
/// guard was evaluated*. A later write to the guard variable (`i += 1`), to
/// the bound's backing storage (`arr.used = …` from an inlined `pop`), or an
/// opaque heap effect (a call that may mutate the bound's object) means a
/// re-read at a check site can observe a different value, so the implication
/// no longer holds there. The eliminators collect these events in document
/// order and permanently retire affected guards (see
/// `array_bounds_elim_oob_guard_var_mutated.wado` /
/// `array_bounds_elim_oob_bound_shrunk.wado`).
#[derive(Default)]
struct KillEvents {
    /// Bare locals written by `local = expr` or exposed via `&mut local`.
    locals: IndexSet<u32>,
    /// `field_index`es written by `obj.f = expr` or exposed via `&mut obj.f`.
    /// Receiver-insensitive: a write to any object's `f` retires bounds
    /// reading field `f`, mirroring the per-field heap model elsewhere.
    fields: IndexSet<u32>,
    /// An opaque heap effect: a non-builtin, possibly-returning call, or a
    /// write through a reference. Retires every field-chain bound.
    heap: bool,
}

/// Collect every kill event in `node`'s subtree. The granularity is one
/// statement: a guard is retired *before* the eliminators look inside a
/// statement that contains a kill for it, so a check and a write inside the
/// same statement conservatively keep the check.
fn collect_kill_events(body: &Body, node: NodeRef, type_table: &TypeTable, out: &mut KillEvents) {
    if let NodeRef::Expr(e) = node {
        record_kill_event(body, e, type_table, out);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_kill_events(body, c, type_table, out);
    }
}

fn record_kill_event(body: &Body, e: ExprId, type_table: &TypeTable, out: &mut KillEvents) {
    match &body.exprs[e].kind {
        ExprKind::Assign { target, .. } => match &body.exprs[*target].kind {
            ExprKind::Local { index, .. } => {
                out.locals.insert(*index);
            }
            ExprKind::FieldAccess { field_index, .. } => {
                out.fields.insert(*field_index);
            }
            // `arr[i] = v` writes an element; bounds read locals and
            // length-carrying fields, never elements.
            ExprKind::Index { .. } => {}
            _ => out.heap = true,
        },
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => match &body.exprs[*inner].kind {
            ExprKind::Local { index, .. } => {
                out.locals.insert(*index);
            }
            ExprKind::FieldAccess { field_index, .. } => {
                out.fields.insert(*field_index);
            }
            _ => out.heap = true,
        },
        ExprKind::Call { func, .. } => {
            // A diverging call never resumes on the path where it ran, so
            // code after it (in walk order) only executes when it did not
            // run. Builtins operate below the struct-field layer
            // (`array_set`, `store_u8`, …) and cannot move a bound.
            if !type_table.is_never(body.exprs[e].type_id)
                && func.builtin_name().is_none()
                && func.monomorphized_builtin_name().is_none()
            {
                out.heap = true;
            }
        }
        ExprKind::MethodCall { .. }
        | ExprKind::IndirectCall { .. }
        | ExprKind::CmRawCall { .. } => {
            if !type_table.is_never(body.exprs[e].type_id) {
                out.heap = true;
            }
        }
        _ => {}
    }
}

/// Whether `bound`'s backing storage may have been written per `k`.
fn bound_killed(bound: &Bound, k: &KillEvents) -> bool {
    match bound {
        Bound::Literal(_) => false,
        Bound::Local(idx) => k.locals.contains(idx),
        Bound::FieldChain {
            root_local,
            field_indices,
        } => {
            k.heap
                || k.locals.contains(root_local)
                || field_indices.iter().any(|f| k.fields.contains(f))
        }
    }
}

/// Per-function summary of locals and `(local, field_index)` pairs
/// whose values may change anywhere in the function body.
/// [`record_def_from_stmt`] and [`record_struct_lit_def`] skip
/// captures that would go stale, so the numeric path in
/// [`check_bound_implied_false`] (via [`Bound::to_constant`]) never
/// reads a `Def` whose underlying storage was later mutated.
#[derive(Default)]
struct Taints {
    /// Locals whose whole value may change: direct `local = expr`,
    /// `&mut local` escapes, `is_mut` call args, and method-call
    /// receivers (auto-`&mut self` is opaque here).
    locals: IndexSet<u32>,
    /// `(local, field_index)` pairs assigned by `local.field = expr`.
    /// [`record_struct_lit_def`] uses this to drop only the stale
    /// field, keeping the rest of the struct literal's captures.
    fields: IndexSet<(u32, u32)>,
}

/// Walk the function body and collect every local whose value (or
/// recorded field) could change anywhere downstream. Sources of taint
/// are described on [`Taints`].
fn collect_tainted_locals(body: &Body) -> Taints {
    let mut taints = Taints::default();
    taint_node(body, NodeRef::Block(body.root), &mut taints);
    taints
}

/// Mirror of `TaintCollector` (a `NirRefVisitor`): record taint at every
/// expression node, then recurse into all id-bearing children.
fn taint_node(body: &Body, node: NodeRef, taints: &mut Taints) {
    if let NodeRef::Expr(e) = node {
        taint_record(body, e, taints);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        taint_node(body, c, taints);
    }
}

fn taint_record(body: &Body, e: ExprId, taints: &mut Taints) {
    match &body.exprs[e].kind {
        ExprKind::Assign { target, .. } => {
            taint_target(body, *target, taints);
        }
        ExprKind::Unary {
            op: NirUnaryOp::MutRef,
            expr: inner,
        } => {
            // `&mut local.field` aliases the field only;
            // `&mut local` aliases the whole local.
            let inner = *inner;
            match &body.exprs[inner].kind {
                ExprKind::Local { index, .. } => {
                    taints.locals.insert(*index);
                }
                ExprKind::FieldAccess {
                    expr: receiver,
                    field_index,
                    ..
                } if matches!(body.exprs[*receiver].kind, ExprKind::Local { .. }) => {
                    if let ExprKind::Local { index, .. } = &body.exprs[*receiver].kind {
                        taints.fields.insert((*index, *field_index));
                    }
                }
                _ => {
                    taint_root_local(body, inner, taints);
                }
            }
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                if arg.is_mut
                    && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                {
                    taints.locals.insert(*index);
                }
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            // Auto-ref makes the receiver implicitly mutable when
            // the callee is a `&mut self` method, but we can't
            // tell statically — taint the whole receiver.
            if let ExprKind::Local { index, .. } = &body.exprs[*receiver].kind {
                taints.locals.insert(*index);
            }
            for arg in args {
                if arg.is_mut
                    && let ExprKind::Local { index, .. } = &body.exprs[arg.expr].kind
                {
                    taints.locals.insert(*index);
                }
            }
        }
        _ => {}
    }
}

/// Resolve an `Assign`'s target shape into the (local, optional `field_index`)
/// it mutates and record it.
fn taint_target(body: &Body, target: ExprId, taints: &mut Taints) {
    match &body.exprs[target].kind {
        // `local = expr`.
        ExprKind::Local { index, .. } => {
            taints.locals.insert(*index);
        }
        // `local.field = expr`: only that field is stale.
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } if matches!(body.exprs[*inner].kind, ExprKind::Local { .. }) => {
            if let ExprKind::Local { index, .. } = &body.exprs[*inner].kind {
                taints.fields.insert((*index, *field_index));
            }
        }
        // Nested receiver: `local.f.g = …` taints `local` whole — the inner
        // field's contents are now in an unknown state from our (one-level)
        // recorder's view.
        ExprKind::FieldAccess { expr: inner, .. } | ExprKind::Unary { expr: inner, .. } => {
            taint_root_local(body, *inner, taints);
        }
        // `local[i] = expr` mutates an array element, not any recorded field.
        // DefMap never captures element values, so no taint is required.
        ExprKind::Index { .. } => {}
        _ => {}
    }
}

/// Walk a sub-expression to find its root Local and taint it whole.
fn taint_root_local(body: &Body, e: ExprId, taints: &mut Taints) {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => {
            taints.locals.insert(*index);
        }
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. } => {
            taint_root_local(body, *inner, taints);
        }
        _ => {}
    }
}

fn process_block(
    engine: &mut Engine,
    block: BlockId,
    defs: &mut DefMap,
    tainted: &Taints,
    type_table: &TypeTable,
) -> bool {
    let mut changed = false;
    let mut guards: Vec<ShortCircuitGuard> = Vec::new();
    let stmts = engine.body.blocks[block].stmts.clone();
    for s in stmts {
        record_def_from_stmt(engine.body, s, defs, tainted);
        record_defs_from_nested(engine.body, s, defs, tainted);
        // Retire guards whose variable or bound this stmt may mutate —
        // BEFORE applying them, so a check and a write inside the same
        // stmt conservatively keep the check.
        if !guards.is_empty() {
            let mut kills = KillEvents::default();
            collect_kill_events(engine.body, NodeRef::Stmt(s), type_table, &mut kills);
            guards.retain(|g| !g.killed_by(&kills));
        }
        // Apply accumulated guards from previous early-exit stmts to this stmt
        for guard in &guards {
            changed |= guard.eliminate_in_stmt(engine, s, defs);
        }
        changed |= BitmaskEliminator { defs }.visit_stmt(engine, s);
        changed |= ShortCircuitEliminator { defs, type_table }.visit_stmt(engine, s);
        changed |= process_stmt(engine, s, defs, tainted, type_table);
        // If this is `if (var >= bound) { return/break }`, extract a guard
        if let Some(guard) = extract_early_exit_guard(engine.body, s, defs) {
            guards.push(guard);
        }
    }
    changed
}

fn process_stmt(
    engine: &mut Engine,
    s: StmtId,
    defs: &mut DefMap,
    tainted: &Taints,
    type_table: &TypeTable,
) -> bool {
    // Record definitions from let bindings
    record_def_from_stmt(engine.body, s, defs, tainted);

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
        StmtShape::Loop(lb) => process_loop(engine, lb, defs, tainted, type_table),
        StmtShape::If(then_b, else_b) => {
            let mut changed = process_block(engine, then_b, defs, tainted, type_table);
            if let Some(eb) = else_b {
                changed |= process_block(engine, eb, defs, tainted, type_table);
            }
            changed
        }
        StmtShape::Labeled(b) => process_block(engine, b, defs, tainted, type_table),
        StmtShape::None => false,
    }
}

enum StmtShape {
    Loop(BlockId),
    If(BlockId, Option<BlockId>),
    Labeled(BlockId),
    None,
}

fn process_loop(
    engine: &mut Engine,
    loop_body: BlockId,
    defs: &mut DefMap,
    tainted: &Taints,
    type_table: &TypeTable,
) -> bool {
    // First, record defs inside the loop body (for copies like `let index = i`)
    // and recurse into nested structures
    let mut changed = false;

    // Collect defs from the loop body before eliminating
    let mut loop_defs = defs.clone();
    let stmts = engine.body.blocks[loop_body].stmts.clone();
    for s in &stmts {
        record_def_from_stmt(engine.body, *s, &mut loop_defs, tainted);
        record_defs_from_nested(engine.body, *s, &mut loop_defs, tainted);
    }

    // Extract the loop guard from the first statement
    let guard = extract_loop_guard(engine, loop_body);

    if let Some(guard) = &guard {
        // Eliminate implied conditions in the loop body (skip the guard itself)
        let mut condition_elim = ConditionEliminator {
            guard,
            guard_alive: true,
            dom_guards: vec![],
            defs: &loop_defs,
            type_table,
        };
        for s in stmts.iter().skip(1) {
            changed |= condition_elim.visit_stmt(engine, *s);
        }
    }

    // Eliminate bitmask-bounded checks in the loop body
    for s in &stmts {
        changed |= BitmaskEliminator { defs: &loop_defs }.visit_stmt(engine, *s);
    }

    // Recurse into nested loops
    for s in &stmts {
        changed |= process_stmt_nested_loops(engine, *s, defs, tainted, type_table);
    }

    changed
}

/// Recurse into nested structures to find inner loops, but don't re-process
/// the current loop level.
fn process_stmt_nested_loops(
    engine: &mut Engine,
    s: StmtId,
    defs: &mut DefMap,
    tainted: &Taints,
    type_table: &TypeTable,
) -> bool {
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
        StmtShape::Loop(lb) => process_loop(engine, lb, defs, tainted, type_table),
        StmtShape::If(then_b, else_b) => {
            let mut changed = false;
            for s in engine.body.blocks[then_b].stmts.clone() {
                changed |= process_stmt_nested_loops(engine, s, defs, tainted, type_table);
            }
            if let Some(eb) = else_b {
                for s in engine.body.blocks[eb].stmts.clone() {
                    changed |= process_stmt_nested_loops(engine, s, defs, tainted, type_table);
                }
            }
            changed
        }
        StmtShape::Labeled(b) => {
            let mut changed = false;
            for s in engine.body.blocks[b].stmts.clone() {
                changed |= process_stmt_nested_loops(engine, s, defs, tainted, type_table);
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
fn extract_loop_guard(engine: &mut Engine, loop_body: BlockId) -> Option<LoopGuard> {
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

    // then_block must be a single Break statement
    if engine.body.blocks[then_block].stmts.len() != 1 {
        return None;
    }
    matches!(
        &engine.body.stmts[engine.body.blocks[then_block].stmts[0]].kind,
        StmtKind::Break { .. }
    )
    .then_some(())?;

    // condition must be `Not(Binary(var, Lt|LtEq, bound))`
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

    let (is_strict, var_expr, bound_expr) = match op {
        NirBinaryOp::Lt => (true, *left, *right),
        NirBinaryOp::LtEq => (false, *left, *right),
        _ => return None,
    };

    let ExprKind::Local { index: var, .. } = &engine.body.exprs[var_expr].kind else {
        return None;
    };
    let var = *var;
    let bound = Bound::extract(engine.body, bound_expr)?;
    // The reads' value-graph identities, captured at the guard position.
    let var_vn = engine.value(var_expr);
    let bound_vn = engine.value(bound_expr);

    Some(LoopGuard {
        var,
        bound,
        is_strict,
        var_vn,
        bound_vn,
    })
}

impl Bound {
    /// Decode an expression as a `Bound`. Returns `None` for shapes
    /// we don't model (arithmetic, casts, method calls, …); `Cast`
    /// in particular may truncate or sign-flip and we don't inspect
    /// source / target types here.
    ///
    /// `IntLiteral.value` is the literal's `u64` bit pattern; use
    /// `i64::try_from` so a payload with bit 63 set bails rather
    /// than silently feed a negative bound to the numeric
    /// comparisons in [`check_bound_implied_false`].
    fn extract(body: &Body, e: ExprId) -> Option<Bound> {
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => Some(Bound::Local(*index)),
            ExprKind::IntLiteral { value, .. } => i64::try_from(*value).ok().map(Bound::Literal),
            ExprKind::FieldAccess { .. } => {
                let (root_local, field_indices) = extract_field_chain(body, e)?;
                if field_indices.is_empty() {
                    Some(Bound::Local(root_local))
                } else {
                    Some(Bound::FieldChain {
                        root_local,
                        field_indices,
                    })
                }
            }
            _ => None,
        }
    }

    /// Project a `Bound` to a concrete integer through the `defs`
    /// chain. `Literal` returns immediately; `Local` walks
    /// [`resolve_constant`]; `FieldChain` only resolves the
    /// single-level form (`local.field`) — multi-level chains would
    /// require chained struct-lit lookup that no `Def` shape
    /// captures today.
    fn to_constant(&self, defs: &DefMap) -> Option<i64> {
        match self {
            Bound::Literal(v) => Some(*v),
            Bound::Local(idx) => resolve_constant(*idx, defs),
            Bound::FieldChain {
                root_local,
                field_indices,
            } if field_indices.len() == 1 => {
                resolve_constant_through_struct(*root_local, field_indices[0], defs, 0)
            }
            Bound::FieldChain { .. } => None,
        }
    }
}

/// True when a comparison `var >= check` is implied false by a loop
/// (or dominating) guard `var (< | <=) guard`.
///
/// Two regimes, chosen by the operand shape:
///
/// - Identity: two `Local`s via [`resolves_to`] (and
///   [`resolves_to_plus_one`] for the non-strict guard), or two
///   `FieldChain`s with the same `field_indices` and roots that
///   resolve to the same local.
/// - Numeric: both bounds project to concrete integers via
///   [`Bound::to_constant`], then `var < guard` proves `var < check`
///   whenever `check >= guard`; `var <= guard` proves it whenever
///   `check > guard`.
///
/// The numeric path catches `for n in 0..=143 { arr[n] = … }` where
/// `arr.used` folded to a literal; the [`Bound::FieldChain`]
/// identity path catches `for n in 0..arr.used { arr[n] = … }`
/// where it didn't.
fn check_bound_implied_false(
    check: &Bound,
    guard: &Bound,
    is_strict_guard: bool,
    defs: &DefMap,
) -> bool {
    // Identity regime: same structural shape, no value comparison.
    // Sound for the strict guard whenever the two bounds denote the
    // same runtime value; the non-strict (`<=`) form needs a +1
    // step that only the Local chain walk currently expresses.
    match (check, guard) {
        (Bound::Local(c), Bound::Local(g)) => {
            return if is_strict_guard {
                resolves_to(*c, *g, defs)
            } else {
                resolves_to_plus_one(*c, *g, defs)
            };
        }
        (
            Bound::FieldChain {
                root_local: cr,
                field_indices: cf,
            },
            Bound::FieldChain {
                root_local: gr,
                field_indices: gf,
            },
        ) if is_strict_guard && cf == gf => {
            if resolves_to(*cr, *gr, defs) {
                return true;
            }
        }
        _ => {}
    }
    // Numeric regime: both sides project to a concrete constant.
    let (Some(c_val), Some(g_val)) = (check.to_constant(defs), guard.to_constant(defs)) else {
        return false;
    };
    if is_strict_guard {
        c_val >= g_val
    } else {
        c_val > g_val
    }
}

fn record_def_from_stmt(body: &Body, s: StmtId, defs: &mut DefMap, tainted: &Taints) {
    let StmtKind::Let {
        local_index, value, ..
    } = &body.stmts[s].kind
    else {
        return;
    };
    let local_index = *local_index;
    let value = *value;

    // Skip when the let-target itself may later be reassigned: the
    // value-extracting path (`Bound::to_constant`) would otherwise
    // observe a stale snapshot. Identity-only paths (`resolves_to`
    // between two Locals) compare names and stay sound regardless.
    // Field-level taint is handled per-field in
    // `record_struct_lit_def`.
    if tainted.locals.contains(&local_index) {
        return;
    }

    // Unwrap LabeledBlock to find the actual defining expression
    // (e.g., `let arr = __inline_...: { ...; break LABEL: StructLiteral { ... }; }`)
    let effective = unwrap_labeled_block_value(body, value);

    match &body.exprs[effective].kind {
        ExprKind::Local { index, .. } => {
            defs.insert(local_index, Def::Copy(*index));
        }
        ExprKind::Binary { left, op, right } => {
            if let (
                ExprKind::Local { index: lhs, .. },
                NirBinaryOp::Add,
                ExprKind::IntLiteral { value: val, .. },
            ) = (&body.exprs[*left].kind, op, &body.exprs[*right].kind)
            {
                defs.insert(local_index, Def::AddConst(*lhs, *val as i64));
            } else if *op == NirBinaryOp::BitAnd {
                if let ExprKind::IntLiteral { value: val, .. } = &body.exprs[*right].kind {
                    defs.insert(local_index, Def::BitAndConst(*val as i64));
                } else if let ExprKind::IntLiteral { value: val, .. } = &body.exprs[*left].kind {
                    defs.insert(local_index, Def::BitAndConst(*val as i64));
                }
            }
        }
        ExprKind::IntLiteral { value: val, .. } => {
            defs.insert(local_index, Def::IntConst(*val as i64));
        }
        ExprKind::FieldAccess {
            expr, field_index, ..
        } => {
            if let ExprKind::Local { index, .. } = &body.exprs[*expr].kind {
                // The chain `let _licm = arr.used` is captured as
                // `Def::FieldAccess { local: arr, field_index: used }`.
                //
                // The *numeric* path is taint-safe without a gate here:
                // `resolve_constant` walks through `arr`'s `Def::StructLit`,
                // whose recorder already drops field-tainted entries.
                //
                // The *identity* path is not: `resolves_to` treats two
                // locals defined as the same `(local, field)` read as equal,
                // which is wrong when the field is written between the two
                // reads (each `let` snapshots a different value). Skip the
                // def whenever the pair — or the receiver as a whole — may
                // be mutated anywhere in the function, so the equivalence
                // only ever connects reads of immutable storage.
                if !tainted.locals.contains(index)
                    && !tainted.fields.contains(&(*index, *field_index))
                {
                    defs.insert(
                        local_index,
                        Def::FieldAccess {
                            local: *index,
                            field_index: *field_index,
                        },
                    );
                }
            }
        }
        ExprKind::StructLiteral { .. } => {
            record_struct_lit_def(body, local_index, effective, defs, tainted);
        }
        _ => {}
    }
}

fn record_struct_lit_def(
    body: &Body,
    local_index: u32,
    struct_lit: ExprId,
    defs: &mut DefMap,
    tainted: &Taints,
) {
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[struct_lit].kind else {
        return;
    };
    let mut field_map = IndexMap::default();
    for f in fields {
        // Skip fields that may be reassigned anywhere — `local.f = …`
        // taints `(local, f)` and we must not capture this StructLit's
        // initial value for that field, otherwise a later
        // bound-constant lookup would observe the stale snapshot.
        if tainted.fields.contains(&(local_index, f.field_index)) {
            continue;
        }
        if let ExprKind::Local { index, .. } = &body.exprs[f.value].kind {
            field_map.insert(f.field_index, FieldSource::Local(*index));
        } else if let ExprKind::IntLiteral { value, .. } = &body.exprs[f.value].kind {
            field_map.insert(f.field_index, FieldSource::Const(*value as i64));
        }
    }
    if !field_map.is_empty() {
        defs.insert(local_index, Def::StructLit(field_map));
    }
}

/// Unwrap a `LabeledBlock` expression to find the value from its break statement.
/// `LABEL: { ...; break LABEL: expr; }` → `expr`
///
/// Also unwraps a plain `Block { ...; tail_expr }` to `tail_expr`. This shape
/// appears after `branch_prune::prune_expr` flattens a tail-break-only labeled
/// block into a plain stmt list with the broken value as the trailing
/// expression statement.
fn unwrap_labeled_block_value(body: &Body, e: ExprId) -> ExprId {
    if let ExprKind::Block(block) = &body.exprs[e].kind
        && let Some(last) = body.blocks[*block].stmts.last()
        && let StmtKind::Expr(tail) = &body.stmts[*last].kind
    {
        return unwrap_labeled_block_value(body, *tail);
    }
    if let ExprKind::LabeledBlock { block, label, .. } = &body.exprs[e].kind {
        // Find the break statement that returns a value from this block
        for stmt in &body.blocks[*block].stmts {
            if let StmtKind::Break {
                label: Some(break_label),
                value: Some(val),
            } = &body.stmts[*stmt].kind
                && break_label == label
            {
                // Recursively unwrap in case of nested labeled blocks
                return unwrap_labeled_block_value(body, *val);
            }
        }
    }
    e
}

/// Record defs from nested blocks within a statement (e.g., labeled blocks in expressions).
fn record_defs_from_nested(body: &Body, s: StmtId, defs: &mut DefMap, tainted: &Taints) {
    match &body.stmts[s].kind {
        StmtKind::Let { value, .. } => {
            record_defs_from_expr(body, *value, defs, tainted);
        }
        StmtKind::Expr(expr) => {
            record_defs_from_expr(body, *expr, defs, tainted);
        }
        StmtKind::LabeledBlock { block, .. } => {
            for s in body.blocks[*block].stmts.clone() {
                record_def_from_stmt(body, s, defs, tainted);
                record_defs_from_nested(body, s, defs, tainted);
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let condition = *condition;
            let then_block = *then_block;
            let else_block = *else_block;
            record_defs_from_expr(body, condition, defs, tainted);
            for s in body.blocks[then_block].stmts.clone() {
                record_def_from_stmt(body, s, defs, tainted);
                record_defs_from_nested(body, s, defs, tainted);
            }
            if let Some(eb) = else_block {
                for s in body.blocks[eb].stmts.clone() {
                    record_def_from_stmt(body, s, defs, tainted);
                    record_defs_from_nested(body, s, defs, tainted);
                }
            }
        }
        StmtKind::Return { value: Some(expr) }
        | StmtKind::Break {
            value: Some(expr), ..
        } => {
            record_defs_from_expr(body, *expr, defs, tainted);
        }
        StmtKind::LetDestructure { value, .. } => {
            record_defs_from_expr(body, *value, defs, tainted);
        }
        // Loop bodies have their own scope handled via process_loop.
        // Remaining kinds (Return/Break with None, Continue) carry no
        // expressions with nested definitions.
        StmtKind::Loop { .. }
        | StmtKind::Return { value: None }
        | StmtKind::Break { value: None, .. }
        | StmtKind::Continue => {}
    }
}

fn record_defs_from_expr(body: &Body, e: ExprId, defs: &mut DefMap, tainted: &Taints) {
    match &body.exprs[e].kind {
        ExprKind::LabeledBlock { block, .. } | ExprKind::Block(block) => {
            for s in body.blocks[*block].stmts.clone() {
                record_def_from_stmt(body, s, defs, tainted);
                record_defs_from_nested(body, s, defs, tainted);
            }
        }
        ExprKind::Binary { left, right, .. }
        | ExprKind::Assign {
            target: left,
            value: right,
        }
        | ExprKind::Index {
            expr: left,
            index: right,
        } => {
            let left = *left;
            let right = *right;
            record_defs_from_expr(body, left, defs, tainted);
            record_defs_from_expr(body, right, defs, tainted);
        }
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::GlobalVarSet { value: inner, .. }
        | ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::ClosureToCanonical { functor: inner, .. } => {
            record_defs_from_expr(body, *inner, defs, tainted);
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let condition = *condition;
            let then_branch = *then_branch;
            let else_branch = *else_branch;
            record_defs_from_expr(body, condition, defs, tainted);
            for s in body.blocks[then_branch].stmts.clone() {
                record_def_from_stmt(body, s, defs, tainted);
                record_defs_from_nested(body, s, defs, tainted);
            }
            if let Some(eb) = else_branch {
                for s in body.blocks[eb].stmts.clone() {
                    record_def_from_stmt(body, s, defs, tainted);
                    record_defs_from_nested(body, s, defs, tainted);
                }
            }
        }
        ExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            let scrutinee = *scrutinee;
            let arm_exprs: Vec<ExprId> = arms
                .iter()
                .flat_map(|arm| arm.guard.iter().copied().chain(std::iter::once(arm.body)))
                .collect();
            record_defs_from_expr(body, scrutinee, defs, tainted);
            for ae in arm_exprs {
                record_defs_from_expr(body, ae, defs, tainted);
            }
        }
        ExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let scrutinee = *scrutinee;
            let arms = arms.clone();
            let default = *default;
            record_defs_from_expr(body, scrutinee, defs, tainted);
            for arm in arms {
                for s in body.blocks[arm].stmts.clone() {
                    record_def_from_stmt(body, s, defs, tainted);
                    record_defs_from_nested(body, s, defs, tainted);
                }
            }
            for s in body.blocks[default].stmts.clone() {
                record_def_from_stmt(body, s, defs, tainted);
                record_defs_from_nested(body, s, defs, tainted);
            }
        }
        ExprKind::Call { args, .. } => {
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            for arg in args {
                record_defs_from_expr(body, arg, defs, tainted);
            }
        }
        ExprKind::CmRawCall { args, .. } => {
            let args = args.clone();
            for arg in args {
                record_defs_from_expr(body, arg, defs, tainted);
            }
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let receiver = *receiver;
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            record_defs_from_expr(body, receiver, defs, tainted);
            for arg in args {
                record_defs_from_expr(body, arg, defs, tainted);
            }
        }
        ExprKind::IndirectCall { callee, args } => {
            let callee = *callee;
            let args = args.clone();
            record_defs_from_expr(body, callee, defs, tainted);
            for arg in args {
                record_defs_from_expr(body, arg, defs, tainted);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            let vals: Vec<ExprId> = fields.iter().map(|f| f.value).collect();
            for v in vals {
                record_defs_from_expr(body, v, defs, tainted);
            }
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            for el in elements {
                record_defs_from_expr(body, el, defs, tainted);
            }
        }
        ExprKind::VariantConstruct { payload, .. } => {
            if let Some(inner) = payload {
                record_defs_from_expr(body, *inner, defs, tainted);
            }
        }
        // Leaf nodes carry no sub-expressions with definitions.
        ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::StringLiteral(_)
        | ExprKind::BytesLiteral(_)
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::Null
        | ExprKind::Unit
        | ExprKind::EnumConstruct { .. } => {}
    }
}

/// Replace the expression at `cond` with `false`, preserving its type and span.
fn set_false(engine: &mut Engine, cond: ExprId) {
    engine.replace_expr_kind(cond, ExprKind::BoolLiteral(false));
}

/// NIR visitor that eliminates loop-guard-implied false bounds checks.
///
/// When a loop guard proves `i < bound`, inner conditions `i >= bound` are
/// replaced with `false`. Dominating if-conditions are also tracked to extend
/// the elimination into their then-blocks.
///
/// Guards are positional facts: every `visit_stmt` first scans the statement
/// for [`KillEvents`] and permanently retires any guard whose variable or
/// bound the statement may mutate, so checks after the mutation (in document
/// order) are never eliminated against the stale fact. Dominating guards are
/// scoped by `Vec` truncation (not clone/restore) so an in-scope kill cannot
/// be resurrected when an inner `if` exits.
struct ConditionEliminator<'a> {
    guard: &'a LoopGuard,
    /// Cleared once the loop body (in document order) may write `guard.var`
    /// or the guard bound's backing storage.
    guard_alive: bool,
    dom_guards: Vec<DomEntry>,
    defs: &'a DefMap,
    type_table: &'a TypeTable,
}

/// A dominating guard plus its positional liveness (see
/// [`ConditionEliminator`]).
struct DomEntry {
    guard: DominatingGuard,
    alive: bool,
}

impl ConditionEliminator<'_> {
    /// Scan `s` for kill events and retire affected guards. Runs before the
    /// statement's checks are considered, so a write and a check inside one
    /// statement conservatively keep the check.
    fn apply_kills(&mut self, body: &Body, s: StmtId) {
        if !self.guard_alive && self.dom_guards.iter().all(|d| !d.alive) {
            return;
        }
        let mut kills = KillEvents::default();
        collect_kill_events(body, NodeRef::Stmt(s), self.type_table, &mut kills);
        if kills.locals.is_empty() && kills.fields.is_empty() && !kills.heap {
            return;
        }
        if self.guard_alive
            && (kills.locals.contains(&self.guard.var) || bound_killed(&self.guard.bound, &kills))
        {
            self.guard_alive = false;
        }
        for d in &mut self.dom_guards {
            if d.alive
                && (kills.locals.contains(&d.guard.var) || bound_killed(&d.guard.bound, &kills))
            {
                d.alive = false;
            }
        }
    }

    fn implied_false(&self, engine: &mut Engine, condition: ExprId) -> bool {
        if self.guard_alive {
            // Fast path: prove `check_lhs >= check_bound` false by plain
            // ValueId equality against the guard's captured identities. This
            // catches field-chain bounds (`arr.used`) the `DefMap` path
            // misses, and is sound by construction — equal `ValueId`s denote
            // the same runtime value at both program points.
            if self.vn_implies_false(engine, condition) {
                return true;
            }
            if is_implied_false(engine.body, condition, self.guard, self.defs) {
                return true;
            }
        }
        self.dom_guards
            .iter()
            .filter(|d| d.alive)
            .any(|d| is_implied_by_dominating_guard(engine.body, condition, &d.guard, self.defs))
    }

    /// Strict-guard identity regime over `ValueId`s: a guard `var < bound`
    /// proves `check_lhs >= check_bound` false when `check_lhs` carries the
    /// guard variable's `ValueId` and `check_bound` the guard bound's. The
    /// non-strict (`<=`) and offset regimes stay on the `DefMap` path.
    fn vn_implies_false(&self, engine: &mut Engine, condition: ExprId) -> bool {
        if !self.guard.is_strict {
            return false;
        }
        let (Some(gv), Some(gb)) = (self.guard.var_vn, self.guard.bound_vn) else {
            return false;
        };
        let ExprKind::Binary {
            left,
            op: NirBinaryOp::GtEq,
            right,
        } = &engine.body.exprs[condition].kind
        else {
            return false;
        };
        let (left, right) = (*left, *right);
        engine.value(left) == Some(gv) && engine.value(right) == Some(gb)
    }
}

impl ArenaOptVisitor for ConditionEliminator<'_> {
    fn visit_stmt(&mut self, engine: &mut Engine, s: StmtId) -> bool {
        self.apply_kills(engine.body, s);
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
                && is_panic_block(engine.body, then_block)
                && self.implied_false(engine, condition)
            {
                set_false(engine, condition);
                return true;
            }

            // Extract a dominating guard from the condition to extend
            // elimination into the then-block.
            let mut changed = self.visit_expr(engine, condition);
            let dom = extract_dominating_guard(engine.body, condition, self.defs);
            let scope_len = self.dom_guards.len();
            if let Some(dg) = dom {
                self.dom_guards.push(DomEntry {
                    guard: dg,
                    alive: true,
                });
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
            let dom = extract_dominating_guard(engine.body, condition, self.defs);
            let scope_len = self.dom_guards.len();
            if let Some(dg) = dom {
                self.dom_guards.push(DomEntry {
                    guard: dg,
                    alive: true,
                });
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

/// NIR visitor that eliminates bitmask-bounded false bounds checks.
///
/// Pattern: `if (x & MASK) >= BOUND { panic(...) }` where `BOUND > MASK >= 0`
/// Since `(x & MASK)` is always in `[0, MASK]`, the condition is always false.
struct BitmaskEliminator<'a> {
    defs: &'a DefMap,
}

impl ArenaOptVisitor for BitmaskEliminator<'_> {
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
            && is_panic_block(engine.body, then_block)
            && is_bitmask_bounded(engine.body, condition, self.defs)
        {
            set_false(engine, condition);
            return true;
        }
        arena_opt_walk(self, engine, NodeRef::Stmt(s))
    }
}

/// Extract a guard from an early-exit if-statement.
///
/// Matches: `if (var + k) >= bound { return/break }` → after this stmt,
/// we know `(var + k) < bound`.
fn extract_early_exit_guard(body: &Body, s: StmtId, defs: &DefMap) -> Option<ShortCircuitGuard> {
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &body.stmts[s].kind
    else {
        return None;
    };
    let condition = *condition;
    let then_block = *then_block;

    // then_block must be all early exits (return/break)
    if !block_always_exits(body, then_block) {
        return None;
    }

    ShortCircuitGuard::extract(body, condition, defs)
}

fn block_always_exits(body: &Body, block: BlockId) -> bool {
    body.blocks[block].stmts.iter().any(|s| {
        matches!(
            body.stmts[*s].kind,
            StmtKind::Return { .. } | StmtKind::Break { .. }
        )
    })
}

/// Eliminate redundant bounds checks inside short-circuit `||` expressions.
///
/// Pattern: `(start + k) >= bound || expr`
/// The right operand `expr` only executes when `(start + k) < bound`.
/// Any `if (index >= bound) { panic }` inside `expr` where `index` resolves
/// to the same value as `start + k` (or `start + j` with `j <= k`) is always false.
///
/// This handles both `Local` and `FieldAccess` bounds (e.g., `chars.used`).
struct ShortCircuitEliminator<'a> {
    defs: &'a DefMap,
    type_table: &'a TypeTable,
}

impl ArenaOptVisitor for ShortCircuitEliminator<'_> {
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
            if let Some(guard) = ShortCircuitGuard::extract(engine.body, left, self.defs) {
                // The rhs may itself mutate the guard's operands (a call or
                // assignment inside the expression); apply only when it
                // provably cannot.
                let mut kills = KillEvents::default();
                collect_kill_events(
                    engine.body,
                    NodeRef::Expr(right),
                    self.type_table,
                    &mut kills,
                );
                if !guard.killed_by(&kills) {
                    changed |= guard.eliminate_in_expr(engine, right, self.defs);
                }
            }
            changed |= self.visit_expr(engine, right);
            return changed;
        }
        arena_opt_walk(self, engine, NodeRef::Expr(e))
    }
}

/// A guard extracted from the left side of `||` being false.
///
/// From `(var + offset) >= bound` being false, we know `(var + k) < bound`
/// for all `k <= offset`.
struct ShortCircuitGuard {
    /// The base variable (e.g., `pos` or `start`)
    var: u32,
    /// Maximum offset proven safe: `var + max_offset < bound`
    max_offset: i64,
    /// The bound expression (Local, Literal, or field chain).
    bound: Bound,
}

/// Decompose `local`, `local.f1`, `local.f1.f2`, ... into a `(root_local,
/// field_indices)` pair. Returns `None` for anything else (method calls,
/// arithmetic, etc.) — the caller treats those as opaque bounds and bails.
fn extract_field_chain(body: &Body, e: ExprId) -> Option<(u32, Vec<u32>)> {
    match &body.exprs[e].kind {
        ExprKind::Local { index, .. } => Some((*index, Vec::new())),
        ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let (root, mut fields) = extract_field_chain(body, *inner)?;
            fields.push(*field_index);
            Some((root, fields))
        }
        _ => None,
    }
}

impl ShortCircuitGuard {
    /// Whether `kills` may mutate this guard's variable or bound, making
    /// the fact stale for code after the kill point.
    fn killed_by(&self, kills: &KillEvents) -> bool {
        kills.locals.contains(&self.var) || bound_killed(&self.bound, kills)
    }

    /// Extract a guard from `(var + k) >= bound` being false.
    fn extract(body: &Body, condition: ExprId, defs: &DefMap) -> Option<Self> {
        let ExprKind::Binary { left, op, right } = &body.exprs[condition].kind else {
            return None;
        };
        if *op != NirBinaryOp::GtEq {
            return None;
        }
        let left = *left;
        let right = *right;

        let bound = Bound::extract(body, right)?;

        let (var, max_offset) = match &body.exprs[left].kind {
            ExprKind::Local { index, .. } => (*index, 0),
            ExprKind::Binary {
                left: inner_left,
                op: NirBinaryOp::Add,
                right: inner_right,
            } => {
                let ExprKind::Local { index: var, .. } = &body.exprs[*inner_left].kind else {
                    return None;
                };
                let var = *var;
                let offset = match &body.exprs[*inner_right].kind {
                    ExprKind::IntLiteral { value, .. } => *value as i64,
                    ExprKind::Local { index, .. } => resolve_constant(*index, defs)?,
                    _ => return None,
                };
                if offset < 0 {
                    return None;
                }
                (var, offset)
            }
            _ => return None,
        };

        Some(ShortCircuitGuard {
            var,
            max_offset,
            bound,
        })
    }

    /// Check if `check_var >= check_bound` is implied false by this guard.
    fn implies_false(&self, body: &Body, condition: ExprId, defs: &DefMap) -> bool {
        let ExprKind::Binary { left, op, right } = &body.exprs[condition].kind else {
            return false;
        };
        if *op != NirBinaryOp::GtEq {
            return false;
        }
        let left = *left;
        let right = *right;

        // Check that the bound matches
        if !self.bound_matches(body, right, defs) {
            return false;
        }

        // Check that check_var resolves to var + k where k <= max_offset
        self.var_in_range(body, left, defs)
    }

    fn bound_matches(&self, body: &Body, e: ExprId, defs: &DefMap) -> bool {
        // Decode `e` as a Bound and compare with the guard's bound via the same
        // identity / numeric regime that `check_bound_implied_false` uses.
        let Some(expr_bound) = Bound::extract(body, e) else {
            return false;
        };
        match (&expr_bound, &self.bound) {
            (Bound::Local(c), Bound::Local(g)) => resolves_to(*c, *g, defs),
            (
                Bound::FieldChain {
                    root_local: cr,
                    field_indices: cf,
                },
                Bound::FieldChain {
                    root_local: gr,
                    field_indices: gf,
                },
            ) if cf == gf => resolves_to(*cr, *gr, defs),
            (Bound::Literal(c), Bound::Literal(g)) => c == g,
            _ => match (expr_bound.to_constant(defs), self.bound.to_constant(defs)) {
                (Some(c), Some(g)) => c == g,
                _ => false,
            },
        }
    }

    fn var_in_range(&self, body: &Body, e: ExprId, defs: &DefMap) -> bool {
        match &body.exprs[e].kind {
            ExprKind::Local { index, .. } => {
                if resolves_to(*index, self.var, defs) {
                    return true; // offset 0 <= max_offset
                }
                // Check if it resolves to var + k through defs
                resolve_offset_from(*index, self.var, defs)
                    .is_some_and(|offset| offset >= 0 && offset <= self.max_offset)
            }
            ExprKind::Binary {
                left,
                op: NirBinaryOp::Add,
                right,
            } => {
                let ExprKind::Local { index, .. } = &body.exprs[*left].kind else {
                    return false;
                };
                if !resolves_to(*index, self.var, defs) {
                    return false;
                }
                let offset = match &body.exprs[*right].kind {
                    ExprKind::IntLiteral { value, .. } => *value as i64,
                    ExprKind::Local { index, .. } => {
                        if let Some(c) = resolve_constant(*index, defs) {
                            c
                        } else {
                            return false;
                        }
                    }
                    _ => return false,
                };
                offset >= 0 && offset <= self.max_offset
            }
            _ => false,
        }
    }

    fn eliminate_in_expr(&self, engine: &mut Engine, e: ExprId, defs: &DefMap) -> bool {
        let walk = match &engine.body.exprs[e].kind {
            ExprKind::LabeledBlock { block, .. } | ExprKind::Block(block) => ScWalk::Block(*block),
            ExprKind::Binary { left, right, .. } => ScWalk::Exprs2(*left, *right),
            ExprKind::Unary { expr: inner, .. }
            | ExprKind::Cast { expr: inner, .. }
            | ExprKind::FieldAccess { expr: inner, .. } => ScWalk::Expr1(*inner),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => ScWalk::If(*condition, *then_branch, *else_branch),
            _ => ScWalk::None,
        };
        match walk {
            ScWalk::Block(b) => self.eliminate_in_block(engine, b, defs),
            ScWalk::Exprs2(left, right) => {
                let mut changed = self.eliminate_in_expr(engine, left, defs);
                changed |= self.eliminate_in_expr(engine, right, defs);
                changed
            }
            ScWalk::Expr1(inner) => self.eliminate_in_expr(engine, inner, defs),
            ScWalk::If(condition, then_branch, else_branch) => {
                let mut changed = self.eliminate_in_expr(engine, condition, defs);
                changed |= self.eliminate_in_block(engine, then_branch, defs);
                if let Some(eb) = else_branch {
                    changed |= self.eliminate_in_block(engine, eb, defs);
                }
                changed
            }
            ScWalk::None => false,
        }
    }

    fn eliminate_in_block(&self, engine: &mut Engine, block: BlockId, defs: &DefMap) -> bool {
        let mut changed = false;
        for s in engine.body.blocks[block].stmts.clone() {
            changed |= self.eliminate_in_stmt(engine, s, defs);
        }
        changed
    }

    fn eliminate_in_stmt(&self, engine: &mut Engine, s: StmtId, defs: &DefMap) -> bool {
        // Check if this is a bounds-check `if (index >= bound) { panic() }` implied false
        let if_ids = match &engine.body.stmts[s].kind {
            StmtKind::If {
                condition,
                then_block,
                else_block: None,
            } => Some((*condition, *then_block)),
            _ => None,
        };
        if let Some((condition, then_block)) = if_ids
            && is_panic_block(engine.body, then_block)
            && self.implies_false(engine.body, condition, defs)
        {
            set_false(engine, condition);
            return true;
        }

        // Recurse into sub-expressions and sub-statements
        let walk = match &engine.body.stmts[s].kind {
            StmtKind::Let { value, .. } => ScStmt::Expr(*value),
            StmtKind::Expr(expr) => ScStmt::Expr(*expr),
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => ScStmt::If(*condition, *then_block, *else_block),
            StmtKind::Return { value: Some(expr) }
            | StmtKind::Break {
                value: Some(expr), ..
            } => ScStmt::Expr(*expr),
            StmtKind::LabeledBlock { block, .. } | StmtKind::Loop { body: block } => {
                ScStmt::Block(*block)
            }
            _ => ScStmt::None,
        };
        match walk {
            ScStmt::Expr(e) => self.eliminate_in_expr(engine, e, defs),
            ScStmt::If(condition, then_block, else_block) => {
                let mut changed = self.eliminate_in_expr(engine, condition, defs);
                changed |= self.eliminate_in_block(engine, then_block, defs);
                if let Some(eb) = else_block {
                    changed |= self.eliminate_in_block(engine, eb, defs);
                }
                changed
            }
            ScStmt::Block(b) => self.eliminate_in_block(engine, b, defs),
            ScStmt::None => false,
        }
    }
}

enum ScWalk {
    Block(BlockId),
    Exprs2(ExprId, ExprId),
    Expr1(ExprId),
    If(ExprId, BlockId, Option<BlockId>),
    None,
}

enum ScStmt {
    Expr(ExprId),
    If(ExprId, BlockId, Option<BlockId>),
    Block(BlockId),
    None,
}

/// Check if `(index >= bound)` is provably false because index is bitmask-bounded.
///
/// `(x & MASK) >= BOUND` is false when `MASK >= 0` and `BOUND > MASK`.
fn is_bitmask_bounded(body: &Body, condition: ExprId, defs: &DefMap) -> bool {
    let ExprKind::Binary { left, op, right } = &body.exprs[condition].kind else {
        return false;
    };

    if *op != NirBinaryOp::GtEq {
        return false;
    }

    let ExprKind::Local {
        index: check_var, ..
    } = &body.exprs[*left].kind
    else {
        return false;
    };
    let check_var = *check_var;
    let Some(check_bound) = Bound::extract(body, *right) else {
        return false;
    };

    // Find the maximum value of check_var (if bitmask-bounded)
    let Some(max_val) = resolve_max_value(check_var, defs) else {
        return false;
    };

    // Resolve `check_bound` to a concrete integer — literal RHS goes
    // through directly, a Local RHS gets walked through `defs`.
    let Some(bound_val) = check_bound.to_constant(defs) else {
        return false;
    };

    // If max possible value < bound, then `check_var >= bound` is always false
    bound_val > 0 && max_val < bound_val
}

/// Resolve the maximum possible value of a variable through definition chains.
/// Returns `Some(max)` if the variable is provably bounded by `max`.
fn resolve_max_value(var: u32, defs: &DefMap) -> Option<i64> {
    resolve_max_value_inner(var, defs, 0)
}

fn resolve_max_value_inner(var: u32, defs: &DefMap, depth: usize) -> Option<i64> {
    if depth >= MAX_CHAIN_DEPTH {
        return None;
    }
    match defs.get(&var) {
        Some(Def::BitAndConst(mask)) if *mask >= 0 => Some(*mask),
        Some(Def::Copy(next)) => resolve_max_value_inner(*next, defs, depth + 1),
        Some(Def::IntConst(val)) => Some(*val),
        _ => None,
    }
}

/// Resolve a variable to a constant value through definition chains.
fn resolve_constant(var: u32, defs: &DefMap) -> Option<i64> {
    resolve_constant_inner(var, defs, 0)
}

fn resolve_constant_inner(var: u32, defs: &DefMap, depth: usize) -> Option<i64> {
    if depth >= MAX_CHAIN_DEPTH {
        return None;
    }
    match defs.get(&var) {
        Some(Def::IntConst(val)) => Some(*val),
        Some(Def::Copy(next)) => resolve_constant_inner(*next, defs, depth + 1),
        Some(Def::FieldAccess { local, field_index }) => {
            resolve_constant_through_struct(*local, *field_index, defs, depth)
        }
        _ => None,
    }
}

fn resolve_constant_through_struct(
    local: u32,
    field_index: u32,
    defs: &DefMap,
    depth: usize,
) -> Option<i64> {
    let struct_def = match defs.get(&local) {
        Some(Def::StructLit(fields)) => Some(fields),
        Some(Def::Copy(next)) => {
            if let Some(Def::StructLit(fields)) = defs.get(next) {
                Some(fields)
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(fields) = struct_def
        && let Some(source) = fields.get(&field_index)
    {
        match source {
            FieldSource::Const(val) => Some(*val),
            FieldSource::Local(local) => resolve_constant_inner(*local, defs, depth + 1),
        }
    } else {
        None
    }
}

/// Check if a condition is implied false by the loop guard.
///
/// For a `<` guard (`var < bound`):
///   `check_var >= check_bound` is false when both resolve to the same locals.
///
/// For a `<=` guard (`var <= limit`):
///   `check_var >= check_bound` is false when `check_var` resolves to var
///   AND `check_bound` resolves to `limit + 1`.
fn is_implied_false(body: &Body, condition: ExprId, guard: &LoopGuard, defs: &DefMap) -> bool {
    let ExprKind::Binary { left, op, right } = &body.exprs[condition].kind else {
        return false;
    };

    // We're looking for `check_var >= check_bound`
    if *op != NirBinaryOp::GtEq {
        return false;
    }

    let ExprKind::Local {
        index: check_var, ..
    } = &body.exprs[*left].kind
    else {
        return false;
    };
    let check_var = *check_var;
    let Some(check_bound) = Bound::extract(body, *right) else {
        return false;
    };

    // check_var must resolve to the guard's induction variable
    if !resolves_to(check_var, guard.var, defs) {
        return false;
    }

    // For `<` guard (`var < B`): check is false iff `check_bound >= B`.
    // For `<=` guard (`var <= B`): check is false iff `check_bound > B`.
    // [`check_bound_implied_false`] applies exact-match for two-Local
    // bounds and >=/> for literal-mixed bounds.
    check_bound_implied_false(&check_bound, &guard.bound, guard.is_strict, defs)
}

/// Check if `check_var >= check_bound` is implied false by a dominating guard.
///
/// A dominating guard `(var + max_offset) < bound` proves that
/// `var + k < bound` for all `k <= max_offset` (assuming non-negative offsets).
/// So `(var + k) >= bound` is false for `k <= max_offset`.
fn is_implied_by_dominating_guard(
    body: &Body,
    condition: ExprId,
    dg: &DominatingGuard,
    defs: &DefMap,
) -> bool {
    let ExprKind::Binary { left, op, right } = &body.exprs[condition].kind else {
        return false;
    };
    if *op != NirBinaryOp::GtEq {
        return false;
    }
    let ExprKind::Local {
        index: check_var, ..
    } = &body.exprs[*left].kind
    else {
        return false;
    };
    let check_var = *check_var;
    let Some(check_bound) = Bound::extract(body, *right) else {
        return false;
    };

    // check_var must resolve to `dg.var + offset` where
    // `0 <= offset <= dg.max_offset`.
    let Some(offset) = resolve_offset_from(check_var, dg.var, defs) else {
        return false;
    };
    if offset < 0 || offset > dg.max_offset {
        return false;
    }

    // `var + max_offset < dg.bound` gives `check_var = var + offset
    // < dg.bound - (max_offset - offset)`, so the check is implied
    // false when `check_bound >= dg.bound - (max_offset - offset)`.
    // Local-only bounds fall back to identity match — the legacy
    // regime, sound only at `offset == max_offset`.
    let tighten = dg.max_offset - offset;
    match (check_bound.to_constant(defs), dg.bound.to_constant(defs)) {
        (Some(check_v), Some(guard_v)) => check_v >= guard_v - tighten,
        _ => check_bound_implied_false(&check_bound, &dg.bound, true, defs),
    }
}

/// Extract a dominating guard from an if-condition.
///
/// Matches: `(var + offset) < bound` → `DominatingGuard` { var, `max_offset`: offset, bound }
///      or: `var < bound` → `DominatingGuard` { var, `max_offset`: 0, bound }
fn extract_dominating_guard(
    body: &Body,
    condition: ExprId,
    defs: &DefMap,
) -> Option<DominatingGuard> {
    let ExprKind::Binary { left, op, right } = &body.exprs[condition].kind else {
        return None;
    };
    if *op != NirBinaryOp::Lt {
        return None;
    }
    let left = *left;
    let right = *right;
    let bound = Bound::extract(body, right)?;

    // Left side: either `var` or `var + offset`
    match &body.exprs[left].kind {
        ExprKind::Local { index: var, .. } => Some(DominatingGuard {
            var: *var,
            max_offset: 0,
            bound,
        }),
        ExprKind::Binary {
            left: inner_left,
            op: NirBinaryOp::Add,
            right: inner_right,
        } => {
            let ExprKind::Local { index: var, .. } = &body.exprs[*inner_left].kind else {
                return None;
            };
            let var = *var;
            // Offset can be a literal or a local resolving to a constant
            let offset = match &body.exprs[*inner_right].kind {
                ExprKind::IntLiteral { value, .. } => *value as i64,
                ExprKind::Local { index, .. } => resolve_constant(*index, defs)?,
                _ => return None,
            };
            if offset < 0 {
                return None;
            }
            Some(DominatingGuard {
                var,
                max_offset: offset,
                bound,
            })
        }
        _ => None,
    }
}

/// Resolve the offset of `source` from `base`: if `source` resolves to `base + k`, return `Some(k)`.
/// If `source` resolves directly to `base`, return `Some(0)`.
fn resolve_offset_from(source: u32, base: u32, defs: &DefMap) -> Option<i64> {
    resolve_offset_from_inner(source, base, defs, 0)
}

fn resolve_offset_from_inner(source: u32, base: u32, defs: &DefMap, depth: usize) -> Option<i64> {
    if source == base {
        return Some(0);
    }
    if depth >= MAX_CHAIN_DEPTH {
        return None;
    }
    match defs.get(&source) {
        Some(Def::Copy(next)) => resolve_offset_from_inner(*next, base, defs, depth + 1),
        Some(Def::AddConst(var, offset)) => {
            let base_offset = resolve_offset_from_inner(*var, base, defs, depth + 1)?;
            Some(base_offset + offset)
        }
        _ => None,
    }
}

const MAX_CHAIN_DEPTH: usize = 10;

/// Check if `source` resolves to `target` by following copy chains.
fn resolves_to(source: u32, target: u32, defs: &DefMap) -> bool {
    resolves_to_inner(source, target, defs, 0)
}

fn resolves_to_inner(source: u32, target: u32, defs: &DefMap, depth: usize) -> bool {
    if source == target {
        return true;
    }
    if depth >= MAX_CHAIN_DEPTH {
        return false;
    }
    match defs.get(&source) {
        Some(Def::Copy(next)) => resolves_to_inner(*next, target, defs, depth + 1),
        Some(Def::FieldAccess {
            local: src_local,
            field_index: src_field,
        }) => {
            // Two locals derived from the same field access on the same base are equal.
            // This handles the pattern where LICM hoists `obj.field` to a new local while
            // the original `let x = obj.field` already exists — both read the same value.
            resolves_field_access_to(target, *src_local, *src_field, defs, depth)
        }
        _ => false,
    }
}

/// Check if `target` also resolves to `FieldAccess(base_local, field_index)`.
fn resolves_field_access_to(
    target: u32,
    base_local: u32,
    field_index: u32,
    defs: &DefMap,
    depth: usize,
) -> bool {
    if depth >= MAX_CHAIN_DEPTH {
        return false;
    }
    match defs.get(&target) {
        Some(Def::Copy(next)) => {
            resolves_field_access_to(*next, base_local, field_index, defs, depth + 1)
        }
        Some(Def::FieldAccess {
            local: tgt_local,
            field_index: tgt_field,
        }) => {
            *tgt_field == field_index && resolves_to_inner(*tgt_local, base_local, defs, depth + 1)
        }
        _ => false,
    }
}

/// Check if `source` resolves to `target + 1` by following definition chains.
///
/// Handles chains like:
///   `_licm_used_9` → FieldAccess(arr, .used) → StructLit(arr).used → `n` → AddConst(limit, 1)
fn resolves_to_plus_one(source: u32, target: u32, defs: &DefMap) -> bool {
    resolves_to_plus_one_inner(source, target, defs, 0)
}

fn resolves_to_plus_one_inner(source: u32, target: u32, defs: &DefMap, depth: usize) -> bool {
    if depth >= MAX_CHAIN_DEPTH {
        return false;
    }
    match defs.get(&source) {
        Some(Def::Copy(next)) => resolves_to_plus_one_inner(*next, target, defs, depth + 1),
        Some(Def::AddConst(base, 1)) => resolves_to_inner(*base, target, defs, depth + 1),
        Some(Def::FieldAccess { local, field_index }) => {
            // Follow: `_licm_used = arr.used` → look up arr's struct literal
            let field_source = resolve_field_source(*local, *field_index, defs);
            match field_source {
                Some(FieldSource::Local(field_local)) => {
                    resolves_to_plus_one_inner(field_local, target, defs, depth + 1)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn resolve_field_source(local: u32, field_index: u32, defs: &DefMap) -> Option<FieldSource> {
    let struct_def = match defs.get(&local) {
        Some(Def::StructLit(fields)) => Some(fields),
        Some(Def::Copy(next)) => {
            if let Some(Def::StructLit(fields)) = defs.get(next) {
                Some(fields)
            } else {
                None
            }
        }
        _ => None,
    };
    struct_def
        .and_then(|fields| fields.get(&field_index))
        .cloned()
}

/// Check if a block consists of a panic call (bounds check failure path).
fn is_panic_block(body: &Body, block: BlockId) -> bool {
    body.blocks[block]
        .stmts
        .iter()
        .any(|s| match &body.stmts[*s].kind {
            StmtKind::Expr(expr) => is_panic_call(body, *expr),
            _ => false,
        })
}

fn is_panic_call(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::Call { func, .. } => func.name.contains("panic"),
        _ => false,
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
