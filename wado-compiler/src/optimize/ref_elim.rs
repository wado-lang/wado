//! Reference elimination optimization for Wado TIR
//!
//! Eliminates unnecessary reference bindings introduced during function inlining.
//! After inlining, we often have patterns like:
//!
//! ```text
//! let self: &Array<T> = &arr;
//! ... self.repr ...
//! ```
//!
//! This can be optimized to:
//!
//! ```text
//! ... arr.repr ...
//! ```
//!
//! The pass also handles bindings whose source is a field-access chain
//! (`let r: &T = &v.f1.f2`), substituting the chain at each `r.field` use.
//! This is what `inline` produces for `&self.tokens.len()`-style calls after
//! the body's `self.used` is rewritten in terms of the inlined receiver.
//!
//! The algorithm uses a two-pass approach that processes ALL ref bindings
//! simultaneously, avoiding the O(K × N) cost of processing each binding
//! separately (where K = number of bindings, N = body size).
//!
//! Pass 1 (analyze): Single traversal to collect all `let r = &v` bindings
//!   and classify every use of each `r` as field-access-only or not.
//! Pass 2 (transform): Single traversal to replace eliminable field accesses
//!   and remove dead let statements.

use crate::hashmap::{IndexMap, IndexSet};
use crate::nir::{NirBlock, NirExpr, NirExprKind, NirFunction, NirStmt, NirStmtKind, NirUnaryOp};
use crate::nir_package::NirPackage;
use crate::tir::TypeTable;

/// Per-binding analysis state, keyed by the ref local index.
struct RefInfo {
    /// The expression `E` from `let r = &E` (or the resolved referent of a
    /// transitive `let r = s` shadow). Must be a chain of `FieldAccess`
    /// bottoming out at a `Local`, so it's safe to clone at each use site.
    referent: NirExpr,
    /// True until a non-field-access use is found
    eliminable: bool,
}

/// An expression is a valid referent if it's a pure read of a local — either
/// a bare `Local` or a chain of `FieldAccess` bottoming out at one. Restricting
/// to this shape keeps substitution cheap (duplicates only struct.get) and
/// observably equivalent (no side effects, no method calls).
fn is_valid_referent(expr: &NirExpr) -> bool {
    match &expr.kind {
        NirExprKind::Local { .. } => true,
        NirExprKind::FieldAccess { expr: inner, .. } => is_valid_referent(inner),
        _ => false,
    }
}

/// Resolve any `Local(idx)` in `expr` whose binding is already tracked, by
/// splicing in the tracked binding's referent. Eliminable bindings are dropped
/// in pass 2; without this resolution, a chained pattern like
/// `let r1 = &v; let r2 = &r1.field; ... r2.x ...` would substitute `r2.x`
/// to `(Local(r1)).field.x` only to find `r1`'s `let` removed underneath it.
/// Pre-resolving at registration time keeps Pass 2 a single substitution.
fn resolve_referent(expr: &NirExpr, refs: &IndexMap<u32, RefInfo>) -> NirExpr {
    match &expr.kind {
        NirExprKind::Local { index, .. } => {
            if let Some(info) = refs.get(index) {
                info.referent.clone()
            } else {
                expr.clone()
            }
        }
        NirExprKind::FieldAccess {
            expr: inner,
            field_index,
            field_name,
        } => {
            let resolved_inner = resolve_referent(inner, refs);
            NirExpr {
                kind: NirExprKind::FieldAccess {
                    expr: Box::new(resolved_inner),
                    field_index: *field_index,
                    field_name: field_name.clone(),
                },
                type_id: expr.type_id,
                span: expr.span,
            }
        }
        _ => expr.clone(),
    }
}

