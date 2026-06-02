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

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::nir::{
    NirBinaryOp, NirBlock, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind, NirUnaryOp,
};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirOptVisitor, NirRefVisitor, opt_walk_expr, opt_walk_stmt};

struct LoopGuard {
    /// Local index of the induction variable (e.g., `i`)
    var: u32,
    /// Bound (e.g., `_licm_used_25` or a literal like `143`).
    bound: BoundValue,
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
    /// Bound (Local index or literal)
    bound: BoundValue,
}

/// Right-hand side of a loop guard, dominating guard, or bounds-check
/// condition. Accepting both `Local` and `Literal` forms is what lets
/// the eliminator handle loops written as `for n in 0..=143 { … }`
/// (literal bound, no `.used` field access) and bounds checks where
/// `field_env` already folded the `local.field` access to a constant.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundValue {
    Local(u32),
    Literal(i64),
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
    let Some(ref mut body) = func.body else {
        return false;
    };
    let tainted = collect_tainted_locals(body);
    let mut defs = DefMap::default();
    process_block(body, &mut defs, &tainted)
}

/// Per-function summary of locals and `(local, field_index)` pairs
/// whose values may change anywhere in the function body. The
/// [`DefMap`] recording layer consults this set to skip captures
/// that would otherwise go stale — the Stage 1.5 / Stage 2
/// literal-mixed bound comparison numerically reads these defs.
#[derive(Default)]
struct Taints {
    /// Locals whose whole value may change: direct `local = expr`
    /// reassignments, `&mut local` escapes, `is_mut` call args, and
    /// method-call receivers (auto-`&mut self` is opaque here).
    locals: IndexSet<u32>,
    /// `(local, field_index)` pairs assigned by `local.field = expr`
    /// (with `field_index` known from the elaborator's resolved
    /// `FieldAccess`). Lets `record_struct_lit_def` drop just the
    /// stale field entry while keeping the rest of the struct
    /// literal's captures.
    fields: IndexSet<(u32, u32)>,
}

/// Walk the function body and collect every local whose value (or
/// recorded field) could change anywhere downstream. Sources of taint
/// are described on [`Taints`].
fn collect_tainted_locals(body: &NirBlock) -> Taints {
    let mut collector = TaintCollector {
        taints: Taints::default(),
    };
    collector.visit_block(body);
    collector.taints
}

struct TaintCollector {
    taints: Taints,
}

impl TaintCollector {
    /// Resolve an `Assign`'s target shape into the (local, optional
    /// `field_index`) it ultimately mutates. Returns the entry to
    /// insert into `taints`.
    fn taint_target(&mut self, target: &NirExpr) {
        match &target.kind {
            // `local = expr`.
            NirExprKind::Local { index, .. } => {
                self.taints.locals.insert(*index);
            }
            // `local.field = expr`: only that field is stale.
            NirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } if matches!(inner.kind, NirExprKind::Local { .. }) => {
                if let NirExprKind::Local { index, .. } = &inner.kind {
                    self.taints.fields.insert((*index, *field_index));
                }
            }
            // Nested receiver: `local.f.g = …` taints `local`
            // whole — the inner field's contents are now in an
            // unknown state from our (one-level) recorder's view.
            NirExprKind::FieldAccess { expr: inner, .. }
            | NirExprKind::Unary { expr: inner, .. } => {
                self.taint_root_local(inner);
            }
            // `local[i] = expr` mutates an array element, not any
            // recorded field. DefMap never captures element values,
            // so no taint is required.
            NirExprKind::Index { .. } => {}
            _ => {}
        }
    }

    /// Walk a sub-expression to find its root Local and taint it
    /// whole. Used when an assignment target nests deeper than one
    /// field-access level and we can't pinpoint a single field.
    fn taint_root_local(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Local { index, .. } => {
                self.taints.locals.insert(*index);
            }
            NirExprKind::FieldAccess { expr: inner, .. }
            | NirExprKind::Unary { expr: inner, .. }
            | NirExprKind::Index { expr: inner, .. }
            | NirExprKind::Cast { expr: inner, .. } => {
                self.taint_root_local(inner);
            }
            _ => {}
        }
    }
}

impl NirRefVisitor for TaintCollector {
    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::Assign { target, .. } => {
                self.taint_target(target);
            }
            NirExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                // `&mut local.field` aliases the field; `&mut local`
                // aliases the whole local. Be precise where we can.
                match &inner.kind {
                    NirExprKind::Local { index, .. } => {
                        self.taints.locals.insert(*index);
                    }
                    NirExprKind::FieldAccess {
                        expr: receiver,
                        field_index,
                        ..
                    } if matches!(receiver.kind, NirExprKind::Local { .. }) => {
                        if let NirExprKind::Local { index, .. } = &receiver.kind {
                            self.taints.fields.insert((*index, *field_index));
                        }
                    }
                    _ => {
                        self.taint_root_local(inner);
                    }
                }
            }
            NirExprKind::Call { args, .. } => {
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
                    {
                        self.taints.locals.insert(*index);
                    }
                }
            }
            NirExprKind::MethodCall { receiver, args, .. } => {
                // Auto-ref makes the receiver implicitly mutable when
                // the callee is a `&mut self` method, but we can't
                // tell statically — taint the whole receiver.
                if let NirExprKind::Local { index, .. } = &receiver.kind {
                    self.taints.locals.insert(*index);
                }
                for arg in args {
                    if arg.is_mut
                        && let NirExprKind::Local { index, .. } = &arg.expr.kind
                    {
                        self.taints.locals.insert(*index);
                    }
                }
            }
            _ => {}
        }
        self.walk_expr(expr);
    }
}

