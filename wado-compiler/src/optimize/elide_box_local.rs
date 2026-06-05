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
use crate::nir::{
    NirBinaryOp, NirBlock, NirExpr, NirExprKind, NirFunction, NirPattern, NirStmt, NirStmtKind,
    NirUnaryOp,
};
use crate::nir_package::NirPackage;
use crate::nir_visitor::{NirMutVisitor, NirOptVisitor, NirRefVisitor, opt_walk_block};
use crate::token::Span;

use super::mod_ref::{ModRef, can_move_past};

pub fn elide_adjacent_box_locals(project: &mut NirPackage) -> bool {
    let mut changed = false;
    let funcs = project.functions.clone();
    for func_rc in &funcs {
        let mut func = func_rc.borrow_mut();
        if elide_in_function(&mut func) {
            changed = true;
        }
    }
    changed
}

fn elide_in_function(func: &mut NirFunction) -> bool {
    if func.body.is_none() {
        return false;
    }

    // Identity / aliasing safety: locals whose reference could escape
    // via callee-side storage are off-limits.
    let mut blacklist: IndexSet<u32> = IndexSet::default();
    blacklist.extend(func.address_taken_locals.iter().copied());
    blacklist.extend(func.stores_aliased_locals.iter().copied());

    let mut owned = func.body_block().unwrap();
    let body = &mut owned;
    let stats = collect_local_stats(body);
    let mut elider = Elider {
        stats: &stats,
        blacklist: &blacklist,
    };
    let mut changed = false;
    while elider.visit_block(body) {
        changed = true;
    }
    func.set_body_block(owned);
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

fn collect_local_stats(body: &NirBlock) -> IndexMap<u32, LocalStats> {
    let mut stats: IndexMap<u32, LocalStats> = IndexMap::default();
    let mut collector = StatsCollector { stats: &mut stats };
    collector.visit_block(body);
    stats
}

struct StatsCollector<'a> {
    stats: &'a mut IndexMap<u32, LocalStats>,
}

impl NirRefVisitor for StatsCollector<'_> {
    fn visit_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => {
                if let NirExprKind::Local { index, .. } = &inner.kind {
                    let s = self.stats.entry(*index).or_default();
                    s.total_reads += 1;
                    s.fieldaccess_reads += 1;
                    s.field_names.insert(field_name.clone());
                    return;
                }
                self.walk_expr(expr);
            }
            NirExprKind::Local { index, .. } => {
                self.stats.entry(*index).or_default().total_reads += 1;
            }
            NirExprKind::Assign { target, value } => {
                if let NirExprKind::Local { index, .. } = &target.kind {
                    self.stats.entry(*index).or_default().defs += 1;
                    self.visit_expr(value);
                    return;
                }
                self.walk_expr(expr);
            }
            _ => self.walk_expr(expr),
        }
    }

    fn visit_stmt(&mut self, stmt: &NirStmt) {
        match &stmt.kind {
            NirStmtKind::Let {
                local_index, value, ..
            } => {
                self.stats.entry(*local_index).or_default().defs += 1;
                self.visit_expr(value);
            }
            NirStmtKind::LetDestructure { pattern, value, .. } => {
                record_pattern_defs(pattern, self.stats);
                self.visit_expr(value);
            }
            _ => self.walk_stmt(stmt),
        }
    }
}

fn record_pattern_defs(pat: &NirPattern, stats: &mut IndexMap<u32, LocalStats>) {
    match pat {
        NirPattern::Binding { local_index, .. } => {
            stats.entry(*local_index).or_default().defs += 1;
        }
        NirPattern::Tuple(patterns, _) => {
            for p in patterns {
                record_pattern_defs(p, stats);
            }
        }
        NirPattern::Variant { bindings, .. } => {
            for p in bindings {
                record_pattern_defs(p, stats);
            }
        }
        NirPattern::Struct { fields, .. } => {
            for f in fields {
                record_pattern_defs(&f.pattern, stats);
            }
        }
        NirPattern::Or(alts) => {
            for a in alts {
                record_pattern_defs(a, stats);
            }
        }
        _ => {}
    }
}

// -----------------------------------------------------------------------
// Elision driver
// -----------------------------------------------------------------------

/// Drives the elision pass via [`NirOptVisitor`]. The default `visit_expr` /
/// `visit_stmt` recursion handles all the boilerplate descent into nested
/// blocks; the only override is `visit_block`, which adds a sibling-window
/// scan AFTER its children have run (so inner-scope candidates fire before
/// we try outer-scope ones).
struct Elider<'a> {
    stats: &'a IndexMap<u32, LocalStats>,
    blacklist: &'a IndexSet<u32>,
}

impl NirOptVisitor for Elider<'_> {
    fn visit_block(&mut self, block: &mut NirBlock) -> bool {
        let mut changed = opt_walk_block(self, block);
        let mut i = 0;
        while i < block.stmts.len() {
            if try_elide_at(&mut block.stmts, i, self.stats, self.blacklist) {
                changed = true;
            }
            i += 1;
        }
        changed
    }
}

// -----------------------------------------------------------------------
// Candidate detection & substitution
// -----------------------------------------------------------------------