/// Pass 1: Collect all ref bindings and analyze uses in a single traversal.
///
/// Walks the entire function body once, building an `IndexMap<u32, RefInfo>` for
/// every binding the pass can eliminate when all uses are field-access-only:
///
/// 1. `let r: &T = &E` / `let r: &mut T = &mut E` — fresh reference to a
///    pure-read referent. `E` must be a `Local` or a chain of `FieldAccess`
///    bottoming out at a `Local`. Replace `r.field` with `<E>.field` and drop
///    the binding. For `E = Local`, this is the classic `let r = &v` shape;
///    for `E = self.tokens`, this is what survives after `inline` expands a
///    method body like `Array::len(&self.tokens)` into a labeled block.
///
/// 2. `let r: &mut T = s` (or `&T`) where `s` is itself a reference-typed
///    local **already tracked in `refs`** — the inlined shadow pattern,
///    e.g. the `let self = self;` that appears at the entry of an inlined
///    `&mut self` method body. Copy `s`'s referent into `r` so `r.field`
///    resolves to the same root.
///
///    If `s` is not tracked yet we leave `r` un-tracked. The pass walks
///    statements in source order, so by the time we encounter
///    `let r = s` any earlier `let s = &v` is already in `refs`; an
///    `s` that becomes tracked later cannot retroactively make `r`
///    eligible because `r`'s uses have already been classified.
///
/// For every expression that uses a tracked local, classifies the use as
/// field-access-only or not. Any non-field-access use marks the binding as
/// non-eliminable.
fn analyze_refs_in_block(
    block: &NirBlock,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    for stmt in &block.stmts {
        // Check if this statement defines a new ref binding
        if let NirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && !rebound.contains(local_index)
        {
            // Pattern (1): `let r = &E` / `let r = &mut E` where E is a
            // pure-read referent (Local or FieldAccess chain). Resolve any
            // tracked Locals in `E` to their referents up front so chained
            // shadows survive Pass 2's drop of intermediate bindings.
            if let NirExprKind::Unary { op, expr } = &value.kind
                && matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
                && is_valid_referent(expr)
            {
                refs.insert(
                    *local_index,
                    RefInfo {
                        referent: resolve_referent(expr, refs),
                        eliminable: true,
                    },
                );
            }
            // Pattern (2): `let r = s` where s is itself a tracked ref local
            // (the inlined shadow). Resolve transitively to s's referent so
            // `r.field` can be replaced with `<root>.field` directly.
            else if let NirExprKind::Local { index, .. } = &value.kind {
                let resolved = refs.get(index).map(|info| RefInfo {
                    referent: info.referent.clone(),
                    eliminable: info.eliminable,
                });
                if let Some(info) = resolved {
                    refs.insert(*local_index, info);
                }
            }
        }
        // Analyze uses within this statement
        analyze_uses_in_stmt(stmt, rebound, refs);
    }
}

/// Walk the function body and count `Let` statements per local index.
/// Locals defined by more than one `Let` are returned in the set —
/// `analyze_refs_in_block` skips Pattern 1/2 registration for them.
fn find_rebound_locals(block: &NirBlock) -> IndexSet<u32> {
    let mut seen = IndexSet::default();
    let mut rebound = IndexSet::default();
    collect_let_indices_in_block(block, &mut seen, &mut rebound);
    rebound
}

fn collect_let_indices_in_block(
    block: &NirBlock,
    seen: &mut IndexSet<u32>,
    rebound: &mut IndexSet<u32>,
) {
    for stmt in &block.stmts {
        collect_let_indices_in_stmt(stmt, seen, rebound);
    }
}

fn collect_let_indices_in_stmt(
    stmt: &NirStmt,
    seen: &mut IndexSet<u32>,
    rebound: &mut IndexSet<u32>,
) {
    match &stmt.kind {
        NirStmtKind::Let { local_index, value, .. } => {
            if !seen.insert(*local_index) {
                rebound.insert(*local_index);
            }
            collect_let_indices_in_expr(value, seen, rebound);
        }
        NirStmtKind::LetDestructure { value, .. } => {
            collect_let_indices_in_expr(value, seen, rebound);
        }
        NirStmtKind::Expr(e) => collect_let_indices_in_expr(e, seen, rebound),
        NirStmtKind::Return { value } | NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_let_indices_in_expr(v, seen, rebound);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_let_indices_in_expr(condition, seen, rebound);
            collect_let_indices_in_block(then_block, seen, rebound);
            if let Some(eb) = else_block {
                collect_let_indices_in_block(eb, seen, rebound);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            collect_let_indices_in_block(body, seen, rebound);
        }
        NirStmtKind::Continue => {}
    }
}

