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
//! The pass reads and mutates the arena [`Body`] directly. The eliminators
//! drive a small [`ArenaOptVisitor`] whose default `visit_*` recurse into every
//! child; condition replacement is an in-place `kind` rewrite.

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::nir::{NirBinaryOp, NirFunction, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, PatId, StmtId, StmtKind};
use crate::nir_package::NirPackage;

struct LoopGuard {
    /// Local index of the induction variable (e.g., `i`)
    var: u32,
    /// Bound: a Local, an `IntLiteral`, or a `local.f1.f2…` chain.
    bound: Bound,
    /// `true` for `<` (strict), `false` for `<=` (inclusive)
    is_strict: bool,
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

pub fn eliminate_implied_conditions(project: &mut NirPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= process_function(&mut func);
    }
    changed
}

fn process_function(func: &mut NirFunction) -> bool {
    let Some(body) = func.body.as_mut() else {
        return false;
    };
    let tainted = collect_tainted_locals(body);
    let mut defs = DefMap::default();
    let root = body.root;
    process_block(body, root, &mut defs, &tainted)
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

fn process_block(body: &mut Body, block: BlockId, defs: &mut DefMap, tainted: &Taints) -> bool {
    let mut changed = false;
    let mut guards: Vec<ShortCircuitGuard> = Vec::new();
    let stmts = body.blocks[block].stmts.clone();
    for s in stmts {
        record_def_from_stmt(body, s, defs, tainted);
        record_defs_from_nested(body, s, defs, tainted);
        // Apply accumulated guards from previous early-exit stmts to this stmt
        for guard in &guards {
            changed |= guard.eliminate_in_stmt(body, s, defs);
        }
        changed |= BitmaskEliminator { defs }.visit_stmt(body, s);
        changed |= ShortCircuitEliminator { defs }.visit_stmt(body, s);
        changed |= process_stmt(body, s, defs, tainted);
        // If this is `if (var >= bound) { return/break }`, extract a guard
        if let Some(guard) = extract_early_exit_guard(body, s, defs) {
            guards.push(guard);
        }
    }
    changed
}

fn process_stmt(body: &mut Body, s: StmtId, defs: &mut DefMap, tainted: &Taints) -> bool {
    // Record definitions from let bindings
    record_def_from_stmt(body, s, defs, tainted);

    let shape = match &body.stmts[s].kind {
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
        StmtShape::Loop(lb) => process_loop(body, lb, defs, tainted),
        StmtShape::If(then_b, else_b) => {
            let mut changed = process_block(body, then_b, defs, tainted);
            if let Some(eb) = else_b {
                changed |= process_block(body, eb, defs, tainted);
            }
            changed
        }
        StmtShape::Labeled(b) => process_block(body, b, defs, tainted),
        StmtShape::None => false,
    }
}

enum StmtShape {
    Loop(BlockId),
    If(BlockId, Option<BlockId>),
    Labeled(BlockId),
    None,
}

fn process_loop(body: &mut Body, loop_body: BlockId, defs: &mut DefMap, tainted: &Taints) -> bool {
    // First, record defs inside the loop body (for copies like `let index = i`)
    // and recurse into nested structures
    let mut changed = false;

    // Collect defs from the loop body before eliminating
    let mut loop_defs = defs.clone();
    let stmts = body.blocks[loop_body].stmts.clone();
    for s in &stmts {
        record_def_from_stmt(body, *s, &mut loop_defs, tainted);
        record_defs_from_nested(body, *s, &mut loop_defs, tainted);
    }

    // Extract the loop guard from the first statement
    let guard = extract_loop_guard(body, loop_body);

    if let Some(guard) = &guard {
        // Eliminate implied conditions in the loop body (skip the guard itself)
        let mut condition_elim = ConditionEliminator {
            guard,
            dom_guards: vec![],
            defs: &loop_defs,
        };
        for s in stmts.iter().skip(1) {
            changed |= condition_elim.visit_stmt(body, *s);
        }
    }

    // Eliminate bitmask-bounded checks in the loop body
    for s in &stmts {
        changed |= BitmaskEliminator { defs: &loop_defs }.visit_stmt(body, *s);
    }

    // Recurse into nested loops
    for s in &stmts {
        changed |= process_stmt_nested_loops(body, *s, defs, tainted);
    }

    changed
}

/// Recurse into nested structures to find inner loops, but don't re-process
/// the current loop level.
fn process_stmt_nested_loops(
    body: &mut Body,
    s: StmtId,
    defs: &mut DefMap,
    tainted: &Taints,
) -> bool {
    let shape = match &body.stmts[s].kind {
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
        StmtShape::Loop(lb) => process_loop(body, lb, defs, tainted),
        StmtShape::If(then_b, else_b) => {
            let mut changed = false;
            for s in body.blocks[then_b].stmts.clone() {
                changed |= process_stmt_nested_loops(body, s, defs, tainted);
            }
            if let Some(eb) = else_b {
                for s in body.blocks[eb].stmts.clone() {
                    changed |= process_stmt_nested_loops(body, s, defs, tainted);
                }
            }
            changed
        }
        StmtShape::Labeled(b) => {
            let mut changed = false;
            for s in body.blocks[b].stmts.clone() {
                changed |= process_stmt_nested_loops(body, s, defs, tainted);
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
fn extract_loop_guard(body: &Body, loop_body: BlockId) -> Option<LoopGuard> {
    let first = *body.blocks[loop_body].stmts.first()?;
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &body.stmts[first].kind
    else {
        return None;
    };
    let condition = *condition;
    let then_block = *then_block;

    // then_block must be a single Break statement
    if body.blocks[then_block].stmts.len() != 1 {
        return None;
    }
    matches!(
        &body.stmts[body.blocks[then_block].stmts[0]].kind,
        StmtKind::Break { .. }
    )
    .then_some(())?;

    // condition must be `Not(Binary(var, Lt|LtEq, bound))`
    let ExprKind::Unary {
        op: NirUnaryOp::Not,
        expr: inner,
    } = &body.exprs[condition].kind
    else {
        return None;
    };

    let ExprKind::Binary { left, op, right } = &body.exprs[*inner].kind else {
        return None;
    };

    let (is_strict, var_expr, bound_expr) = match op {
        NirBinaryOp::Lt => (true, *left, *right),
        NirBinaryOp::LtEq => (false, *left, *right),
        _ => return None,
    };

    let ExprKind::Local { index: var, .. } = &body.exprs[var_expr].kind else {
        return None;
    };
    let var = *var;
    let bound = Bound::extract(body, bound_expr)?;

    Some(LoopGuard {
        var,
        bound,
        is_strict,
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
                // When `resolve_constant` walks the chain it goes
                // through `arr`'s `Def::StructLit`; if `arr.used` is
                // field-tainted, the StructLit recorder will have
                // already dropped that field, so the FieldAccess
                // walk naturally returns `None`. Recording the
                // FieldAccess def itself is always sound — the
                // soundness check happens at the StructLit layer.
                defs.insert(
                    local_index,
                    Def::FieldAccess {
                        local: *index,
                        field_index: *field_index,
                    },
                );
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
fn set_false(body: &mut Body, cond: ExprId) {
    body.exprs[cond].kind = ExprKind::BoolLiteral(false);
}

/// NIR visitor that eliminates loop-guard-implied false bounds checks.
///
/// When a loop guard proves `i < bound`, inner conditions `i >= bound` are
/// replaced with `false`. Dominating if-conditions are also tracked to extend
/// the elimination into their then-blocks.
struct ConditionEliminator<'a> {
    guard: &'a LoopGuard,
    dom_guards: Vec<DominatingGuard>,
    defs: &'a DefMap,
}

impl ArenaOptVisitor for ConditionEliminator<'_> {
    fn visit_stmt(&mut self, body: &mut Body, s: StmtId) -> bool {
        let if_ids = match &body.stmts[s].kind {
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
                && is_panic_block(body, then_block)
                && is_implied_false_by_any(body, condition, self.guard, &self.dom_guards, self.defs)
            {
                set_false(body, condition);
                return true;
            }

            // Extract a dominating guard from the condition to extend
            // elimination into the then-block.
            let mut changed = self.visit_expr(body, condition);
            let dom = extract_dominating_guard(body, condition, self.defs);
            let saved = self.dom_guards.clone();
            if let Some(dg) = dom {
                self.dom_guards.push(dg);
            }
            changed |= self.visit_block(body, then_block);
            self.dom_guards = saved;
            if let Some(eb) = else_block {
                changed |= self.visit_block(body, eb);
            }
            return changed;
        }

        arena_opt_walk(self, body, NodeRef::Stmt(s))
    }

    fn visit_expr(&mut self, body: &mut Body, e: ExprId) -> bool {
        // For If exprs: extract a dominating guard and propagate into then-branch.
        let if_ids = match &body.exprs[e].kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => Some((*condition, *then_branch, *else_branch)),
            _ => None,
        };
        if let Some((condition, then_branch, else_branch)) = if_ids {
            let mut changed = self.visit_expr(body, condition);
            let dom = extract_dominating_guard(body, condition, self.defs);
            let saved = self.dom_guards.clone();
            if let Some(dg) = dom {
                self.dom_guards.push(dg);
            }
            changed |= self.visit_block(body, then_branch);
            self.dom_guards = saved;
            if let Some(eb) = else_branch {
                changed |= self.visit_block(body, eb);
            }
            return changed;
        }
        arena_opt_walk(self, body, NodeRef::Expr(e))
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
    fn visit_stmt(&mut self, body: &mut Body, s: StmtId) -> bool {
        let if_ids = match &body.stmts[s].kind {
            StmtKind::If {
                condition,
                then_block,
                else_block: None,
            } => Some((*condition, *then_block)),
            _ => None,
        };
        if let Some((condition, then_block)) = if_ids
            && is_panic_block(body, then_block)
            && is_bitmask_bounded(body, condition, self.defs)
        {
            set_false(body, condition);
            return true;
        }
        arena_opt_walk(self, body, NodeRef::Stmt(s))
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
}

impl ArenaOptVisitor for ShortCircuitEliminator<'_> {
    fn visit_expr(&mut self, body: &mut Body, e: ExprId) -> bool {
        let or_ids = match &body.exprs[e].kind {
            ExprKind::Binary {
                left,
                op: NirBinaryOp::Or,
                right,
            } => Some((*left, *right)),
            _ => None,
        };
        if let Some((left, right)) = or_ids {
            let mut changed = self.visit_expr(body, left);
            if let Some(guard) = ShortCircuitGuard::extract(body, left, self.defs) {
                changed |= guard.eliminate_in_expr(body, right, self.defs);
            }
            changed |= self.visit_expr(body, right);
            return changed;
        }
        arena_opt_walk(self, body, NodeRef::Expr(e))
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

    fn eliminate_in_expr(&self, body: &mut Body, e: ExprId, defs: &DefMap) -> bool {
        let walk = match &body.exprs[e].kind {
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
            ScWalk::Block(b) => self.eliminate_in_block(body, b, defs),
            ScWalk::Exprs2(left, right) => {
                let mut changed = self.eliminate_in_expr(body, left, defs);
                changed |= self.eliminate_in_expr(body, right, defs);
                changed
            }
            ScWalk::Expr1(inner) => self.eliminate_in_expr(body, inner, defs),
            ScWalk::If(condition, then_branch, else_branch) => {
                let mut changed = self.eliminate_in_expr(body, condition, defs);
                changed |= self.eliminate_in_block(body, then_branch, defs);
                if let Some(eb) = else_branch {
                    changed |= self.eliminate_in_block(body, eb, defs);
                }
                changed
            }
            ScWalk::None => false,
        }
    }

    fn eliminate_in_block(&self, body: &mut Body, block: BlockId, defs: &DefMap) -> bool {
        let mut changed = false;
        for s in body.blocks[block].stmts.clone() {
            changed |= self.eliminate_in_stmt(body, s, defs);
        }
        changed
    }

    fn eliminate_in_stmt(&self, body: &mut Body, s: StmtId, defs: &DefMap) -> bool {
        // Check if this is a bounds-check `if (index >= bound) { panic() }` implied false
        let if_ids = match &body.stmts[s].kind {
            StmtKind::If {
                condition,
                then_block,
                else_block: None,
            } => Some((*condition, *then_block)),
            _ => None,
        };
        if let Some((condition, then_block)) = if_ids
            && is_panic_block(body, then_block)
            && self.implies_false(body, condition, defs)
        {
            set_false(body, condition);
            return true;
        }

        // Recurse into sub-expressions and sub-statements
        let walk = match &body.stmts[s].kind {
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
            ScStmt::Expr(e) => self.eliminate_in_expr(body, e, defs),
            ScStmt::If(condition, then_block, else_block) => {
                let mut changed = self.eliminate_in_expr(body, condition, defs);
                changed |= self.eliminate_in_block(body, then_block, defs);
                if let Some(eb) = else_block {
                    changed |= self.eliminate_in_block(body, eb, defs);
                }
                changed
            }
            ScStmt::Block(b) => self.eliminate_in_block(body, b, defs),
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

/// Check if a condition is implied false by the loop guard OR any dominating guard.
fn is_implied_false_by_any(
    body: &Body,
    condition: ExprId,
    guard: &LoopGuard,
    dom_guards: &[DominatingGuard],
    defs: &DefMap,
) -> bool {
    if is_implied_false(body, condition, guard, defs) {
        return true;
    }
    for dg in dom_guards {
        if is_implied_by_dominating_guard(body, condition, dg, defs) {
            return true;
        }
    }
    false
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
    fn visit_stmt(&mut self, body: &mut Body, s: StmtId) -> bool
    where
        Self: Sized,
    {
        arena_opt_walk(self, body, NodeRef::Stmt(s))
    }
    fn visit_expr(&mut self, body: &mut Body, e: ExprId) -> bool
    where
        Self: Sized,
    {
        arena_opt_walk(self, body, NodeRef::Expr(e))
    }
    fn visit_block(&mut self, body: &mut Body, b: BlockId) -> bool
    where
        Self: Sized,
    {
        arena_opt_walk(self, body, NodeRef::Block(b))
    }
    fn visit_pattern(&mut self, body: &mut Body, p: PatId) -> bool
    where
        Self: Sized,
    {
        arena_opt_walk(self, body, NodeRef::Pat(p))
    }
}

/// Recurse into every id-bearing child of `node`, dispatching by category, and
/// OR the per-child change flags. The eliminators here only rewrite condition
/// kinds in place (never add/remove nodes), so the upfront child snapshot stays
/// valid through the walk.
fn arena_opt_walk<V: ArenaOptVisitor>(v: &mut V, body: &mut Body, node: NodeRef) -> bool {
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    let mut changed = false;
    for c in kids {
        changed |= match c {
            NodeRef::Stmt(s) => v.visit_stmt(body, s),
            NodeRef::Expr(e) => v.visit_expr(body, e),
            NodeRef::Block(b) => v.visit_block(body, b),
            NodeRef::Pat(p) => v.visit_pattern(body, p),
        };
    }
    changed
}
