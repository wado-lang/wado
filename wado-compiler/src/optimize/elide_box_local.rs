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

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBinaryOp, NirUnaryOp};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, NodeRef, PatKind, StmtId, StmtKind};
use crate::nir_engine::{Engine, Rule};

use super::mod_ref::{ModRef, can_move_past};

/// Adjacent-use single-field struct local elimination as a rule on the unified
/// post-inline peephole session (combine migration; formerly the standalone
/// `nir/elide_box_local` pass). `build_elide_box_local` computes the same
/// whole-function read/def `stats` the standalone pass did, plus the
/// escape-safety `blacklist` (`address_taken` + `stores_aliased` locals), once
/// per function from the pristine body. The `stats` are read-only oracles: a
/// peephole rewrite can only remove a read, so a stale entry can only refuse an
/// otherwise-valid elision (safe), never enable an unsound one. Each
/// `apply_block` performs at most one elision — move the single-field
/// initializer into its one `r.field` use via `become_expr` and drop the
/// binding `Let` via `set_block_stmts` — then the worklist re-runs for the next.
pub(super) struct ElideBoxLocalRule {
    stats: IndexMap<u32, LocalStats>,
    blacklist: IndexSet<u32>,
}

/// Build an [`ElideBoxLocalRule`] for one function from its pristine body and
/// the function's escape-analysis sets.
pub(super) fn build_elide_box_local(
    body: &Body,
    address_taken: &IndexSet<u32>,
    stores_aliased: &IndexSet<u32>,
) -> ElideBoxLocalRule {
    let mut blacklist: IndexSet<u32> = IndexSet::default();
    blacklist.extend(address_taken.iter().copied());
    blacklist.extend(stores_aliased.iter().copied());
    ElideBoxLocalRule {
        stats: collect_local_stats(body),
        blacklist,
    }
}

impl Rule for ElideBoxLocalRule {
    fn apply_block(&self, engine: &mut Engine, block: BlockId) -> bool {
        let stmts = engine.body.blocks[block].stmts.clone();
        for i in 0..stmts.len() {
            let Some((candidate, field_name, inner_mr)) =
                describe_candidate(engine.body, stmts[i], &self.stats, &self.blacklist)
            else {
                continue;
            };
            let Some(j) = find_use_site(engine.body, &stmts, i + 1, candidate, &field_name, &inner_mr)
            else {
                continue;
            };
            let inner = candidate_inner(engine.body, stmts[i]);
            let Some(use_site) =
                find_subst_target(engine.body, NodeRef::Stmt(stmts[j]), candidate, &field_name)
            else {
                continue;
            };
            // Move the single-field initializer's kind into its one `r.field`
            // use, then drop the now-dead binding. The use site keeps its own
            // `type_id` / `span` (the field-access node's), matching the old
            // `substitute_node` — `become_expr` would instead adopt the
            // initializer's, which only coincides because value semantics force
            // the field type to equal the initializer's.
            let inner_kind = std::mem::replace(&mut engine.body.exprs[inner].kind, ExprKind::Unit);
            engine.replace_expr_kind(use_site, inner_kind);
            let kept: Vec<StmtId> = stmts
                .iter()
                .enumerate()
                .filter(|(k, _)| *k != i)
                .map(|(_, s)| *s)
                .collect();
            engine.set_block_stmts(block, kept);
            return true;
        }
        false
    }
}

/// The single-field initializer expression of a candidate `let x = Struct { f }`.
fn candidate_inner(body: &Body, stmt: StmtId) -> ExprId {
    let StmtKind::Let { value, .. } = &body.stmts[stmt].kind else {
        unreachable!("guarded by describe_candidate");
    };
    let ExprKind::StructLiteral { fields, .. } = &body.exprs[*value].kind else {
        unreachable!("guarded by describe_candidate");
    };
    fields[0].value
}