fn collect_let_indices_in_expr(
    expr: &NirExpr,
    seen: &mut IndexSet<u32>,
    rebound: &mut IndexSet<u32>,
) {
    match &expr.kind {
        NirExprKind::Block(b) | NirExprKind::LabeledBlock { block: b, .. } => {
            collect_let_indices_in_block(b, seen, rebound);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_let_indices_in_expr(condition, seen, rebound);
            collect_let_indices_in_block(then_branch, seen, rebound);
            if let Some(eb) = else_branch {
                collect_let_indices_in_block(eb, seen, rebound);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            collect_let_indices_in_expr(inner, seen, rebound);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    collect_let_indices_in_expr(g, seen, rebound);
                }
                collect_let_indices_in_expr(&arm.body, seen, rebound);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_let_indices_in_expr(scrutinee, seen, rebound);
            for arm in arms {
                collect_let_indices_in_block(arm, seen, rebound);
            }
            collect_let_indices_in_block(default, seen, rebound);
        }
        NirExprKind::Binary { left, right, .. } => {
            collect_let_indices_in_expr(left, seen, rebound);
            collect_let_indices_in_expr(right, seen, rebound);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::FieldAccess { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            collect_let_indices_in_expr(inner, seen, rebound);
        }
        NirExprKind::Assign { target, value } => {
            collect_let_indices_in_expr(target, seen, rebound);
            collect_let_indices_in_expr(value, seen, rebound);
        }
        NirExprKind::Index { expr: inner, index } => {
            collect_let_indices_in_expr(inner, seen, rebound);
            collect_let_indices_in_expr(index, seen, rebound);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                collect_let_indices_in_expr(&arg.expr, seen, rebound);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_let_indices_in_expr(arg, seen, rebound);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            collect_let_indices_in_expr(receiver, seen, rebound);
            for arg in args {
                collect_let_indices_in_expr(&arg.expr, seen, rebound);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            collect_let_indices_in_expr(callee, seen, rebound);
            for arg in args {
                collect_let_indices_in_expr(arg, seen, rebound);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            collect_let_indices_in_expr(functor, seen, rebound);
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for f in fields {
                collect_let_indices_in_expr(&f.value, seen, rebound);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for e in elements {
                collect_let_indices_in_expr(e, seen, rebound);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                collect_let_indices_in_expr(p, seen, rebound);
            }
        }
        // Leaf nodes / nothing to descend into
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
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

fn analyze_uses_in_stmt(
    stmt: &NirStmt,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => analyze_uses_in_expr(value, rebound, refs),
        NirStmtKind::Expr(expr) => analyze_uses_in_expr(expr, rebound, refs),
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                analyze_uses_in_expr(v, rebound, refs);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            analyze_uses_in_expr(condition, rebound, refs);
            analyze_refs_in_block(then_block, rebound, refs);
            if let Some(eb) = else_block {
                analyze_refs_in_block(eb, rebound, refs);
            }
        }
        NirStmtKind::Loop { body } => analyze_refs_in_block(body, rebound, refs),
        NirStmtKind::LabeledBlock { block, .. } => analyze_refs_in_block(block, rebound, refs),
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                analyze_uses_in_expr(v, rebound, refs);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => analyze_uses_in_expr(value, rebound, refs),
    }
}

fn analyze_uses_in_expr(
    expr: &NirExpr,
    rebound: &IndexSet<u32>,
    refs: &mut IndexMap<u32, RefInfo>,
) {
    match &expr.kind {
        // Field access on a tracked ref local: this is the pattern we want to optimize.
        // The use is acceptable (field-access-only), so we DON'T mark it as non-eliminable.
        NirExprKind::FieldAccess { expr: inner, .. } => {
            if let NirExprKind::Local { index, .. } = &inner.kind
                && refs.contains_key(index)
            {
                // This is a field access on a tracked ref - acceptable use, skip.
                return;
            }
            // Not a field access on tracked ref - recurse normally
            analyze_uses_in_expr(inner, rebound, refs);
        }
        // Direct use of a tracked ref local (not through field access): non-eliminable
        NirExprKind::Local { index, .. } => {
            if let Some(info) = refs.get_mut(index) {
                info.eliminable = false;
            }
        }
        // Recursive cases
        NirExprKind::Binary { left, right, .. } => {
            analyze_uses_in_expr(left, rebound, refs);
            analyze_uses_in_expr(right, rebound, refs);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            analyze_uses_in_expr(inner, rebound, refs);
        }
        NirExprKind::Assign { target, value } => {
            analyze_uses_in_expr(target, rebound, refs);
            analyze_uses_in_expr(value, rebound, refs);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                analyze_uses_in_expr(&arg.expr, rebound, refs);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                analyze_uses_in_expr(arg, rebound, refs);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            analyze_uses_in_expr(receiver, rebound, refs);
            for arg in args {
                analyze_uses_in_expr(&arg.expr, rebound, refs);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            analyze_uses_in_expr(callee, rebound, refs);
            for arg in args {
                analyze_uses_in_expr(arg, rebound, refs);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            analyze_uses_in_expr(functor, rebound, refs);
        }
        NirExprKind::Index { expr: inner, index } => {
            analyze_uses_in_expr(inner, rebound, refs);
            analyze_uses_in_expr(index, rebound, refs);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            analyze_refs_in_block(block, rebound, refs);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_uses_in_expr(condition, rebound, refs);
            analyze_refs_in_block(then_branch, rebound, refs);
            if let Some(eb) = else_branch {
                analyze_refs_in_block(eb, rebound, refs);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_uses_in_expr(&field.value, rebound, refs);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                analyze_uses_in_expr(elem, rebound, refs);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                analyze_uses_in_expr(payload_expr, rebound, refs);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            analyze_uses_in_expr(inner, rebound, refs);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    analyze_uses_in_expr(guard, rebound, refs);
                }
                analyze_uses_in_expr(&arm.body, rebound, refs);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            analyze_uses_in_expr(scrutinee, rebound, refs);
            for arm in arms {
                analyze_refs_in_block(arm, rebound, refs);
            }
            analyze_refs_in_block(default, rebound, refs);
        }
        // Leaf nodes
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Pass 2: Replace field accesses and remove dead bindings in a single traversal.
///
/// For each eliminable binding `let r = &v`, replaces `r.field` with `v.field`
/// and removes the dead `let r` statement.
fn transform_block(block: &mut NirBlock, eliminable: &IndexMap<u32, RefInfo>) {
    // Remove dead let statements for eliminable bindings
    block.stmts.retain(|stmt| {
        if let NirStmtKind::Let { local_index, .. } = &stmt.kind {
            !eliminable.contains_key(local_index)
        } else {
            true
        }
    });

    for stmt in &mut block.stmts {
        transform_stmt(stmt, eliminable);
    }
}

fn transform_stmt(stmt: &mut NirStmt, eliminable: &IndexMap<u32, RefInfo>) {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } => transform_expr(value, eliminable),
        NirStmtKind::Expr(expr) => transform_expr(expr, eliminable),
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                transform_expr(v, eliminable);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            transform_expr(condition, eliminable);
            transform_block(then_block, eliminable);
            if let Some(eb) = else_block {
                transform_block(eb, eliminable);
            }
        }
        NirStmtKind::Loop { body } => transform_block(body, eliminable),
        NirStmtKind::LabeledBlock { block, .. } => transform_block(block, eliminable),
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                transform_expr(v, eliminable);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => transform_expr(value, eliminable),
    }
}

fn transform_expr(expr: &mut NirExpr, eliminable: &IndexMap<u32, RefInfo>) {
    match &mut expr.kind {
        NirExprKind::FieldAccess { expr: inner, .. } => {
            // Check if the inner expression is a local that should be replaced
            if let NirExprKind::Local { index, .. } = &inner.kind
                && let Some(info) = eliminable.get(index)
            {
                // Replace `r` with the referent expression. For `let r = &v`
                // the referent is `Local(v)`; for `let r = &v.f` it's
                // `FieldAccess(Local(v), "f")`. We swap only the `kind` and
                // keep `inner.type_id`/`inner.span` — the surrounding code
                // (the outer `FieldAccess.field_index` we're inside, plus
                // any downstream consumers) was sized to the ref-type tag
                // that `r` had at this position, and changing it here would
                // ripple incorrect types into codegen.
                inner.kind = info.referent.clone().kind;
                return;
            }
            transform_expr(inner, eliminable);
        }
        NirExprKind::Binary { left, right, .. } => {
            transform_expr(left, eliminable);
            transform_expr(right, eliminable);
        }
        NirExprKind::Unary { expr: inner, .. }
        | NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            transform_expr(inner, eliminable);
        }
        NirExprKind::Assign { target, value } => {
            transform_expr(target, eliminable);
            transform_expr(value, eliminable);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                transform_expr(&mut arg.expr, eliminable);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                transform_expr(arg, eliminable);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            transform_expr(receiver, eliminable);
            for arg in args {
                transform_expr(&mut arg.expr, eliminable);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            transform_expr(callee, eliminable);
            for arg in args {
                transform_expr(arg, eliminable);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            transform_expr(functor, eliminable);
        }
        NirExprKind::Index { expr: inner, index } => {
            transform_expr(inner, eliminable);
            transform_expr(index, eliminable);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            transform_block(block, eliminable);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            transform_expr(condition, eliminable);
            transform_block(then_branch, eliminable);
            if let Some(eb) = else_branch {
                transform_block(eb, eliminable);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                transform_expr(&mut field.value, eliminable);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                transform_expr(elem, eliminable);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                transform_expr(payload_expr, eliminable);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            transform_expr(inner, eliminable);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    transform_expr(guard, eliminable);
                }
                transform_expr(&mut arm.body, eliminable);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            transform_expr(scrutinee, eliminable);
            for arm in arms {
                transform_block(arm, eliminable);
            }
            transform_block(default, eliminable);
        }
        // Leaf nodes
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
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Tracking state for `let r = &struct_or_tuple_literal` where r is only used as `*r`.
struct DerefOnlyRef {
    /// The source expression (struct or tuple literal) from `let r = &source_expr`
    source_expr: NirExpr,
    /// True until a non-deref use is found
    eliminable: bool,
    /// Number of times r is used as `*r`
    use_count: u32,
}

/// Collect `let r = &StructLiteral` / `let r = &TupleLiteral` candidates.
fn collect_deref_only_refs_in_block(block: &NirBlock, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    for stmt in &block.stmts {
        if let NirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let NirExprKind::Unary { op, expr } = &value.kind
            && matches!(op, NirUnaryOp::Ref | NirUnaryOp::MutRef)
            && matches!(
                expr.kind,
                NirExprKind::StructLiteral { .. } | NirExprKind::TupleLiteral { .. }
            )
        {
            refs.insert(
                *local_index,
                DerefOnlyRef {
                    source_expr: *expr.clone(),
                    eliminable: true,
                    use_count: 0,
                },
            );
        }
        check_deref_only_uses_in_stmt(stmt, refs);
    }
}

fn check_deref_only_uses_in_stmt(stmt: &NirStmt, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    match &stmt.kind {
        NirStmtKind::Let { value, .. } => check_deref_only_uses_in_expr(value, refs),
        NirStmtKind::Expr(expr) => check_deref_only_uses_in_expr(expr, refs),
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                check_deref_only_uses_in_expr(v, refs);
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            check_deref_only_uses_in_expr(condition, refs);
            collect_deref_only_refs_in_block(then_block, refs);
            if let Some(eb) = else_block {
                collect_deref_only_refs_in_block(eb, refs);
            }
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            collect_deref_only_refs_in_block(body, refs);
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                check_deref_only_uses_in_expr(v, refs);
            }
        }
        NirStmtKind::Continue => {}
        NirStmtKind::LetDestructure { value, .. } => check_deref_only_uses_in_expr(value, refs),
    }
}

fn check_deref_only_uses_in_expr(expr: &NirExpr, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    match &expr.kind {
        // `*r` where r is a deref-only candidate: acceptable use
        NirExprKind::Unary {
            op: NirUnaryOp::Deref,
            expr: inner,
        } => {
            if let NirExprKind::Local { index, .. } = &inner.kind
                && let Some(info) = refs.get_mut(index)
            {
                info.use_count += 1;
                return;
            }
            check_deref_only_uses_in_expr(inner, refs);
        }
        // Any other use of r (bare local, field access on ref, etc.): disqualify
        NirExprKind::Local { index, .. } => {
            if let Some(info) = refs.get_mut(index) {
                info.eliminable = false;
            }
        }
        // Recurse into subexpressions
        NirExprKind::Unary { expr: inner, .. } => check_deref_only_uses_in_expr(inner, refs),
        NirExprKind::Binary { left, right, .. } => {
            check_deref_only_uses_in_expr(left, refs);
            check_deref_only_uses_in_expr(right, refs);
        }
        NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            check_deref_only_uses_in_expr(inner, refs);
        }
        NirExprKind::Assign { target, value } => {
            check_deref_only_uses_in_expr(target, refs);
            check_deref_only_uses_in_expr(value, refs);
        }
        NirExprKind::Call { args, .. } => {
            for arg in args {
                check_deref_only_uses_in_expr(&arg.expr, refs);
            }
        }
        NirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                check_deref_only_uses_in_expr(arg, refs);
            }
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            check_deref_only_uses_in_expr(receiver, refs);
            for arg in args {
                check_deref_only_uses_in_expr(&arg.expr, refs);
            }
        }
        NirExprKind::IndirectCall { callee, args } => {
            check_deref_only_uses_in_expr(callee, refs);
            for arg in args {
                check_deref_only_uses_in_expr(arg, refs);
            }
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            check_deref_only_uses_in_expr(functor, refs);
        }
        NirExprKind::Index { expr: inner, index } => {
            check_deref_only_uses_in_expr(inner, refs);
            check_deref_only_uses_in_expr(index, refs);
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            collect_deref_only_refs_in_block(block, refs);
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            check_deref_only_uses_in_expr(condition, refs);
            collect_deref_only_refs_in_block(then_branch, refs);
            if let Some(eb) = else_branch {
                collect_deref_only_refs_in_block(eb, refs);
            }
        }
        NirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                check_deref_only_uses_in_expr(&field.value, refs);
            }
        }
        NirExprKind::TupleLiteral { elements } => {
            for elem in elements {
                check_deref_only_uses_in_expr(elem, refs);
            }
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                check_deref_only_uses_in_expr(p, refs);
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            check_deref_only_uses_in_expr(inner, refs);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    check_deref_only_uses_in_expr(guard, refs);
                }
                check_deref_only_uses_in_expr(&arm.body, refs);
            }
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            check_deref_only_uses_in_expr(scrutinee, refs);
            for arm in arms {
                collect_deref_only_refs_in_block(arm, refs);
            }
            collect_deref_only_refs_in_block(default, refs);
        }
        NirExprKind::FieldAccess { expr: inner, .. } => {
            check_deref_only_uses_in_expr(inner, refs);
        }
        NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => {}
    }
}

