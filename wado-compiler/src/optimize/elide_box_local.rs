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
use crate::nir_visitor::NirRefVisitor;
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
    let mut changed = false;
    while elide_in_block(body, &stats, &blacklist) {
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

fn elide_in_block(
    block: &mut NirBlock,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> bool {
    let mut changed = false;

    // Recurse into nested blocks first so inner-scope candidates get a
    // chance to fire before we scan this block's siblings.
    for stmt in &mut block.stmts {
        changed |= elide_in_nested_stmt(stmt, stats, blacklist);
    }

    let mut i = 0;
    while i < block.stmts.len() {
        if try_elide_at(&mut block.stmts, i, stats, blacklist) {
            changed = true;
        }
        i += 1;
    }

    changed
}

fn elide_in_nested_stmt(
    stmt: &mut NirStmt,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> bool {
    let mut changed = false;
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } => {
            changed |= elide_in_nested_expr(value, stats, blacklist);
        }
        NirStmtKind::LetDestructure { value, .. } => {
            changed |= elide_in_nested_expr(value, stats, blacklist);
        }
        NirStmtKind::Expr(e) => {
            changed |= elide_in_nested_expr(e, stats, blacklist);
        }
        NirStmtKind::Return { value: Some(v) } => {
            changed |= elide_in_nested_expr(v, stats, blacklist);
        }
        NirStmtKind::Break { value: Some(v), .. } => {
            changed |= elide_in_nested_expr(v, stats, blacklist);
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            changed |= elide_in_nested_expr(condition, stats, blacklist);
            changed |= elide_in_block(then_block, stats, blacklist);
            if let Some(eb) = else_block {
                changed |= elide_in_block(eb, stats, blacklist);
            }
        }
        NirStmtKind::Loop { body } => {
            changed |= elide_in_block(body, stats, blacklist);
        }
        NirStmtKind::LabeledBlock { block, .. } => {
            changed |= elide_in_block(block, stats, blacklist);
        }
        NirStmtKind::Return { value: None }
        | NirStmtKind::Break { value: None, .. }
        | NirStmtKind::Continue => {}
    }
    changed
}

fn elide_in_nested_expr(
    expr: &mut NirExpr,
    stats: &IndexMap<u32, LocalStats>,
    blacklist: &IndexSet<u32>,
) -> bool {
    let mut changed = false;
    match &mut expr.kind {
        NirExprKind::Block(block) => {
            changed |= elide_in_block(block, stats, blacklist);
        }
        NirExprKind::LabeledBlock { block, .. } => {
            changed |= elide_in_block(block, stats, blacklist);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            changed |= elide_in_nested_expr(condition, stats, blacklist);
            changed |= elide_in_block(then_branch, stats, blacklist);
            if let Some(eb) = else_branch {
                changed |= elide_in_block(eb, stats, blacklist);
            }
        }
        NirExprKind::Match { expr: scrut, arms } => {
            changed |= elide_in_nested_expr(scrut, stats, blacklist);
            for arm in arms {
                if let Some(g) = &mut arm.guard {
                    changed |= elide_in_nested_expr(g, stats, blacklist);
                }
                changed |= elide_in_nested_expr(&mut arm.body, stats, blacklist);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            changed |= elide_in_nested_expr(scrutinee, stats, blacklist);
            for arm in arms {
                changed |= elide_in_block(arm, stats, blacklist);
            }
            changed |= elide_in_block(default, stats, blacklist);
        }
        _ => {
            recurse_expr_children(expr, &mut |child| {
                changed |= elide_in_nested_expr(child, stats, blacklist);
            });
        }
    }
    changed
}

fn recurse_expr_children<F: FnMut(&mut NirExpr)>(expr: &mut NirExpr, f: &mut F) {
    match &mut expr.kind {
        NirExprKind::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        NirExprKind::Unary { expr: inner, .. } => f(inner),
        NirExprKind::Cast { expr: inner, .. } => f(inner),
        NirExprKind::Assign { target, value } => {
            f(target);
            f(value);
        }
        NirExprKind::GlobalVarSet { value, .. } => f(value),
        NirExprKind::Call { args, .. } => {
            for a in args {
                f(&mut a.expr);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            f(receiver);
            for a in args {
                f(&mut a.expr);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            f(callee);
            for a in args {
                f(a);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for a in args {
                f(a);
            }
        }
        NirExprKind::FieldAccess { expr: inner, .. } => f(inner),
        NirExprKind::Index { expr: inner, index } => {
            f(inner);
            f(index);
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for fld in fields {
                f(&mut fld.value);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for e in elements {
                f(e);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                f(p);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => f(functor),
        NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. } => f(inner),
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::Local { .. }
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. }
        | NirExprKind::Block(_)
        | NirExprKind::LabeledBlock { .. }
        | NirExprKind::If { .. }
        | NirExprKind::Match { .. }
        | NirExprKind::Switch { .. } => {}
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

fn substitute_first_use(
    stmt: &mut NirStmt,
    candidate: u32,
    field_name: &str,
    replacement: NirExpr,
) {
    let mut slot = Some(replacement);
    sub_in_stmt(stmt, candidate, field_name, &mut slot);
}

fn sub_in_stmt(stmt: &mut NirStmt, candidate: u32, field_name: &str, slot: &mut Option<NirExpr>) {
    if slot.is_none() {
        return;
    }
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } => sub_in_expr(value, candidate, field_name, slot),
        NirStmtKind::LetDestructure { value, .. } => {
            sub_in_expr(value, candidate, field_name, slot)
        }
        NirStmtKind::Expr(e) => sub_in_expr(e, candidate, field_name, slot),
        NirStmtKind::Return { value: Some(v) } | NirStmtKind::Break { value: Some(v), .. } => {
            sub_in_expr(v, candidate, field_name, slot)
        }
        _ => {}
    }
}

fn sub_in_expr(expr: &mut NirExpr, candidate: u32, field_name: &str, slot: &mut Option<NirExpr>) {
    if slot.is_none() {
        return;
    }
    if let NirExprKind::FieldAccess {
        expr: inner,
        field_name: fname,
        ..
    } = &expr.kind
        && fname == field_name
        && let NirExprKind::Local { index, .. } = &inner.kind
        && *index == candidate
    {
        *expr = slot.take().unwrap();
        return;
    }
    recurse_expr_children(expr, &mut |child| {
        if slot.is_some() {
            sub_in_expr(child, candidate, field_name, slot);
        }
    });
}
