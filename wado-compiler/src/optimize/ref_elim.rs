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
//! The algorithm uses a two-pass approach that processes ALL ref bindings
//! simultaneously, avoiding the O(K × N) cost of processing each binding
//! separately (where K = number of bindings, N = body size).
//!
//! Pass 1 (analyze): Single traversal to collect all `let r = &v` bindings
//!   and classify every use of each `r` as field-access-only or not.
//! Pass 2 (transform): Single traversal to replace eliminable field accesses
//!   and remove dead let statements.

use crate::flat_package::FlatPackage;
use crate::hashmap::IndexMap;
use crate::tir::{
    TirBlock, TirExpr, TirExprKind, TirFunction, TirStmt, TirStmtKind, TirUnaryOp, TypeTable,
};

/// Per-binding analysis state, keyed by the ref local index.
struct RefInfo {
    /// Local index of the original variable (`local_var` in `let r = &local_var`)
    target_local: u32,
    /// Name of the original variable
    target_name: String,
    /// True until a non-field-access use is found
    eliminable: bool,
}

/// Pass 1: Collect all ref bindings and analyze uses in a single traversal.
///
/// Walks the entire function body once, building an `IndexMap<u32, RefInfo>` for
/// every binding the pass can eliminate when all uses are field-access-only:
///
/// 1. `let r: &T = &v` / `let r: &mut T = &mut v` — fresh reference to a local.
///    Replace `r.field` with `v.field` and drop the binding.
///
/// 2. `let r: &mut T = s` (or `&T`) where `s` is itself a reference-typed
///    local **already tracked in `refs`** — the inlined shadow pattern,
///    e.g. the `let self = self;` that appears at the entry of an inlined
///    `&mut self` method body. Copy `s`'s `RefInfo` into `r` so `r.field`
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
fn analyze_refs_in_block(block: &TirBlock, refs: &mut IndexMap<u32, RefInfo>) {
    for stmt in &block.stmts {
        // Check if this statement defines a new ref binding
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
        {
            // Pattern (1): `let r = &v` / `let r = &mut v`
            if let TirExprKind::Unary { op, expr } = &value.kind
                && matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                && let TirExprKind::Local { index, name } = &expr.kind
            {
                refs.insert(
                    *local_index,
                    RefInfo {
                        target_local: *index,
                        target_name: name.clone(),
                        eliminable: true,
                    },
                );
            }
            // Pattern (2): `let r = s` where s is itself a tracked ref local
            // (the inlined shadow). Resolve transitively to s's target so
            // `r.field` can be replaced with `<root>.field` directly.
            else if let TirExprKind::Local { index, .. } = &value.kind {
                let resolved = refs.get(index).map(|info| RefInfo {
                    target_local: info.target_local,
                    target_name: info.target_name.clone(),
                    eliminable: info.eliminable,
                });
                if let Some(info) = resolved {
                    refs.insert(*local_index, info);
                }
            }
        }
        // Analyze uses within this statement
        analyze_uses_in_stmt(stmt, refs);
    }
}