/// Replace `*r` with the source expression for deref-only eliminable refs.
fn rewrite_deref_only_refs_in_block(
    block: &mut NirBlock,
    eliminable: &IndexMap<u32, NirExpr>,
) -> bool {
    // Remove Let bindings for eliminated refs
    let before = block.stmts.len();
    block.stmts.retain(|stmt| {
        if let NirStmtKind::Let { local_index, .. } = &stmt.kind {
            !eliminable.contains_key(local_index)
        } else {
            true
        }
    });
    let mut changed = block.stmts.len() < before;

    for stmt in &mut block.stmts {
        changed |= rewrite_deref_only_refs_in_stmt(stmt, eliminable);
    }
    changed
}

fn rewrite_deref_only_refs_in_stmt(
    stmt: &mut NirStmt,
    eliminable: &IndexMap<u32, NirExpr>,
) -> bool {
    match &mut stmt.kind {
        NirStmtKind::Let { value, .. } => rewrite_deref_only_refs_in_expr(value, eliminable),
        NirStmtKind::Expr(expr) => rewrite_deref_only_refs_in_expr(expr, eliminable),
        NirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_deref_only_refs_in_expr(v, eliminable)
            } else {
                false
            }
        }
        NirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let mut changed = rewrite_deref_only_refs_in_expr(condition, eliminable);
            changed |= rewrite_deref_only_refs_in_block(then_block, eliminable);
            if let Some(eb) = else_block {
                changed |= rewrite_deref_only_refs_in_block(eb, eliminable);
            }
            changed
        }
        NirStmtKind::Loop { body } | NirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_deref_only_refs_in_block(body, eliminable)
        }
        NirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_deref_only_refs_in_expr(v, eliminable)
            } else {
                false
            }
        }
        NirStmtKind::Continue => false,
        NirStmtKind::LetDestructure { value, .. } => {
            rewrite_deref_only_refs_in_expr(value, eliminable)
        }
    }
}

