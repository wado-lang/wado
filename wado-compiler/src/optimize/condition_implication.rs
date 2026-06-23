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

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, NodeRef, Operand, PatId, StmtId, StmtKind,
};
use crate::nir_engine::Engine;
use crate::nir_value_graph::{HeapVersion, ValueId, ValueKind};

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

/// Whether `(lhs >= rhs)` is provably false because `lhs` is bitmask-bounded:
/// `(x & MASK) >= BOUND` is false when `MASK >= 0` and `BOUND > MASK`. The
/// mask may appear on either side of the `&`; copy chains are already
/// resolved by the `ValueGraph`.
fn is_bitmask_bounded(engine: &mut Engine, condition: Operand) -> bool {
    let Some((lhs_vn, rhs_vn)) = failure_ge_operands(engine, condition) else {
        return false;
    };
    let ValueKind::Binary {
        op: NirBinaryOp::BitAnd,
        lhs,
        rhs,
        ..
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

/// Run condition implication at the body root on an existing engine session.
/// The combined `licm` session reuses its (value-preserving) `ValueGraph`, so
/// cond-impl needs no separate build; it runs after licm in document order, so
/// it still sees the hoisted body.
pub(super) fn eliminate_at_root(engine: &mut Engine) -> bool {
    reseed_invariant_fields(engine);
    reseed_loop_stable_operands(engine);
    let root = engine.body.root;
    process_block(engine, root)
}

/// Re-seed every read of a function-invariant field (`recv.field` where the
/// receiver is never `&mut`-escaped, reassigned, or its field assigned) — both
/// direct `recv.field` accesses and `let L = recv.field` snapshot copies — to a
/// single identity. Unlike the loop-stable re-seed this covers straight-line
/// early-exit / short-circuit guards (`if pos >= arr.len() { return }; arr[pos]`),
/// whose guard bound is a *direct* field access, not a copy.
///
/// Sound only for an invariant field: re-seeding a direct field access to a
/// version-free identity would otherwise defeat the write detection
/// `array_bounds_elim_oob_bound_shrunk` pins (a `pop()` between guard and check).
/// The `mut_escaped` gate excludes exactly such a receiver (`pop()` takes
/// `&mut self`). `WADO_VERIFY_VG` + the oob suite are the oracle.
fn reseed_invariant_fields(engine: &mut Engine) {
    if engine.body.value_graph.is_none() {
        return;
    }
    // Only straight-line functions: a loop's induction-variable / hoisted-bound
    // identities are the loop-scoped re-seed's job, and a function-wide leaf /
    // derived re-seed here would clash with them (e.g. re-seed a `let n =
    // arr.len()` bound to a fresh `canonical_local` and break the loop guard).
    if engine
        .body
        .stmts
        .iter()
        .any(|(_, s)| matches!(s.kind, StmtKind::Loop { .. }))
    {
        return;
    }
    let mut_escaped = engine.mut_escaped().clone();
    // Reassigned receivers and directly-assigned `(receiver, field)` slots.
    let mut reassigned: IndexSet<u32> = IndexSet::default();
    let mut assigned_fields: IndexSet<(u32, u32)> = IndexSet::default();
    for (_, enode) in &engine.body.exprs {
        if let ExprKind::Assign { target, .. } = &enode.kind {
            match &engine.body.exprs[*target].kind {
                ExprKind::Local { index, .. } => {
                    reassigned.insert(*index);
                }
                ExprKind::FieldAccess {
                    expr: inner,
                    field_index,
                    ..
                } => {
                    if let Some(ie) = inner.as_expr()
                        && let ExprKind::Local { index, .. } = engine.body.exprs[ie].kind
                    {
                        assigned_fields.insert((index, *field_index));
                    }
                }
                _ => {}
            }
        }
    }
    let invariant = |recv: &RecvKey, field: u32| match recv {
        RecvKey::Global(_) => true,
        RecvKey::Local(idx) => {
            !mut_escaped.contains(idx)
                && !reassigned.contains(idx)
                && !assigned_fields.contains(&(*idx, field))
        }
    };
    // Group every invariant field-access read by `(receiver, field)`.
    let maps = build_reseed_maps(engine.body);
    let field_copies = &maps.field_copies;
    // Function-stable leaf locals (a param / single-assignment leaf the guard
    // variable is): never reassigned, `&mut`-escaped, or a field/derived/plain
    // copy. Every read holds one value, so re-seed dropped reads to a single
    // `canonical_local` — the non-loop counterpart of the loop induction var.
    let address_taken = engine.body_address_taken().clone();
    for (read, local) in collect_local_reads(engine.body, engine.body.root) {
        if reassigned.contains(&local)
            || mut_escaped.contains(&local)
            || address_taken.contains(&local)
            || field_copies.contains_key(&local)
            || maps.copy_lets.contains_key(&local)
            || maps.derived_lets.iter().any(|(l, _)| *l == local)
            || has_value(engine, read)
        {
            continue;
        }
        let ty = engine.body.exprs[read].type_id;
        let v = engine
            .body
            .values
            .existing_local_opaque(local)
            .unwrap_or_else(|| engine.body.values.canonical_local(local, ty));
        engine.set_value(read, v);
    }
    let mut groups: IndexMap<(RecvKey, u32), (crate::tir::TypeId, Vec<ExprId>)> =
        IndexMap::default();
    for (e, enode) in &engine.body.exprs {
        if let ExprKind::FieldAccess {
            expr: inner,
            field_index,
            ..
        } = &enode.kind
            && let Some(ie) = inner.as_expr()
        {
            let recv = match &engine.body.exprs[ie].kind {
                ExprKind::Local { index, .. } => Some(RecvKey::Local(*index)),
                ExprKind::GlobalVarGet {
                    module_source,
                    name,
                } => Some(RecvKey::Global(format!("{module_source:?}\u{1}{name}"))),
                _ => None,
            };
            if let Some(recv) = recv
                && invariant(&recv, *field_index)
            {
                let recv_ty = engine.body.exprs[ie].type_id;
                groups
                    .entry((recv, *field_index))
                    .or_insert((recv_ty, Vec::new()))
                    .1
                    .push(e);
            }
        }
    }
    for (read, local) in collect_local_reads(engine.body, engine.body.root) {
        if let Some((recv, field, recv_ty)) = field_copies.get(&local)
            && invariant(recv, *field)
        {
            groups
                .entry((recv.clone(), *field))
                .or_insert((*recv_ty, Vec::new()))
                .1
                .push(read);
        }
    }
    // Re-seed every dropped read in a group to one value: a surviving read's
    // value if any, else the synthesized canonical field identity.
    for ((recv, field), (recv_ty, exprs)) in groups {
        let surviving = exprs.iter().find_map(|&e| {
            engine
                .body
                .value_graph
                .as_ref()
                .and_then(|g| g.value_of.get(&e).copied())
        });
        let value = surviving.unwrap_or_else(|| {
            let recv_val = match &recv {
                RecvKey::Local(src) => engine.body.values.canonical_local(*src, recv_ty),
                RecvKey::Global(key) => engine.body.values.canonical_global(key, recv_ty),
            };
            engine
                .body
                .values
                .field_access(recv_val, field, HeapVersion::INITIAL)
        });
        for e in exprs {
            if !has_value(engine, e) {
                let ty = engine.body.exprs[e].type_id;
                engine.body.values.set_type(value, ty);
                engine.set_value(e, value);
            }
        }
    }
    // Derived `let L = <pure expr>` (function-wide), in definition order so a
    // copy reading an earlier one resolves: re-seed L's reads — and copies of L —
    // to the binding's value, now that the field / leaf operands are seeded.
    let reads = collect_local_reads(engine.body, engine.body.root);
    for &(local, binding) in &maps.derived_lets {
        if reassigned.contains(&local) {
            continue;
        }
        let Some(v) = engine.value(binding) else {
            continue;
        };
        for &(read, l) in &reads {
            if reseed_root(l, &maps.copy_lets) == local && !has_value(engine, read) {
                engine.set_value(read, v);
            }
        }
    }
}

/// Loop-stable operand re-seed.
///
/// The guard / check matching reads the live (build-once, maintained) value
/// graph. Across `licm`'s structural edits the maintenance drops the leaf value
/// of every read of a reassigned local (`drop_local_readers`): the induction
/// variable (`i = i + 1`) and the hoisted bound (`let _licm_used = arr.used`)
/// both go value-less, so no comparison resolves. A fresh build would value
/// them — but rebuilding violates build-once.
///
/// Restore them incrementally: a local whose value is constant across the reads
/// being matched (a *loop-stable* local — never reassigned in the body except as
/// the final induction update, never `&mut`-escaped) holds one value at every
/// such read. Re-seed each dropped read of such a local with a single stable
/// identity (`canonical_local`), so the guard's and the check's copies share a
/// `ValueId`. A field copy (`let L = src.field`) is re-seeded to the source
/// field's identity instead, so two copies of the same `src.field` hoisted in
/// different fixed-point iterations still match.
///
/// Sound by construction: a re-seed only adds an equality the reads already
/// satisfy (refines a fresh build — `WADO_VERIFY_VG` is the oracle); a
/// reassigned / `&mut`-escaped local is excluded, so a guard variable or bound
/// the program actually mutates between guard and check is never merged.
fn reseed_loop_stable_operands(engine: &mut Engine) {
    // Unbuilt graph: the upcoming first query builds it fresh (already correct);
    // only a present, maintenance-degraded graph needs the re-seed.
    if engine.body.value_graph.is_none() {
        return;
    }
    let maps = build_reseed_maps(engine.body);
    // An existing value for a `recv.field` (from a copy whose read the
    // maintenance left intact): a dropped copy of the same `recv.field` re-seeds
    // to it, so a guard copy that survived and a check copy that was dropped
    // share one identity (the canonical synthetic value is only the fallback
    // when every copy was dropped).
    let recv_values = collect_recv_field_values(engine, &maps);
    reseed_scan_block(engine, engine.body.root, &maps, &recv_values);
}

fn collect_recv_field_values(
    engine: &Engine,
    maps: &ReseedMaps,
) -> IndexMap<(RecvKey, u32), ValueId> {
    let mut out: IndexMap<(RecvKey, u32), ValueId> = IndexMap::default();
    let Some(graph) = engine.body.value_graph.as_ref() else {
        return out;
    };
    for (e, enode) in &engine.body.exprs {
        if let ExprKind::Local { index, .. } = enode.kind
            && let Some((recv, field, _)) = maps.field_copies.get(&index)
            && let Some(&v) = graph.value_of.get(&e)
        {
            out.entry((recv.clone(), *field)).or_insert(v);
        }
    }
    out
}

/// The receiver of a field copy: the value whose `.field` a `let L = recv.field`
/// binds. Two copies of the same receiver + field share a re-seed identity.
#[derive(Clone, PartialEq, Eq, Hash)]
enum RecvKey {
    /// `let L = src.field` over a local `src`.
    Local(u32),
    /// `let L = global.field` over a `GlobalVarGet` (e.g. a const-object).
    Global(String),
}

/// The copy classification the re-seed needs, computed once per function.
struct ReseedMaps {
    /// `local → (receiver, field_index, receiver_type)` for `let L = recv.field`
    /// (a loop-invariant field copy, e.g. licm's `_licm_used = arr.used`); both
    /// copies of one `recv.field` re-seed to the same identity.
    field_copies: IndexMap<u32, (RecvKey, u32, crate::tir::TypeId)>,
    /// `(local, binding_expr)` for `let L = <pure Binary/Unary/Cast>` (e.g. the
    /// `let __cond = i < used` a bounds check binds), in definition order. Such
    /// a copy re-seeds to the value of its binding once the leaves it reads are
    /// seeded — so the comparison keeps its arithmetic shape, not a flat opaque.
    derived_lets: Vec<(u32, ExprId)>,
    /// `local → source_local` for a `let L = M` plain copy (both single-assignment).
    /// A copy of a hoisted bound (`let n_copy = _licm_used`) resolves to the
    /// source's classification, so the guard's bound and the check's copy of it
    /// share one identity. Followed transitively by [`reseed_root`].
    copy_lets: IndexMap<u32, u32>,
    /// Locals that are the bare-`Local` target of an `Assign` (`recv = other`)
    /// somewhere in the body — i.e. rebound after their `let`. A
    /// `construction_field_value` receiver in this set has a stale construction
    /// value. (A `&mut`-escape is *not* a rebind: element/field mutation through
    /// `&mut arr` leaves `arr.used` intact, so it stays eligible and is gated
    /// separately by `mut_escaped` for callee field-writes.)
    rebound: IndexSet<u32>,
}

fn build_reseed_maps(body: &Body) -> ReseedMaps {
    // Reassigned / `&mut`-escaped locals are excluded: a copy classification is
    // only valid for a single-assignment binding.
    let mut reassigned: IndexSet<u32> = IndexSet::default();
    // Rebinds only (`L = other`), the subset that staleness-checks a
    // construction value — kept apart from the `&mut`-escapes below.
    let mut rebound: IndexSet<u32> = IndexSet::default();
    for (_, enode) in &body.exprs {
        match &enode.kind {
            ExprKind::Assign { target, .. } => {
                if let ExprKind::Local { index, .. } = body.exprs[*target].kind {
                    reassigned.insert(index);
                    rebound.insert(index);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => {
                if let Some(ie) = expr.as_expr()
                    && let ExprKind::Local { index, .. } = body.exprs[ie].kind
                {
                    reassigned.insert(index);
                }
            }
            _ => {}
        }
    }
    let mut field_copies = IndexMap::default();
    let mut derived_lets = Vec::new();
    let mut copy_lets = IndexMap::default();
    for (_, node) in &body.stmts {
        let StmtKind::Let {
            local_index,
            value: Operand::Expr(v),
            ..
        } = &node.kind
        else {
            continue;
        };
        if reassigned.contains(local_index) {
            continue;
        }
        match &body.exprs[*v].kind {
            ExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                if let Some(ie) = inner.as_expr() {
                    let recv_ty = body.exprs[ie].type_id;
                    let recv = match &body.exprs[ie].kind {
                        ExprKind::Local { index, .. } => Some(RecvKey::Local(*index)),
                        ExprKind::GlobalVarGet {
                            module_source,
                            name,
                        } => Some(RecvKey::Global(format!("{module_source:?}\u{1}{name}"))),
                        _ => None,
                    };
                    if let Some(recv) = recv {
                        field_copies.insert(*local_index, (recv, *field_index, recv_ty));
                    }
                }
            }
            ExprKind::Binary { .. } | ExprKind::Unary { .. } | ExprKind::Cast { .. } => {
                derived_lets.push((*local_index, *v));
            }
            ExprKind::Local { index: src, .. } if !reassigned.contains(src) => {
                copy_lets.insert(*local_index, *src);
            }
            _ => {}
        }
    }
    ReseedMaps {
        field_copies,
        derived_lets,
        copy_lets,
        rebound,
    }
}

/// The value `recv.field` was constructed with: the operand of `field` in the
/// `let recv = … S { field: V … }` that defines `recv` (peering through `Block`
/// tails to the producing struct literal, like the builder's
/// `seed_struct_literal_fields`). Recovers the concrete value a hoisted
/// loop-invariant field copy lost — e.g. `List::filled(n)` constructs
/// `used: n`, so a `_licm_used = arr.used` check bound resolves to `n` and a
/// `i <= limit` / `arr.used == limit + 1` bounds check decomposes and folds.
///
/// Gated on `recv` being neither `mut_escaped` (a callee / `&mut` method like
/// `pop()` may rewrite the field) nor `rebound` (`recv = other` after the `let`
/// makes the construction value stale); either falls back to the opaque field
/// identity (no elimination).
fn construction_field_value(
    engine: &mut Engine,
    recv: u32,
    field: u32,
    rebound: &IndexSet<u32>,
) -> Option<ValueId> {
    if engine.mut_escaped().contains(&recv) || rebound.contains(&recv) {
        return None;
    }
    let def = engine.local_def(recv)?;
    let StmtKind::Let {
        value: Operand::Expr(v),
        ..
    } = &engine.body.stmts[def].kind
    else {
        return None;
    };
    let mut producer = *v;
    loop {
        match &engine.body.exprs[producer].kind {
            ExprKind::StructLiteral { .. } => break,
            ExprKind::Block(b) => {
                let last = *engine.body.blocks[*b].stmts.last()?;
                let StmtKind::Expr(Operand::Expr(tail)) = &engine.body.stmts[last].kind else {
                    return None;
                };
                producer = *tail;
            }
            _ => return None,
        }
    }
    let ExprKind::StructLiteral { fields, .. } = &engine.body.exprs[producer].kind else {
        return None;
    };
    let field_value = fields.iter().find(|f| f.field_index == field)?.value;
    engine.operand_value(field_value)
}

/// Follow a `let L = M` copy chain to its non-copy root (cycle-guarded).
fn reseed_root(local: u32, copy_lets: &IndexMap<u32, u32>) -> u32 {
    let mut cur = local;
    for _ in 0..=copy_lets.len() {
        match copy_lets.get(&cur) {
            Some(&src) => cur = src,
            None => break,
        }
    }
    cur
}

/// Walk the body, re-seeding each loop's stable operands. Recurses into nested
/// loops (an inner loop's stable set is computed against its own body).
fn reseed_scan_block(
    engine: &mut Engine,
    block: BlockId,
    maps: &ReseedMaps,
    recv_values: &IndexMap<(RecvKey, u32), ValueId>,
) {
    for s in engine.body.blocks[block].stmts.clone() {
        match &engine.body.stmts[s].kind {
            StmtKind::Loop { body: lb } => {
                let lb = *lb;
                reseed_loop(engine, lb, maps, recv_values);
                reseed_scan_block(engine, lb, maps, recv_values);
            }
            StmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                let (tb, eb) = (*then_block, *else_block);
                reseed_scan_block(engine, tb, maps, recv_values);
                if let Some(eb) = eb {
                    reseed_scan_block(engine, eb, maps, recv_values);
                }
            }
            StmtKind::LabeledBlock { block, .. } => {
                let b = *block;
                reseed_scan_block(engine, b, maps, recv_values);
            }
            _ => {}
        }
    }
}

fn has_value(engine: &Engine, read: ExprId) -> bool {
    engine
        .body
        .value_graph
        .as_ref()
        .is_some_and(|g| g.value_of.contains_key(&read))
}

fn reseed_loop(
    engine: &mut Engine,
    loop_body: BlockId,
    maps: &ReseedMaps,
    recv_values: &IndexMap<(RecvKey, u32), ValueId>,
) {
    let unstable = loop_unstable_locals(engine.body, loop_body);
    let reads = collect_local_reads(engine.body, loop_body);
    // Pass 1: leaf reads (induction variables, params) and field copies — the
    // values the maintenance cannot re-derive. A plain copy (`let n = _licm_used`)
    // resolves to its source's classification via the copy chain.
    for &(read, local) in &reads {
        if unstable.contains(&local) || has_value(engine, read) {
            continue;
        }
        let root = reseed_root(local, &maps.copy_lets);
        if maps.derived_lets.iter().any(|(l, _)| *l == root) {
            continue; // pass 2
        }
        let ty = engine.body.exprs[read].type_id;
        let value = match maps.field_copies.get(&root) {
            Some((recv, field, recv_ty)) => {
                // Prefer the value a surviving copy of this `recv.field` already
                // holds; only synthesize the canonical field value when every
                // copy was dropped.
                if let Some(&v) = recv_values.get(&(recv.clone(), *field)) {
                    v
                } else if let RecvKey::Local(src) = recv
                    && let Some(cv) = construction_field_value(engine, *src, *field, &maps.rebound)
                {
                    // The field's construction value (`filled(n)` → `used: n`):
                    // a concrete value the bounds check can decompose, where the
                    // opaque field identity below only proves consistency.
                    cv
                } else {
                    let recv_val = match recv {
                        RecvKey::Local(src) => engine.body.values.canonical_local(*src, *recv_ty),
                        RecvKey::Global(key) => engine.body.values.canonical_global(key, *recv_ty),
                    };
                    let fv =
                        engine
                            .body
                            .values
                            .field_access(recv_val, *field, HeapVersion::INITIAL);
                    engine.body.values.set_type(fv, ty);
                    fv
                }
            }
            None => engine
                .body
                .values
                .existing_local_opaque(root)
                .unwrap_or_else(|| engine.body.values.canonical_local(root, ty)),
        };
        engine.set_value(read, value);
    }
    // Pass 2: derived `let L = <pure expr>` copies, in definition order, so a
    // copy reading an earlier copy resolves. The binding's value is recomputed
    // from the now-seeded leaves, then applied to the derived local and any plain
    // copy of it.
    for &(local, binding) in &maps.derived_lets {
        if unstable.contains(&local) {
            continue;
        }
        let Some(v) = engine.value(binding) else {
            continue;
        };
        for &(read, l) in &reads {
            if reseed_root(l, &maps.copy_lets) == local && !has_value(engine, read) {
                engine.set_value(read, v);
            }
        }
    }
}

/// Locals not loop-stable: reassigned or `&mut`-escaped anywhere in the body,
/// except the induction update (the final top-level statement's direct
/// `Local = …` assign target — every read precedes its effect, so all reads see
/// the loop-top value).
fn loop_unstable_locals(body: &Body, loop_body: BlockId) -> IndexSet<u32> {
    let mut out = IndexSet::default();
    let stmts = &body.blocks[loop_body].stmts;
    for (i, &s) in stmts.iter().enumerate() {
        let is_final = i + 1 == stmts.len();
        if is_final
            && let StmtKind::Expr(op) = &body.stmts[s].kind
            && let Some(e) = op.as_expr()
            && let ExprKind::Assign { target, value } = &body.exprs[e].kind
            && matches!(body.exprs[*target].kind, ExprKind::Local { .. })
        {
            // Induction update: skip the direct target, scan its value for any
            // other mutation.
            if let Some(ve) = value.as_expr() {
                collect_mutated_node(body, NodeRef::Expr(ve), &mut out);
            }
            continue;
        }
        collect_mutated_node(body, NodeRef::Stmt(s), &mut out);
    }
    out
}

/// Collect every local that node (recursively) reassigns (`Assign` target) or
/// `&mut`-escapes (a `MutRef` of a `Local`). Descends into nested loops too — a
/// nested mutation of `L` still makes `L` unstable in the outer loop.
fn collect_mutated_node(body: &Body, node: NodeRef, out: &mut IndexSet<u32>) {
    if let NodeRef::Expr(e) = node {
        match &body.exprs[e].kind {
            ExprKind::Assign { target, .. } => {
                if let ExprKind::Local { index, .. } = body.exprs[*target].kind {
                    out.insert(index);
                }
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr,
            } => {
                if let Some(ie) = expr.as_expr()
                    && let ExprKind::Local { index, .. } = body.exprs[ie].kind
                {
                    out.insert(index);
                }
            }
            _ => {}
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_mutated_node(body, c, out);
    }
}

/// Every `(read_expr, local_index)` of a bare `Local` read reachable from
/// `block` (including nested loops/blocks).
fn collect_local_reads(body: &Body, block: BlockId) -> Vec<(ExprId, u32)> {
    let mut out = Vec::new();
    collect_local_reads_node(body, NodeRef::Block(block), &mut out);
    out
}

fn collect_local_reads_node(body: &Body, node: NodeRef, out: &mut Vec<(ExprId, u32)>) {
    if let NodeRef::Expr(e) = node
        && let ExprKind::Local { index, .. } = body.exprs[e].kind
    {
        out.push((e, index));
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        collect_local_reads_node(body, c, out);
    }
}

fn process_block(engine: &mut Engine, block: BlockId) -> bool {
    let mut changed = false;
    // Guard facts from earlier early-exit statements. No staleness
    // tracking: a mutation between guard and check changes the check
    // operand's `ValueId`, so the fact stops matching by itself.
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

    // condition must be `Not(<comparison>)`; the comparison may sit behind a
    // CSE temporary.
    let ExprKind::Unary {
        op: NirUnaryOp::Not,
        expr: inner,
    } = &engine.body.exprs[condition.as_expr()?].kind
    else {
        return None;
    };
    let inner = *inner;
    let (lhs_vn, rhs_vn, is_strict) = lt_comparison(engine, inner.as_expr()?)?;
    // Loop guards keep the plain-variable shape: the induction variable is
    // compared directly (`i < bound`), so any `Add` decomposition would
    // describe a different program object. Restrict to offset 0.
    let fact = GuardFact::from_values(engine, lhs_vn, rhs_vn, is_strict);
    (fact.max_offset == 0).then_some((fact, guard_idx + 1))
}

/// The `(lhs, rhs, is_strict)` of the `<` / `<=` comparison `expr` resolves to,
/// directly or through a `__cse` / `__cond` temporary the value graph resolves.
fn lt_comparison(engine: &mut Engine, expr: ExprId) -> Option<(ValueId, ValueId, bool)> {
    let value = engine.value(expr)?;
    lt_of_value(engine, value)
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
    } = &engine.body.exprs[condition.as_expr()?].kind
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
    fact_eliminates_panic(engine, condition, then_block, else_block, NodeRef::Stmt(s), fact)
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
            if self.try_eliminate_panic(engine, condition, then_branch, else_branch, NodeRef::Expr(e))
            {
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
            eliminate_condition(engine, NodeRef::Stmt(s), condition);
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
            let mut changed = left.as_expr().is_some_and(|le| self.visit_expr(engine, le));
            let fact = if let Some(le) = left.as_expr()
                && let ExprKind::Binary {
                    left: cmp_l,
                    op: NirBinaryOp::GtEq,
                    right: cmp_r,
                } = &engine.body.exprs[le].kind
            {
                let (cmp_l, cmp_r) = (*cmp_l, *cmp_r);
                GuardFact::from_comparison(engine, cmp_l, cmp_r, true)
            } else {
                None
            };
            if let Some(fact) = fact
                && let Some(re) = right.as_expr()
            {
                changed |= GuardEliminator { fact }.visit_expr(engine, re);
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
