//! Adjacent-use single-field struct local elimination for Wado NIR.
//!
//! NIR analog of `wir_optimize/elide_struct.rs`'s
//! `elide_adjacent_single_use_struct_locals`. Targets the common
//! `Box<T>` pattern produced by [`lower::translate`]'s `wrap_in_box`
//! and exposed by the NIR `sroa_param` pass after it strips
//! `&primitive` parameters down to scalars:
//!
//! ```text
//! let snapshot = c;                           // (1) value-copy
//! let __v0: i32 = snapshot.value;             // (2) field extracted by sroa_param
//! let __cond = __v0 == 0;
//! ```
//!
//! When a local is exactly one `Let { StructLiteral { single field } }`
//! and has exactly one `FieldAccess` use, this pass substitutes the
//! single-field initializer directly at the use site and drops the
//! `Let`. The substitution is safe when every intervening sibling
//! statement passes [`mod_ref::can_move_past`].
//!
//! Why at NIR (not WIR): the NIR-level alias machinery
//! (`address_taken_locals` / `stores_aliased_locals`) feeds the
//! identity-escape gate directly, and the substituted expressions
//! flow back into the same fix-point loop where `copy_prop` /
//! `const_fold` / `dce` can fold them further. The legacy WIR
//! variant was retired once NIR `sroa_param` made the receiver
//! `FieldAccess`-shaped at the call site (issue #1184).
//!
//! ## Scope
//!
//! - Single-field `StructLiteral` only. Multi-field elision is left
//!   to the existing NIR `sroa` pass.
//! - The candidate local must be defined exactly once and read
//!   exactly once via `FieldAccess { expr: Local(idx), field_name }`.
//! - The use must be the leftmost evaluated sub-expression of a
//!   subsequent sibling statement. Conditional control (`If` /
//!   `Match` / `Switch` / `LabeledBlock` / nested `Block`) blocks
//!   substitution — those constructs may not execute the use on
//!   every path.
//!
//! ## Arena port
//!
//! Ported off the `Body ↔ tree` bridge (Phase 4 stage C; see
//! `docs/wep-2026-06-05-nir-rewrite-engine-design.md`): the body traversal
//! (stats, the bottom-up driver, candidate detection, substitution) reads and
//! mutates the arena `Body` directly. The two soundness gates — `ModRef` and
//! the leftmost-evaluated-subexpression walker — are still tree-shaped and are
//! invoked on materialized subtrees (`Body::to_tree_{expr,stmt}`); their full
//! arena port is deferred. Materializing a subtree is bit-identical to the
//! original, so the soundness decision (and codegen) is unchanged.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{
    NirBinaryOp, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind, NirUnaryOp,
};
use crate::nir_arena::{
    BlockId, Body, ExprId, ExprKind, ExprNode, NodeRef, PatKind, StmtId, StmtKind,
};
use crate::nir_package::NirPackage;
use crate::tir::TypeTable;

use super::mod_ref::{ModRef, can_move_past};

pub fn elide_adjacent_box_locals(project: &mut NirPackage) -> bool {
    let mut changed = false;
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        if elide_in_function(&mut func) {
            changed = true;
        }
    }
    changed
}

fn elide_in_function(func: &mut NirFunction) -> bool {
    // Identity / aliasing safety: locals whose reference could escape via
    // callee-side storage are off-limits.
    let mut blacklist: IndexSet<u32> = IndexSet::default();
    blacklist.extend(func.address_taken_locals.iter().copied());
    blacklist.extend(func.stores_aliased_locals.iter().copied());

    let Some(body) = func.body.as_mut() else {
        return false;
    };
    let stats = collect_local_stats(body);
    let mut changed = false;
    let root = body.root;
    while elide_block(body, root, &stats, &blacklist) {
        changed = true;
    }
    changed
}

