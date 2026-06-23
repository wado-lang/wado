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
//!
//! Runs via [`eliminate_at_root`], sharing licm's engine session (see
//! `licm.rs`). The single rewrite point promotes a condition to constant
//! `false` (`set_false`), replacing already-judged condition nodes only, so the
//! shared value graph stays valid for every later query.

use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{BlockId, ExprId, ExprKind, NodeRef, Operand, PatId, StmtId, StmtKind};
use crate::nir_engine::Engine;
use crate::nir_value_graph::{ValueId, ValueKind};

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
        lhs: Operand,
        rhs: Operand,
        is_strict: bool,
    ) -> Option<GuardFact> {
        let lhs_vn = engine.operand_value(lhs)?;
        let rhs_vn = engine.operand_value(rhs)?;
        Some(Self::from_values(engine, lhs_vn, rhs_vn, is_strict))
    }

    /// Build a fact directly from the comparison operands' `ValueId`s, for
    /// guards whose `<` sits behind a CSE temporary the value graph resolves
    /// through (see [`lt_comparison`]).
    fn from_values(
        engine: &mut Engine,
        lhs_vn: ValueId,
        rhs_vn: ValueId,
        is_strict: bool,
    ) -> GuardFact {
        // `decompose_add_const` only accumulates non-negative constants,
        // so `max_offset >= 0` holds by construction.
        let (var_vn, max_offset) = decompose_add_const(engine, lhs_vn);
        GuardFact {
            var_vn,
            max_offset,
            bound_vn: rhs_vn,
            is_strict,
        }
    }

    /// Whether this fact proves the bounds check guarded by `condition` dead.
    /// `condition` denotes the failing predicate `check_lhs >= check_rhs`
    /// (see [`failure_ge_operands`]).
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
    fn implies_false(&self, engine: &mut Engine, condition: Operand) -> bool {
        let Some((lhs_vn, rhs_vn)) = failure_ge_operands(engine, condition) else {
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

/// The `(lhs, rhs)` value-ids of the `lhs >= rhs` predicate whose truth makes
/// the bounds check guarded by `condition` *fail*.
///
/// Matching is on the condition's *interned boolean value*, not its surface
/// syntax: `engine.value(condition)` resolves through copy temporaries, and
/// [`ge_operands_of_value`] recognizes the predicate however the optimizer
/// spelled it — `lhs >= rhs`, `!(lhs < rhs)`, or the post-CSE `(lhs < rhs) ==
/// false`. A new equivalent spelling only needs an arm there, not a new
/// expr-shape matcher at every call site.
fn failure_ge_operands(engine: &mut Engine, condition: Operand) -> Option<(ValueId, ValueId)> {
    let value = engine.operand_value(condition)?;
    ge_operands_of_value(engine, value)
}

/// [`failure_ge_operands`] over an already-resolved boolean value.
fn ge_operands_of_value(engine: &mut Engine, value: ValueId) -> Option<(ValueId, ValueId)> {
    // Copy each shape's operands out so the `value_kind` borrow ends before the
    // recursive `&mut engine` queries below.
    enum Shape {
        Ge(ValueId, ValueId),
        Not(ValueId),
        Eq(ValueId, ValueId),
        Other,
    }
    let shape = match engine.value_kind(value) {
        ValueKind::Binary {
            op: NirBinaryOp::GtEq,
            lhs,
            rhs,
            ..
        } => Shape::Ge(*lhs, *rhs),
        ValueKind::Unary {
            op: NirUnaryOp::Not,
            operand,
            ..
        } => Shape::Not(*operand),
        ValueKind::Binary {
            op: NirBinaryOp::Eq,
            lhs,
            rhs,
            ..
        } => Shape::Eq(*lhs, *rhs),
        _ => Shape::Other,
    };
    match shape {
        Shape::Ge(lhs, rhs) => Some((lhs, rhs)),
        Shape::Not(inner) => strict_lt_operands(engine, inner),
        // `(x < y) == false` ≡ `!(x < y)`; the false literal may be on either side.
        Shape::Eq(lhs, rhs) => {
            if is_false_value(engine, rhs) {
                strict_lt_operands(engine, lhs)
            } else if is_false_value(engine, lhs) {
                strict_lt_operands(engine, rhs)
            } else {
                None
            }
        }
        Shape::Other => None,
    }
}

/// The `(lhs, rhs)` of the strict `<` comparison `value` denotes — the operands
/// of the `lhs >= rhs` predicate that `!value` denotes. Only `<` negates to
/// `>=` (`!(x <= y)` is `x > y`, a different predicate), so a `<=` resolution
/// is rejected.
fn strict_lt_operands(engine: &mut Engine, value: ValueId) -> Option<(ValueId, ValueId)> {
    match lt_of_value(engine, value)? {
        (lhs, rhs, true) => Some((lhs, rhs)),
        _ => None,
    }
}

/// The operands and strictness (`true` = `<`) if `value` is a `<` / `<=`.
fn lt_of_value(engine: &mut Engine, value: ValueId) -> Option<(ValueId, ValueId, bool)> {
    match engine.value_kind(value) {
        ValueKind::Binary {
            op: NirBinaryOp::Lt,
            lhs,
            rhs,
            ..
        } => Some((*lhs, *rhs, true)),
        ValueKind::Binary {
            op: NirBinaryOp::LtEq,
            lhs,
            rhs,
            ..
        } => Some((*lhs, *rhs, false)),
        _ => None,
    }
}

/// Whether `value` is the `false` / `0` literal (`b == 0` is the CSE spelling
/// of `!b`).
fn is_false_value(engine: &mut Engine, value: ValueId) -> bool {
    matches!(
        engine.value_kind(value),
        ValueKind::Bool(false) | ValueKind::Int(0, _)
    )
}

/// Decompose a value as `base + k` (`k >= 0` enforcement is the caller's):
/// nested `Binary(Add, …, Int(k))` layers are peeled and their constants
/// summed, so a chained derived cursor (`let p = pos + 1; let q = p + 2`)
/// yields `(vn(pos), 3)` — the value graph resolves the copy chain, this
/// resolves the addition chain. Constants that do not fit a non-negative
/// `i64` (bit 63 set — a negative signed literal's bit pattern) and sums
/// that would overflow stop the peel at the current layer, leaving the
/// remaining `Add` opaque inside `base`, which only costs precision.
fn decompose_add_const(engine: &mut Engine, v: ValueId) -> (ValueId, i64) {
    let mut base = v;
    let mut total: i64 = 0;
    loop {
        let ValueKind::Binary {
            op: NirBinaryOp::Add,
            lhs,
            rhs,
            ..
        } = engine.value_kind(base)
        else {
            return (base, total);
        };
        let (lhs, rhs) = (*lhs, *rhs);
        let ValueKind::Int(k, _) = engine.value_kind(rhs) else {
            return (base, total);
        };
        let Some(step) = i64::try_from(*k).ok().filter(|s| *s >= 0) else {
            return (base, total);
        };
        let Some(sum) = total.checked_add(step) else {
            return (base, total);
        };
        base = lhs;
        total = sum;
    }
}

/// The integer constant a value denotes, if its kind is `Int`. Bit patterns
/// that do not fit `i64` (bit 63 set) yield `None` rather than feeding a
/// negative to the numeric comparisons.
fn int_const(engine: &mut Engine, v: ValueId) -> Option<i64> {
    match engine.value_kind(v) {
        ValueKind::Int(value, _) => i64::try_from(*value).ok(),
        _ => None,
    }
}

/// Whether `v` is structurally `base + 1`.
fn is_plus_one_of(engine: &mut Engine, v: ValueId, base: ValueId) -> bool {
    if let ValueKind::Binary {
        op: NirBinaryOp::Add,
        lhs,
        rhs,
        ..
    } = engine.value_kind(v)
    {
        let (lhs, rhs) = (*lhs, *rhs);
        return lhs == base && matches!(engine.value_kind(rhs), ValueKind::Int(1, _));
    }
    false
}

/// Run condition implication at the body root on an existing engine session.
/// The combined `licm` session reuses its (value-preserving) `ValueGraph`, so
/// cond-impl needs no separate build; it runs after licm in document order, so
/// it still sees the hoisted body.
pub(super) fn eliminate_at_root(engine: &mut Engine) -> bool {
    // The `value_of` re-seed band-aids are removed (WEP item 3: retiring the
    // side-table). The BCE matching now sees only the build's identities and the
    // operands promotion produces; loop induction vars / hoisted bounds and
    // dropped invariant-field reads no longer get a restored identity, so some
    // loop / straight-line bounds checks are no longer eliminated (sound
    // under-elimination — never a miscompile). That regression is the honest cost
    // of the side-table; recovering it is the LoopPhi-operand work.
    let root = engine.body.root;
    process_block(engine, root)
}

/// A structural bound: the right-hand side of a guard / check comparison,
/// compared **syntactically** (no value graph). Two `BoundKey`s are equal iff
/// they denote the same program object by structure.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoundKey {
    Local(u32),
    /// `root_local . field_index` — a field read of a by-value local.
    Field(u32, u32),
}

/// Copy/CSE temp bindings: a single-assignment local `t` bound by
/// `let t = <op>` maps to `<op>`. Lets the structural matcher see through the
/// `let __cond = i < n; if !__cond { panic }` shape CSE produces.
type Binds = crate::hashmap::IndexMap<u32, Operand>;

/// Build [`Binds`] over `body`: every `let t = <value>` whose `t` is never
/// reassigned (`Assign` / `&mut`). Conservative — a reassigned temp is excluded,
/// so resolving through it can never read a stale value.
fn build_copy_bindings(body: &crate::nir_arena::Body) -> Binds {
    let mut reassigned = crate::hashmap::IndexSet::default();
    let mut record = |node: NodeRef| {
        if let NodeRef::Expr(e) = node {
            match &body.exprs[e].kind {
                ExprKind::Assign { target, .. } => {
                    if let Some(r) = super::arena_query::projection_root_local(body, *target) {
                        reassigned.insert(r);
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr,
                } => {
                    if let Some(ie) = expr.as_expr()
                        && let Some(r) = super::arena_query::projection_root_local(body, ie)
                    {
                        reassigned.insert(r);
                    }
                }
                _ => {}
            }
        }
    };
    let mut stack = vec![NodeRef::Block(body.root)];
    while let Some(n) = stack.pop() {
        record(n);
        body.for_each_child(n, |c| stack.push(c));
    }
    let mut binds = Binds::default();
    for (_, st) in &body.stmts {
        if let StmtKind::Let {
            local_index, value, ..
        } = &st.kind
            && !reassigned.contains(local_index)
        {
            binds.insert(*local_index, *value);
        }
    }
    binds
}

/// Resolve an operand through [`Binds`] (bounded depth).
fn resolve(engine: &Engine, binds: &Binds, op: Operand) -> Operand {
    let mut cur = op;
    for _ in 0..8 {
        let Some(e) = cur.as_expr() else { break };
        let ExprKind::Local { index, .. } = &engine.body.exprs[e].kind else {
            break;
        };
        let Some(&b) = binds.get(index) else { break };
        cur = b;
    }
    cur
}

/// The `Local idx` an `Opaque` value sources from, if any (pool read — not
/// `value_of`).
fn opaque_local(engine: &Engine, v: crate::nir_value_graph::ValueId) -> Option<u32> {
    if let ValueKind::Opaque(o) = engine.body.values.kind(v)
        && let Some(crate::nir_value_graph::OpaqueSource::Local(i)) =
            engine.body.values.opaque_source(*o)
    {
        Some(i)
    } else {
        None
    }
}

/// Parse an operand (through copy temps) as a structural bound: a bare local or
/// a `local.field` read over a by-value local root. Handles both the skeleton
/// form and a **promoted** `Operand::Value` (the freeze promotes a `FieldAccess`
/// bound), decomposed through the value **pool** (`body.values`, not `value_of`).
fn parse_bound(engine: &Engine, binds: &Binds, op: Operand) -> Option<BoundKey> {
    match resolve(engine, binds, op) {
        Operand::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::Local { index, .. } => Some(BoundKey::Local(*index)),
            ExprKind::FieldAccess { field_index, .. } => {
                let root = super::arena_query::projection_root_local(engine.body, e)?;
                Some(BoundKey::Field(root, *field_index))
            }
            _ => None,
        },
        Operand::Value(v) => match engine.body.values.kind(v) {
            ValueKind::Opaque(_) => opaque_local(engine, v).map(BoundKey::Local),
            ValueKind::FieldAccess {
                receiver,
                field_index,
                ..
            } => Some(BoundKey::Field(
                opaque_local(engine, *receiver)?,
                *field_index,
            )),
            _ => None,
        },
    }
}

/// Parse an operand (through copy temps) as a constant `i64`. Constants live in
/// the value **pool** as `Operand::Value(Int)` — that is `body.values` (the IR's
/// value pool), **not** the `value_of` side-table, so reading it stays
/// `value_of`-free.
fn parse_const_i64(engine: &Engine, binds: &Binds, op: Operand) -> Option<i64> {
    if let Operand::Value(v) = resolve(engine, binds, op)
        && let ValueKind::Int(val, _) = engine.body.values.kind(v)
    {
        return Some(*val as i64);
    }
    None
}

/// Parse an operand (through copy temps) as `var_local + const_offset`. A bare
/// local is offset 0; `+ const` accumulates through `Binary(Add)` (either side).
/// Bounded recursion via the copy-temp chain. Lets `arr[q]` where `q = p + 2`,
/// `p = pos + 1` decompose to `(pos, 3)`.
fn parse_var_offset(engine: &Engine, binds: &Binds, op: Operand) -> Option<(u32, i64)> {
    match resolve(engine, binds, op) {
        Operand::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::Local { index, .. } => Some((*index, 0)),
            ExprKind::Binary {
                left,
                op: NirBinaryOp::Add,
                right,
            } => {
                if let Some((v, o)) = parse_var_offset(engine, binds, *left)
                    && let Some(c) = parse_const_i64(engine, binds, *right)
                {
                    return Some((v, o + c));
                }
                if let Some((v, o)) = parse_var_offset(engine, binds, *right)
                    && let Some(c) = parse_const_i64(engine, binds, *left)
                {
                    return Some((v, o + c));
                }
                None
            }
            _ => None,
        },
        // A promoted (`Operand::Value`) `var + const` — the freeze promotes pure
        // arith — decomposed through the value pool (not `value_of`).
        Operand::Value(v) => parse_value_offset(engine, v),
    }
}