/// The leftmost `candidate.field_name` field-access expression reachable from
/// `node`, in the same pre-order the old `substitute_node` replaced.
fn find_subst_target(
    body: &Body,
    node: NodeRef,
    candidate: u32,
    field_name: &str,
) -> Option<ExprId> {
    if let NodeRef::Expr(id) = node
        && let ExprKind::FieldAccess {
            expr: fa_inner,
            field_name: fname,
            ..
        } = &body.exprs[id].kind
        && fname == field_name
        && matches!(&body.exprs[*fa_inner].kind, ExprKind::Local { index, .. } if *index == candidate)
    {
        return Some(id);
    }
    let mut kids = Vec::new();
    body.for_each_child(node, |c| kids.push(c));
    for c in kids {
        if let Some(found) = find_subst_target(body, c, candidate, field_name) {
            return Some(found);
        }
    }
    None
}

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
        if matches!(
            walk_stmt_for_leftmost(body, stmt, candidate, field_name),
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

fn walk_stmt_for_leftmost(
    body: &Body,
    stmt: StmtId,
    candidate: u32,
    field_name: &str,
) -> LeftmostWalk {
    match &body.stmts[stmt].kind {
        StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
            match walk_expr_for_leftmost(body, *value, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                _ => LeftmostWalk::Blocked,
            }
        }
        StmtKind::Expr(e) => walk_expr_for_leftmost(body, *e, candidate, field_name),
        StmtKind::Return { value: Some(v) } | StmtKind::Break { value: Some(v), .. } => {
            match walk_expr_for_leftmost(body, *v, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                _ => LeftmostWalk::Blocked,
            }
        }
        StmtKind::Return { value: None }
        | StmtKind::Break { value: None, .. }
        | StmtKind::Continue
        | StmtKind::If { .. }
        | StmtKind::Loop { .. }
        | StmtKind::LabeledBlock { .. } => LeftmostWalk::Blocked,
    }
}