/// Visit nested blocks bottom-up (inner first), then sibling-scan this block —
/// the old `Elider`'s bottom-up-then-scan order.
fn elide_block(
    body: &mut Body,
    block: BlockId,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> bool {
    let mut changed = false;
    let mut kids = Vec::new();
    body.for_each_child(NodeRef::Block(block), |c| kids.push(c));
    for c in kids {
        changed |= elide_descend(body, c, stats, blacklist);
    }
    let stmts = body.blocks[block].stmts.clone();
    for i in 0..stmts.len() {
        if try_elide_at(body, &stmts, i, stats, blacklist) {
            changed = true;
        }
    }
    changed
}

fn elide_descend(
    body: &mut Body,
    node: NodeRef,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> bool {
    if let NodeRef::Block(b) = node {
        return elide_block(body, b, stats, blacklist);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    let mut changed = false;
    for c in kids {
        changed |= elide_descend(body, c, stats, blacklist);
    }
    changed
}

// -----------------------------------------------------------------------
// Stats collection
// -----------------------------------------------------------------------

#[derive(Default)]
struct LocalStats {
    /// Number of `Local { index }` reads anywhere in the body
    /// (including those wrapped in `FieldAccess`).
    total_reads: u32,
    /// Number of `FieldAccess { expr: Local{ index }, .. }` reads.
    fieldaccess_reads: u32,
    /// Distinct field names seen in `FieldAccess` reads of this local.
    field_names: IndexSet<String>,
    /// Number of `Let { local_index } | LetDestructure-binding |
    /// Assign target Local`.
    defs: u32,
}

fn collect_local_stats(body: &Body) -> IndexMap<u32, LocalStats> {
    let mut stats: IndexMap<u32, LocalStats> = IndexMap::default();
    stats_node(body, NodeRef::Block(body.root), &mut stats);
    stats
}

fn stats_node(body: &Body, node: NodeRef, stats: &mut IndexMap<u32, LocalStats>) {
    match node {
        NodeRef::Expr(id) => stats_expr(body, id, stats),
        NodeRef::Stmt(s) => stats_stmt(body, s, stats),
        _ => body.for_each_child(node, |c| stats_node(body, c, stats)),
    }
}

fn stats_stmt(body: &Body, stmt: StmtId, stats: &mut IndexMap<u32, LocalStats>) {
    match &body.stmts[stmt].kind {
        StmtKind::Let {
            local_index, value, ..
        } => {
            let (local_index, value) = (*local_index, *value);
            stats.entry(local_index).or_default().defs += 1;
            stats_node(body, NodeRef::Expr(value), stats);
        }
        StmtKind::LetDestructure { pattern, value, .. } => {
            let (pattern, value) = (*pattern, *value);
            record_pattern_defs(body, pattern, stats);
            stats_node(body, NodeRef::Expr(value), stats);
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Stmt(stmt), |c| kids.push(c));
            for c in kids {
                stats_node(body, c, stats);
            }
        }
    }
}

fn stats_expr(body: &Body, id: ExprId, stats: &mut IndexMap<u32, LocalStats>) {
    match &body.exprs[id].kind {
        ExprKind::FieldAccess {
            expr: inner,
            field_name,
            ..
        } => {
            let inner = *inner;
            let field_name = field_name.clone();
            if let ExprKind::Local { index, .. } = &body.exprs[inner].kind {
                let index = *index;
                let s = stats.entry(index).or_default();
                s.total_reads += 1;
                s.fieldaccess_reads += 1;
                s.field_names.insert(field_name);
            } else {
                stats_node(body, NodeRef::Expr(inner), stats);
            }
        }
        ExprKind::Local { index, .. } => {
            stats.entry(*index).or_default().total_reads += 1;
        }
        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            if let ExprKind::Local { index, .. } = &body.exprs[target].kind {
                let index = *index;
                stats.entry(index).or_default().defs += 1;
                stats_node(body, NodeRef::Expr(value), stats);
            } else {
                stats_node(body, NodeRef::Expr(target), stats);
                stats_node(body, NodeRef::Expr(value), stats);
            }
        }
        _ => {
            let mut kids = Vec::new();
            body.for_each_child(NodeRef::Expr(id), |c| kids.push(c));
            for c in kids {
                stats_node(body, c, stats);
            }
        }
    }
}

fn record_pattern_defs(
    body: &Body,
    pat: crate::nir_arena::PatId,
    stats: &mut IndexMap<u32, LocalStats>,
) {
    match &body.pats[pat].kind {
        PatKind::Binding { local_index, .. } => {
            stats.entry(*local_index).or_default().defs += 1;
        }
        PatKind::Tuple(patterns, _) | PatKind::Or(patterns) => {
            let patterns = patterns.clone();
            for p in patterns {
                record_pattern_defs(body, p, stats);
            }
        }
        PatKind::Variant { bindings, .. } => {
            let bindings = bindings.clone();
            for p in bindings {
                record_pattern_defs(body, p, stats);
            }
        }
        PatKind::Struct { fields, .. } => {
            let fields: Vec<crate::nir_arena::PatId> = fields.iter().map(|f| f.pattern).collect();
            for p in fields {
                record_pattern_defs(body, p, stats);
            }
        }
        _ => {}
    }
}

// -----------------------------------------------------------------------
// Candidate detection & substitution
// -----------------------------------------------------------------------

fn try_elide_at(
    body: &mut Body,
    stmts: &[StmtId],
    i: usize,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> bool {
    let Some((candidate, field_name, inner_mr)) =
        describe_candidate(body, stmts[i], stats, blacklist)
    else {
        return false;
    };

    let Some(j) = find_use_site(body, stmts, i + 1, candidate, &field_name, &inner_mr) else {
        return false;
    };

    let inner = take_candidate_inner(body, stmts[i]);
    substitute_first_use(body, stmts[j], candidate, &field_name, inner);
    true
}

fn describe_candidate(
    body: &Body,
    stmt: StmtId,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> Option<(u32, String, ModRef)> {
    let StmtKind::Let {
        local_index, value, ..
    } = &body.stmts[stmt].kind
    else {
        return None;
    };
    let (local_index, value) = (*local_index, *value);
    if blacklist.contains(&local_index) {
        return None;
    }
    let s = stats.get(&local_index)?;
    if s.defs != 1 || s.fieldaccess_reads != 1 || s.total_reads != 1 {
        return None;
    }
    if s.field_names.len() != 1 {
        return None;
    }
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[value].kind else {
        return None;
    };
    if fields.len() != 1 {
        return None;
    }
    let inner_value = fields[0].value;
    let field_name = s.field_names.iter().next().unwrap().clone();
    let inner_mr = ModRef::of_expr(body, inner_value);
    Some((local_index, field_name, inner_mr))
}

/// Extract the candidate's single-field initializer (returning its expr id) and
/// replace the `let` with a void `Expr(Unit)` placeholder. The placeholder must
/// carry the actual Unit type so downstream passes recognise it as void.
fn take_candidate_inner(body: &mut Body, stmt: StmtId) -> ExprId {
    let StmtKind::Let { value, .. } = &body.stmts[stmt].kind else {
        unreachable!("guarded by describe_candidate");
    };
    let value = *value;
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[value].kind else {
        unreachable!("guarded by describe_candidate");
    };
    let inner = fields[0].value;
    let span = body.stmts[stmt].span;
    let unit = body.exprs.push(ExprNode {
        kind: ExprKind::Unit,
        type_id: TypeTable::UNIT,
        span,
    });
    body.stmts[stmt].kind = StmtKind::Expr(unit);
    inner
}

fn find_use_site(
    body: &Body,
    stmts: &[StmtId],
    from: usize,
    candidate: u32,
    field_name: &str,
    inner_mr: &ModRef,
) -> Option<usize> {
    let mut k = from;
    while k < stmts.len() {
        let stmt = stmts[k];
        if is_placeholder(body, stmt) {
            k += 1;
            continue;
        }
        // The leftmost-walker is still a tree analysis; run it on the
        // materialized statement subtree. ModRef reads the arena directly.
        let tree = body.to_tree_stmt(stmt);
        if matches!(
            walk_stmt_for_leftmost(&tree, candidate, field_name),
            LeftmostWalk::Found
        ) {
            return Some(k);
        }
        let int_mr = ModRef::of_stmt(body, stmt);
        if !can_move_past(inner_mr, &int_mr, candidate) {
            return None;
        }
        k += 1;
    }
    None
}

fn is_placeholder(body: &Body, stmt: StmtId) -> bool {
    matches!(&body.stmts[stmt].kind, StmtKind::Expr(e) if matches!(body.exprs[*e].kind, ExprKind::Unit))
}

// -----------------------------------------------------------------------
// Leftmost-evaluated-subexpression walker
// -----------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeftmostWalk {
    Found,
    Pure,
    Blocked,
}

fn walk_stmt_for_leftmost(stmt: &NirStmt, candidate: u32, field_name: &str) -> LeftmostWalk {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => {
            match walk_expr_for_leftmost(value, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                _ => LeftmostWalk::Blocked,
            }
        }
        NirStmtKind::LetDestructure { value, .. } => {
            match walk_expr_for_leftmost(value, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                _ => LeftmostWalk::Blocked,
            }
        }
        NirStmtKind::Expr(e) => walk_expr_for_leftmost(e, candidate, field_name),
        NirStmtKind::Return { value: Some(v) } | NirStmtKind::Break { value: Some(v), .. } => {
            match walk_expr_for_leftmost(v, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                _ => LeftmostWalk::Blocked,
            }
        }
        NirStmtKind::Return { value: None }
        | NirStmtKind::Break { value: None, .. }
        | NirStmtKind::Continue
        | NirStmtKind::If { .. }
        | NirStmtKind::Loop { .. }
        | NirStmtKind::LabeledBlock { .. } => LeftmostWalk::Blocked,
    }
}

fn walk_expr_for_leftmost(expr: &NirExpr, candidate: u32, field_name: &str) -> LeftmostWalk {
    if let NirExprKind::FieldAccess {
        expr: inner,
        field_name: fname,
        ..
    } = &expr.kind
        && fname == field_name
        && let NirExprKind::Local { index, .. } = &inner.kind
        && *index == candidate
    {
        return LeftmostWalk::Found;
    }

    match &expr.kind {
        NirExprKind::If { .. }
        | NirExprKind::Match { .. }
        | NirExprKind::Switch { .. }
        | NirExprKind::LabeledBlock { .. }
        | NirExprKind::Block(_) => LeftmostWalk::Blocked,

        NirExprKind::Assign { target, value } => {
            match walk_assign_target(target, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                LeftmostWalk::Blocked => LeftmostWalk::Blocked,
                LeftmostWalk::Pure => match walk_expr_for_leftmost(value, candidate, field_name) {
                    LeftmostWalk::Found => LeftmostWalk::Found,
                    _ => LeftmostWalk::Blocked,
                },
            }
        }
        NirExprKind::GlobalVarSet { value, .. } => {
            match walk_expr_for_leftmost(value, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                _ => LeftmostWalk::Blocked,
            }
        }
        NirExprKind::Call { args, .. } => {
            walk_children_observable(args.iter().map(|a| &a.expr), candidate, field_name)
        }
        NirExprKind::MethodCall { receiver, args, .. } => walk_children_observable(
            std::iter::once(receiver.as_ref()).chain(args.iter().map(|a| &a.expr)),
            candidate,
            field_name,
        ),
        NirExprKind::IndirectCall { callee, args } => walk_children_observable(
            std::iter::once(callee.as_ref()).chain(args.iter()),
            candidate,
            field_name,
        ),
        NirExprKind::CmRawCall { args, .. } => {
            walk_children_observable(args.iter(), candidate, field_name)
        }

        NirExprKind::Binary { left, right, op } => match op {
            // `&&` and `||` short-circuit: the right operand is
            // evaluated only when the left operand permits, so a
            // candidate use anchored in the right operand would
            // execute conditionally after substitution while the
            // original `let` ran unconditionally. Treat the right
            // operand the same way as an `if` branch — a Found there
            // must block elision, not anchor it.
            NirBinaryOp::And | NirBinaryOp::Or => {
                match walk_expr_for_leftmost(left, candidate, field_name) {
                    LeftmostWalk::Found => LeftmostWalk::Found,
                    LeftmostWalk::Blocked => LeftmostWalk::Blocked,
                    LeftmostWalk::Pure => {
                        match walk_expr_for_leftmost(right, candidate, field_name) {
                            LeftmostWalk::Pure => LeftmostWalk::Pure,
                            _ => LeftmostWalk::Blocked,
                        }
                    }
                }
            }
            // `Div` / `Mod` may trap on a zero divisor; the operation
            // itself is observable, so a Pure subtree below it does
            // not make the surrounding context Pure.
            NirBinaryOp::Div | NirBinaryOp::Mod => observable_propagate(walk_children_pure(
                [left.as_ref(), right.as_ref()].into_iter(),
                candidate,
                field_name,
            )),
            _ => walk_children_pure(
                [left.as_ref(), right.as_ref()].into_iter(),
                candidate,
                field_name,
            ),
        },
        NirExprKind::Unary { expr: inner, op } => match op {
            // Deref may trap on a null receiver; the op itself is
            // observable.
            NirUnaryOp::Deref => {
                observable_propagate(walk_expr_for_leftmost(inner, candidate, field_name))
            }
            // Arithmetic / logical / address-taking unaries are pure.
            NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot => {
                walk_expr_for_leftmost(inner, candidate, field_name)
            }
            NirUnaryOp::Ref | NirUnaryOp::MutRef => {
                walk_expr_for_leftmost(inner, candidate, field_name)
            }
        },
        // `as` lowers to `ref.cast` / numeric narrowing — both may trap.
        NirExprKind::Cast { expr: inner, .. } => {
            observable_propagate(walk_expr_for_leftmost(inner, candidate, field_name))
        }
        // FieldAccess on a non-candidate receiver: a fresh `struct.get`
        // on a possibly-null reference, so the op itself may trap. A
        // Found in the receiver still anchors at the receiver position
        // (the FieldAccess applies AFTER the substituted inner), but
        // a Pure receiver does NOT make this subtree Pure.
        NirExprKind::FieldAccess { expr: inner, .. } => {
            observable_propagate(walk_expr_for_leftmost(inner, candidate, field_name))
        }
        // `List<T>::index_value`-shaped Index may trap on a null base
        // and on OOB; the op itself is observable.
        NirExprKind::Index { expr: inner, index } => observable_propagate(walk_children_pure(
            [inner.as_ref(), index.as_ref()].into_iter(),
            candidate,
            field_name,
        )),
        NirExprKind::StructLiteral { fields, .. } => {
            walk_children_pure(fields.iter().map(|f| &f.value), candidate, field_name)
        }
        NirExprKind::TupleLiteral { elements } | NirExprKind::ArrayLiteral { elements } => {
            walk_children_pure(elements.iter(), candidate, field_name)
        }
        NirExprKind::VariantConstruct { payload, .. } => match payload {
            Some(p) => walk_expr_for_leftmost(p, candidate, field_name),
            None => LeftmostWalk::Pure,
        },
        NirExprKind::ClosureToCanonical { functor, .. } => {
            walk_expr_for_leftmost(functor, candidate, field_name)
        }
        // `VariantTag` / `VariantTest` / `VariantPayload` all read the
        // discriminant or payload via `ref.cast` + `struct.get` on a
        // possibly-null receiver; each may trap.
        NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => {
            observable_propagate(walk_expr_for_leftmost(inner, candidate, field_name))
        }

        NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::EnumConstruct { .. } => LeftmostWalk::Pure,
    }
}

fn walk_assign_target(target: &NirExpr, candidate: u32, field_name: &str) -> LeftmostWalk {
    match &target.kind {
        NirExprKind::Local { .. } => LeftmostWalk::Pure,
        NirExprKind::FieldAccess { expr, .. } => {
            walk_expr_for_leftmost(expr, candidate, field_name)
        }
        NirExprKind::Index { expr, index } => walk_children_pure(
            [expr.as_ref(), index.as_ref()].into_iter(),
            candidate,
            field_name,
        ),
        NirExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr,
        } => walk_expr_for_leftmost(expr, candidate, field_name),
        _ => walk_expr_for_leftmost(target, candidate, field_name),
    }
}