/// Decompose a pooled value as `Opaque(Local) + const` (pool read, not `value_of`).
fn parse_value_offset(engine: &Engine, v: crate::nir_value_graph::ValueId) -> Option<(u32, i64)> {
    match engine.body.values.kind(v) {
        ValueKind::Opaque(_) => opaque_local(engine, v).map(|i| (i, 0)),
        ValueKind::Binary {
            op: NirBinaryOp::Add,
            lhs,
            rhs,
            ..
        } => {
            let (lhs, rhs) = (*lhs, *rhs);
            if let Some((var, o)) = parse_value_offset(engine, lhs)
                && let ValueKind::Int(c, _) = engine.body.values.kind(rhs)
            {
                return Some((var, o + *c as i64));
            }
            if let Some((var, o)) = parse_value_offset(engine, rhs)
                && let ValueKind::Int(c, _) = engine.body.values.kind(lhs)
            {
                return Some((var, o + *c as i64));
            }
            None
        }
        _ => None,
    }
}

/// Parse a condition (through copy temps) as `(var + off) OP bound`. Returns
/// `(var_local, off, bound, op)`.
fn parse_cmp(
    engine: &Engine,
    binds: &Binds,
    cond: Operand,
) -> Option<(u32, i64, BoundKey, NirBinaryOp)> {
    let ce = resolve(engine, binds, cond).as_expr()?;
    if let ExprKind::Binary { left, op, right } = &engine.body.exprs[ce].kind {
        let op = *op;
        if matches!(
            op,
            NirBinaryOp::Lt | NirBinaryOp::LtEq | NirBinaryOp::Gt | NirBinaryOp::GtEq
        ) {
            let (var, off) = parse_var_offset(engine, binds, *left)?;
            let bound = parse_bound(engine, binds, *right)?;
            return Some((var, off, bound, op));
        }
    }
    None
}