fn process_block(block: &mut NirBlock, defs: &mut DefMap, tainted: &Taints) -> bool {
    let mut changed = false;
    let mut guards: Vec<ShortCircuitGuard> = Vec::new();
    for i in 0..block.stmts.len() {
        record_def_from_stmt(&block.stmts[i], defs, tainted);
        record_defs_from_nested(&block.stmts[i], defs, tainted);
        // Apply accumulated guards from previous early-exit stmts to this stmt
        for guard in &guards {
            changed |= guard.eliminate_in_stmt(&mut block.stmts[i], defs);
        }
        changed |= BitmaskEliminator { defs }.visit_stmt(&mut block.stmts[i]);
        changed |= ShortCircuitEliminator { defs }.visit_stmt(&mut block.stmts[i]);
        changed |= process_stmt(&mut block.stmts[i], defs, tainted);
        // If this is `if (var >= bound) { return/break }`, extract a guard
        if let Some(guard) = extract_early_exit_guard(&block.stmts[i], defs) {
            guards.push(guard);
        }
    }
    changed
}

fn process_stmt(stmt: &mut NirStmt, defs: &mut DefMap, tainted: &Taints) -> bool {
    // Record definitions from let bindings
    record_def_from_stmt(stmt, defs, tainted);

    match &mut stmt.kind {
        NirStmtKind::Loop { body } => process_loop(body, defs, tainted),
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let mut changed = process_block(then_block, defs, tainted);
            if let Some(else_block) = else_block {
                changed |= process_block(else_block, defs, tainted);
            }
            changed
        }
        NirStmtKind::LabeledBlock { block, .. } => process_block(block, defs, tainted),
        _ => false,
    }
}

fn process_loop(body: &mut NirBlock, defs: &mut DefMap, tainted: &Taints) -> bool {
    // First, record defs inside the loop body (for copies like `let index = i`)
    // and recurse into nested structures
    let mut changed = false;

    // Collect defs from the loop body before eliminating
    let mut loop_defs = defs.clone();
    for stmt in &body.stmts {
        record_def_from_stmt(stmt, &mut loop_defs, tainted);
        record_defs_from_nested(stmt, &mut loop_defs, tainted);
    }

    // Extract the loop guard from the first statement
    let guard = extract_loop_guard(&body.stmts);

    if let Some(guard) = &guard {
        // Eliminate implied conditions in the loop body (skip the guard itself)
        let mut condition_elim = ConditionEliminator {
            guard,
            dom_guards: vec![],
            defs: &loop_defs,
        };
        for stmt in body.stmts.iter_mut().skip(1) {
            changed |= condition_elim.visit_stmt(stmt);
        }
    }

    // Eliminate bitmask-bounded checks in the loop body
    for stmt in &mut body.stmts {
        changed |= BitmaskEliminator { defs: &loop_defs }.visit_stmt(stmt);
    }

    // Recurse into nested loops
    for stmt in &mut body.stmts {
        changed |= process_stmt_nested_loops(stmt, defs, tainted);
    }

    changed
}

/// Recurse into nested structures to find inner loops, but don't re-process
/// the current loop level.
fn process_stmt_nested_loops(stmt: &mut NirStmt, defs: &mut DefMap, tainted: &Taints) -> bool {
    match &mut stmt.kind {
        NirStmtKind::Loop { body } => process_loop(body, defs, tainted),
        NirStmtKind::If {
            then_block,
            else_block,
            ..
        } => {
            let mut changed = false;
            for s in &mut then_block.stmts {
                changed |= process_stmt_nested_loops(s, defs, tainted);
            }
            if let Some(else_block) = else_block {
                for s in &mut else_block.stmts {
                    changed |= process_stmt_nested_loops(s, defs, tainted);
                }
            }
            changed
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            let mut changed = false;
            for s in &mut block.stmts {
                changed |= process_stmt_nested_loops(s, defs, tainted);
            }
            changed
        }
        _ => false,
    }
}

/// Extract a loop guard from the first statement of a loop body.
///
/// Matches: `if !(var < bound) { break LABEL; }` → guard `var < bound`
///      or: `if !(var <= bound) { break LABEL; }` → guard `var <= bound`
fn extract_loop_guard(stmts: &[NirStmt]) -> Option<LoopGuard> {
    let first = stmts.first()?;
    let NirStmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &first.kind
    else {
        return None;
    };

    // then_block must be a single Break statement
    if then_block.stmts.len() != 1 {
        return None;
    }
    matches!(&then_block.stmts[0].kind, NirStmtKind::Break { .. }).then_some(())?;

    // condition must be `Not(Binary(var, Lt|LtEq, bound))`
    let NirExprKind::Unary {
        op: NirUnaryOp::Not,
        expr: inner,
    } = &condition.kind
    else {
        return None;
    };

    let NirExprKind::Binary { left, op, right } = &inner.kind else {
        return None;
    };

    let (is_strict, var_expr, bound_expr) = match op {
        NirBinaryOp::Lt => (true, left, right),
        NirBinaryOp::LtEq => (false, left, right),
        _ => return None,
    };

    let NirExprKind::Local { index: var, .. } = &var_expr.kind else {
        return None;
    };
    let bound = extract_bound_value(bound_expr)?;

    Some(LoopGuard {
        var: *var,
        bound,
        is_strict,
    })
}