fn walk_children_pure<'a>(
    children: impl Iterator<Item = &'a NirExpr>,
    candidate: u32,
    field_name: &str,
) -> LeftmostWalk {
    for c in children {
        match walk_expr_for_leftmost(c, candidate, field_name) {
            LeftmostWalk::Found => return LeftmostWalk::Found,
            LeftmostWalk::Blocked => return LeftmostWalk::Blocked,
            LeftmostWalk::Pure => {}
        }
    }
    LeftmostWalk::Pure
}

/// Wrap a child walk result so that an *observable-itself* operation
/// (one that may trap, read the heap, or otherwise have a side effect
/// independent of its children) does not let a Pure subtree make the
/// surrounding context appear Pure. A `Found` child is still
/// propagated unchanged — the substitution happens INSIDE the
/// observable op, so the op's own effect fires AFTER the substituted
/// inner expression, same as before elision.
fn observable_propagate(child: LeftmostWalk) -> LeftmostWalk {
    match child {
        LeftmostWalk::Found => LeftmostWalk::Found,
        LeftmostWalk::Pure | LeftmostWalk::Blocked => LeftmostWalk::Blocked,
    }
}

fn walk_children_observable<'a>(
    children: impl Iterator<Item = &'a NirExpr>,
    candidate: u32,
    field_name: &str,
) -> LeftmostWalk {
    match walk_children_pure(children, candidate, field_name) {
        LeftmostWalk::Found => LeftmostWalk::Found,
        _ => LeftmostWalk::Blocked,
    }
}