/// Parse a bounds-check condition (through copy temps) that **panics when the
/// guard `var + K < bound` is violated**: `(var + off) >= bound`
/// (`Binary(GtEq)`) or its lowered form `!((var + off) < bound)`. Returns
/// `(var, off, bound)`.
fn parse_check(engine: &Engine, binds: &Binds, cond: Operand) -> Option<(u32, i64, BoundKey)> {
    let (left, right) = ge_check_operands(engine, binds, cond)?;
    let (var, off) = parse_var_offset(engine, binds, left)?;
    let bound = parse_bound(engine, binds, right)?;
    Some((var, off, bound))
}

/// The `(left, right)` of the failing `left >= right` predicate a bounds-check
/// condition denotes: `(left >= right)` directly, or its lowered form
/// `!(left < right)`. Handles both the skeleton (`Operand::Expr`) and a
/// **promoted** comparison (`Operand::Value`, decomposed through the value
/// **pool** — never `value_of`). Operands are returned raw (caller decides how
/// to parse each side).
fn ge_check_operands(engine: &Engine, binds: &Binds, cond: Operand) -> Option<(Operand, Operand)> {
    match resolve(engine, binds, cond) {
        Operand::Expr(ce) => match &engine.body.exprs[ce].kind {
            ExprKind::Binary {
                left,
                op: NirBinaryOp::GtEq,
                right,
            } => Some((*left, *right)),
            ExprKind::Unary {
                op: NirUnaryOp::Not,
                expr: inner,
            } => lt_operands(engine, binds, *inner),
            _ => None,
        },
        Operand::Value(v) => match engine.body.values.kind(v) {
            ValueKind::Binary {
                op: NirBinaryOp::GtEq,
                lhs,
                rhs,
                ..
            } => Some((Operand::Value(*lhs), Operand::Value(*rhs))),
            ValueKind::Unary {
                op: NirUnaryOp::Not,
                operand,
                ..
            } => lt_operands(engine, binds, Operand::Value(*operand)),
            _ => None,
        },
    }
}