/// Decode an expression as a [`BoundValue`]: an immediate `Local`,
/// `IntLiteral`, or a `Cast` around either of those. Anything else
/// (arithmetic, method calls, …) yields `None` so the caller bails.
///
/// `IntLiteral.value` is `u64` (bit pattern). Reinterpreting as `i64`
/// via `as i64` flips the sign of literals in `[2^63, 2^64)`, which
/// would silently feed a negative bound into the numeric comparisons
/// in [`check_bound_implied_false`]. Use `i64::try_from` so out-of-
/// range u64 literals bail rather than fold to negative.
///
/// `Cast { expr: <literal-or-local>, .. }` arises naturally when
/// niri folds a typed-numeric `(N as u32) >= bound`-shaped check.
/// The bit pattern is unambiguous for sign-extending / zero-extending
/// casts; we recurse and let the inner extraction succeed.
fn extract_bound_value(expr: &NirExpr) -> Option<BoundValue> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some(BoundValue::Local(*index)),
        NirExprKind::IntLiteral { value, .. } => {
            i64::try_from(*value).ok().map(BoundValue::Literal)
        }
        NirExprKind::Cast { expr: inner, .. } => extract_bound_value(inner),
        _ => None,
    }
}

/// Project a [`BoundValue`] to a concrete integer when one can be
/// proven through the `defs` chain. Used by comparisons that need to
/// equate two bounds with different surface shapes (e.g. a guard
/// bound recorded as `BoundValue::Local(_licm_used_25)` where
/// `_licm_used_25 = 288`, and a check bound that already folded to
/// `BoundValue::Literal(288)`).
fn bound_to_constant(bound: BoundValue, defs: &DefMap) -> Option<i64> {
    match bound {
        BoundValue::Literal(v) => Some(v),
        BoundValue::Local(idx) => resolve_constant(idx, defs),
    }
}

/// True when a comparison `var >= check` is implied false by a loop
/// (or dominating) guard `var (< | <=) guard`.
///
/// Two regimes, chosen by the operand shape:
///
/// - Both sides `Local`: legacy behavior, exact-match via the chain
///   walk. A strict (`<`) guard requires `check` resolves to the same
///   local as `guard`; a non-strict (`<=`) requires `check` resolves
///   to `guard + 1`. Sound without literal information.
/// - At least one side `Literal` (or resolvable to one through
///   `defs`): compare integer values. `var < guard` proves
///   `var < check` whenever `check >= guard`; `var <= guard` proves
///   it whenever `check > guard`, i.e. `check >= guard + 1`.
///
/// The mixed regime is what catches `for n in 0..=143 { arr[n] = … }`
/// where `arr.used` already folded to the literal `288` and the
/// resulting `n >= 288` bounds check needs to be eliminated by the
/// loop guard `n <= 143`.
fn check_bound_implied_false(
    check: BoundValue,
    guard: BoundValue,
    is_strict_guard: bool,
    defs: &DefMap,
) -> bool {
    if let (BoundValue::Local(c), BoundValue::Local(g)) = (check, guard) {
        return if is_strict_guard {
            resolves_to(c, g, defs)
        } else {
            resolves_to_plus_one(c, g, defs)
        };
    }
    let (Some(c_val), Some(g_val)) = (
        bound_to_constant(check, defs),
        bound_to_constant(guard, defs),
    ) else {
        return false;
    };
    if is_strict_guard {
        c_val >= g_val
    } else {
        c_val > g_val
    }
}

