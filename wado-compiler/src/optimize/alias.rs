//! Per-function alias analysis used by const-fold's field-knowledge
//! tracking.
//!
//! [`build_alias_info`] computes the [`crate::tiri::AliasInfo`] that
//! [`crate::tiri::Interpreter`] consults whenever the const-fold
//! visitor calls `bind_field` / `invalidate_field` /
//! `invalidate_aliased_fields`. The structure of this module mirrors
//! the original `field_forward` pass (issue #1009) — a flow-
//! insensitive walk that seeds the `aliased` set from the function's
//! stable annotations plus a body scan for transient inlined-in
//! copies, builds the union-find of reference-typed `let dst = src`
//! aliases, and lifts the `stores_aliased_locals` set verbatim into
//! `untrackable`.
//!
//! [`build_value_copy_helpers`] bundles the synthesized
//! `$value_copy$T<id>` helpers exposed by
//! [`crate::tir::TirFunction::value_copy_type`] into the lookup the
//! visitor uses to recognize `Call(helper, [arg])` shapes that
//! transfer field knowledge across deep copies.
//!
//! [`recognize_value_copy`] is the single-call recognizer.

use crate::flat_package::FlatPackage;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::ModuleSource;
use crate::tir::{
    ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::tiri::AliasInfo;

/// Build the `(module_source, func_name) → struct type id` map of
/// synthesized `$value_copy$T<id>` helpers. The const-fold visitor
/// uses the map to recognize `Call(helper, [arg])` shapes that
/// transfer field knowledge from `arg` to the binding's target.
pub(super) fn build_value_copy_helpers(
    project: &FlatPackage,
) -> IndexMap<(ModuleSource, String), TypeId> {
    project
        .functions
        .iter()
        .filter_map(|f| {
            let f = f.borrow();
            f.value_copy_type()
                .map(|t| ((f.module_source.clone(), f.name.clone()), t))
        })
        .collect()
}

/// Compute per-function alias annotations for a function body.
///
/// Returns an [`AliasInfo`] populated as follows:
///
/// - `aliased`: seeds from `address_taken_locals` ∪
///   `stores_aliased_locals`, then augmented with locals whose
///   aliasing is visible only inside `body` (transient inlined-in
///   copies, captures, struct-field-stores). The seeded sets persist
///   across optimization iterations, so subsequent passes (`ref_elim`,
///   SROA) erasing the syntactic markers can't make us forget the
///   alias.
/// - `untrackable`: mirrors `stores_aliased_locals` exactly. An
///   inlined `stores`-annotated callee has stashed the reference
///   somewhere the analyzer cannot see, so any later read may
///   observe a mutation we never witnessed. The const-fold visitor
///   refuses to record fields for these locals (matches the OLD
///   WIR-level `const_forward` conservatism).
/// - `alias_groups`: union-find over reference-typed `let dst = src`
///   Local→Local copies in `body` (`Box<T>`, `Array<T>`, `&T`,
///   `&mut T`). Used to widen field-assignment invalidation: writing
///   `dst.field = …` drops the same field on every alias.
pub(super) fn build_alias_info(
    body: &TirBlock,
    address_taken_locals: &IndexSet<u32>,
    stores_aliased_locals: &IndexSet<u32>,
    type_table: &TypeTable,
) -> AliasInfo {
    let mut aliased = address_taken_locals.clone();
    for idx in stores_aliased_locals {
        aliased.insert(*idx);
    }
    let untrackable = stores_aliased_locals.clone();
    collect_aliased_in_block(body, &mut aliased);
    let alias_groups = collect_alias_groups(body, type_table);
    AliasInfo {
        aliased,
        untrackable,
        alias_groups,
    }
}

/// Recognize `Call(helper, [arg])` where `helper` is a synthesized
/// `$value_copy$T<id>` registered in the helpers map. Returns the
/// argument expression so the caller can copy `arg`'s field
/// knowledge to the binding's target.
pub(super) fn recognize_value_copy<'a>(
    expr: &'a TirExpr,
    helpers: &IndexMap<(ModuleSource, String), TypeId>,
) -> Option<&'a TirExpr> {
    let TirExprKind::Call { func, args, .. } = &expr.kind else {
        return None;
    };
    if args.len() != 1 {
        return None;
    }
    helpers
        .get(&(func.module_source.clone(), func.name.clone()))
        .map(|_| &args[0].expr)
}

// ──────────────────────────────────────────────────────────────────────────────
// Aliasing collection
// ──────────────────────────────────────────────────────────────────────────────