/// The `(left, right)` of a strict `left < right`, skeleton or promoted.
fn lt_operands(engine: &Engine, binds: &Binds, op: Operand) -> Option<(Operand, Operand)> {
    match resolve(engine, binds, op) {
        Operand::Expr(ie) => {
            if let ExprKind::Binary {
                left,
                op: NirBinaryOp::Lt,
                right,
            } = &engine.body.exprs[ie].kind
            {
                Some((*left, *right))
            } else {
                None
            }
        }
        Operand::Value(v) => {
            if let ValueKind::Binary {
                op: NirBinaryOp::Lt,
                lhs,
                rhs,
                ..
            } = engine.body.values.kind(v)
            {
                Some((Operand::Value(*lhs), Operand::Value(*rhs)))
            } else {
                None
            }
        }
    }
}

/// Structural bitmask-bounded check (value_of-free): the failing predicate is
/// `(x & MASK) >= BOUND` with `MASK` and `BOUND` constants and `BOUND > MASK >=
/// 0`, so `x & MASK ∈ [0, MASK]` can never reach `BOUND`. The masked value is
/// read through copy temps (`let idx = i & MASK`) and decomposed via the value
/// **pool** when promoted — never the `value_of` side-table.
fn is_bitmask_bounded_structural(engine: &Engine, binds: &Binds, cond: Operand) -> bool {
    let Some((left, right)) = ge_check_operands(engine, binds, cond) else {
        return false;
    };
    let Some(bound) = parse_const_i64(engine, binds, right) else {
        return false;
    };
    let Some(mask) = bitand_mask(engine, binds, left) else {
        return false;
    };
    mask >= 0 && bound > mask
}

/// The constant `MASK` of a `value & MASK` (through copy temps / value pool), if
/// either operand is a constant.
fn bitand_mask(engine: &Engine, binds: &Binds, op: Operand) -> Option<i64> {
    match resolve(engine, binds, op) {
        Operand::Expr(e) => {
            let ExprKind::Binary {
                left,
                op: NirBinaryOp::BitAnd,
                right,
            } = &engine.body.exprs[e].kind
            else {
                return None;
            };
            let (l, r) = (*left, *right);
            parse_const_i64(engine, binds, r).or_else(|| parse_const_i64(engine, binds, l))
        }
        Operand::Value(v) => {
            let ValueKind::Binary {
                op: NirBinaryOp::BitAnd,
                lhs,
                rhs,
                ..
            } = engine.body.values.kind(v)
            else {
                return None;
            };
            let (l, r) = (*lhs, *rhs);
            pool_int_const(engine, r).or_else(|| pool_int_const(engine, l))
        }
    }
}

/// A pooled value's constant `i64`, if it is an `Int` (pool read, not `value_of`).
fn pool_int_const(engine: &Engine, v: crate::nir_value_graph::ValueId) -> Option<i64> {
    if let ValueKind::Int(val, _) = engine.body.values.kind(v) {
        Some(*val as i64)
    } else {
        None
    }
}