fn rewrite_deref_only_refs_in_expr(
    expr: &mut NirExpr,
    eliminable: &IndexMap<u32, NirExpr>,
) -> bool {
    // Replace `*r` with the source expression
    if let NirExprKind::Unary { op, expr: inner } = &expr.kind
        && matches!(op, NirUnaryOp::Deref)
        && let NirExprKind::Local { index, .. } = &inner.kind
        && let Some(source) = eliminable.get(index)
    {
        let span = expr.span;
        let type_id = expr.type_id;
        *expr = NirExpr {
            kind: source.kind.clone(),
            type_id,
            span,
        };
        return true;
    }

    // Recurse into subexpressions
    match &mut expr.kind {
        NirExprKind::Unary { expr: inner, .. } => {
            rewrite_deref_only_refs_in_expr(inner, eliminable)
        }
        NirExprKind::Binary { left, right, .. } => {
            let a = rewrite_deref_only_refs_in_expr(left, eliminable);
            let b = rewrite_deref_only_refs_in_expr(right, eliminable);
            a | b
        }
        NirExprKind::Cast { expr: inner, .. }
        | NirExprKind::VariantTag { expr: inner }
        | NirExprKind::VariantTest { expr: inner, .. }
        | NirExprKind::VariantPayload { expr: inner, .. }
        | NirExprKind::GlobalVarSet { value: inner, .. } => {
            rewrite_deref_only_refs_in_expr(inner, eliminable)
        }
        NirExprKind::Assign { target, value } => {
            let a = rewrite_deref_only_refs_in_expr(target, eliminable);
            let b = rewrite_deref_only_refs_in_expr(value, eliminable);
            a | b
        }
        NirExprKind::Call { args, .. } => {
            let mut changed = false;
            for arg in args {
                changed |= rewrite_deref_only_refs_in_expr(&mut arg.expr, eliminable);
            }
            changed
        }
        NirExprKind::CmRawCall { args, .. } => {
            let mut changed = false;
            for arg in args {
                changed |= rewrite_deref_only_refs_in_expr(arg, eliminable);
            }
            changed
        }
        NirExprKind::MethodCall { receiver, args, .. } => {
            let mut changed = rewrite_deref_only_refs_in_expr(receiver, eliminable);
            for arg in args {
                changed |= rewrite_deref_only_refs_in_expr(&mut arg.expr, eliminable);
            }
            changed
        }
        NirExprKind::IndirectCall { callee, args } => {
            let mut changed = rewrite_deref_only_refs_in_expr(callee, eliminable);
            for arg in args {
                changed |= rewrite_deref_only_refs_in_expr(arg, eliminable);
            }
            changed
        }
        NirExprKind::ClosureToCanonical { functor, .. } => {
            rewrite_deref_only_refs_in_expr(functor, eliminable)
        }
        NirExprKind::Index { expr: inner, index } => {
            let a = rewrite_deref_only_refs_in_expr(inner, eliminable);
            let b = rewrite_deref_only_refs_in_expr(index, eliminable);
            a | b
        }
        NirExprKind::Block(block) | NirExprKind::LabeledBlock { block, .. } => {
            rewrite_deref_only_refs_in_block(block, eliminable)
        }
        NirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let mut changed = rewrite_deref_only_refs_in_expr(condition, eliminable);
            changed |= rewrite_deref_only_refs_in_block(then_branch, eliminable);
            if let Some(eb) = else_branch {
                changed |= rewrite_deref_only_refs_in_block(eb, eliminable);
            }
            changed
        }
        NirExprKind::StructLiteral { fields, .. } => {
            let mut changed = false;
            for field in fields {
                changed |= rewrite_deref_only_refs_in_expr(&mut field.value, eliminable);
            }
            changed
        }
        NirExprKind::TupleLiteral { elements } => {
            let mut changed = false;
            for elem in elements {
                changed |= rewrite_deref_only_refs_in_expr(elem, eliminable);
            }
            changed
        }
        NirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_deref_only_refs_in_expr(p, eliminable)
            } else {
                false
            }
        }
        NirExprKind::Match { expr: inner, arms } => {
            let mut changed = rewrite_deref_only_refs_in_expr(inner, eliminable);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= rewrite_deref_only_refs_in_expr(guard, eliminable);
                }
                changed |= rewrite_deref_only_refs_in_expr(&mut arm.body, eliminable);
            }
            changed
        }
        NirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            let mut changed = rewrite_deref_only_refs_in_expr(scrutinee, eliminable);
            for arm in arms {
                changed |= rewrite_deref_only_refs_in_block(arm, eliminable);
            }
            changed |= rewrite_deref_only_refs_in_block(default, eliminable);
            changed
        }
        NirExprKind::FieldAccess { expr: inner, .. } => {
            rewrite_deref_only_refs_in_expr(inner, eliminable)
        }
        NirExprKind::Local { .. }
        | NirExprKind::IntLiteral { .. }
        | NirExprKind::FloatLiteral { .. }
        | NirExprKind::BoolLiteral(_)
        | NirExprKind::CharLiteral(_)
        | NirExprKind::StringLiteral(_)
        | NirExprKind::BytesLiteral(_)
        | NirExprKind::Null
        | NirExprKind::Unit
        | NirExprKind::GlobalVarGet { .. }
        | NirExprKind::EnumConstruct { .. } => false,
    }
}