/// Build the alias-group map. Two locals end up in the same group
/// when they're connected by a chain of `let dst = src` Local→Local
/// copies of a reference-typed value (`Box<T>`, `Array<T>`, `&T`,
/// `&mut T`). For value-semantic types (plain structs, variants),
/// `let dst = src` will later be wrapped in `$value_copy$T(src)` by
/// the value-copy synthesis pass — `dst` is then a fresh allocation
/// and does not share storage with `src`, so we don't connect them.
///
/// The group is used to widen field-assignment invalidation: writing
/// `dst.field = ...` invalidates the same field of every alias.
fn collect_alias_groups(body: &TirBlock, type_table: &TypeTable) -> IndexMap<u32, IndexSet<u32>> {
    let mut edges: Vec<(u32, u32)> = Vec::new();
    collect_alias_edges_in_block(body, type_table, &mut edges);
    if edges.is_empty() {
        return IndexMap::default();
    }
    // Union-find via simple parent pointers; locals are sparse u32s.
    let mut parent: IndexMap<u32, u32> = IndexMap::default();
    fn find(parent: &mut IndexMap<u32, u32>, x: u32) -> u32 {
        let p = *parent.get(&x).unwrap_or(&x);
        if p == x {
            x
        } else {
            let r = find(parent, p);
            parent.insert(x, r);
            r
        }
    }
    for (a, b) in edges {
        parent.entry(a).or_insert(a);
        parent.entry(b).or_insert(b);
        let ra = find(&mut parent, a);
        let rb = find(&mut parent, b);
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    let keys: Vec<u32> = parent.keys().copied().collect();
    let mut groups: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
    for k in &keys {
        let r = find(&mut parent, *k);
        groups.entry(r).or_default().insert(*k);
    }
    let mut out: IndexMap<u32, IndexSet<u32>> = IndexMap::default();
    for set in groups.into_values() {
        for &k in &set {
            out.insert(k, set.clone());
        }
    }
    out
}

fn collect_alias_edges_in_block(
    block: &TirBlock,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    for stmt in &block.stmts {
        collect_alias_edges_in_stmt(stmt, type_table, edges);
    }
}

/// True when assigning a value of `type_id` from one local to another
/// produces aliasing — both names refer to the same heap object. This
/// is the case for reference types (`Box<T>`, `Array<T>`, `&T`,
/// `&mut T`). Value-semantic types (plain structs, variants) are
/// turned into a `$value_copy$T(src)` wrapper post-loop, so during
/// the loop a `let dst = src` edge between two value-typed locals
/// would over-merge groups that should stay separate.
///
/// `Box<T>` and `Array<T>` may surface either as `GenericInstance`
/// (pre-monomorphization) or as concrete monomorphized `Struct`
/// records carrying the original generic name in `base_name`.
fn type_creates_alias(type_id: TypeId, type_table: &TypeTable) -> bool {
    match type_table.get(type_id) {
        ResolvedType::Ref { .. } => true,
        ResolvedType::GenericInstance { name, .. } if name == "Box" || name == "Array" => true,
        ResolvedType::Struct { base_name, .. }
            if base_name.as_deref() == Some("Box") || base_name.as_deref() == Some("Array") =>
        {
            true
        }
        _ => false,
    }
}

fn collect_alias_edges_in_stmt(
    stmt: &TirStmt,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    match &stmt.kind {
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if let TirExprKind::Local { index: src, .. } = &value.kind
                && type_creates_alias(value.type_id, type_table)
            {
                edges.push((*local_index, *src));
            }
            collect_alias_edges_in_expr(value, type_table, edges);
        }
        TirStmtKind::LetDestructure { value, .. } => {
            collect_alias_edges_in_expr(value, type_table, edges);
        }
        TirStmtKind::Expr(expr) => {
            if let TirExprKind::Assign { target, value } = &expr.kind
                && let TirExprKind::Local { index: dst, .. } = &target.kind
                && let TirExprKind::Local { index: src, .. } = &value.kind
                && type_creates_alias(value.type_id, type_table)
            {
                edges.push((*dst, *src));
            }
            collect_alias_edges_in_expr(expr, type_table, edges);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_alias_edges_in_expr(v, type_table, edges);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_alias_edges_in_expr(condition, type_table, edges);
            collect_alias_edges_in_block(then_block, type_table, edges);
            if let Some(eb) = else_block {
                collect_alias_edges_in_block(eb, type_table, edges);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_alias_edges_in_block(body, type_table, edges);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_alias_edges_in_expr(scrutinee, type_table, edges);
            collect_alias_edges_in_block(then_block, type_table, edges);
            if let Some(eb) = else_block {
                collect_alias_edges_in_block(eb, type_table, edges);
            }
        }
        _ => {}
    }
}

fn collect_alias_edges_in_expr(
    expr: &TirExpr,
    type_table: &TypeTable,
    edges: &mut Vec<(u32, u32)>,
) {
    expr_for_each_child(expr, &mut |child| {
        collect_alias_edges_in_expr(child, type_table, edges);
    });
}

/// Walk `expr`'s direct sub-expressions. Used to recurse without
/// duplicating the case list at every call site.
fn expr_for_each_child(expr: &TirExpr, f: &mut dyn FnMut(&TirExpr)) {
    match &expr.kind {
        TirExprKind::Assign { target, value } => {
            f(target);
            f(value);
        }
        TirExprKind::Unary { expr: inner, .. }
        | TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => f(inner),
        TirExprKind::Binary { left, right, .. } => {
            f(left);
            f(right);
        }
        TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
            if let TirExprKind::MethodCall { receiver, .. } = &expr.kind {
                f(receiver);
            }
            for arg in args {
                f(&arg.expr);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                f(arg);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            f(callee);
            for arg in args {
                f(arg);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => f(functor),
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            f(inner);
            f(index);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            for stmt in &block.stmts {
                stmt_for_each_child(stmt, f);
            }
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            f(condition);
            for stmt in &then_branch.stmts {
                stmt_for_each_child(stmt, f);
            }
            if let Some(eb) = else_branch {
                for stmt in &eb.stmts {
                    stmt_for_each_child(stmt, f);
                }
            }
        }
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                f(&field.value);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                f(elem);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                f(p);
            }
        }
        TirExprKind::Closure { body, .. } => f(body),
        TirExprKind::Match { expr: inner, arms } => {
            f(inner);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    f(g);
                }
                f(&arm.body);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => f(value),
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            f(scrutinee);
            for arm in arms {
                for stmt in &arm.stmts {
                    stmt_for_each_child(stmt, f);
                }
            }
            for stmt in &default.stmts {
                stmt_for_each_child(stmt, f);
            }
        }
        TirExprKind::Local { .. }
        | TirExprKind::FuncRef { .. }
        | TirExprKind::GlobalVarGet { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::StringLiteral(_)
        | TirExprKind::BytesLiteral(_)
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Null
        | TirExprKind::Unit
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

fn stmt_for_each_child(stmt: &TirStmt, f: &mut dyn FnMut(&TirExpr)) {
    match &stmt.kind {
        TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => f(value),
        TirStmtKind::Expr(e) => f(e),
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                f(v);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            f(condition);
            for s in &then_block.stmts {
                stmt_for_each_child(s, f);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    stmt_for_each_child(s, f);
                }
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            for s in &body.stmts {
                stmt_for_each_child(s, f);
            }
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            f(scrutinee);
            for s in &then_block.stmts {
                stmt_for_each_child(s, f);
            }
            if let Some(eb) = else_block {
                for s in &eb.stmts {
                    stmt_for_each_child(s, f);
                }
            }
        }
        _ => {}
    }
}

/// Augment `out` with body-visible aliasing markers. Used in
/// addition to the function's stable `address_taken_locals` /
/// `stores_aliased_locals` to catch transient aliasings introduced
/// by inlining. Conservative — false positives only cost missed
/// optimizations.
fn collect_aliased_in_block(block: &TirBlock, out: &mut IndexSet<u32>) {
    for stmt in &block.stmts {
        collect_aliased_in_stmt(stmt, out);
    }
}

fn collect_aliased_in_stmt(stmt: &TirStmt, out: &mut IndexSet<u32>) {
    match &stmt.kind {
        // `let dst = src` (Local→Local copy) → both share storage.
        TirStmtKind::Let {
            local_index, value, ..
        } => {
            if let TirExprKind::Local { index: src, .. } = &value.kind {
                out.insert(*local_index);
                out.insert(*src);
            }
            collect_aliased_in_expr(value, out);
        }
        TirStmtKind::LetDestructure { value, .. } => collect_aliased_in_expr(value, out),
        TirStmtKind::Expr(expr) => {
            // `dst = src` (Assign Local→Local) — same aliasing.
            if let TirExprKind::Assign { target, value } = &expr.kind
                && let TirExprKind::Local { index: dst, .. } = &target.kind
                && let TirExprKind::Local { index: src, .. } = &value.kind
            {
                out.insert(*dst);
                out.insert(*src);
            }
            collect_aliased_in_expr(expr, out);
        }
        TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
            if let Some(v) = value {
                collect_aliased_in_expr(v, out);
            }
        }
        TirStmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            collect_aliased_in_expr(condition, out);
            collect_aliased_in_block(then_block, out);
            if let Some(eb) = else_block {
                collect_aliased_in_block(eb, out);
            }
        }
        TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
            collect_aliased_in_block(body, out);
        }
        TirStmtKind::IfLet {
            scrutinee,
            then_block,
            else_block,
            ..
        } => {
            collect_aliased_in_expr(scrutinee, out);
            collect_aliased_in_block(then_block, out);
            if let Some(eb) = else_block {
                collect_aliased_in_block(eb, out);
            }
        }
        _ => {}
    }
}