fn try_elide_at(
    stmts: &mut [NirStmt],
    i: usize,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> bool {
    let Some((candidate, field_name, inner_mr)) = describe_candidate(&stmts[i], stats, blacklist)
    else {
        return false;
    };

    let Some(j) = find_use_site(stmts, i + 1, candidate, &field_name, &inner_mr) else {
        return false;
    };

    let inner = take_candidate_inner(&mut stmts[i]);
    substitute_first_use(&mut stmts[j], candidate, &field_name, inner);
    true
}

fn describe_candidate(
    stmt: &NirStmt,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> Option<(u32, String, ModRef)> {
    let NirStmtKind::Let {
        local_index, value, ..
    } = &stmt.kind
    else {
        return None;
    };
    if blacklist.contains(local_index) {
        return None;
    }
    let s = stats.get(local_index)?;
    if s.defs != 1 || s.fieldaccess_reads != 1 || s.total_reads != 1 {
        return None;
    }
    if s.field_names.len() != 1 {
        return None;
    }
    let NirExprKind::StructLiteral { fields, .. } = &value.kind else {
        return None;
    };
    if fields.len() != 1 {
        return None;
    }
    let field_name = s.field_names.iter().next().unwrap().clone();
    let inner_mr = ModRef::of_expr(&fields[0].value);
    Some((*local_index, field_name, inner_mr))
}

fn take_candidate_inner(stmt: &mut NirStmt) -> NirExpr {
    let placeholder = NirStmt::new(NirStmtKind::Expr(unit_expr(stmt.span)), stmt.span);
    let taken = std::mem::replace(stmt, placeholder);
    let NirStmtKind::Let { value, .. } = taken.kind else {
        unreachable!("guarded by describe_candidate");
    };
    let NirExprKind::StructLiteral { mut fields, .. } = value.kind else {
        unreachable!("guarded by describe_candidate");
    };
    fields.remove(0).value
}

fn unit_expr(span: Span) -> NirExpr {
    // The placeholder must carry the actual Unit type so downstream passes
    // (`infer_stmts_result_type`, `branch_prune`, `wir_build::translate_stmt`'s
    // `Drop` wrap heuristic) recognise it as a void expression. `TypeId(0)`
    // is `I8` (`tir.rs:529`) — using it would tag the Unit placeholder as
    // an i8 value and tempt downstream code to emit a stray `drop`.
    NirExpr::new(NirExprKind::Unit, crate::tir::TypeTable::UNIT, span)
}

fn find_use_site(
    stmts: &[NirStmt],
    from: usize,
    candidate: u32,
    field_name: &str,
    inner_mr: &ModRef,
) -> Option<usize> {
    let mut k = from;
    while k < stmts.len() {
        let stmt = &stmts[k];
        if is_placeholder(stmt) {
            k += 1;
            continue;
        }
        if matches!(
            walk_stmt_for_leftmost(stmt, candidate, field_name),
            LeftmostWalk::Found
        ) {
            return Some(k);
        }
        let int_mr = ModRef::of_stmt(stmt);
        if !can_move_past(inner_mr, &int_mr, candidate) {
            return None;
        }
        k += 1;
    }
    None
}

fn is_placeholder(stmt: &NirStmt) -> bool {
    matches!(
        &stmt.kind,
        NirStmtKind::Expr(e) if matches!(e.kind, NirExprKind::Unit)
    )
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

/// Replaces the first `FieldAccess(Local(candidate), field_name)` reached
/// in eval order with the saved `replacement` (consumed at most once).
/// Drives the traversal via [`NirMutVisitor`]: each `visit_expr` either
/// performs the replacement (consuming `slot`) or recurses through
/// `walk_expr`, which already enumerates children in evaluation order.
/// Once `slot` is `None` the visitor short-circuits.
struct Substituter<'a> {
    candidate: u32,
    field_name: &'a str,
    slot: Option<NirExpr>,
}

impl NirMutVisitor for Substituter<'_> {
    fn visit_expr(&mut self, expr: &mut NirExpr) {
        if self.slot.is_none() {
            return;
        }
        if let NirExprKind::FieldAccess {
            expr: inner,
            field_name: fname,
            ..
        } = &expr.kind
            && fname == self.field_name
            && let NirExprKind::Local { index, .. } = &inner.kind
            && *index == self.candidate
        {
            // Preserve the outer FieldAccess's `type_id` and `span`.
            // The candidate's inner expression and the field access
            // share the same declared type, but the field-access node
            // is the one downstream passes have type-resolved against
            // (post-monomorphization type registries are keyed by the
            // node's `type_id`). Replacing the whole `NirExpr` would
            // swap in the slot's `type_id`, which is structurally
            // equal but identity-distinct from the field-access type
            // and can drift through generic specialization.
            let slot = self.slot.take().unwrap();
            *expr = NirExpr::new(slot.kind, expr.type_id, expr.span);
            return;
        }
        self.walk_expr(expr);
    }
}

fn substitute_first_use(
    stmt: &mut NirStmt,
    candidate: u32,
    field_name: &str,
    replacement: NirExpr,
) {
    let mut sub = Substituter {
        candidate,
        field_name,
        slot: Some(replacement),
    };
    sub.visit_stmt(stmt);
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