fn walk_expr_for_leftmost(
    body: &Body,
    expr: ExprId,
    candidate: u32,
    field_name: &str,
) -> LeftmostWalk {
    if let ExprKind::FieldAccess {
        expr: inner,
        field_name: fname,
        ..
    } = &body.exprs[expr].kind
        && fname == field_name
        && let ExprKind::Local { index, .. } = &body.exprs[*inner].kind
        && *index == candidate
    {
        return LeftmostWalk::Found;
    }

    match &body.exprs[expr].kind {
        ExprKind::If { .. }
        | ExprKind::Match { .. }
        | ExprKind::Switch { .. }
        | ExprKind::LabeledBlock { .. }
        | ExprKind::Block(_) => LeftmostWalk::Blocked,

        ExprKind::Assign { target, value } => {
            let (target, value) = (*target, *value);
            match walk_assign_target(body, target, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                LeftmostWalk::Blocked => LeftmostWalk::Blocked,
                LeftmostWalk::Pure => {
                    match walk_expr_for_leftmost(body, value, candidate, field_name) {
                        LeftmostWalk::Found => LeftmostWalk::Found,
                        _ => LeftmostWalk::Blocked,
                    }
                }
            }
        }
        ExprKind::GlobalVarSet { value, .. } => {
            match walk_expr_for_leftmost(body, *value, candidate, field_name) {
                LeftmostWalk::Found => LeftmostWalk::Found,
                _ => LeftmostWalk::Blocked,
            }
        }
        ExprKind::Call { args, .. } => {
            let args: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
            walk_children_observable(body, args.into_iter(), candidate, field_name)
        }
        ExprKind::MethodCall { receiver, args, .. } => {
            let children: Vec<ExprId> = std::iter::once(*receiver)
                .chain(args.iter().map(|a| a.expr))
                .collect();
            walk_children_observable(body, children.into_iter(), candidate, field_name)
        }
        ExprKind::IndirectCall { callee, args } => {
            let children: Vec<ExprId> = std::iter::once(*callee)
                .chain(args.iter().copied())
                .collect();
            walk_children_observable(body, children.into_iter(), candidate, field_name)
        }
        ExprKind::CmRawCall { args, .. } => {
            let args = args.clone();
            walk_children_observable(body, args.into_iter(), candidate, field_name)
        }

        ExprKind::Binary { left, right, op } => {
            let (left, right) = (*left, *right);
            match op {
                // `&&` and `||` short-circuit: the right operand is
                // evaluated only when the left operand permits, so a
                // candidate use anchored in the right operand would
                // execute conditionally after substitution while the
                // original `let` ran unconditionally. Treat the right
                // operand the same way as an `if` branch — a Found there
                // must block elision, not anchor it.
                NirBinaryOp::And | NirBinaryOp::Or => {
                    match walk_expr_for_leftmost(body, left, candidate, field_name) {
                        LeftmostWalk::Found => LeftmostWalk::Found,
                        LeftmostWalk::Blocked => LeftmostWalk::Blocked,
                        LeftmostWalk::Pure => {
                            match walk_expr_for_leftmost(body, right, candidate, field_name) {
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
                    body,
                    [left, right].into_iter(),
                    candidate,
                    field_name,
                )),
                _ => walk_children_pure(body, [left, right].into_iter(), candidate, field_name),
            }
        }
        ExprKind::Unary { expr: inner, op } => {
            let inner = *inner;
            match op {
                // Deref may trap on a null receiver; the op itself is
                // observable.
                NirUnaryOp::Deref => {
                    observable_propagate(walk_expr_for_leftmost(body, inner, candidate, field_name))
                }
                // Arithmetic / logical / address-taking unaries are pure.
                NirUnaryOp::Neg | NirUnaryOp::Not | NirUnaryOp::BitNot => {
                    walk_expr_for_leftmost(body, inner, candidate, field_name)
                }
                NirUnaryOp::Ref | NirUnaryOp::MutRef => {
                    walk_expr_for_leftmost(body, inner, candidate, field_name)
                }
            }
        }
        // `as` lowers to `ref.cast` / numeric narrowing — both may trap.
        ExprKind::Cast { expr: inner, .. } => {
            observable_propagate(walk_expr_for_leftmost(body, *inner, candidate, field_name))
        }
        // FieldAccess on a non-candidate receiver: a fresh `struct.get`
        // on a possibly-null reference, so the op itself may trap. A
        // Found in the receiver still anchors at the receiver position
        // (the FieldAccess applies AFTER the substituted inner), but
        // a Pure receiver does NOT make this subtree Pure.
        ExprKind::FieldAccess { expr: inner, .. } => {
            observable_propagate(walk_expr_for_leftmost(body, *inner, candidate, field_name))
        }
        // `List<T>::index_value`-shaped Index may trap on a null base
        // and on OOB; the op itself is observable.
        ExprKind::Index { expr: inner, index } => {
            let (inner, index) = (*inner, *index);
            observable_propagate(walk_children_pure(
                body,
                [inner, index].into_iter(),
                candidate,
                field_name,
            ))
        }
        ExprKind::StructLiteral { fields, .. } => {
            let fields: Vec<ExprId> = fields.iter().map(|f| f.value).collect();
            walk_children_pure(body, fields.into_iter(), candidate, field_name)
        }
        ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            walk_children_pure(body, elements.into_iter(), candidate, field_name)
        }
        ExprKind::VariantConstruct { payload, .. } => match *payload {
            Some(p) => walk_expr_for_leftmost(body, p, candidate, field_name),
            None => LeftmostWalk::Pure,
        },
        ExprKind::ClosureToCanonical { functor, .. } => {
            walk_expr_for_leftmost(body, *functor, candidate, field_name)
        }
        // `VariantTag` / `VariantTest` / `VariantPayload` all read the
        // discriminant or payload via `ref.cast` + `struct.get` on a
        // possibly-null receiver; each may trap.
        ExprKind::VariantTag { expr: inner }
        | ExprKind::VariantTest { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. } => {
            observable_propagate(walk_expr_for_leftmost(body, *inner, candidate, field_name))
        }

        ExprKind::Local { .. }
        | ExprKind::GlobalVarGet { .. }
        | ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::StringLiteral(_)
        | ExprKind::BytesLiteral(_)
        | ExprKind::Null
        | ExprKind::Unit
        | ExprKind::EnumConstruct { .. } => LeftmostWalk::Pure,
    }
}

fn walk_assign_target(
    body: &Body,
    target: ExprId,
    candidate: u32,
    field_name: &str,
) -> LeftmostWalk {
    match &body.exprs[target].kind {
        ExprKind::Local { .. } => LeftmostWalk::Pure,
        ExprKind::FieldAccess { expr, .. } => {
            walk_expr_for_leftmost(body, *expr, candidate, field_name)
        }
        ExprKind::Index { expr, index } => {
            let (expr, index) = (*expr, *index);
            walk_children_pure(body, [expr, index].into_iter(), candidate, field_name)
        }
        ExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr,
        } => walk_expr_for_leftmost(body, *expr, candidate, field_name),
        _ => walk_expr_for_leftmost(body, target, candidate, field_name),
    }
}