fn analyze_uses_in_stmt(stmt: &TirStmt, refs: &mut IndexMap<u32, RefInfo>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => analyze_uses_in_expr(value, refs),
        TirStmtKind::Expr(expr) => analyze_uses_in_expr(expr, refs),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                analyze_uses_in_expr(v, refs);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            analyze_uses_in_expr(condition, refs);
            analyze_refs_in_block(then_block, refs);
            if let Some(eb) = else_block {
                analyze_refs_in_block(eb, refs);
            }
        }
        TirStmtKind::Loop { body } => analyze_refs_in_block(body, refs),
        TirStmtKind::LabeledBlock { block, .. } => analyze_refs_in_block(block, refs),
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            analyze_uses_in_expr(scrutinee, refs);
            analyze_refs_in_block(then_block, refs);
            if let Some(eb) = else_block {
                analyze_refs_in_block(eb, refs);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                analyze_uses_in_expr(v, refs);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => analyze_uses_in_expr(value, refs),
        TirStmtKind::TaskReturn { .. } => {}
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn analyze_uses_in_expr(expr: &TirExpr, refs: &mut IndexMap<u32, RefInfo>) {
    match &expr.kind {
        // Field access on a tracked ref local: this is the pattern we want to optimize.
        // The use is acceptable (field-access-only), so we DON'T mark it as non-eliminable.
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            if let TirExprKind::Local { index, .. } = &inner.kind
                && refs.contains_key(index)
            {
                // This is a field access on a tracked ref - acceptable use, skip.
                return;
            }
            // Not a field access on tracked ref - recurse normally
            analyze_uses_in_expr(inner, refs);
        }
        // Direct use of a tracked ref local (not through field access): non-eliminable
        TirExprKind::Local { index, .. } => {
            if let Some(info) = refs.get_mut(index) {
                info.eliminable = false;
            }
        }
        // Recursive cases
        TirExprKind::Binary { left, right, .. } => {
            analyze_uses_in_expr(left, refs);
            analyze_uses_in_expr(right, refs);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            analyze_uses_in_expr(inner, refs);
        }
        TirExprKind::Assign { target, value } => {
            analyze_uses_in_expr(target, refs);
            analyze_uses_in_expr(value, refs);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                analyze_uses_in_expr(&arg.expr, refs);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                analyze_uses_in_expr(arg, refs);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            analyze_uses_in_expr(receiver, refs);
            for arg in args {
                analyze_uses_in_expr(&arg.expr, refs);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            analyze_uses_in_expr(callee, refs);
            for arg in args {
                analyze_uses_in_expr(arg, refs);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            analyze_uses_in_expr(functor, refs);
        }
        TirExprKind::Index { expr: inner, index } => {
            analyze_uses_in_expr(inner, refs);
            analyze_uses_in_expr(index, refs);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            analyze_refs_in_block(block, refs);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            analyze_uses_in_expr(condition, refs);
            analyze_refs_in_block(then_branch, refs);
            if let Some(eb) = else_branch {
                analyze_refs_in_block(eb, refs);
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                analyze_uses_in_expr(&field.value, refs);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::MultiValueLiteral { elements } => {
            for elem in elements {
                analyze_uses_in_expr(elem, refs);
            }
        }
        TirExprKind::MultiValueProject { source, .. } => {
            analyze_uses_in_expr(source, refs);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                analyze_uses_in_expr(payload_expr, refs);
            }
        }
        TirExprKind::Closure { body, .. } => {
            analyze_uses_in_expr(body, refs);
        }
        TirExprKind::Match { expr: inner, arms } => {
            analyze_uses_in_expr(inner, refs);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    analyze_uses_in_expr(guard, refs);
                }
                analyze_uses_in_expr(&arm.body, refs);
            }
        }
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            analyze_uses_in_expr(scrutinee, refs);
            for arm in arms {
                analyze_refs_in_block(arm, refs);
            }
            analyze_refs_in_block(default, refs);
        }
        // Leaf nodes
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Pass 2: Replace field accesses and remove dead bindings in a single traversal.
///
/// For each eliminable binding `let r = &v`, replaces `r.field` with `v.field`
/// and removes the dead `let r` statement.
fn transform_block(block: &mut TirBlock, eliminable: &IndexMap<u32, RefInfo>) {
    // Remove dead let statements for eliminable bindings
    block.stmts.retain(|stmt| {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
            !eliminable.contains_key(local_index)
        } else {
            true
        }
    });

    for stmt in &mut block.stmts {
        transform_stmt(stmt, eliminable);
    }
}

fn transform_stmt(stmt: &mut TirStmt, eliminable: &IndexMap<u32, RefInfo>) {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => transform_expr(value, eliminable),
        TirStmtKind::Expr(expr) => transform_expr(expr, eliminable),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                transform_expr(v, eliminable);
            }
        }
        TirStmtKind::If {
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
        TirStmtKind::Loop { body } => transform_block(body, eliminable),
        TirStmtKind::LabeledBlock { block, .. } => transform_block(block, eliminable),
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            transform_expr(scrutinee, eliminable);
            transform_block(then_block, eliminable);
            if let Some(eb) = else_block {
                transform_block(eb, eliminable);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                transform_expr(v, eliminable);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => transform_expr(value, eliminable),
        TirStmtKind::TaskReturn { .. } => {}
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn transform_expr(expr: &mut TirExpr, eliminable: &IndexMap<u32, RefInfo>) {
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            // Check if the inner expression is a local that should be replaced
            if let TirExprKind::Local { index, .. } = &inner.kind
                && let Some(info) = eliminable.get(index)
            {
                **inner = TirExpr::new(
                    TirExprKind::Local {
                        index: info.target_local,
                        name: info.target_name.clone(),
                    },
                    inner.type_id,
                    inner.span,
                );
                return;
            }
            transform_expr(inner, eliminable);
        }
        TirExprKind::Binary { left, right, .. } => {
            transform_expr(left, eliminable);
            transform_expr(right, eliminable);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            transform_expr(inner, eliminable);
        }
        TirExprKind::Assign { target, value } => {
            transform_expr(target, eliminable);
            transform_expr(value, eliminable);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                transform_expr(&mut arg.expr, eliminable);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                transform_expr(arg, eliminable);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            transform_expr(receiver, eliminable);
            for arg in args {
                transform_expr(&mut arg.expr, eliminable);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            transform_expr(callee, eliminable);
            for arg in args {
                transform_expr(arg, eliminable);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            transform_expr(functor, eliminable);
        }
        TirExprKind::Index { expr: inner, index } => {
            transform_expr(inner, eliminable);
            transform_expr(index, eliminable);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            transform_block(block, eliminable);
        }
        TirExprKind::If {
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
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                transform_expr(&mut field.value, eliminable);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::MultiValueLiteral { elements } => {
            for elem in elements {
                transform_expr(elem, eliminable);
            }
        }
        TirExprKind::MultiValueProject { source, .. } => {
            transform_expr(source, eliminable);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(payload_expr) = payload {
                transform_expr(payload_expr, eliminable);
            }
        }
        TirExprKind::Closure { body, .. } => {
            transform_expr(body, eliminable);
        }
        TirExprKind::Match { expr: inner, arms } => {
            transform_expr(inner, eliminable);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    transform_expr(guard, eliminable);
                }
                transform_expr(&mut arm.body, eliminable);
            }
        }
        TirExprKind::Switch {
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
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Tracking state for `let r = &struct_or_tuple_literal` where r is only used as `*r`.
struct DerefOnlyRef {
    /// The source expression (struct or tuple literal) from `let r = &source_expr`
    source_expr: TirExpr,
    /// True until a non-deref use is found
    eliminable: bool,
    /// Number of times r is used as `*r`
    use_count: u32,
}

/// Collect `let r = &StructLiteral` / `let r = &TupleLiteral` candidates.
/// `MultiValueLiteral` (the multi-value-form tuple, default since Phase 4)
/// is recognised here too — both forms have identical heap-construction
/// shape at the deref-only-elision boundary.
fn collect_deref_only_refs_in_block(block: &TirBlock, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    for stmt in &block.stmts {
        if let TirStmtKind::Let {
            local_index, value, ..
        } = &stmt.kind
            && let TirExprKind::Unary { op, expr } = &value.kind
            && matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
            && matches!(
                expr.kind,
                TirExprKind::StructLiteral { .. }
                    | TirExprKind::TupleLiteral { .. }
                    | TirExprKind::MultiValueLiteral { .. }
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

fn check_deref_only_uses_in_stmt(stmt: &TirStmt, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } => check_deref_only_uses_in_expr(value, refs),
        TirStmtKind::Expr(expr) => check_deref_only_uses_in_expr(expr, refs),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                check_deref_only_uses_in_expr(v, refs);
            }
        }
        TirStmtKind::If {
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
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_deref_only_refs_in_block(body, refs);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            check_deref_only_uses_in_expr(scrutinee, refs);
            collect_deref_only_refs_in_block(then_block, refs);
            if let Some(eb) = else_block {
                collect_deref_only_refs_in_block(eb, refs);
            }
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                check_deref_only_uses_in_expr(v, refs);
            }
        }
        TirStmtKind::Continue => {}
        TirStmtKind::LetDestructure { value, .. } => check_deref_only_uses_in_expr(value, refs),
        TirStmtKind::TaskReturn { .. } => {}
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn check_deref_only_uses_in_expr(expr: &TirExpr, refs: &mut IndexMap<u32, DerefOnlyRef>) {
    match &expr.kind {
        // `*r` where r is a deref-only candidate: acceptable use
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: inner,
        } => {
            if let TirExprKind::Local { index, .. } = &inner.kind
                && let Some(info) = refs.get_mut(index)
            {
                info.use_count += 1;
                return;
            }
            check_deref_only_uses_in_expr(inner, refs);
        }
        // Any other use of r (bare local, field access on ref, etc.): disqualify
        TirExprKind::Local { index, .. } => {
            if let Some(info) = refs.get_mut(index) {
                info.eliminable = false;
            }
        }
        // Recurse into subexpressions
        TirExprKind::Unary { expr: inner, .. } => check_deref_only_uses_in_expr(inner, refs),
        TirExprKind::Binary { left, right, .. } => {
            check_deref_only_uses_in_expr(left, refs);
            check_deref_only_uses_in_expr(right, refs);
        }
        TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            check_deref_only_uses_in_expr(inner, refs);
        }
        TirExprKind::Assign { target, value } => {
            check_deref_only_uses_in_expr(target, refs);
            check_deref_only_uses_in_expr(value, refs);
        }
        TirExprKind::Call { args, .. } => {
            for arg in args {
                check_deref_only_uses_in_expr(&arg.expr, refs);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                check_deref_only_uses_in_expr(arg, refs);
            }
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            check_deref_only_uses_in_expr(receiver, refs);
            for arg in args {
                check_deref_only_uses_in_expr(&arg.expr, refs);
            }
        }
        TirExprKind::IndirectCall { callee, args } => {
            check_deref_only_uses_in_expr(callee, refs);
            for arg in args {
                check_deref_only_uses_in_expr(arg, refs);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            check_deref_only_uses_in_expr(functor, refs);
        }
        TirExprKind::Index { expr: inner, index } => {
            check_deref_only_uses_in_expr(inner, refs);
            check_deref_only_uses_in_expr(index, refs);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_deref_only_refs_in_block(block, refs);
        }
        TirExprKind::If {
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
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                check_deref_only_uses_in_expr(&field.value, refs);
            }
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::MultiValueLiteral { elements } => {
            for elem in elements {
                check_deref_only_uses_in_expr(elem, refs);
            }
        }
        TirExprKind::MultiValueProject { source, .. } => {
            check_deref_only_uses_in_expr(source, refs);
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                check_deref_only_uses_in_expr(p, refs);
            }
        }
        TirExprKind::Closure { body, .. } => {
            check_deref_only_uses_in_expr(body, refs);
        }
        TirExprKind::Match { expr: inner, arms } => {
            check_deref_only_uses_in_expr(inner, refs);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    check_deref_only_uses_in_expr(guard, refs);
                }
                check_deref_only_uses_in_expr(&arm.body, refs);
            }
        }
        TirExprKind::Switch {
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
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => {
            check_deref_only_uses_in_expr(inner, refs);
        }
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. } => {}
        TirExprKind::TemplateString { .. } => {
            unreachable!("TemplateString should be expanded before this phase")
        }
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
    }
}