/// The local that `BoundKey` reads through, if any (for write-tracking).
fn bound_root(b: BoundKey) -> Option<u32> {
    match b {
        BoundKey::Local(l) | BoundKey::Field(l, _) => Some(l),
    }
}

/// Conservatively, does statement `s` modify `var` or `bound`'s backing — an
/// assignment to the local / its field, a `&mut` escape of either root, or a
/// method call whose receiver is either root (may take `&mut self`)? Sound
/// over-approximation: a false "modifies" only forgoes an elimination. The
/// guard/check's own `panic(msg)` is a free call on neither root, so it does not
/// trip this — keeping a clean check eliminable.
fn stmt_modifies(engine: &Engine, s: StmtId, var: u32, bound: BoundKey) -> bool {
    node_modifies(engine, NodeRef::Stmt(s), var, bound)
}

/// [`stmt_modifies`] over an arbitrary node subtree (e.g. the right operand of a
/// short-circuit `||`).
fn node_modifies(engine: &Engine, node: NodeRef, var: u32, bound: BoundKey) -> bool {
    let roots = [Some(var), bound_root(bound)];
    let is_root = |l: u32| roots.iter().any(|r| *r == Some(l));
    let mut hit = false;
    let mut visit = |node: NodeRef| {
        if let NodeRef::Expr(e) = node {
            match &engine.body.exprs[e].kind {
                ExprKind::Assign { target, .. } => {
                    if let Some(root) =
                        super::arena_query::projection_root_local(engine.body, *target)
                        && is_root(root)
                    {
                        hit = true;
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr: inner,
                } => {
                    if let Some(ie) = inner.as_expr()
                        && let Some(root) =
                            super::arena_query::projection_root_local(engine.body, ie)
                        && is_root(root)
                    {
                        hit = true;
                    }
                }
                ExprKind::MethodCall { receiver, .. } => {
                    if let Some(re) = receiver.as_expr()
                        && let Some(root) =
                            super::arena_query::projection_root_local(engine.body, re)
                        && is_root(root)
                    {
                        hit = true;
                    }
                }
                _ => {}
            }
        }
    };
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        visit(n);
        engine.body.for_each_child(n, |c| stack.push(c));
    }
    hit
}

/// Structural loop-guard BCE (value_of-free, mirrors the `licm` migration off
/// the side-table). Recognises the loop guard `if !(var < bound) { break }` at
/// the loop head and drives to `false` a dominated bounds-check
/// `if (var >= bound) { panic }` in the body — by **structural** comparison of
/// the skeleton reads plus a position-aware "no modification of `var`/`bound`
/// between the guard and the check" scan. No value graph, no promotion.
fn structural_loop_guard(engine: &mut Engine, loop_body: BlockId) -> bool {
    let stmts = engine.body.blocks[loop_body].stmts.clone();
    let guard_idx = stmts.iter().position(|s| {
        !matches!(
            engine.body.stmts[*s].kind,
            StmtKind::Let { .. } | StmtKind::LetDestructure { .. }
        )
    });
    let Some(guard_idx) = guard_idx else {
        return false;
    };
    // Guard: `if !(var < bound) { break }` (single-stmt break then-block).
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &engine.body.stmts[stmts[guard_idx]].kind
    else {
        return false;
    };
    let (gcond, gthen) = (*condition, *then_block);
    if engine.body.blocks[gthen].stmts.len() != 1
        || !matches!(
            engine.body.stmts[engine.body.blocks[gthen].stmts[0]].kind,
            StmtKind::Break { .. }
        )
    {
        return false;
    }
    let Some(ge) = gcond.as_expr() else {
        return false;
    };
    let ExprKind::Unary {
        op: NirUnaryOp::Not,
        expr: inner,
    } = &engine.body.exprs[ge].kind
    else {
        return false;
    };
    let binds = build_copy_bindings(engine.body);
    let Some((var, goff, bound, gop)) = parse_cmp(engine, &binds, *inner) else {
        return false;
    };
    // After `if !(var + goff < bound) break`, the surviving body has
    // `var + goff < bound`. Strict guard only (a `var + j >= bound` check is
    // refuted for `j <= goff`).
    if gop != NirBinaryOp::Lt {
        return false;
    }
    // Walk body statements after the guard, in order. While no statement has
    // modified `var` / `bound` since the guard, eliminate every dominated
    // bounds-check `var + j >= bound` (`j <= goff`) nested anywhere in the
    // statement (the inlined `index_value` check is a `StmtKind::If` /
    // `ExprKind::If` inside the statement's expression block, not a top-level
    // statement). A statement that modifies `var` / `bound` (e.g. the `i += 1`
    // induction update) stops the scan — the guard fact no longer holds past it.
    let mut changed = false;
    for &s in stmts.iter().skip(guard_idx + 1) {
        if stmt_modifies(engine, s, var, bound) {
            break;
        }
        changed |= eliminate_checks_in_node(engine, NodeRef::Stmt(s), var, goff, bound, &binds);
    }
    changed
}