fn record_def_from_stmt(stmt: &NirStmt, defs: &mut DefMap, tainted: &Taints) {
    let NirStmtKind::Let {
        local_index, value, ..
    } = &stmt.kind
    else {
        return;
    };

    // Skip bindings whose let-target itself may be reassigned
    // (Assign / &mut / mut-arg / method receiver). Without this
    // gate, the Stage 1.5 / Stage 2 literal-mixed bound comparison
    // would numerically consult a stale `Def::IntConst(N)` /
    // `Def::Copy(…)` / `Def::AddConst(…)` chain captured at the
    // let point and silently eliminate bounds checks the runtime
    // actually exercises. The identity-only paths (`resolves_to`
    // between two Locals) remain sound — they compare names only —
    // but value-extracting paths must not see tainted entries.
    //
    // Field-level taint (`local.f = …`) is filtered per-field in
    // `record_struct_lit_def` instead; the whole-local check here
    // only triggers when the target itself was reassigned.
    if tainted.locals.contains(local_index) {
        return;
    }

    // Unwrap LabeledBlock to find the actual defining expression
    // (e.g., `let arr = __inline_...: { ...; break LABEL: StructLiteral { ... }; }`)
    let effective = unwrap_labeled_block_value(value);

    match &effective.kind {
        NirExprKind::Local { index, .. } => {
            defs.insert(*local_index, Def::Copy(*index));
        }
        NirExprKind::Binary { left, op, right } => {
            if let (
                NirExprKind::Local { index: lhs, .. },
                NirBinaryOp::Add,
                NirExprKind::IntLiteral { value: val, .. },
            ) = (&left.kind, op, &right.kind)
            {
                defs.insert(*local_index, Def::AddConst(*lhs, *val as i64));
            } else if *op == NirBinaryOp::BitAnd {
                if let NirExprKind::IntLiteral { value: val, .. } = &right.kind {
                    defs.insert(*local_index, Def::BitAndConst(*val as i64));
                } else if let NirExprKind::IntLiteral { value: val, .. } = &left.kind {
                    defs.insert(*local_index, Def::BitAndConst(*val as i64));
                }
            }
        }
        NirExprKind::IntLiteral { value: val, .. } => {
            defs.insert(*local_index, Def::IntConst(*val as i64));
        }
        NirExprKind::FieldAccess {
            expr, field_index, ..
        } => {
            if let NirExprKind::Local { index, .. } = &expr.kind {
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
                    *local_index,
                    Def::FieldAccess {
                        local: *index,
                        field_index: *field_index,
                    },
                );
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            record_struct_lit_def(*local_index, fields, defs, tainted);
        }
        _ => {}
    }
}