fn collect_aliased_in_expr(expr: &TirExpr, out: &mut IndexSet<u32>) {
    match &expr.kind {
        // `&local` or `&mut local` escapes a reference. The OLD
        // WIR-level pass distinguished by `stores` annotation, but
        // at TIR we don't have a callee-level view here — be
        // conservative and treat any Ref/MutRef on a Local as
        // alias-creating.
        TirExprKind::Unary { op, expr: inner } => {
            if matches!(op, TirUnaryOp::MutRef | TirUnaryOp::Ref)
                && let TirExprKind::Local { index, .. } = &inner.kind
            {
                out.insert(*index);
            }
            collect_aliased_in_expr(inner, out);
        }
        // Calls with mut args may stash the reference — alias.
        TirExprKind::Call { args, .. } | TirExprKind::MethodCall { args, .. } => {
            for arg in args {
                if arg.is_mut
                    && let TirExprKind::Local { index, .. } = &arg.expr.kind
                {
                    out.insert(*index);
                }
                collect_aliased_in_expr(&arg.expr, out);
            }
            if let TirExprKind::MethodCall { receiver, .. } = &expr.kind {
                // Auto-ref: receiver may be passed as `&mut self`.
                if let TirExprKind::Local { index, .. } = &receiver.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(receiver, out);
            }
        }
        TirExprKind::CmRawCall { args, .. } => {
            for arg in args {
                collect_aliased_in_expr(arg, out);
            }
        }
        TirExprKind::IndirectCall { callee, args, .. } => {
            collect_aliased_in_expr(callee, out);
            for arg in args {
                collect_aliased_in_expr(arg, out);
            }
        }
        TirExprKind::Closure { captures, body, .. } => {
            for capture in captures {
                out.insert(capture.outer_index);
            }
            collect_aliased_in_expr(body, out);
        }
        TirExprKind::Block(block) | TirExprKind::LabeledBlock { block, .. } => {
            collect_aliased_in_block(block, out);
        }
        TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_aliased_in_expr(condition, out);
            collect_aliased_in_block(then_branch, out);
            if let Some(eb) = else_branch {
                collect_aliased_in_block(eb, out);
            }
        }
        TirExprKind::Match { expr: inner, arms } => {
            collect_aliased_in_expr(inner, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    collect_aliased_in_expr(g, out);
                }
                collect_aliased_in_expr(&arm.body, out);
            }
        }
        TirExprKind::ClosureToCanonical { functor, .. } => {
            collect_aliased_in_expr(functor, out);
        }
        TirExprKind::Assign { target, value } => {
            collect_aliased_in_expr(target, out);
            collect_aliased_in_expr(value, out);
        }
        TirExprKind::Binary { left, right, .. } => {
            collect_aliased_in_expr(left, out);
            collect_aliased_in_expr(right, out);
        }
        TirExprKind::FieldAccess { expr: inner, .. }
        | TirExprKind::Cast { expr: inner, .. }
        | TirExprKind::TupleSpread { expr: inner }
        | TirExprKind::TupleZip { expr: inner }
        | TirExprKind::TypePackExpansion {
            call_expr: inner, ..
        }
        | TirExprKind::VariantTag { expr: inner }
        | TirExprKind::VariantTest { expr: inner, .. }
        | TirExprKind::VariantPayload { expr: inner, .. } => {
            collect_aliased_in_expr(inner, out);
        }
        TirExprKind::Index {
            expr: inner, index, ..
        } => {
            collect_aliased_in_expr(inner, out);
            collect_aliased_in_expr(index, out);
        }
        // Locals stored as field values of a fresh aggregate become
        // reachable through that aggregate; future reads through the
        // aggregate (including via captured-closure access or stored
        // references) may modify them. Mark aliased.
        TirExprKind::StructLiteral { fields, .. } => {
            for field in fields {
                if let TirExprKind::Local { index, .. } = &field.value.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(&field.value, out);
            }
        }
        TirExprKind::TupleLiteral { elements, .. } => {
            for elem in elements {
                if let TirExprKind::Local { index, .. } = &elem.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(elem, out);
            }
        }
        TirExprKind::VariantConstruct { payload, .. } => {
            if let Some(p) = payload {
                if let TirExprKind::Local { index, .. } = &p.kind {
                    out.insert(*index);
                }
                collect_aliased_in_expr(p, out);
            }
        }
        TirExprKind::GlobalVarSet { value, .. } => collect_aliased_in_expr(value, out),
        TirExprKind::Switch {
            scrutinee,
            arms,
            default,
            ..
        } => {
            collect_aliased_in_expr(scrutinee, out);
            for arm in arms {
                collect_aliased_in_block(arm, out);
            }
            collect_aliased_in_block(default, out);
        }
        _ => {}
    }
}