fn walk_children_pure(
    body: &Body,
    children: impl Iterator<Item = ExprId>,
    candidate: u32,
    field_name: &str,
) -> LeftmostWalk {
    for c in children {
        match walk_expr_for_leftmost(body, c, candidate, field_name) {
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

fn walk_children_observable(
    body: &Body,
    children: impl Iterator<Item = ExprId>,
    candidate: u32,
    field_name: &str,
) -> LeftmostWalk {
    match walk_children_pure(body, children, candidate, field_name) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nir::{NirBinaryOp, NirUnaryOp};
    use crate::nir_arena::ExprNode;
    use crate::tir::TypeId;
    use crate::token::Span;

    /// Build an expression into a fresh arena and run the leftmost walker on it.
    fn leftmost(
        build: impl FnOnce(&mut Body) -> ExprId,
        candidate: u32,
        field_name: &str,
    ) -> LeftmostWalk {
        let mut body = Body::empty();
        let id = build(&mut body);
        walk_expr_for_leftmost(&body, id, candidate, field_name)
    }

    fn ty() -> TypeId {
        TypeId(0)
    }
    fn sp() -> Span {
        Span::default()
    }
    fn push(body: &mut Body, kind: ExprKind) -> ExprId {
        body.exprs.push(ExprNode {
            kind,
            type_id: ty(),
            span: sp(),
        })
    }
    fn local(body: &mut Body, index: u32) -> ExprId {
        push(
            body,
            ExprKind::Local {
                index,
                name: format!("__l{index}"),
            },
        )
    }
    fn int(body: &mut Body, v: i64) -> ExprId {
        push(
            body,
            ExprKind::IntLiteral {
                value: v as u64,
                repr: v.to_string(),
            },
        )
    }
    fn field(body: &mut Body, receiver: ExprId, name: &str) -> ExprId {
        push(
            body,
            ExprKind::FieldAccess {
                expr: receiver,
                field_index: 0,
                field_name: name.to_string(),
            },
        )
    }
    fn index(body: &mut Body, arr: ExprId, idx: ExprId) -> ExprId {
        push(
            body,
            ExprKind::Index {
                expr: arr,
                index: idx,
            },
        )
    }
    fn binary(body: &mut Body, op: NirBinaryOp, lhs: ExprId, rhs: ExprId) -> ExprId {
        push(
            body,
            ExprKind::Binary {
                left: lhs,
                right: rhs,
                op,
            },
        )
    }
    fn unary(body: &mut Body, op: NirUnaryOp, e: ExprId) -> ExprId {
        push(body, ExprKind::Unary { op, expr: e })
    }
    fn cast(body: &mut Body, e: ExprId, target: TypeId) -> ExprId {
        push(
            body,
            ExprKind::Cast {
                expr: e,
                target_type: target,
            },
        )
    }

    /// Use site `boxed.v + 0` — FieldAccess(Local(7), "v") is the LEFT
    /// operand of `+`, the leftmost evaluated subexpression. Walker
    /// must return Found.
    #[test]
    fn walker_finds_left_field_access() {
        let build = |b: &mut Body| {
            let l = local(b, 7);
            let f = field(b, l, "v");
            let i = int(b, 0);
            binary(b, NirBinaryOp::Add, f, i)
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Found));
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
        let build = |b: &mut Body| {
            let a1 = local(b, 1);
            let a2 = local(b, 2);
            let idx = index(b, a1, a2);
            let l = local(b, 7);
            let f = field(b, l, "v");
            binary(b, NirBinaryOp::Add, idx, f)
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Blocked));
    }

    /// Use site `(other.f) + boxed.v` — non-target `FieldAccess` may
    /// trap on a null receiver, so its Pure-children must not paint
    /// the surrounding context Pure.
    #[test]
    fn walker_blocks_when_non_target_field_precedes_field() {
        let build = |b: &mut Body| {
            let l2 = local(b, 2);
            let other = field(b, l2, "other");
            let l7 = local(b, 7);
            let f = field(b, l7, "v");
            binary(b, NirBinaryOp::Add, other, f)
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Blocked));
    }

    /// Use site `(x as i32) + boxed.v` — `Cast` may trap, observable.
    #[test]
    fn walker_blocks_when_cast_precedes_field() {
        let build = |b: &mut Body| {
            let l2 = local(b, 2);
            let c = cast(b, l2, ty());
            let l7 = local(b, 7);
            let f = field(b, l7, "v");
            binary(b, NirBinaryOp::Add, c, f)
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Blocked));
    }

    /// Use site `*p + boxed.v` — `Unary::Deref` may trap, observable.
    #[test]
    fn walker_blocks_when_deref_precedes_field() {
        let build = |b: &mut Body| {
            let l2 = local(b, 2);
            let d = unary(b, NirUnaryOp::Deref, l2);
            let l7 = local(b, 7);
            let f = field(b, l7, "v");
            binary(b, NirBinaryOp::Add, d, f)
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Blocked));
    }

    /// Use site `(a / b) + boxed.v` — `Binary::Div` may trap on zero
    /// divisor, observable.
    #[test]
    fn walker_blocks_when_div_precedes_field() {
        let build = |b: &mut Body| {
            let l1 = local(b, 1);
            let l2 = local(b, 2);
            let div = binary(b, NirBinaryOp::Div, l1, l2);
            let l7 = local(b, 7);
            let f = field(b, l7, "v");
            binary(b, NirBinaryOp::Add, div, f)
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Blocked));
    }

    /// Use site `boxed.v + arr[idx]` — observable on RIGHT side
    /// only. Walker still returns Found at the leftmost `FieldAccess`
    /// (left operand), since the observable Index runs AFTER the
    /// substituted inner expression.
    #[test]
    fn walker_allows_observable_after_field() {
        let build = |b: &mut Body| {
            let l7 = local(b, 7);
            let f = field(b, l7, "v");
            let a1 = local(b, 1);
            let a2 = local(b, 2);
            let idx = index(b, a1, a2);
            binary(b, NirBinaryOp::Add, f, idx)
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Found));
    }

    /// Use site `Cast(boxed.v)` — `FieldAccess` inside an observable
    /// op. Substitution is safe because the observable op runs
    /// AFTER the substituted inner.
    #[test]
    fn walker_finds_when_field_is_inside_observable() {
        let build = |b: &mut Body| {
            let l7 = local(b, 7);
            let f = field(b, l7, "v");
            cast(b, f, ty())
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Found));
    }

    /// Use site `cond && boxed.v` — the right operand of `&&` is
    /// conditionally evaluated; a Found in the right side must
    /// Block, not anchor.
    #[test]
    fn walker_blocks_field_in_right_of_and() {
        let build = |b: &mut Body| {
            let l2 = local(b, 2);
            let l7 = local(b, 7);
            let f = field(b, l7, "v");
            binary(b, NirBinaryOp::And, l2, f)
        };
        assert!(matches!(leftmost(build, 7, "v"), LeftmostWalk::Blocked));
    }
}
