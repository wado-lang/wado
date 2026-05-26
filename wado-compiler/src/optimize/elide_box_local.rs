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
    NirBlock, NirExpr, NirExprKind, NirFunction, NirPattern, NirStmt, NirStmtKind, NirUnaryOp,
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

    let body = func.body.as_mut().unwrap();
    let stats = collect_local_stats(body);
    let mut elider = Elider {
        stats: &stats,
        blacklist: &blacklist,
    };
    let mut changed = false;
    while elider.visit_block(body) {
        changed = true;
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
    NirExpr::new(NirExprKind::Unit, crate::tir::TypeId(0), span)
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

        NirExprKind::Binary { left, right, .. } => walk_children_pure(
            [left.as_ref(), right.as_ref()].into_iter(),
            candidate,
            field_name,
        ),
        NirExprKind::Unary { expr: inner, .. } | NirExprKind::Cast { expr: inner, .. } => {
            walk_expr_for_leftmost(inner, candidate, field_name)
        }
        NirExprKind::FieldAccess { expr: inner, .. } => {
            walk_expr_for_leftmost(inner, candidate, field_name)
        }
        NirExprKind::Index { expr: inner, index } => walk_children_pure(
            [inner.as_ref(), index.as_ref()].into_iter(),
            candidate,
            field_name,
        ),
        NirExprKind::StructLiteral { fields, .. } => {
            walk_children_pure(fields.iter().map(|f| &f.value), candidate, field_name)
        }
        NirExprKind::TupleLiteral { elements } => {
            walk_children_pure(elements.iter(), candidate, field_name)
        }
        NirExprKind::VariantConstruct { payload, .. } => match payload {
            Some(p) => walk_expr_for_leftmost(p, candidate, field_name),
            None => LeftmostWalk::Pure,
        },
        NirExprKind::ClosureToCanonical { functor, .. } => {
            walk_expr_for_leftmost(functor, candidate, field_name)
        }
        NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => {
            walk_expr_for_leftmost(inner, candidate, field_name)
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
            LeftmostWalk::Pure => continue,
        }
    }
    LeftmostWalk::Pure
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
            *expr = self.slot.take().unwrap();
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