/// Replace `*r` with the source expression for deref-only eliminable refs.
fn rewrite_deref_only_refs_in_block(
    block: &mut TirBlock,
    eliminable: &IndexMap<u32, TirExpr>,
) -> bool {
    // Remove Let bindings for eliminated refs
    let before = block.stmts.len();
    block.stmts.retain(|stmt| {
        if let TirStmtKind::Let { local_index, .. } = &stmt.kind {
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
    stmt: &mut TirStmt,
    eliminable: &IndexMap<u32, TirExpr>,
) -> bool {
    match &mut stmt.kind {
        TirStmtKind::Let { value, .. } => rewrite_deref_only_refs_in_expr(value, eliminable),
        TirStmtKind::Expr(expr) => rewrite_deref_only_refs_in_expr(expr, eliminable),
        TirStmtKind::Return { value } => {
            if let Some(v) = value {
                rewrite_deref_only_refs_in_expr(v, eliminable)
            } else {
                false
            }
        }
        TirStmtKind::If {
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
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            rewrite_deref_only_refs_in_block(body, eliminable)
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            let mut changed = rewrite_deref_only_refs_in_expr(scrutinee, eliminable);
            changed |= rewrite_deref_only_refs_in_block(then_block, eliminable);
            if let Some(eb) = else_block {
                changed |= rewrite_deref_only_refs_in_block(eb, eliminable);
            }
            changed
        }
        TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                rewrite_deref_only_refs_in_expr(v, eliminable)
            } else {
                false
            }
        }
        TirStmtKind::Continue => false,
        TirStmtKind::LetDestructure { value, .. } => {
            rewrite_deref_only_refs_in_expr(value, eliminable)
        }
        TirStmtKind::TaskReturn { .. } => false,
        TirStmtKind::VariadicForOf { .. } => {
            unreachable!("VariadicForOf should be expanded during monomorphization")
        }
    }
}

fn rewrite_deref_only_refs_in_expr(
    expr: &mut TirExpr,
    eliminable: &IndexMap<u32, TirExpr>,
) -> bool {
    // Replace `*r` with the source expression
    if let TirExprKind::Unary { op, expr: inner } = &expr.kind
        && matches!(op, TirUnaryOp::Deref)
        && let TirExprKind::Local { index, .. } = &inner.kind
        && let Some(source) = eliminable.get(index)
    {
        let span = expr.span;
        let type_id = expr.type_id;
        *expr = TirExpr {
            kind: source.kind.clone(),
            type_id,
            span,
        };
        return true;
    }

    // Recurse into subexpressions
    match &mut expr.kind {
        TirExprKind::Unary { expr: inner, .. } => {
            rewrite_deref_only_refs_in_expr(inner, eliminable)
        }
        TirExprKind::Binary { left, right, .. } => {
            let a = rewrite_deref_only_refs_in_expr(left, eliminable);
            let b = rewrite_deref_only_refs_in_expr(right, eliminable);
            a | b
        }
        TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. }
        | TirExprKind::GlobalVarSet { value: inner, .. } => {
            rewrite_deref_only_refs_in_expr(inner, eliminable)
        }
        TirExprKind::Assign { target, value } => {
            let a = rewrite_deref_only_refs_in_expr(target, eliminable);
            let b = rewrite_deref_only_refs_in_expr(value, eliminable);
            a | b
        }
        TirExprKind::Call { args, .. } => {
            let mut changed = false;
            for arg in args {
                changed |= rewrite_deref_only_refs_in_expr(&mut arg.expr, eliminable);
            }
            changed
        }
        TirExprKind::CmRawCall { args, .. } => {
            let mut changed = false;
            for arg in args {
                changed |= rewrite_deref_only_refs_in_expr(arg, eliminable);
            }
            changed
        }
        TirExprKind::MethodCall { receiver, args, .. } => {
            let mut changed = rewrite_deref_only_refs_in_expr(receiver, eliminable);
            for arg in args {
                changed |= rewrite_deref_only_refs_in_expr(&mut arg.expr, eliminable);
            }
            changed
        }
        TirExprKind::IndirectCall { callee, args } => {
            let mut changed = rewrite_deref_only_refs_in_expr(callee, eliminable);
            for arg in args {
                changed |= rewrite_deref_only_refs_in_expr(arg, eliminable);
            }
            changed
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            rewrite_deref_only_refs_in_expr(functor, eliminable)
        }
        TirExprKind::Index { expr: inner, index } => {
            let a = rewrite_deref_only_refs_in_expr(inner, eliminable);
            let b = rewrite_deref_only_refs_in_expr(index, eliminable);
            a | b
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            rewrite_deref_only_refs_in_block(block, eliminable)
        }
        TirExprKind::If {
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
        TirExprKind::StructLiteral { fields, .. } => {
            let mut changed = false;
            for field in fields {
                changed |= rewrite_deref_only_refs_in_expr(&mut field.value, eliminable);
            }
            changed
        }
        TirExprKind::TupleLiteral { elements } | TirExprKind::MultiValueLiteral { elements } => {
            let mut changed = false;
            for elem in elements {
                changed |= rewrite_deref_only_refs_in_expr(elem, eliminable);
            }
            changed
        }
        TirExprKind::MultiValueProject { source, .. } => {
            rewrite_deref_only_refs_in_expr(source, eliminable)
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                rewrite_deref_only_refs_in_expr(p, eliminable)
            } else {
                false
            }
        }
        TirExprKind::Closure { body, .. } => rewrite_deref_only_refs_in_expr(body, eliminable),
        TirExprKind::Match { expr: inner, arms } => {
            let mut changed = rewrite_deref_only_refs_in_expr(inner, eliminable);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    changed |= rewrite_deref_only_refs_in_expr(guard, eliminable);
                }
                changed |= rewrite_deref_only_refs_in_expr(&mut arm.body, eliminable);
            }
            changed
        }
        TirExprKind::Switch {
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
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        } => rewrite_deref_only_refs_in_expr(inner, eliminable),
        TirExprKind::Local { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::EnumConstruct { .. }
        | TirExprKind::TemplateString { .. } => false,
        TirExprKind::WithHandler { .. } | TirExprKind::Resume { .. } => {
            unreachable!(
                "WithHandler/Resume should be desugared by effect-dispatch synthesis before this phase"
            )
        }
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
fn eliminate_deref_ref_pairs_in_function(func: &mut TirFunction) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    let mut refs: IndexMap<u32, DerefOnlyRef> = IndexMap::default();
    collect_deref_only_refs_in_block(body, &mut refs);

    // Only eliminate refs that: (a) are still eliminable, (b) used exactly once via *r
    let eliminable: IndexMap<u32, TirExpr> = refs
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
fn eliminate_refs_in_function(func: &mut TirFunction, _type_table: &TypeTable) -> bool {
    let Some(body) = &mut func.body else {
        return false;
    };

    // Pass 1: Collect all ref bindings and analyze uses in a single traversal.
    let mut refs: IndexMap<u32, RefInfo> = IndexMap::default();
    analyze_refs_in_block(body, &mut refs);

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
pub fn eliminate_unnecessary_refs(project: &mut FlatPackage) -> bool {
    let mut changed = false;
    let type_table = project.type_table.borrow();
    for func_rc in &project.functions {
        let mut func = func_rc.borrow_mut();
        changed |= eliminate_refs_in_function(&mut func, &type_table);
        changed |= eliminate_deref_ref_pairs_in_function(&mut func);
    }
    changed
}
