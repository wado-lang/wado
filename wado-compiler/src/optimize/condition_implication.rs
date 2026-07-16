//! Condition Implication — eliminates bounds checks implied false by guards.
//!
//! When a loop guard proves `i < bound`, any inner check `i >= bound` is known
//! false and is replaced with `false`. The existing `const_branch_prune` pass
//! then removes the dead branch on the next iteration.
//!
//! Recognised patterns (all **structural** and `value_of`-free):
//!
//! - loop guard `if !(var < bound) { break }` → checks in the body
//!   (`structural_loop_guard`);
//! - dominating `if (var + k) < bound { … }` → checks in the then-block
//!   (`apply_dominating_if`);
//! - straight-line early-exit `if (var + k) >= bound { return/break }` → checks
//!   in the statements after it (`recognize_early_exit`);
//! - short-circuit `(var + k) >= bound || expr` → checks in `expr`
//!   (`ShortCircuitEliminator`);
//! - bitmask `(x & MASK) >= BOUND`, `BOUND > MASK >= 0` (`BitmaskEliminator`);
//! - redundant re-check: the implicit panic-guard `arr[i]` lowers to itself
//!   proves `i < arr.used`, so a later `arr[i]` (same index, array unmodified
//!   between) re-check is dropped — a forward walk in execution order harvesting
//!   guard facts and refuting dominated re-checks (`rbce_walk`), which needs no
//!   source-level guard, only a first access. This is what removes the doubled
//!   `arr[i]`/`arr[j]` checks in an `if arr[i] > arr[j] { arr[j] = arr[i] }`.
//!
//! Matching is **syntactic over the skeleton plus the value pool**, never the
//! `value_of` side-table: a guard's `var` / `bound` are compared by structure
//! ([`BoundKey`], copy temps resolved through [`Binds`]); a promoted operand
//! (`Operand::Value`) is decomposed through `body.values` (the IR's value pool).
//! Flow-correctness comes from a position-aware "no statement modified
//! `var` / `bound` between the guard and the check" scan ([`stmt_modifies`] /
//! [`node_modifies`]) rather than from `ValueId` identity. See
//! `array_bounds_elim_oob_guard_var_mutated.wado` /
//! `array_bounds_elim_oob_bound_shrunk.wado` for the fixtures pinning this.
//!
//! Runs via [`eliminate_at_root`], sharing licm's engine session (see
//! `licm.rs`). The single rewrite point promotes a condition to constant
//! `false` (`set_false`), replacing already-judged condition nodes only.

use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{BlockId, ExprId, ExprKind, NodeRef, Operand, PatId, StmtId, StmtKind};
use crate::nir_engine::Engine;
use crate::nir_value_graph::ValueKind;

/// Run condition implication at the body root on an existing engine session.
/// The combined `licm` session reuses its (value-preserving) `ValueGraph`, so
/// cond-impl needs no separate build; it runs after licm in document order, so
/// it still sees the hoisted body.
pub(super) fn eliminate_at_root(engine: &mut Engine) -> bool {
    let root = engine.body.root;
    // Built once and threaded down: sound because eliminations never add
    // reassignments, so the snapshot only omits bindings, never holds a stale one.
    let binds = build_copy_bindings(engine.body);
    let mut changed = process_block(engine, root, &binds);
    let mut facts: Vec<ProvenLt> = Vec::new();
    changed |= rbce_walk(engine, NodeRef::Block(root), &mut facts, &binds);
    changed
}

/// The [`FuncId`](crate::nir::FuncId)s of the diverging panic / `unreachable`
/// builtins, recognized by name on the function record. Resolved once per pass
/// run so the panic-block matcher identifies a panic callee by id. The driver
/// hands the result to the engine via [`Engine::set_panic_callee_ids`].
pub(super) fn resolve_panic_ids(
    project: &crate::nir_package::NirPackage,
) -> crate::hashmap::IndexSet<crate::nir::FuncId> {
    project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            (f.name.contains("panic") || f.name == "unreachable")
                .then_some(f.id)
                .flatten()
        })
        .collect()
}