/// Eliminate `let r = &struct_literal` bindings where all uses of r are `*r`.
///
/// This handles the pattern produced by inlining `into_iter()` on struct iterators:
/// ```text
/// let self: &StrUtf8ByteIter = &StrUtf8ByteIter { repr, used, index: 0 };
/// break label: *self;
/// ```
/// After elimination:
/// ```text
/// break label: StrUtf8ByteIter { repr, used, index: 0 };
/// ```
fn eliminate_deref_ref_pairs_in_function(func: &mut NirFunction) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    let mut refs: IndexMap<u32, DerefOnlyRef> = IndexMap::default();
    collect_deref_only_refs_in_block(body, &mut refs);

    // Only eliminate refs that: (a) are still eliminable, (b) used exactly once via *r
    let eliminable: IndexMap<u32, NirExpr> = refs
        .into_iter()
        .filter(|(_, info)| info.eliminable && info.use_count == 1)
        .map(|(idx, info)| (idx, info.source_expr))
        .collect();

    if eliminable.is_empty() {
        return false;
    }

    rewrite_deref_only_refs_in_block(body, &eliminable)
}

/// Eliminate unnecessary reference bindings in a single function.
fn eliminate_refs_in_function(func: &mut NirFunction, _type_table: &TypeTable) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    // Pre-scan: locals bound by more than one `Let` (the inliner reuses an
    // index when expanding mutually-exclusive branches that each rebind the
    // same temporary, e.g. an inlined function whose body has two `let g =
    // &variant_payload(...)` shadows for different match arms). Each `Let`
    // can carry a different referent, but `refs.insert` is keyed by
    // `local_index` and would silently overwrite — leaving every use of
    // the local substituted with the last-seen referent, even in branches
    // that initialised it differently. The cheapest correct response is
    // to refuse elimination on any rebound local.
    let rebound = find_rebound_locals(body);

    // Pass 1: Collect all ref bindings and analyze uses in a single traversal.
    let mut refs: IndexMap<u32, RefInfo> = IndexMap::default();
    analyze_refs_in_block(body, &rebound, &mut refs);

    // Filter to only eliminable bindings
    let eliminable: IndexMap<u32, RefInfo> = refs
        .into_iter()
        .filter(|(_, info)| info.eliminable)
        .collect();

    if eliminable.is_empty() {
        return false;
    }

    // Pass 2: Replace field accesses and remove dead bindings in a single traversal.
    transform_block(body, &eliminable);
    true
}

/// Eliminate unnecessary reference bindings in all functions.
///
/// Main entry point for reference elimination optimization.
pub fn eliminate_unnecessary_refs(project: &mut NirPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= eliminate_refs_in_function(&mut func, &type_table);
        changed |= eliminate_deref_ref_pairs_in_function(&mut func);
    }
    changed
}