fn record_struct_lit_def(
    local_index: u32,
    fields: &[crate::nir::NirStructField],
    defs: &mut DefMap,
    tainted: &Taints,
) {
    let mut field_map = IndexMap::default();
    for f in fields {
        // Skip fields that may be reassigned anywhere — `local.f = …`
        // taints `(local, f)` and we must not capture this StructLit's
        // initial value for that field, otherwise a later
        // bound-constant lookup would observe the stale snapshot.
        if tainted.fields.contains(&(local_index, f.field_index)) {
            continue;
        }
        if let NirExprKind::Local { index, .. } = &f.value.kind {
            field_map.insert(f.field_index, FieldSource::Local(*index));
        } else if let NirExprKind::IntLiteral { value, .. } = &f.value.kind {
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
fn unwrap_labeled_block_value(expr: &NirExpr) -> &NirExpr {
    if let NirExprKind::Block(block) = &expr.kind
        && let Some(NirStmt {
            kind: NirStmtKind::Expr(tail),
            ..
        }) = block.stmts.last()
    {
        return unwrap_labeled_block_value(tail);
    }
    if let NirExprKind::LabeledBlock { block, label, .. } = &expr.kind {
        // Find the break statement that returns a value from this block
        for stmt in &block.stmts {
            if let NirStmtKind::Break {
                label: Some(break_label),
                value: Some(val),
            } = &stmt.kind
                && break_label == label
            {
                // Recursively unwrap in case of nested labeled blocks
                return unwrap_labeled_block_value(val);
            }
        }
    }
    expr
}

/// Record defs from nested blocks within a statement (e.g., labeled blocks in expressions).
fn record_defs_from_nested(stmt: &NirStmt, defs: &mut DefMap, tainted: &Taints) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => {
            record_defs_from_expr(value, defs, tainted);
        }
        NirStmtKind::Expr(expr) => {
            record_defs_from_expr(expr, defs, tainted);
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            for s in &block.stmts {
                record_def_from_stmt(s, defs, tainted);
                record_defs_from_nested(s, defs, tainted);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            record_defs_from_expr(condition, defs, tainted);
            for s in &then_block.stmts {
                record_def_from_stmt(s, defs, tainted);
                record_defs_from_nested(s, defs, tainted);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    record_def_from_stmt(s, defs, tainted);
                    record_defs_from_nested(s, defs, tainted);
                }
            }
        }
        NirStmtKind::Return { value: Some(expr) }
        | NirStmtKind::Break {
            value: Some(expr), ..
        } => {
            record_defs_from_expr(expr, defs, tainted);
        }
        NirStmtKind::LetDestructure { value, .. } => {
            record_defs_from_expr(value, defs, tainted);
        }
        // Loop bodies have their own scope handled via process_loop.
        // Remaining kinds (Return/Break with None, Continue) carry no
        // expressions with nested definitions.
        NirStmtKind::Loop { .. }
        | NirStmtKind::Return { value: None }
        | NirStmtKind::Break { value: None, .. }
        | NirStmtKind::Continue => {}
    }
}

fn record_defs_from_expr(expr: &NirExpr, defs: &mut DefMap, tainted: &Taints) {
    match &expr.kind {
        NirExprKind::LabeledBlock { block, .. } | NirExprKind::Block(block) => {
            for s in &block.stmts {
                record_def_from_stmt(s, defs, tainted);
                record_defs_from_nested(s, defs, tainted);
            }
        }
        NirExprKind::Binary { left, right, .. }
        | NirExprKind::Assign {
            target: left,
            value: right,
        }
        | NirExprKind::Index {
            expr: left,
            index: right,
        } => {
            record_defs_from_expr(left, defs, tainted);
            record_defs_from_expr(right, defs, tainted);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::ClosureToCanonical { functor: inner, .. } => {
            record_defs_from_expr(inner, defs, tainted);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            record_defs_from_expr(condition, defs, tainted);
            for s in &then_branch.stmts {
                record_def_from_stmt(s, defs, tainted);
                record_defs_from_nested(s, defs, tainted);
            }
            if let Some(eb) = else_branch {
                for s in &eb.stmts {
                    record_def_from_stmt(s, defs, tainted);
                    record_defs_from_nested(s, defs, tainted);
                }
            }
        }
        NirExprKind::Match {
            expr: scrutinee,
            arms,
        } => {
            record_defs_from_expr(scrutinee, defs, tainted);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    record_defs_from_expr(guard, defs, tainted);
                }
                record_defs_from_expr(&arm.body, defs, tainted);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            record_defs_from_expr(scrutinee, defs, tainted);
            for arm in arms {
                for s in &arm.stmts {
                    record_def_from_stmt(s, defs, tainted);
                    record_defs_from_nested(s, defs, tainted);
                }
            }
            for s in &default.stmts {
                record_def_from_stmt(s, defs, tainted);
                record_defs_from_nested(s, defs, tainted);
            }
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                record_defs_from_expr(&arg.expr, defs, tainted);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                record_defs_from_expr(arg, defs, tainted);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            record_defs_from_expr(receiver, defs, tainted);
            for arg in args {
                record_defs_from_expr(&arg.expr, defs, tainted);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            record_defs_from_expr(callee, defs, tainted);
            for arg in args {
                record_defs_from_expr(arg, defs, tainted);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                record_defs_from_expr(&f.value, defs, tainted);
            }
        }
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            for e in elements {
                record_defs_from_expr(e, defs, tainted);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(inner) = payload {
                record_defs_from_expr(inner, defs, tainted);
            }
        }
        // Leaf nodes carry no sub-expressions with definitions.
        NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Eliminate implied-false conditions within a statement.
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

impl NirOptVisitor for ConditionEliminator<'_> {
    fn visit_stmt(&mut self, stmt: &mut NirStmt) -> bool {
        // Check if this statement is a bounds check that can be eliminated.
        if let NirStmtKind::If {
            condition,
            then_block,
            else_block: None,
        } = &mut stmt.kind
            && is_panic_block(then_block)
            && is_implied_false_by_any(condition, self.guard, &self.dom_guards, self.defs)
        {
            let type_id = condition.type_id;
            let span = condition.span;
            *condition = NirExpr {
                kind: NirExprKind::BoolLiteral(false),
                type_id,
                span,
            };
            return true;
        }

        // For If stmts: extract a dominating guard from the condition to extend
        // elimination into the then-block.
        if let NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } = &mut stmt.kind
        {
            let mut changed = self.visit_expr(condition);
            let dom = extract_dominating_guard(condition, self.defs);
            let saved = self.dom_guards.clone();
            if let Some(dg) = dom {
                self.dom_guards.push(dg);
            }
            changed |= self.visit_block(then_block);
            self.dom_guards = saved;
            if let Some(eb) = else_block {
                changed |= self.visit_block(eb);
            }
            return changed;
        }

        opt_walk_stmt(self, stmt)
    }

    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        // For If exprs: extract a dominating guard and propagate into then-branch.
        if let NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &mut expr.kind
        {
            let mut changed = self.visit_expr(condition);
            let dom = extract_dominating_guard(condition, self.defs);
            let saved = self.dom_guards.clone();
            if let Some(dg) = dom {
                self.dom_guards.push(dg);
            }
            changed |= self.visit_block(then_branch);
            self.dom_guards = saved;
            if let Some(eb) = else_branch {
                changed |= self.visit_block(eb);
            }
            return changed;
        }
        opt_walk_expr(self, expr)
    }
}

/// NIR visitor that eliminates bitmask-bounded false bounds checks.
///
/// Pattern: `if (x & MASK) >= BOUND { panic(...) }` where `BOUND > MASK >= 0`
/// Since `(x & MASK)` is always in `[0, MASK]`, the condition is always false.
struct BitmaskEliminator<'a> {
    defs: &'a DefMap,
}

impl NirOptVisitor for BitmaskEliminator<'_> {
    fn visit_stmt(&mut self, stmt: &mut NirStmt) -> bool {
        if let NirStmtKind::If {
            condition,
            then_block,
            else_block: None,
        } = &mut stmt.kind
            && is_panic_block(then_block)
            && is_bitmask_bounded(condition, self.defs)
        {
            let type_id = condition.type_id;
            let span = condition.span;
            *condition = NirExpr {
                kind: NirExprKind::BoolLiteral(false),
                type_id,
                span,
            };
            return true;
        }
        opt_walk_stmt(self, stmt)
    }
}

/// Extract a guard from an early-exit if-statement.
///
/// Matches: `if (var + k) >= bound { return/break }` → after this stmt,
/// we know `(var + k) < bound`.
fn extract_early_exit_guard(stmt: &NirStmt, defs: &DefMap) -> Option<ShortCircuitGuard> {
    let NirStmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &stmt.kind
    else {
        return None;
    };

    // then_block must be all early exits (return/break)
    if !block_always_exits(then_block) {
        return None;
    }

    ShortCircuitGuard::extract(condition, defs)
}