// -----------------------------------------------------------------------
// Substitution at use site
// -----------------------------------------------------------------------

/// Replace the single `FieldAccess(Local(candidate), field_name)` use with the
/// candidate's inner initializer, moving `inner`'s content into the field-access
/// node while keeping that node's `type_id` / `span` (downstream type registries
/// are keyed by the field-access node's `type_id`). The candidate has exactly
/// one such use (guaranteed by `fieldaccess_reads == 1` / `total_reads == 1`),
/// so the first pre-order match is the only one.
fn substitute_first_use(
    body: &mut Body,
    use_stmt: StmtId,
    candidate: u32,
    field_name: &str,
    inner: ExprId,
) {
    substitute_node(body, NodeRef::Stmt(use_stmt), candidate, field_name, inner);
}

fn substitute_node(
    body: &mut Body,
    node: NodeRef,
    candidate: u32,
    field_name: &str,
    inner: ExprId,
) -> bool {
    if let NodeRef::Expr(id) = node {
        let is_match = if let ExprKind::FieldAccess {
            expr: fa_inner,
            field_name: fname,
            ..
        } = &body.exprs[id].kind
        {
            fname == field_name
                && matches!(&body.exprs[*fa_inner].kind, ExprKind::Local { index, .. } if *index == candidate)
        } else {
            false
        };
        if is_match {
            let kind = std::mem::replace(&mut body.exprs[inner].kind, ExprKind::Unit);
            body.exprs[id].kind = kind;
            return true;
        }
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        if substitute_node(body, c, candidate, field_name, inner) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirUnaryOp};
    use crate::tir::TypeId;
    use crate::token::Span;

    fn ty() -> TypeId {
        TypeId(0)
    }
    fn sp() -> Span {
        Span::default()
    }
    fn local(index: u32) -> NirExpr {
        NirExpr::new(
            NirExprKind::Local {
                index,
                name: format!("__l{index}"),
            },
            ty(),
            sp(),
        )
    }
    fn int(v: i64) -> NirExpr {
        NirExpr::new(
            NirExprKind::IntLiteral {
                value: v as u64,
                repr: v.to_string(),
            },
            ty(),
            sp(),
        )
    }
    fn field(receiver: NirExpr, name: &str) -> NirExpr {
        NirExpr::new(
            NirExprKind::FieldAccess {
                expr: Box::new(receiver),
                field_index: 0,
                field_name: name.to_string(),
            },
            ty(),
            sp(),
        )
    }
    fn index(arr: NirExpr, idx: NirExpr) -> NirExpr {
        NirExpr::new(
            NirExprKind::Index {
                expr: Box::new(arr),
                index: Box::new(idx),
            },
            ty(),
            sp(),
        )
    }
    fn binary(op: NirBinaryOp, lhs: NirExpr, rhs: NirExpr) -> NirExpr {
        NirExpr::new(
            NirExprKind::Binary {
                left: Box::new(lhs),
                right: Box::new(rhs),
                op,
            },
            ty(),
            sp(),
        )
    }
    fn unary(op: NirUnaryOp, e: NirExpr) -> NirExpr {
        NirExpr::new(
            NirExprKind::Unary {
                op,
                expr: Box::new(e),
            },
            ty(),
            sp(),
        )
    }
    fn cast(e: NirExpr, target: TypeId) -> NirExpr {
        NirExpr::new(
            NirExprKind::Cast {
                expr: Box::new(e),
                target_type: target,
            },
            ty(),
            sp(),
        )
    }

    /// Use site `boxed.v + 0` — FieldAccess(Local(7), "v") is the LEFT
    /// operand of `+`, the leftmost evaluated subexpression. Walker
    /// must return Found.
    #[test]
    fn walker_finds_left_field_access() {
        let expr = binary(NirBinaryOp::Add, field(local(7), "v"), int(0));
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Found
        ));
    }

    /// Use site `arr[idx] + boxed.v` — `arr[idx]` is observable
    /// (may trap on OOB / null), so a Pure result from its children
    /// must NOT make the surrounding context Pure. The walker has to
    /// return Blocked, not Found, because substituting the inner
    /// expression at boxed.v's position would reorder the
    /// candidate's effects AFTER `arr[idx]`'s heap read and possible
    /// trap. Regression for the "trap relocation / observable
    /// pass-through" finding.
    #[test]
    fn walker_blocks_when_observable_index_precedes_field() {
        let expr = binary(
            NirBinaryOp::Add,
            index(local(1), local(2)),
            field(local(7), "v"),
        );
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Blocked
        ));
    }

    /// Use site `(other.f) + boxed.v` — non-target `FieldAccess` may
    /// trap on a null receiver, so its Pure-children must not paint
    /// the surrounding context Pure.
    #[test]
    fn walker_blocks_when_non_target_field_precedes_field() {
        let expr = binary(
            NirBinaryOp::Add,
            field(local(2), "other"),
            field(local(7), "v"),
        );
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Blocked
        ));
    }

    /// Use site `(x as i32) + boxed.v` — `Cast` may trap, observable.
    #[test]
    fn walker_blocks_when_cast_precedes_field() {
        let expr = binary(NirBinaryOp::Add, cast(local(2), ty()), field(local(7), "v"));
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Blocked
        ));
    }

    /// Use site `*p + boxed.v` — `Unary::Deref` may trap, observable.
    #[test]
    fn walker_blocks_when_deref_precedes_field() {
        let expr = binary(
            NirBinaryOp::Add,
            unary(NirUnaryOp::Deref, local(2)),
            field(local(7), "v"),
        );
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Blocked
        ));
    }

    /// Use site `(a / b) + boxed.v` — `Binary::Div` may trap on zero
    /// divisor, observable.
    #[test]
    fn walker_blocks_when_div_precedes_field() {
        let expr = binary(
            NirBinaryOp::Add,
            binary(NirBinaryOp::Div, local(1), local(2)),
            field(local(7), "v"),
        );
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Blocked
        ));
    }

    /// Use site `boxed.v + arr[idx]` — observable on RIGHT side
    /// only. Walker still returns Found at the leftmost `FieldAccess`
    /// (left operand), since the observable Index runs AFTER the
    /// substituted inner expression.
    #[test]
    fn walker_allows_observable_after_field() {
        let expr = binary(
            NirBinaryOp::Add,
            field(local(7), "v"),
            index(local(1), local(2)),
        );
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Found
        ));
    }

    /// Use site `Cast(boxed.v)` — `FieldAccess` inside an observable
    /// op. Substitution is safe because the observable op runs
    /// AFTER the substituted inner.
    #[test]
    fn walker_finds_when_field_is_inside_observable() {
        let expr = cast(field(local(7), "v"), ty());
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Found
        ));
    }

    /// Use site `cond && boxed.v` — the right operand of `&&` is
    /// conditionally evaluated; a Found in the right side must
    /// Block, not anchor.
    #[test]
    fn walker_blocks_field_in_right_of_and() {
        let expr = binary(NirBinaryOp::And, local(2), field(local(7), "v"));
        assert!(matches!(
            walk_expr_for_leftmost(&expr, 7, "v"),
            LeftmostWalk::Blocked
        ));
    }
}