/// Drive to `false` every `if (var >= bound) { panic }` (no else) nested in
/// `node`, matching the loop guard's `var` / `bound` structurally. Both
/// `StmtKind::If` and `ExprKind::If` holders are handled (an inlined bounds
/// check sits in value position).
fn eliminate_checks_in_node(
    engine: &mut Engine,
    node: NodeRef,
    var: u32,
    k: i64,
    bound: BoundKey,
    binds: &Binds,
) -> bool {
    // Collect candidate If holders first (the walk borrows the body immutably),
    // then rewrite.
    let mut holders: Vec<(NodeRef, Operand)> = Vec::new();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let cand = match n {
            NodeRef::Stmt(s) => match &engine.body.stmts[s].kind {
                StmtKind::If {
                    condition,
                    then_block,
                    else_block: None,
                } => Some((*condition, *then_block)),
                _ => None,
            },
            NodeRef::Expr(e) => match &engine.body.exprs[e].kind {
                ExprKind::If {
                    condition,
                    then_branch,
                    else_branch: None,
                } => Some((*condition, *then_branch)),
                _ => None,
            },
            _ => None,
        };
        // The guard gives `var + k < bound`; a check `var + j >= bound` is
        // refuted for `0 <= j <= k` (`var + j <= var + k < bound`).
        if let Some((cond, then_b)) = cand
            && is_panic_block(engine, then_b)
            && let Some((cvar, cj, cbound)) = parse_check(engine, binds, cond)
            && cvar == var
            && cbound == bound
            && (0..=k).contains(&cj)
        {
            holders.push((n, cond));
        }
        engine.body.for_each_child(n, |c| stack.push(c));
    }
    let mut changed = false;
    for (holder, cond) in holders {
        eliminate_condition(engine, holder, cond);
        changed = true;
    }
    changed
}

/// A dominating `if (var + K) < bound { … }` proves `var + j < bound` for
/// `0 <= j <= K` inside its then-block (structural, value_of-free). Drive to
/// `false` every dominated bounds-check `var + j >= bound` nested in the
/// then-block, walked in order and stopped once a statement modifies
/// `var` / `bound` (so the fact no longer holds past it).
fn apply_dominating_if(engine: &mut Engine, s: StmtId, binds: &Binds) -> bool {
    let StmtKind::If {
        condition,
        then_block,
        ..
    } = &engine.body.stmts[s].kind
    else {
        return false;
    };
    let (cond, then_b) = (*condition, *then_block);
    let Some((var, k, bound, op)) = parse_cmp(engine, binds, cond) else {
        return false;
    };
    // Only `<` proves `var + j < bound`; `var + j >= bound` is then refuted for
    // `0 <= j <= k`.
    if op != NirBinaryOp::Lt || k < 0 {
        return false;
    }
    let mut changed = false;
    for &ts in engine.body.blocks[then_b].stmts.clone().iter() {
        if stmt_modifies(engine, ts, var, bound) {
            break;
        }
        changed |= eliminate_checks_in_node(engine, NodeRef::Stmt(ts), var, k, bound, binds);
    }
    changed
}

/// A straight-line early-exit guard `if (var + K >= bound) { return/break }`:
/// after it, the surviving path has `var + K < bound` (structural,
/// value_of-free). Returns `(var, K, bound)`.
fn recognize_early_exit(engine: &Engine, s: StmtId, binds: &Binds) -> Option<(u32, i64, BoundKey)> {
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &engine.body.stmts[s].kind
    else {
        return None;
    };
    let (cond, then_b) = (*condition, *then_block);
    if !block_always_exits(engine, then_b) {
        return None;
    }
    parse_check(engine, binds, cond)
}