/// Standalone post-`promote_fields` run of the structural BCE matcher.
///
/// `promote_fields` (born-as-operands) freezes invariant field reads — an array
/// bound `arr.used` over a stable receiver — into `Operand::Value`, but it runs
/// *after* the optimization loop, so the in-loop cond-impl never sees the
/// promoted bound. This re-runs the matcher on the post-promotion body, where
/// `(idx & MASK) < BOUND`'s bound is now a constant operand the structural
/// recogniser reads through the value **pool** (no `value_of`). The caller pairs
/// it with `const_branch_prune` to fixpoint so the now-`false` checks' panic
/// blocks are removed.
pub(super) fn eliminate_post_promote(project: &mut crate::nir_package::NirPackage) -> bool {
    use crate::nir::NirFunction;
    use crate::nir_engine::EngineBuffers;
    let type_table = project.type_table.borrow();
    let first_param_types = super::alias::first_param_types(project);
    let call_immutability = super::alias::CallImmutability::new(project, &type_table);
    let panic_ids = resolve_panic_ids(project);
    let pure_builtin_callees = project.pure_builtin_callee_ids();
    let mut buffers = EngineBuffers::default();
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if func.body.is_none() {
            continue;
        }
        let NirFunction {
            body,
            locals,
            params,
            address_taken_locals,
            stores_aliased_locals,
            ..
        } = &mut *func;
        let body = body.as_mut().expect("checked above");
        let (aliased, untrackable, mut_escaped) = super::alias::builder_alias_sets(
            body,
            locals,
            address_taken_locals,
            stores_aliased_locals,
            &type_table,
            &first_param_types,
            &call_immutability,
        );
        let param_locals: Vec<u32> = params.iter().map(|p| p.local_index).collect();
        let mut engine = Engine::new(body, &mut buffers, locals);
        engine.set_alias_sets(aliased, untrackable, mut_escaped);
        engine.set_value_graph_type_table(&type_table);
        engine.set_param_locals(param_locals);
        engine.set_panic_callee_ids(&panic_ids);
        engine.set_pure_builtin_callees(&pure_builtin_callees);
        changed |= eliminate_at_root(&mut engine);
    }
    changed
}

/// A structural bound: the right-hand side of a guard / check comparison,
/// compared **syntactically** (no value graph). Two `BoundKey`s are equal iff
/// they denote the same program object by structure.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BoundKey {
    Local(u32),
    /// `root_local . field_index` — a field read of a by-value local.
    Field(u32, u32),
}

/// Copy/CSE temp bindings: a single-assignment local `t` bound by
/// `let t = <op>` maps to `<op>`. Lets the structural matcher see through the
/// `let __cond = i < n; if !__cond { panic }` shape CSE produces.
pub(super) type Binds = crate::hashmap::IndexMap<u32, Operand>;