fn block_always_exits(block: &NirBlock) -> bool {
    block.stmts.iter().any(|s| {
        matches!(
            s.kind,
            NirStmtKind::Return { .. } | NirStmtKind::Break { .. }
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

impl NirOptVisitor for ShortCircuitEliminator<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) -> bool {
        if let NirExprKind::Binary {
            left,
            op: NirBinaryOp::Or,
            right,
        } = &mut expr.kind
        {
            let mut changed = self.visit_expr(left);
            if let Some(guard) = ShortCircuitGuard::extract(left, self.defs) {
                changed |= guard.eliminate_in_expr(right, self.defs);
            }
            changed |= self.visit_expr(right);
            return changed;
        }
        opt_walk_expr(self, expr)
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
    /// The bound expression — either a local index or a field access descriptor
    bound: BoundExpr,
}

#[derive(Clone)]
enum BoundExpr {
    Local(u32),
    /// A chain of field accesses from a root local:
    /// `root_local.field_indices[0].field_indices[1]...`.
    /// `FieldChain { root_local, field_indices: [] }` would be equivalent to
    /// `Local(root_local)`, so an empty chain is never constructed.
    FieldChain {
        root_local: u32,
        field_indices: Vec<u32>,
    },
}

/// Decompose `local`, `local.f1`, `local.f1.f2`, ... into a `(root_local,
/// field_indices)` pair. Returns `None` for anything else (method calls,
/// arithmetic, etc.) — the caller treats those as opaque bounds and bails.
fn extract_field_chain(expr: &NirExpr) -> Option<(u32, Vec<u32>)> {
    match &expr.kind {
        NirExprKind::Local { index, .. } => Some((*index, Vec::new())),
        NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } => {
            let (root, mut fields) = extract_field_chain(inner)?;
            fields.push(*field_index);
            Some((root, fields))
        }
        _ => None,
    }
}

impl ShortCircuitGuard {
    /// Extract a guard from `(var + k) >= bound` being false.
    fn extract(condition: &NirExpr, defs: &DefMap) -> Option<Self> {
        let NirExprKind::Binary { left, op, right } = &condition.kind else {
            return None;
        };
        if *op != NirBinaryOp::GtEq {
            return None;
        }

        let bound = match &right.kind {
            NirExprKind::Local { index, .. } => BoundExpr::Local(*index),
            NirExprKind::FieldAccess { .. } => {
                let (root_local, field_indices) = extract_field_chain(right)?;
                if field_indices.is_empty() {
                    BoundExpr::Local(root_local)
                } else {
                    BoundExpr::FieldChain {
                        root_local,
                        field_indices,
                    }
                }
            }
            _ => return None,
        };

        let (var, max_offset) = match &left.kind {
            NirExprKind::Local { index, .. } => (*index, 0),
            NirExprKind::Binary {
                left: inner_left,
                op: NirBinaryOp::Add,
                right: inner_right,
            } => {
                let NirExprKind::Local { index: var, .. } = &inner_left.kind else {
                    return None;
                };
                let offset = match &inner_right.kind {
                    NirExprKind::IntLiteral { value, .. } => *value as i64,
                    NirExprKind::Local { index, .. } => resolve_constant(*index, defs)?,
                    _ => return None,
                };
                if offset < 0 {
                    return None;
                }
                (*var, offset)
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
    fn implies_false(&self, condition: &NirExpr, defs: &DefMap) -> bool {
        let condition = peel_branch_hint(condition);
        let NirExprKind::Binary { left, op, right } = &condition.kind else {
            return false;
        };
        if *op != NirBinaryOp::GtEq {
            return false;
        }

        // Check that the bound matches
        if !self.bound_matches(right, defs) {
            return false;
        }

        // Check that check_var resolves to var + k where k <= max_offset
        self.var_in_range(left, defs)
    }

    fn bound_matches(&self, expr: &NirExpr, defs: &DefMap) -> bool {
        match &self.bound {
            BoundExpr::Local(guard_bound) => {
                if let NirExprKind::Local { index, .. } = &expr.kind {
                    resolves_to(*index, *guard_bound, defs)
                } else {
                    false
                }
            }
            BoundExpr::FieldChain {
                root_local: guard_root,
                field_indices: guard_fields,
            } => {
                // Walk `expr` outermost-first against `guard_fields` in
                // reverse without materialising a new Vec — `bound_matches`
                // is a hot per-condition check.
                let mut current = expr;
                for &expected_field in guard_fields.iter().rev() {
                    let NirExprKind::FieldAccess {
                        expr: inner,
                        field_index,
                        ..
                    } = &current.kind
                    else {
                        return false;
                    };
                    if *field_index != expected_field {
                        return false;
                    }
                    current = inner;
                }
                if let NirExprKind::Local { index, .. } = &current.kind {
                    resolves_to(*index, *guard_root, defs)
                } else {
                    false
                }
            }
        }
    }

    fn var_in_range(&self, expr: &NirExpr, defs: &DefMap) -> bool {
        match &expr.kind {
            NirExprKind::Local { index, .. } => {
                if resolves_to(*index, self.var, defs) {
                    return true; // offset 0 <= max_offset
                }
                // Check if it resolves to var + k through defs
                resolve_offset_from(*index, self.var, defs)
                    .is_some_and(|offset| offset >= 0 && offset <= self.max_offset)
            }
            NirExprKind::Binary {
                left,
                op: NirBinaryOp::Add,
                right,
            } => {
                let NirExprKind::Local { index, .. } = &left.kind else {
                    return false;
                };
                if !resolves_to(*index, self.var, defs) {
                    return false;
                }
                let offset = match &right.kind {
                    NirExprKind::IntLiteral { value, .. } => *value as i64,
                    NirExprKind::Local { index, .. } => {
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

    fn eliminate_in_expr(&self, expr: &mut NirExpr, defs: &DefMap) -> bool {
        match &mut expr.kind {
            NirExprKind::LabeledBlock { block, .. } | NirExprKind::Block(block) => {
                self.eliminate_in_block(block, defs)
            }
            NirExprKind::Binary { left, right, .. } => {
                let mut changed = self.eliminate_in_expr(left, defs);
                changed |= self.eliminate_in_expr(right, defs);
                changed
            }
            NirExprKind::Unary { expr: inner, .. }
            | NirExprKind::Cast { expr: inner, .. }
            | NirExprKind::FieldAccess { expr: inner, .. } => self.eliminate_in_expr(inner, defs),
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut changed = self.eliminate_in_expr(condition, defs);
                changed |= self.eliminate_in_block(then_branch, defs);
                if let Some(eb) = else_branch {
                    changed |= self.eliminate_in_block(eb, defs);
                }
                changed
            }
            _ => false,
        }
    }

    fn eliminate_in_block(&self, block: &mut NirBlock, defs: &DefMap) -> bool {
        let mut changed = false;
        for stmt in &mut block.stmts {
            changed |= self.eliminate_in_stmt(stmt, defs);
        }
        changed
    }

    fn eliminate_in_stmt(&self, stmt: &mut NirStmt, defs: &DefMap) -> bool {
        // Check if this is a bounds-check `if (index >= bound) { panic() }` implied false
        if let NirStmtKind::If {
            condition,
            then_block,
            else_block: None,
        } = &mut stmt.kind
            && is_panic_block(then_block)
            && self.implies_false(condition, defs)
        {
            let type_id = condition.type_id;
            let span = condition.span;
            *condition = NirExpr {
                kind: NirExprKind::BoolLiteral(false),
                type_id,
                span,
            };
            return true;
        }

        // Recurse into sub-expressions and sub-statements
        match &mut stmt.kind {
            NirStmtKind::Let { value, .. } => self.eliminate_in_expr(value, defs),
            NirStmtKind::Expr(expr) => self.eliminate_in_expr(expr, defs),
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut changed = self.eliminate_in_expr(condition, defs);
                changed |= self.eliminate_in_block(then_block, defs);
                if let Some(eb) = else_block {
                    changed |= self.eliminate_in_block(eb, defs);
                }
                changed
            }
            NirStmtKind::Return { value: Some(expr) }
            | NirStmtKind::Break {
                value: Some(expr), ..
            } => self.eliminate_in_expr(expr, defs),
            NirStmtKind::LabeledBlock { block, .. } | NirStmtKind::Loop { body: block } => {
                self.eliminate_in_block(block, defs)
            }
            _ => false,
        }
    }
}

/// Check if `(index >= bound)` is provably false because index is bitmask-bounded.
///
/// `(x & MASK) >= BOUND` is false when `MASK >= 0` and `BOUND > MASK`.
fn is_bitmask_bounded(condition: &NirExpr, defs: &DefMap) -> bool {
    let condition = peel_branch_hint(condition);
    let NirExprKind::Binary { left, op, right } = &condition.kind else {
        return false;
    };

    if *op != NirBinaryOp::GtEq {
        return false;
    }

    let NirExprKind::Local {
        index: check_var, ..
    } = &left.kind
    else {
        return false;
    };
    let Some(check_bound) = extract_bound_value(right) else {
        return false;
    };

    // Find the maximum value of check_var (if bitmask-bounded)
    let Some(max_val) = resolve_max_value(*check_var, defs) else {
        return false;
    };

    // Resolve `check_bound` to a concrete integer — literal RHS goes
    // through directly, a Local RHS gets walked through `defs`.
    let Some(bound_val) = bound_to_constant(check_bound, defs) else {
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
fn is_implied_false(condition: &NirExpr, guard: &LoopGuard, defs: &DefMap) -> bool {
    let condition = peel_branch_hint(condition);
    let NirExprKind::Binary { left, op, right } = &condition.kind else {
        return false;
    };

    // We're looking for `check_var >= check_bound`
    if *op != NirBinaryOp::GtEq {
        return false;
    }

    let NirExprKind::Local {
        index: check_var, ..
    } = &left.kind
    else {
        return false;
    };
    let Some(check_bound) = extract_bound_value(right) else {
        return false;
    };

    // check_var must resolve to the guard's induction variable
    if !resolves_to(*check_var, guard.var, defs) {
        return false;
    }

    // For `<` guard (`var < B`): check is false iff `check_bound >= B`.
    // For `<=` guard (`var <= B`): check is false iff `check_bound > B`.
    // [`check_bound_implied_false`] applies exact-match for two-Local
    // bounds and >=/> for literal-mixed bounds.
    check_bound_implied_false(check_bound, guard.bound, guard.is_strict, defs)
}

/// Peel a `builtin::likely` / `builtin::unlikely` branch-hint wrapper so the
/// underlying condition can be analyzed. The hint annotates branch prediction
/// without changing the condition's value, so a guarded bounds check written as
/// `if builtin::unlikely(i >= len) { panic }` must be seen through to reach the
/// `i >= len` comparison.
///
/// Inclusion criteria are funnelled through
/// [`FunctionRef::is_branch_hint_call`] so this matcher, niri's
/// `try_fold` peel, and the WIR builder's `BranchHint` lowering stay
/// in sync.
fn peel_branch_hint(condition: &NirExpr) -> &NirExpr {
    if let NirExprKind::Call { func, args, .. } = &condition.kind
        && args.len() == 1
        && func.is_branch_hint_call()
    {
        return peel_branch_hint(&args[0].expr);
    }
    condition
}

/// Check if a condition is implied false by the loop guard OR any dominating guard.
fn is_implied_false_by_any(
    condition: &NirExpr,
    guard: &LoopGuard,
    dom_guards: &[DominatingGuard],
    defs: &DefMap,
) -> bool {
    if is_implied_false(condition, guard, defs) {
        return true;
    }
    for dg in dom_guards {
        if is_implied_by_dominating_guard(condition, dg, defs) {
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
    condition: &NirExpr,
    dg: &DominatingGuard,
    defs: &DefMap,
) -> bool {
    let condition = peel_branch_hint(condition);
    let NirExprKind::Binary { left, op, right } = &condition.kind else {
        return false;
    };
    if *op != NirBinaryOp::GtEq {
        return false;
    }
    let NirExprKind::Local {
        index: check_var, ..
    } = &left.kind
    else {
        return false;
    };
    let Some(check_bound) = extract_bound_value(right) else {
        return false;
    };

    // check_var must resolve to `dg.var + offset` where
    // `0 <= offset <= dg.max_offset`.
    let Some(offset) = resolve_offset_from(*check_var, dg.var, defs) else {
        return false;
    };
    if offset < 0 || offset > dg.max_offset {
        return false;
    }

    // Domain analysis with offset:
    //   guard:      var + max_offset < dg.bound
    //               ⇒ var <= dg.bound - max_offset - 1
    //   check_var = var + offset
    //               ⇒ check_var <= dg.bound - max_offset + offset - 1
    //   check (check_var >= check_bound) is false when
    //               check_bound > check_var,
    //               i.e. check_bound >= dg.bound - max_offset + offset.
    // Define `tighten = max_offset - offset` and the condition is
    // `check_bound >= dg.bound - tighten`. For two-Local bounds we
    // fall back to the legacy exact-match (sound but precision-
    // limited to the `tighten == 0` case); for literal-mixed bounds
    // we can subtract directly.
    let tighten = dg.max_offset - offset;
    match (
        bound_to_constant(check_bound, defs),
        bound_to_constant(dg.bound, defs),
    ) {
        (Some(check_v), Some(guard_v)) => check_v >= guard_v - tighten,
        _ => {
            // Local-only path: rely on identity-equal bound (the
            // legacy soundness regime — see check_bound_implied_false
            // for the rationale).
            check_bound_implied_false(check_bound, dg.bound, true, defs)
        }
    }
}

/// Extract a dominating guard from an if-condition.
///
/// Matches: `(var + offset) < bound` → `DominatingGuard` { var, `max_offset`: offset, bound }
///      or: `var < bound` → `DominatingGuard` { var, `max_offset`: 0, bound }
fn extract_dominating_guard(condition: &NirExpr, defs: &DefMap) -> Option<DominatingGuard> {
    let NirExprKind::Binary { left, op, right } = &condition.kind else {
        return None;
    };
    if *op != NirBinaryOp::Lt {
        return None;
    }
    let bound = extract_bound_value(right)?;

    // Left side: either `var` or `var + offset`
    match &left.kind {
        NirExprKind::Local { index: var, .. } => Some(DominatingGuard {
            var: *var,
            max_offset: 0,
            bound,
        }),
        NirExprKind::Binary {
            left: inner_left,
            op: NirBinaryOp::Add,
            right: inner_right,
        } => {
            let NirExprKind::Local { index: var, .. } = &inner_left.kind else {
                return None;
            };
            // Offset can be a literal or a local resolving to a constant
            let offset = match &inner_right.kind {
                NirExprKind::IntLiteral { value, .. } => *value as i64,
                NirExprKind::Local { index, .. } => resolve_constant(*index, defs)?,
                _ => return None,
            };
            if offset < 0 {
                return None;
            }
            Some(DominatingGuard {
                var: *var,
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
fn is_panic_block(block: &NirBlock) -> bool {
    block.stmts.iter().any(|s| match &s.kind {
        NirStmtKind::Expr(expr) => is_panic_call(expr),
        _ => false,
    })
}

fn is_panic_call(expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::Call { func, .. } => func.name.contains("panic"),
        _ => false,
    }
}