fn process_block(engine: &mut Engine, block: BlockId) -> bool {
    let mut changed = false;
    // Guard facts from earlier early-exit statements. No staleness
    // tracking: a mutation between guard and check changes the check
    // operand's `ValueId`, so the fact stops matching by itself.
    let mut guards: Vec<GuardFact> = Vec::new();
    // Structural early-exit facts `var < bound` (value_of-free): a fact is used
    // for a later statement's checks while no statement since the guard has
    // modified `var` / `bound` (`stmt_modifies`), then dropped when one does.
    let binds = build_copy_bindings(engine.body);
    let mut seguards: Vec<(u32, i64, BoundKey)> = Vec::new();
    let stmts = engine.body.blocks[block].stmts.clone();
    for s in stmts {
        for guard in &guards {
            changed |= GuardEliminator { fact: *guard }.visit_stmt(engine, s);
        }
        for &(var, k, bound) in &seguards {
            if !stmt_modifies(engine, s, var, bound) {
                changed |=
                    eliminate_checks_in_node(engine, NodeRef::Stmt(s), var, k, bound, &binds);
            }
        }
        changed |= apply_dominating_if(engine, s, &binds);
        changed |= BitmaskEliminator { binds: &binds }.visit_stmt(engine, s);
        changed |= ShortCircuitEliminator { binds: &binds }.visit_stmt(engine, s);
        changed |= process_stmt(engine, s);
        seguards.retain(|&(var, _, bound)| !stmt_modifies(engine, s, var, bound));
        if let Some(guard) = extract_early_exit_guard(engine, s) {
            guards.push(guard);
        }
        if let Some(fact) = recognize_early_exit(engine, s, &binds) {
            seguards.push(fact);
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
    let mut changed = structural_loop_guard(engine, loop_body);

    // Dominating `if (var + K) < bound { … }` in the loop body proves the
    // dominated checks inside its then-block (value_of-free, structural).
    let binds = build_copy_bindings(engine.body);
    for s in engine.body.blocks[loop_body].stmts.clone() {
        changed |= apply_dominating_if(engine, s, &binds);
    }

    if let Some((guard, body_start)) = extract_loop_guard(engine, loop_body) {
        // Eliminate implied conditions in the loop body. `body_start` is the
        // index past the guard, so leading `let`s (e.g. a CSE-hoisted
        // `let __c = i < n`) and the guard itself are excluded — the fact only
        // holds after the guard.
        let mut condition_elim = ConditionEliminator {
            guard,
            dom_guards: vec![],
        };
        for s in engine.body.blocks[loop_body]
            .stmts
            .clone()
            .iter()
            .skip(body_start)
        {
            changed |= condition_elim.visit_stmt(engine, *s);
        }
    }

    // Eliminate bitmask-bounded checks in the loop body.
    for s in engine.body.blocks[loop_body].stmts.clone() {
        changed |= BitmaskEliminator { binds: &binds }.visit_stmt(engine, s);
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

/// Extract a loop guard, returning the fact and the index of the first
/// body statement after it.
///
/// Matches `if !(var < bound) { break LABEL; }` → guard `var < bound` (and
/// `<=` likewise). The guard is the loop body's first non-binding statement:
/// for-loop desugaring (after CSE) may emit leading pure `let`s before it
/// (e.g. `let __c = i < n`), and the guard condition may itself be such a
/// temporary — both are resolved through the value graph.
fn extract_loop_guard(engine: &mut Engine, loop_body: BlockId) -> Option<(GuardFact, usize)> {
    // Leading `let`s have no control flow, so the first `if … { break }` after
    // them still dominates the body.
    let guard_idx = engine.body.blocks[loop_body].stmts.iter().position(|s| {
        !matches!(
            engine.body.stmts[*s].kind,
            StmtKind::Let { .. } | StmtKind::LetDestructure { .. }
        )
    })?;
    let guard = engine.body.blocks[loop_body].stmts[guard_idx];

    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &engine.body.stmts[guard].kind
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

    // condition must resolve to `Not(<comparison>)`. Read its interned value so a
    // promoted (`Operand::Value`) condition is handled, not only a skeleton `Not`;
    // the comparison may sit behind a CSE / copy temporary the value graph resolves.
    let cond_v = engine.operand_value(condition)?;
    let inner_v = match engine.value_kind(cond_v) {
        ValueKind::Unary {
            op: NirUnaryOp::Not,
            operand,
            ..
        } => *operand,
        _ => return None,
    };
    let (lhs_vn, rhs_vn, is_strict) = lt_of_value(engine, inner_v)?;
    // Loop guards keep the plain-variable shape: the induction variable is
    // compared directly (`i < bound`), so any `Add` decomposition would
    // describe a different program object. Restrict to offset 0.
    let fact = GuardFact::from_values(engine, lhs_vn, rhs_vn, is_strict);
    (fact.max_offset == 0).then_some((fact, guard_idx + 1))
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
    // Read the condition's interned value so a promoted (`Operand::Value`)
    // `var + k >= bound` guard is handled, not only a skeleton `Binary`.
    let cond_v = engine.operand_value(condition)?;
    let (lhs_vn, rhs_vn) = match engine.value_kind(cond_v) {
        ValueKind::Binary {
            op: NirBinaryOp::GtEq,
            lhs,
            rhs,
            ..
        } => (*lhs, *rhs),
        _ => return None,
    };
    Some(GuardFact::from_values(engine, lhs_vn, rhs_vn, true))
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

/// Promote the condition at `cond` to the constant `false` in its parent slot.
fn set_false(engine: &mut Engine, cond: ExprId) {
    engine.replace_expr_with_value(cond, crate::const_eval::Value::Bool(false));
}

/// Set the `if` condition held by `holder` (a `StmtKind::If` or `ExprKind::If`)
/// to a pooled `false`, for a **promoted** condition (`Operand::Value`) that has
/// no skeleton expr to [`set_false`]. Mirrors `replace_expr_with_value`'s end
/// state (the condition operand becomes `Operand::Value(false)`) for the operand
/// case (WEP: operand promotion — the value passes read operands, not `value_of`).
fn force_condition_false(engine: &mut Engine, holder: NodeRef) {
    let false_v = engine
        .body
        .values
        .alloc_unshared(ValueKind::Bool(false), crate::tir::TypeTable::BOOL);
    match holder {
        NodeRef::Stmt(s) => {
            if let StmtKind::If { condition, .. } = &mut engine.body.stmts[s].kind {
                *condition = Operand::Value(false_v);
            }
        }
        NodeRef::Expr(e) => {
            if let ExprKind::If { condition, .. } = &mut engine.body.exprs[e].kind {
                *condition = Operand::Value(false_v);
            }
        }
        _ => {}
    }
    engine.enqueue(holder);
}

/// Drive the proven-false `if` condition held by `holder` to `false`: a skeleton
/// condition through [`set_false`] (graph-maintaining redirect, keeping the
/// default path byte-identical), a promoted condition through
/// [`force_condition_false`].
fn eliminate_condition(engine: &mut Engine, holder: NodeRef, condition: Operand) {
    match condition {
        Operand::Expr(ce) => set_false(engine, ce),
        Operand::Value(_) => force_condition_false(engine, holder),
    }
}

/// Check if a block traps (bounds check failure path): a `panic`, or the bare
/// `unreachable` that `-f bare-asserts` lowers an assertion failure into.
fn is_panic_block(engine: &Engine, block: BlockId) -> bool {
    engine.body.blocks[block]
        .stmts
        .iter()
        .any(|s| match &engine.body.stmts[*s].kind {
            StmtKind::Expr(expr) => expr.as_expr().is_some_and(|e| is_panic_call(engine, e)),
            _ => false,
        })
}

fn is_panic_call(engine: &Engine, e: ExprId) -> bool {
    match &engine.body.exprs[e].kind {
        ExprKind::Call { func, .. } => func.name.contains("panic") || func.name == "unreachable",
        _ => false,
    }
}

/// Whether `s` is a `if (cond) { panic-block }` bounds check whose condition
/// `fact` proves false; rewrites and reports if so.
fn fact_eliminates_panic(
    engine: &mut Engine,
    condition: Operand,
    then_block: BlockId,
    else_block: Option<BlockId>,
    holder: NodeRef,
    fact: &GuardFact,
) -> bool {
    if else_block.is_none()
        && is_panic_block(engine, then_block)
        && fact.implies_false(engine, condition)
    {
        eliminate_condition(engine, holder, condition);
        return true;
    }
    false
}

fn eliminate_panic_check(engine: &mut Engine, s: StmtId, fact: &GuardFact) -> bool {
    let StmtKind::If {
        condition,
        then_block,
        else_block,
    } = &engine.body.stmts[s].kind
    else {
        return false;
    };
    let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
    fact_eliminates_panic(
        engine,
        condition,
        then_block,
        else_block,
        NodeRef::Stmt(s),
        fact,
    )
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
    fn implied_false(&self, engine: &mut Engine, condition: Operand) -> bool {
        if self.guard.implies_false(engine, condition) {
            return true;
        }
        self.dom_guards
            .iter()
            .any(|d| d.implies_false(engine, condition))
    }

    /// Eliminate a panic guard `if cond { panic }` whose condition the active
    /// guards prove false. Shared by the statement and expression `If` arms — a
    /// bounds check inlined into value position (`sum + index(arr, i)`) is an
    /// `ExprKind::If`, not a `StmtKind::If`.
    fn try_eliminate_panic(
        &self,
        engine: &mut Engine,
        condition: Operand,
        then_block: BlockId,
        else_block: Option<BlockId>,
        holder: NodeRef,
    ) -> bool {
        if else_block.is_none()
            && is_panic_block(engine, then_block)
            && self.implied_false(engine, condition)
        {
            eliminate_condition(engine, holder, condition);
            return true;
        }
        false
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
            let condition_e = condition.as_expr();
            // Check if this statement is a bounds check that can be eliminated.
            if self.try_eliminate_panic(engine, condition, then_block, else_block, NodeRef::Stmt(s))
            {
                return true;
            }

            // Extract a dominating guard from the condition to extend
            // elimination into the then-block.
            let mut changed = condition_e.is_some_and(|ce| self.visit_expr(engine, ce));
            let dom = condition_e.and_then(|ce| extract_dominating_guard(engine, ce));
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
            // A bounds check inlined into value position is an `ExprKind::If`.
            if self.try_eliminate_panic(
                engine,
                condition,
                then_branch,
                else_branch,
                NodeRef::Expr(e),
            ) {
                return true;
            }
            let mut changed = condition
                .as_expr()
                .is_some_and(|ce| self.visit_expr(engine, ce));
            let dom = condition
                .as_expr()
                .and_then(|ce| extract_dominating_guard(engine, ce));
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

    fn visit_expr(&mut self, engine: &mut Engine, e: ExprId) -> bool {
        // A bounds check inlined into value position is an `ExprKind::If`.
        if let ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &engine.body.exprs[e].kind
        {
            let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
            if fact_eliminates_panic(
                engine,
                condition,
                then_branch,
                else_branch,
                NodeRef::Expr(e),
                &self.fact,
            ) {
                return true;
            }
        }
        arena_opt_walk(self, engine, NodeRef::Expr(e))
    }
}

/// Eliminates bitmask-bounded false bounds checks:
/// `if (x & MASK) >= BOUND { panic(...) }` with `BOUND > MASK >= 0`.
struct BitmaskEliminator<'a> {
    binds: &'a Binds,
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
            && is_panic_block(engine, then_block)
            && is_bitmask_bounded_structural(engine, self.binds, condition)
        {
            eliminate_condition(engine, NodeRef::Stmt(s), condition);
            return true;
        }
        arena_opt_walk(self, engine, NodeRef::Stmt(s))
    }
}

/// Eliminates redundant bounds checks inside short-circuit `||` expressions
/// (value_of-free, structural): in `(var + k) >= bound || expr`, the right
/// operand only runs when `(var + k) < bound`, refuting every dominated check
/// `var + j >= bound` (`0 <= j <= k`) nested in it. Skipped when the right
/// operand modifies `var` / `bound` (the fact would no longer hold).
struct ShortCircuitEliminator<'a> {
    binds: &'a Binds,
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
            let mut changed = left.as_expr().is_some_and(|le| self.visit_expr(engine, le));
            if let Some((var, k, bound)) = parse_check(engine, self.binds, left)
                && let Some(re) = right.as_expr()
                && !node_modifies(engine, NodeRef::Expr(re), var, bound)
            {
                changed |=
                    eliminate_checks_in_node(engine, NodeRef::Expr(re), var, k, bound, self.binds);
            }
            if let Some(re) = right.as_expr() {
                changed |= self.visit_expr(engine, re);
            }
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