/// Build [`Binds`] over `body`: every `let t = <value>` whose `t` is never
/// reassigned (`Assign` / `&mut`). Conservative — a reassigned temp is excluded,
/// so resolving through it can never read a stale value.
pub(super) fn build_copy_bindings(body: &crate::nir_arena::Body) -> Binds {
    let mut reassigned = crate::hashmap::IndexSet::default();
    let mut record = |node: NodeRef| {
        if let NodeRef::Expr(e) = node {
            match &body.exprs[e].kind {
                ExprKind::Assign { target, .. } => {
                    if let Some(r) = super::arena_query::storage_root(body, *target) {
                        reassigned.insert(r);
                    }
                }
                ExprKind::Unary {
                    op: NirUnaryOp::MutRef,
                    expr,
                } => {
                    if let Some(ie) = expr.as_expr()
                        && let Some(r) = super::arena_query::storage_root(body, ie)
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
pub(super) fn resolve(engine: &Engine, binds: &Binds, op: Operand) -> Operand {
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
pub(super) fn opaque_local(engine: &Engine, v: crate::nir_value_graph::ValueId) -> Option<u32> {
    if let ValueKind::Opaque(o) = engine.body.values.kind(v)
        && let Some(crate::nir_value_graph::OpaqueSource::Local(i)) =
            engine.body.values.opaque_source(*o)
    {
        Some(i)
    } else {
        None
    }
}

/// The root a [`BoundKey::Field`] keys on. Path-sensitive on purpose: since the
/// key is `(root, field_index)`, the walk must not collapse a variant-payload
/// projection (whose field 0 is not the scrutinee's field 0), so it does not use
/// `arena_query::storage_root`.
fn field_bound_root(body: &crate::nir_arena::Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::Unary { expr: inner, .. }
        | ExprKind::Cast { expr: inner, .. }
        | ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::Index { expr: inner, .. } => field_bound_root(body, inner.as_expr()?),
        _ => None,
    }
}

/// Parse an operand (through copy temps) as a structural bound: a bare local or
/// a `local.field` read over a by-value local root. Handles both the skeleton
/// form and a **promoted** `Operand::Value` (the freeze promotes a `FieldAccess`
/// bound), decomposed through the value **pool** (`body.values`, not `value_of`).
pub(super) fn parse_bound(engine: &Engine, binds: &Binds, op: Operand) -> Option<BoundKey> {
    match resolve(engine, binds, op) {
        Operand::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::Local { index, .. } => Some(BoundKey::Local(*index)),
            ExprKind::FieldAccess { field_index, .. } => {
                let root = field_bound_root(engine.body, e)?;
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
///
/// A bare `Local(idx)` is a variable in its own right: its bind is followed only
/// when that exposes a cleaner var+offset (`let q = p + 2`), otherwise the local
/// itself is the var. So `let parent = <opaque computation>` (a single-assignment
/// local bound to a whole guarded-index block, not a copy) still matches by its
/// stable local index — matching a check `parent >= bound` against a guard on the
/// same `parent` — instead of dead-ending in the block resolution.
pub(super) fn parse_var_offset(engine: &Engine, binds: &Binds, op: Operand) -> Option<(u32, i64)> {
    if let Operand::Expr(e) = op
        && let ExprKind::Local { index, .. } = &engine.body.exprs[e].kind
    {
        let idx = *index;
        if let Some(&b) = binds.get(&idx)
            && let Some(vo) = parse_var_offset(engine, binds, b)
        {
            return Some(vo);
        }
        return Some((idx, 0));
    }
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

/// The tail value operand of a block expression: `{ …; tail }` (the last
/// statement is `Expr(tail)`) or a labeled block whose last statement is
/// `break label tail`.
fn block_tail_operand(body: &crate::nir_arena::Body, e: ExprId) -> Option<Operand> {
    match &body.exprs[e].kind {
        ExprKind::Block(block) => block_id_tail(body, *block),
        ExprKind::LabeledBlock { label, block, .. } => {
            let last = *body.blocks[*block].stmts.last()?;
            let StmtKind::Break {
                label: Some(bl),
                value: Some(v),
            } = &body.stmts[last].kind
            else {
                return None;
            };
            (bl == label).then_some(*v)
        }
        _ => None,
    }
}

/// The initialiser operand of `recv.field`, when `recv` resolves (through copy
/// temps and block tails) to a struct literal. Sound because `recv` reaching a
/// `let` binding via [`Binds`] means it is never reassigned or `&mut`-escaped
/// (Wado value semantics), so the field value is fixed at construction.
fn struct_field_init(
    engine: &Engine,
    binds: &Binds,
    recv: Operand,
    field_name: &str,
) -> Option<Operand> {
    let mut cur = resolve(engine, binds, recv);
    for _ in 0..8 {
        let Operand::Expr(e) = cur else { break };
        if matches!(&engine.body.exprs[e].kind, ExprKind::StructLiteral { .. }) {
            break;
        }
        let tail = block_tail_operand(engine.body, e)?;
        cur = resolve(engine, binds, tail);
    }
    let Operand::Expr(e) = cur else {
        return None;
    };
    let ExprKind::StructLiteral { fields, .. } = &engine.body.exprs[e].kind else {
        return None;
    };
    fields
        .iter()
        .find(|f| f.name == field_name)
        .map(|f| f.value)
}

/// `c >= 0` such that `bound`'s value equals `guard_var + c`: either `bound` is
/// `guard_var + c` directly, or it is an invariant struct-field read
/// (`arr.used` over `List { used: guard_var + c }`) projected via
/// [`struct_field_init`]. Lets a `<=` guard relate its bound to a check bound.
fn bound_offset_over(
    engine: &Engine,
    binds: &Binds,
    bound: Operand,
    guard_var: u32,
) -> Option<i64> {
    if let Some((v, c)) = parse_var_offset(engine, binds, bound)
        && v == guard_var
    {
        return Some(c);
    }
    let Operand::Expr(e) = resolve(engine, binds, bound) else {
        return None;
    };
    let ExprKind::FieldAccess {
        expr: recv,
        field_name,
        ..
    } = &engine.body.exprs[e].kind
    else {
        return None;
    };
    let (recv, field_name) = (*recv, field_name.clone());
    let init = struct_field_init(engine, binds, recv, &field_name)?;
    match parse_var_offset(engine, binds, init) {
        Some((v, c)) if v == guard_var => Some(c),
        _ => None,
    }
}

/// Parse a condition (through copy temps) as `(var + off) OP bound`. Returns
/// `(var_local, off, bound, op)`. Handles both the skeleton form and a
/// **promoted** `Operand::Value` comparison (decomposed through the value pool),
/// so a post-`promote_fields` guard/check condition parses the same as a
/// skeleton one.
pub(super) fn parse_cmp(
    engine: &Engine,
    binds: &Binds,
    cond: Operand,
) -> Option<(u32, i64, BoundKey, NirBinaryOp)> {
    let is_cmp = |op: NirBinaryOp| {
        matches!(
            op,
            NirBinaryOp::Lt | NirBinaryOp::LtEq | NirBinaryOp::Gt | NirBinaryOp::GtEq
        )
    };
    match resolve(engine, binds, cond) {
        Operand::Expr(ce) => {
            if let ExprKind::Binary { left, op, right } = &engine.body.exprs[ce].kind
                && is_cmp(*op)
            {
                let op = *op;
                let (var, off) = parse_var_offset(engine, binds, *left)?;
                let bound = parse_bound(engine, binds, *right)?;
                return Some((var, off, bound, op));
            }
            None
        }
        Operand::Value(v) => {
            if let ValueKind::Binary { op, lhs, rhs, .. } = engine.body.values.kind(v)
                && is_cmp(*op)
            {
                let (op, lhs, rhs) = (*op, *lhs, *rhs);
                let (var, off) = parse_var_offset(engine, binds, Operand::Value(lhs))?;
                let bound = parse_bound(engine, binds, Operand::Value(rhs))?;
                return Some((var, off, bound, op));
            }
            None
        }
    }
}

/// The loop-guard head both structural BCE and loop versioning recognise:
/// after any leading `let`s, the first statement is `if !<cond> { break }` with
/// a single-statement break then-block. Returns the guard's statement index and
/// the (still-negated) condition operand. One shared definition keeps the two
/// passes from drifting on what a loop guard looks like.
pub(super) fn parse_break_guard_head(
    engine: &Engine,
    loop_body: BlockId,
) -> Option<(usize, Operand)> {
    let stmts = &engine.body.blocks[loop_body].stmts;
    let guard_idx = stmts.iter().position(|s| {
        !matches!(
            engine.body.stmts[*s].kind,
            StmtKind::Let { .. } | StmtKind::LetDestructure { .. }
        )
    })?;
    let StmtKind::If {
        condition,
        then_block,
        else_block: None,
    } = &engine.body.stmts[stmts[guard_idx]].kind
    else {
        return None;
    };
    let (cond, gthen) = (*condition, *then_block);
    if engine.body.blocks[gthen].stmts.len() != 1
        || !matches!(
            engine.body.stmts[engine.body.blocks[gthen].stmts[0]].kind,
            StmtKind::Break { .. }
        )
    {
        return None;
    }
    Some((guard_idx, cond))
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
pub(super) fn ge_check_operands(
    engine: &Engine,
    binds: &Binds,
    cond: Operand,
) -> Option<(Operand, Operand)> {
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
pub(super) fn stmt_modifies(engine: &Engine, s: StmtId, var: u32, bound: BoundKey) -> bool {
    node_modifies(engine, NodeRef::Stmt(s), var, bound)
}

/// [`stmt_modifies`] over an arbitrary node subtree (e.g. the right operand of a
/// short-circuit `||`).
pub(super) fn node_modifies(engine: &Engine, node: NodeRef, var: u32, bound: BoundKey) -> bool {
    let roots = [Some(var), bound_root(bound)];
    let is_root = |l: u32| roots.contains(&Some(l));
    let mut hit = false;
    let mut visit = |node: NodeRef| {
        if let NodeRef::Expr(e) = node {
            match &engine.body.exprs[e].kind {
                ExprKind::Assign { target, .. } => {
                    if let Some(root) = super::arena_query::storage_root(engine.body, *target)
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
                        && let Some(root) = super::arena_query::storage_root(engine.body, ie)
                        && is_root(root)
                    {
                        hit = true;
                    }
                }
                ExprKind::MethodCall { receiver, .. } => {
                    if let Some(re) = receiver.as_expr()
                        && let Some(root) = super::arena_query::storage_root(engine.body, re)
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
fn structural_loop_guard(engine: &mut Engine, loop_body: BlockId, binds: &Binds) -> bool {
    let Some((guard_idx, gcond)) = parse_break_guard_head(engine, loop_body) else {
        return false;
    };
    let stmts = engine.body.blocks[loop_body].stmts.clone();
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
    let Some((var, goff, bound, gop)) = parse_cmp(engine, binds, *inner) else {
        return false;
    };
    // Walk body statements after the guard, in order. While no statement has
    // modified `var` / `bound` since the guard, eliminate the dominated checks
    // nested anywhere in the statement (the inlined `index_value` check is a
    // `StmtKind::If` / `ExprKind::If` inside the statement's expression block,
    // not a top-level statement). A statement that modifies `var` / `bound`
    // (e.g. the `i += 1` induction update) stops the scan — the guard fact no
    // longer holds past it.
    //
    // - `<` guard: surviving `var + goff < bound`; a check `var + j >= bound` is
    //   refuted for `j <= goff` (same bound, [`eliminate_checks_in_node`]).
    // - `<=` guard (`goff == 0`, `bound` a local `gbl`): surviving `var <= gbl`,
    //   i.e. `var < gbl + 1`; a check `var + j >= B` is refuted when `B` relates
    //   to `gbl + c` with `c >= j + 1` ([`eliminate_le_checks_in_node`]). This is
    //   the `arr.used == limit + 1` case.
    // `<=` guards require a local bound; `<` accepts any. Reject other ops.
    let le_gbl = match gop {
        NirBinaryOp::Lt => None,
        NirBinaryOp::LtEq if goff == 0 => match bound {
            BoundKey::Local(gbl) => Some(gbl),
            BoundKey::Field(..) => return false,
        },
        _ => return false,
    };
    let mut changed = false;
    for &s in stmts.iter().skip(guard_idx + 1) {
        if stmt_modifies(engine, s, var, bound) {
            break;
        }
        changed |= match le_gbl {
            None => eliminate_checks_in_node(engine, NodeRef::Stmt(s), var, goff, bound, binds),
            Some(gbl) => eliminate_le_checks_in_node(engine, NodeRef::Stmt(s), var, gbl, binds),
        };
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

/// `<=` loop-guard elimination (`var <= gbound`, surviving `var < gbound + 1`):
/// drive to `false` every dominated check `var + j >= B` whose bound `B` relates
/// to `gbound + c` with `c >= j + 1` ([`bound_offset_over`]), since then
/// `var + j <= gbound + j < gbound + c = B`. Recovers `arr.used == limit + 1`
/// where the guard is `i <= limit` (structural, value_of-free).
fn eliminate_le_checks_in_node(
    engine: &mut Engine,
    node: NodeRef,
    var: u32,
    gbound: u32,
    binds: &Binds,
) -> bool {
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
        if let Some((cond, then_b)) = cand
            && is_panic_block(engine, then_b)
            && let Some((left, right)) = ge_check_operands(engine, binds, cond)
            && let Some((cvar, cj)) = parse_var_offset(engine, binds, left)
            && cvar == var
            && let Some(c) = bound_offset_over(engine, binds, right, gbound)
            && c > cj
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
    for &ts in &engine.body.blocks[then_b].stmts.clone() {
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

fn process_block(engine: &mut Engine, block: BlockId, binds: &Binds) -> bool {
    let mut changed = false;
    // Structural early-exit facts `var < bound` (value_of-free): a fact is used
    // for a later statement's checks while no statement since the guard has
    // modified `var` / `bound` (`stmt_modifies`), then dropped when one does.
    let mut seguards: Vec<(u32, i64, BoundKey)> = Vec::new();
    let stmts = engine.body.blocks[block].stmts.clone();
    for s in stmts {
        for &(var, k, bound) in &seguards {
            if !stmt_modifies(engine, s, var, bound) {
                changed |= eliminate_checks_in_node(engine, NodeRef::Stmt(s), var, k, bound, binds);
            }
        }
        changed |= apply_dominating_if(engine, s, binds);
        changed |= BitmaskEliminator { binds }.visit_stmt(engine, s);
        changed |= ConstBoundIndexEliminator { binds }.visit_stmt(engine, s);
        changed |= ShortCircuitEliminator { binds }.visit_stmt(engine, s);
        changed |= process_stmt(engine, s, binds);
        seguards.retain(|&(var, _, bound)| !stmt_modifies(engine, s, var, bound));
        if let Some(fact) = recognize_early_exit(engine, s, binds) {
            seguards.push(fact);
        }
    }
    changed
}

fn process_stmt(engine: &mut Engine, s: StmtId, binds: &Binds) -> bool {
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
        StmtShape::Loop(lb) => process_loop(engine, lb, binds),
        StmtShape::If(then_b, else_b) => {
            let mut changed = process_block(engine, then_b, binds);
            if let Some(eb) = else_b {
                changed |= process_block(engine, eb, binds);
            }
            changed
        }
        StmtShape::Labeled(b) => process_block(engine, b, binds),
        StmtShape::None => false,
    }
}

enum StmtShape {
    Loop(BlockId),
    If(BlockId, Option<BlockId>),
    Labeled(BlockId),
    None,
}

fn process_loop(engine: &mut Engine, loop_body: BlockId, binds: &Binds) -> bool {
    let mut changed = structural_loop_guard(engine, loop_body, binds);

    // Dominating `if (var + K) < bound { … }` in the loop body proves the
    // dominated checks inside its then-block, and bitmask-bounded checks
    // `(x & MASK) >= BOUND` are refuted by the mask (value_of-free, structural).
    for s in engine.body.blocks[loop_body].stmts.clone() {
        changed |= apply_dominating_if(engine, s, binds);
        changed |= BitmaskEliminator { binds }.visit_stmt(engine, s);
    }

    // Recurse into nested loops.
    for s in engine.body.blocks[loop_body].stmts.clone() {
        changed |= process_stmt_nested_loops(engine, s, binds);
    }

    changed
}

/// Recurse into nested structures to find inner loops, but don't re-process
/// the current loop level.
fn process_stmt_nested_loops(engine: &mut Engine, s: StmtId, binds: &Binds) -> bool {
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
        StmtShape::Loop(lb) => process_loop(engine, lb, binds),
        StmtShape::If(then_b, else_b) => {
            let mut changed = false;
            for s in engine.body.blocks[then_b].stmts.clone() {
                changed |= process_stmt_nested_loops(engine, s, binds);
            }
            if let Some(eb) = else_b {
                for s in engine.body.blocks[eb].stmts.clone() {
                    changed |= process_stmt_nested_loops(engine, s, binds);
                }
            }
            changed
        }
        StmtShape::Labeled(b) => {
            let mut changed = false;
            for s in engine.body.blocks[b].stmts.clone() {
                changed |= process_stmt_nested_loops(engine, s, binds);
            }
            changed
        }
        StmtShape::None => false,
    }
}

fn block_always_exits(engine: &Engine, block: BlockId) -> bool {
    engine.body.blocks[block].stmts.iter().any(|s| {
        matches!(
            engine.body.stmts[*s].kind,
            StmtKind::Return { .. } | StmtKind::Break { .. }
        )
    })
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
pub(super) fn eliminate_condition(engine: &mut Engine, holder: NodeRef, condition: Operand) {
    match condition {
        Operand::Expr(ce) => set_false(engine, ce),
        Operand::Value(_) => force_condition_false(engine, holder),
    }
}

/// Check if a block traps (bounds check failure path): a `panic`, or the bare
/// `unreachable` that `-f bare-asserts` lowers an assertion failure into.
pub(super) fn is_panic_block(engine: &Engine, block: BlockId) -> bool {
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
        ExprKind::Call { func_id, .. } => engine.is_panic_callee(*func_id),
        _ => false,
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

fn index_upper_bound(engine: &Engine, binds: &Binds, op: Operand) -> Option<i64> {
    if let Some(c) = parse_const_i64(engine, binds, op) {
        return Some(c);
    }
    let Operand::Expr(e) = resolve(engine, binds, op) else {
        return None;
    };
    let ExprKind::If {
        condition,
        then_branch,
        else_branch: Some(else_branch),
    } = &engine.body.exprs[e].kind
    else {
        return None;
    };
    let (condition, then_branch, else_branch) = (*condition, *then_branch, *else_branch);
    let Operand::Expr(ce) = resolve(engine, binds, condition) else {
        return None;
    };
    let ExprKind::Binary {
        left,
        op: cmp,
        right,
    } = &engine.body.exprs[ce].kind
    else {
        return None;
    };
    let (left, cmp, right) = (*left, *cmp, *right);
    let k = parse_const_i64(engine, binds, right)?;
    let then_const = parse_const_i64(engine, binds, block_id_tail(engine.body, then_branch)?)?;
    let else_tail = block_id_tail(engine.body, else_branch)?;
    if !operand_same(engine, binds, left, else_tail) {
        return None;
    }
    if let Operand::Expr(le) = resolve(engine, binds, left)
        && let ExprKind::Local { index, .. } = &engine.body.exprs[le].kind
    {
        let idx = *index;
        if engine.body.blocks[else_branch]
            .stmts
            .iter()
            .any(|&s| stmt_modifies(engine, s, idx, BoundKey::Local(idx)))
        {
            return None;
        }
    }
    let else_ub = match cmp {
        NirBinaryOp::Gt => k,
        NirBinaryOp::GtEq => k.checked_sub(1)?,
        _ => return None,
    };
    Some(then_const.max(else_ub))
}

fn operand_same(engine: &Engine, binds: &Binds, a: Operand, b: Operand) -> bool {
    match (resolve(engine, binds, a), resolve(engine, binds, b)) {
        (Operand::Expr(ea), Operand::Expr(eb)) => {
            ea == eb
                || matches!(
                    (&engine.body.exprs[ea].kind, &engine.body.exprs[eb].kind),
                    (
                        ExprKind::Local { index: ia, .. },
                        ExprKind::Local { index: ib, .. },
                    ) if ia == ib
                )
        }
        (Operand::Value(va), Operand::Value(vb)) => va == vb,
        _ => false,
    }
}

fn block_id_tail(body: &crate::nir_arena::Body, block: BlockId) -> Option<Operand> {
    match &body.stmts[*body.blocks[block].stmts.last()?].kind {
        StmtKind::Expr(op) => Some(*op),
        _ => None,
    }
}

struct ConstBoundIndexEliminator<'a> {
    binds: &'a Binds,
}

impl ArenaOptVisitor for ConstBoundIndexEliminator<'_> {
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
            && let Some((idx, bound)) = ge_check_operands(engine, self.binds, condition)
            && let Some(b) = parse_const_i64(engine, self.binds, bound)
            && let Some(ub) = index_upper_bound(engine, self.binds, idx)
            && ub < b
        {
            eliminate_condition(engine, NodeRef::Stmt(s), condition);
            return true;
        }
        arena_opt_walk(self, engine, NodeRef::Stmt(s))
    }
}

// ---------------------------------------------------------------------------
// Redundant bounds-check elimination (forward, dominating panic-guards)
// ---------------------------------------------------------------------------

/// A proven fact `var + off < bound`, established by a dominating panic-guard's
/// fall-through and held while walking in execution order.
type ProvenLt = (u32, i64, BoundKey);

/// True if some fact refutes the check `var + j >= bound`: a fact
/// `var + off < bound` with `off >= j` gives `var + j <= var + off < bound`.
fn facts_refute(facts: &[ProvenLt], var: u32, j: i64, bound: BoundKey) -> bool {
    facts
        .iter()
        .any(|&(fv, foff, fb)| fv == var && fb == bound && foff >= j)
}

/// Drop every fact whose `var` / `bound` root `node` may modify (conservative:
/// a false "modifies" only keeps a fact, never invents one).
fn invalidate(engine: &Engine, node: NodeRef, facts: &mut Vec<ProvenLt>) {
    facts.retain(|&(v, _, b)| !node_modifies(engine, node, v, b));
}

/// A panic-guard `if <check> { panic }` (no else, `then` a panic block): the
/// implicit bounds check every `arr[i]` lowers to. Returns its check condition.
fn panic_guard_check(engine: &Engine, node: NodeRef) -> Option<Operand> {
    let (cond, then_b) = match node {
        NodeRef::Stmt(s) => match &engine.body.stmts[s].kind {
            StmtKind::If {
                condition,
                then_block,
                else_block: None,
            } => (*condition, *then_block),
            _ => return None,
        },
        NodeRef::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch: None,
            } => (*condition, *then_branch),
            _ => return None,
        },
        _ => return None,
    };
    is_panic_block(engine, then_b).then_some(cond)
}

/// The `if`'s `(condition, then, else)` when `node` is a `StmtKind::If` /
/// `ExprKind::If` — the two share a shape but different field names.
fn if_parts(engine: &Engine, node: NodeRef) -> Option<(Operand, BlockId, Option<BlockId>)> {
    match node {
        NodeRef::Stmt(s) => match &engine.body.stmts[s].kind {
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => Some((*condition, *then_block, *else_block)),
            _ => None,
        },
        NodeRef::Expr(e) => match &engine.body.exprs[e].kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => Some((*condition, *then_branch, *else_branch)),
            _ => None,
        },
        _ => None,
    }
}

/// The short-circuit `&&` / `||` operands of `node`, if any: the left runs
/// unconditionally, the right only conditionally.
fn short_circuit_parts(engine: &Engine, node: NodeRef) -> Option<(Operand, Operand)> {
    let NodeRef::Expr(e) = node else { return None };
    match &engine.body.exprs[e].kind {
        ExprKind::Binary {
            left,
            op: NirBinaryOp::And | NirBinaryOp::Or,
            right,
        } => Some((*left, *right)),
        _ => None,
    }
}

/// Forward redundant-bounds-check elimination: walk `node` in execution order
/// maintaining a set of proven `var + off < bound` facts harvested from
/// panic-guard checks at *unconditionally-executed* positions, and drive to
/// `false` any later dominated check the facts already refute (e.g. `arr[i]`
/// re-checked after an earlier `arr[i]` with the array unmodified between).
///
/// `facts` are threaded by `&mut` through straight-line positions (an earlier
/// sibling's harvest refutes a later one) and by clone into conditional /
/// looping sub-positions (a branch's harvest must not escape). A processed node
/// that may modify a fact's root drops it ([`invalidate`]), so a resize between
/// the two checks preserves the later one — the same mod-tracking predicate the
/// straight-line early-exit elimination above relies on.
fn rbce_walk(engine: &mut Engine, node: NodeRef, facts: &mut Vec<ProvenLt>, binds: &Binds) -> bool {
    // A panic-guard: harvest its `var + off < bound` fact for later siblings, or
    // eliminate it when an earlier guard already proved it. Its condition is a
    // plain comparison (no nested guards) and its `then` diverges, so neither is
    // walked.
    if let Some(cond) = panic_guard_check(engine, node) {
        if let Some((var, off, bound)) = parse_check(engine, binds, cond) {
            if facts_refute(facts, var, off, bound) {
                eliminate_condition(engine, node, cond);
                return true;
            }
            facts.push((var, off, bound));
            return false;
        }
        // Non-parseable guard: its condition still runs unconditionally.
        if let Some(ce) = cond.as_expr() {
            return rbce_walk(engine, NodeRef::Expr(ce), facts, binds);
        }
        return false;
    }

    // A general `if`: the condition runs unconditionally (its facts persist to
    // siblings); each branch runs conditionally (a clone, discarded after).
    if let Some((cond, then_b, else_b)) = if_parts(engine, node) {
        let mut changed = false;
        if let Some(ce) = cond.as_expr() {
            changed |= rbce_walk(engine, NodeRef::Expr(ce), facts, binds);
            invalidate(engine, NodeRef::Expr(ce), facts);
        }
        let mut ft = facts.clone();
        changed |= rbce_walk(engine, NodeRef::Block(then_b), &mut ft, binds);
        if let Some(eb) = else_b {
            let mut fe = facts.clone();
            changed |= rbce_walk(engine, NodeRef::Block(eb), &mut fe, binds);
            invalidate(engine, NodeRef::Block(eb), facts);
        }
        // Either branch may have modified a fact's root before a later sibling.
        invalidate(engine, NodeRef::Block(then_b), facts);
        return changed;
    }

    // Short-circuit `&&` / `||`: left unconditional, right conditional.
    if let Some((left, right)) = short_circuit_parts(engine, node) {
        let mut changed = false;
        if let Some(le) = left.as_expr() {
            changed |= rbce_walk(engine, NodeRef::Expr(le), facts, binds);
            invalidate(engine, NodeRef::Expr(le), facts);
        }
        if let Some(re) = right.as_expr() {
            let mut fr = facts.clone();
            changed |= rbce_walk(engine, NodeRef::Expr(re), &mut fr, binds);
            invalidate(engine, NodeRef::Expr(re), facts);
        }
        return changed;
    }

    // A loop / labeled block runs its body conditionally and (for a loop)
    // repeatedly, so outer facts must not enter and inner facts must not escape:
    // process it as its own region with a fresh fact set.
    let fresh_region = match node {
        NodeRef::Stmt(s) => matches!(engine.body.stmts[s].kind, StmtKind::Loop { .. }),
        NodeRef::Expr(e) => matches!(engine.body.exprs[e].kind, ExprKind::LabeledBlock { .. }),
        _ => false,
    };
    if fresh_region {
        let mut inner: Vec<ProvenLt> = Vec::new();
        let mut changed = false;
        let mut kids = Vec::new();
        engine.body.for_each_child(node, |c| kids.push(c));
        for c in kids {
            changed |= rbce_walk(engine, c, &mut inner, binds);
        }
        invalidate(engine, node, facts);
        return changed;
    }

    // A `match` / `switch` runs its scrutinee unconditionally but each arm
    // conditionally: process arm bodies with a clone. `for_each_child` yields
    // the scrutinee first, so persist facts through it and clone for the rest.
    let arm_split = match node {
        NodeRef::Expr(e) => matches!(
            engine.body.exprs[e].kind,
            ExprKind::Match { .. } | ExprKind::Switch { .. }
        ),
        _ => false,
    };
    if arm_split {
        let mut kids = Vec::new();
        engine.body.for_each_child(node, |c| kids.push(c));
        let mut changed = false;
        let mut first = true;
        for c in kids {
            if first && matches!(c, NodeRef::Expr(_)) {
                // The scrutinee (an operand) — unconditional.
                changed |= rbce_walk(engine, c, facts, binds);
                invalidate(engine, c, facts);
                first = false;
            } else {
                let mut arm = facts.clone();
                changed |= rbce_walk(engine, c, &mut arm, binds);
            }
        }
        invalidate(engine, node, facts);
        return changed;
    }

    // Default: every child runs unconditionally and in order (arithmetic, casts,
    // field/index reads, assignments, calls, aggregate literals, plain blocks).
    // Thread facts through each, invalidating after — a harvest by an earlier
    // child refutes a later one.
    let mut kids = Vec::new();
    engine.body.for_each_child(node, |c| kids.push(c));
    let mut changed = false;
    for c in kids {
        changed |= rbce_walk(engine, c, facts, binds);
        invalidate(engine, c, facts);
    }
    changed
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
